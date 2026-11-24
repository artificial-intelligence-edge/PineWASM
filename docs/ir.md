# Intermediate Representations

The compiler moves a program through four representations. Each one
removes a class of concerns so the next stage can assume them solved:

| IR   | Crate                  | Knows about                          | No longer contains            |
|------|------------------------|--------------------------------------|-------------------------------|
| AST  | `pinewasm_syntax`   | surface syntax, spans                | —                             |
| HIR  | `pinewasm_hir`      | symbols, types, qualifiers, effects  | unresolved names and surface call/history syntax |
| MIR  | `pinewasm_mir`      | registers, basic blocks, terminators | expression trees, `if` nesting|
| CSIL | `pinewasm_csil`       | operand stack, absolute jump targets | registers, block structure    |

Every node and instruction at every level carries a `Span` (UTF-8 byte
range into the source), so diagnostics and runtime errors can always
point at source text.

During MIR-to-CSIL lowering, a single-definition, single-use slot can be
coalesced with its producer's stack value when the store and reload are
adjacent. Lowering removes both transfers and preserves the consumer's source
span and remapped branch targets. History, dynamic reads, persistence, and
written-flag observers prevent this optimization. It adds no execution-time
checks or stack capacity; instruction metering counts the resulting CSIL.

Immutable variable copies in the straight-line entry prefix also share their
source slot. This requires a single earlier source definition, a single copy
definition, and no read before that copy. A branch entering the copy's store,
or any history or persistence observer of its destination, keeps the copy
materialized. The source's computation and all side effects stay in place.

Copies anywhere else, including inside loops, share their source slot under a
flow-sensitive condition (`crates/pinewasm_csil/src/lower/flow_alias.rs`).
Inlining binds every user-function parameter to a hidden slot, so a helper
called in a loop body copies each argument on every iteration; these copies
are what the pass removes. The destination must have a single store (the
copy's), no history, dynamic, window, written-flag or `var`/`varip` observer,
and no branch into that store. At each of its reads, every path from the
entry must pass through the copy, with no store to the source after the most
recent copy. A parameter read with `[]` history therefore keeps its own series
lane. Arrays, maps and objects are references, so reading the caller's slot
reads the same handle. One round never resolves a copy of a removed copy; the
chain is resolved by the next round, for at most eight rounds. Validation
replays these passes on a serialized budget reference, which is untrusted, so
each round's work is bounded. A round makes no change when its blocks-by-copies
bit matrix would exceed 2^20 words, when its store-times-copy invalidations
would exceed 2^24, or when its fixed point needs more than 64 sweeps. Each bound
depends only on the round's input, so the compiler and the replay agree. Jump
threading resolves every chain end once, in linear time.

Jump threading then retargets every branch whose target is an unconditional
`JUMP` to the end of that chain, and removes an unconditional forward `JUMP`
when every instruction it skips is also removed. This includes a jump to the
next instruction (`crates/pinewasm_csil/src/lower/jump_thread.rs`). Both passes
only remove executed instructions or shorten executed paths. A recompiled
program therefore never charges more budget than before. The budget
reference records which pipeline produced the program (version 2 for the
current pipeline; see `docs/runtime.md`), so previously cached artifacts still
validate. The Bytecode
Interpreter and the native backends meter the same optimized CSIL, so the
reduction applies to both. The opt-in validated lowering checks
every flow-alias round and the jump-threading rewrite with an independent
checker. The flow-alias checker re-derives availability by searching backward
from each renamed read.

## AST

The AST (`pinewasm_syntax::ast`) is a faithful, span-carrying
representation of what was written. A `Program` carries its version/compiler
annotations and a list of top-level items: indicator, strategy, or library
metadata; imports; functions and methods; enum and user-defined-type
declarations; and statements. Statements cover typed or persistent
declarations (`var`/`varip`), reassignment and destructuring, expressions,
conditionals and switches, counted/collection/while loops, `break`, and
`continue`. Expressions retain literals, tuples, groups, calls and method
syntax, field access, ternaries, and `History { target, offset }` for `x[n]`.
The AST is untyped and unresolved; `pinewasm_parse` produces it totally
(malformed input yields diagnostics plus a best-effort tree).

## HIR

HIR is not a copy of the AST. `pinewasm_semantics::analyze` performs
name resolution, type checking, and qualifier checking, and produces a
`HirProgram` in which:

- **Every identifier is resolved.** A name is either a builtin series
  (the `BuiltinSeries` enum — OHLCV, `BarIndex`, the derived averages
  `Hl2`/`Hlc3`/`Ohlc4`/`Hlcc4`, the time/calendar series `Time`,
  `TimeClose`, `Timenow`, `LastBarIndex`, `LastBarTime`, `DayOfWeek`,
  `DayOfMonth`, `Month`, `Year`, `Hour`, `Minute`, `Second`, and the
  `series bool` `barstate.*` predicates `BarstateIsfirst`/`…Islast`/
  `…Ishistory`/`…Isrealtime`/`…Isconfirmed`/`…Isnew`/
  `…Islastconfirmedhistory`, dotted names resolved through the same series
  table) or a numbered `SymbolId` with a `SeriesSlot`. Unknown names are
  diagnostics (`E0201`), not nodes.
- **Every expression carries a `Type` and a `Qualifier`.** Types include
  scalar `Bool`/`Int`/`Float`/`String`/`Color`, `Void`, typed plot handles,
  reference collections (`Array`/`Matrix`/`Map`), nominal enums and objects,
  plus `Unknown` during inference and `Error` for cascade suppression.
  Qualifiers form the lattice `Const < Input < Simple < Series`, joined
  upward through operators.
- **Int → float coercions are explicit nodes.** Wherever a float is
  required but an int is given (mixed arithmetic/comparisons, mixed
  equality, `plot(...)` values), the checker wraps the int side in
  `HirExprKind::IntToFloat`, so later stages never re-infer promotion.
- **History references are desugared into offset reads.** `close[1]`
  becomes `HirExprKind::ReadBuiltin { series: Close, offset: 1 }`;
  `x[2]` becomes `HirExprKind::ReadVar { symbol, offset: 2 }`. A plain
  `close` or `x` is the same node with `offset: 0` — after HIR there is
  no distinction between "a variable" and "a history read at offset 0".
  Literal offsets take this compact path. An integer expression offset uses
  `ReadBuiltinDynamic`/`ReadVarDynamic`. Fractional runtime values from an
  int-typed expression truncate toward zero, so `-0.5` selects offset zero;
  a negative truncated value, `na`, or unavailable dynamic offset evaluates
  to `na` rather than indexing memory.
  History on a **compound** series expression (`(close - open)[1]`,
  `close[1][2]`) becomes
  `HirExprKind::MaterializeHistory { slot, expr, offset }`: a hidden
  series slot — owned by a synthetic `Symbol` named `%h0`, `%h1`, ...
  (marked `synthetic`, never persistent, invisible to name lookup) — that
  the site stores each time it executes and reads back at the offset.
  Hidden slots appear in slot tables and dumps like any other slot. In
  MIR the node lowers to the existing `StoreUserSeriesCurrent` +
  `ReadUserSeries` pair, and in CSIL to `STORE_USER_SLOT_CURRENT` +
  `READ_USER_SLOT` — no new instructions or opcodes (see the
  `compound_history` snapshots in the MIR and CSIL crates).
- **Pure builtin calls are one node, no hidden state.** `math.sqrt(...)`,
  `na(...)`, `nz(...)` become `HirExprKind::CallPure { func, args }`,
  where `func` is a `PureFunc` tag (one variant per function) and the
  argument list is fully explicit: the checker inserts `IntToFloat`
  coercions per the function's result rule (float-only functions coerce
  int arguments; `math.abs`/`math.sign` preserve the numeric type;
  `math.min`/`math.max` stay int only when every argument is int) and
  materializes optional-argument defaults as ordinary constants —
  `nz(x)` gains its typed zero (`0`, `0.0`, `false`) and `math.random()`
  gains its `0.0`/`1.0` bounds. Namespaced **constants** (`math.pi`,
  `math.e`, `math.phi`, `math.rphi`) resolve during analysis and appear
  in HIR as plain `ConstFloat` nodes; the **`na` literal** appears as
  `ConstNa` carrying the type its context supplied (see
  `docs/language-support.md`; positions with no expected type are `E0220`).
- **Stateful builtins use two coexisting lowering strategies.** The two
  functions below are **slot-based**: `ta.change(...)` and `ta.sma(...)`
  become `HirExprKind::TaCall { func, slot, source, length }` — each
  syntactic call site allocates its own hidden `%hN` slot (the same
  synthetic-symbol machinery as materialized history), its `source` is
  coerced to float, and its `length` carries the validated default integer
  (1..=100000, `E0219` otherwise; `ta.change` defaults to 1). An input-backed
  length additionally records a `ta_input_bindings` entry. In MIR,
  `ta.change` lowers to `StoreUserSeriesCurrent` + two `ReadUserSeries`
  (offsets 0 and `length`) + a float `Binary -` — no new instruction —
  while `ta.sma` lowers to `StoreUserSeriesCurrent` + the dedicated
  `WindowMean { dst, slot, length }` instruction (`window_mean` in dumps),
  which becomes the `WINDOW_MEAN` opcode in CSIL (see the `ta_change_sma`
  snapshots in the semantics, MIR, and CSIL crates). Every other stateful
  builtin (`fixnan`, `math.sum`, and the `ta.*` engine batch — see
  `docs/language-support.md`) is **engine-based**: the call site becomes
  `HirExprKind::CallTaState { func, state, args }`, where `state` is a
  `TaStateId` into the program's `ta_states` table
  (`TaStateDecl { func, params }`), allocated by semantics **in syntactic
  order**, one per call site. Compile-time const parameters (window
  lengths, `ta.alma`'s offset/sigma as `f64` bit patterns, boolean flags
  as 0/1, `ta.valuewhen`'s occurrence, `ta.linreg`'s offset) are baked
  into the declaration's `params`; only the runtime source operands
  (numeric sources coerced to float, bool conditions for
  `ta.barssince`/`ta.valuewhen`) remain as argument expressions. No hidden
  slot is allocated — the VM owns the per-site state. The slot-based pair
  keeps this lowering because it also exercises the hidden-slot machinery that
  compound history needs; everything new goes through the engine, whose ring
  buffers avoid burning one series slot (a full per-bar history vector)
  per call site.
- **The ternary conditional is a lazy expression node.**
  `cond ? a : b` becomes `HirExprKind::Ternary { cond, then_e, else_e }`:
  the condition is checked bool, the branch types are unified (identical
  types stand; mixed int/float coerces the int side; an `na` literal
  branch adopts the other branch's type; `E0207` otherwise), and the
  result qualifier joins the condition and both branches. Only the
  selected branch runs, so series-controlled branches
  warn `W0004` like `if`/`switch` expression branches.
- **`if`/`switch` expressions desugar to a hidden result slot — no new
  opcodes.** When an `if`/`switch` is consumed as a value, the checker
  allocates a hidden non-persistent `%hN` slot (the same synthetic-symbol
  machinery as materialized history), rewrites each branch so its (unified,
  coerced) value is stored into the slot (`HirStmt::Assign`, `init = false`),
  emits the resulting `HirStmt::If` chain **before** the enclosing statement,
  and replaces the expression with a `ReadVar { slot, offset: 0 }`. A
  `switch` first evaluates its subject **once** into its own hidden slot,
  then lowers to an if/else-if chain of `==` comparisons against that slot
  (subjectless switches use the arm conditions directly); an unmatched
  switch stores `na` of the result type. Because the desugaring produces
  only ordinary `HirStmt::If`/`Assign`/`ReadVar` nodes, these forms are
  **lazy** (only the taken branch runs, so `W0004` *does* fire for `ta.*` in
  a branch) and keep the stack-height-0 block-boundary invariant — the
  existing `HirStmt::If` MIR lowering and CSIL validator merge rules apply
  unchanged.
- **Effect calls are desugared into effect statements and metadata.** Numeric
  plots, color-only visuals, fills, logs, alerts, and strategy orders become
  typed `HirStmt` variants with stable output slots/site metadata. Top-level
  indicator/strategy/library declarations become program metadata. Typed plot
  and hline handles are values only for the consumers that accept them (such
  as `fill`); other void effects cannot be used as scalar expressions.
- **Assignment allocates storage.** `x = expr` declares a symbol and
  assigns it a `SeriesSlot`, so `x[1]` works on later bars. `var x = expr`
  declares the same way but marks the `Symbol` `persistent`. A `:=`
  reassignment does **not** declare: the resolver resolves it to the
  existing symbol in the nearest enclosing scope, and declarations and
  reassignments share `HirStmt::Assign` (distinguished by its `init` flag,
  which is true only at the declaration site).

`HirStmt::If` is still structured (condition, then-branch, else-branch
as statement lists); an `else if` chain arrives as a nested `If` in the
else branch.

## MIR

`pinewasm_mir::lower` makes execution order and control flow
explicit. Lowering is infallible: it requires an error-free
`HirProgram`, which the pipeline guarantees by gating on diagnostics.

- **Expression trees flatten to instruction sequences.** Sub-expressions
  are emitted in evaluation order (post-order); each instruction writes
  a fresh virtual register (`ValueId`). Every register is defined
  exactly once and, because trees are flattened in order, used
  immediately.
- **Literal int-to-float conversions are resolved once.** An `IntToFloat`
  over `ConstInt` becomes a float constant using the VM's same `i64 as f64`
  conversion; a `ConstNa` stays `na`. Input overrides, history reads, and
  integer expressions still execute at runtime, including checked integer
  overflow before widening. The emitted instruction count is consequently
  smaller for literal conversions. Budgets and profiler counts describe the
  emitted program, so recompilation can change those counts; existing
  serialized programs keep their original instruction accounting.
- **Literal color constructors become color constants.** `color.new` and
  `color.rgb` are folded when every argument is a literal (including the
  checker's explicit literal widening). This preserves the runtime's channel
  rounding, clamping, and `na` propagation. Dynamic arguments still execute,
  even if another argument is `na`; folding does not hide their errors or
  freeze input overrides or history. Nested calls and arithmetic are not
  evaluated by this limited folder.
- **`if` becomes basic blocks.** Each `if` produces a diamond: the
  current block ends with `Terminator::Branch { cond, then_block,
  else_block }`; both branch blocks end with `Terminator::Jump` to a
  join block. Straight-line programs are a single block ending in
  `Terminator::Return`.
- **Loops become block graphs with back-edges — zero new opcodes.** A
  `while` lowers to a header block (evaluate the condition, `Branch` to
  body or exit), a body block that ends with a `Jump` **back** to the
  header, and an exit block. A `for i = start to end [by step]` evaluates
  `start` and `step` once, and reevaluates `end` before each iteration. The
  initial `end` is used only to choose the direction of an
  implicit step. These values use hidden non-persistent slots; the loop stores
  `start` into the counter's slot and signs a positive explicit `by` magnitude
  according to the initial bounds. A nonpositive explicit step branches to the existing
  `runtime.error` instruction before entering the loop body. The header
  recomputes `step >= 0 ? counter <= end : counter >= end` with the existing
  comparison and `Select` instructions, then branches to the body or exit.
  The body leads to a **step** block that advances the counter and `Jump`s
  back to the header. The loop-carried counter, `end`, and `step` are ordinary
  user series slots; `break` lowers to a `Jump` to the exit block and
  `continue` to a `Jump` to the step block (`for`) or header (`while`).
  Because loops reuse `Jump`/`Branch` and the existing arithmetic/
  comparison/`Select` instructions, **no new MIR instruction or CSIL
  opcode is introduced**, and the CSIL validator's worklist converges over
  the back-edges exactly as for forward diamonds (`AbstractTy` only ever
  joins upward toward `Any`, and loop headers are entered with the same
  operand stack on every edge). See the `for_loop`/`while_loop` snapshots in the MIR and CSIL
  crates and `nested_loops_validator_worklist_converges` in
  `pinewasm_csil`.
- **`var` declarations become initialize-once diamonds.** A persistent
  declaration ends the current block with
  `Terminator::BranchIfSlotWritten { slot, written_block,
  unwritten_block }` (shown as `branch_if_slot_written` in dumps): the
  `unwritten_block` evaluates the initializer and stores it, the
  `written_block` skips it, and both jump to a join block. The branch
  consults the VM's per-slot written flag (see `docs/runtime.md`), so the
  initializer runs exactly once per run. A `:=` reassignment is just a
  plain `StoreUserSeriesCurrent`.
- **Effects are dedicated instructions,** not calls:
  `ReadBuiltinSeries`, `ReadUserSeries`, `StoreUserSeriesCurrent`,
  `Plot`, and `Discard` (bare expression statements keep an explicit
  discard so the stack lowering can emit a balancing `Pop`). Pure
  builtin calls become the register-consuming
  `CallPure { dst, func, args }` instruction (`call_pure` in dumps),
  which takes its arguments in evaluation order and defines one result;
  the `na` literal is an ordinary `Const` with value `na`. Engine-backed
  stateful calls become `CallTa { dsts, state, args }` (`call_ta` in
  dumps), consuming only the runtime source operands and defining
  `dsts.len()` results — one for a scalar call, or the tuple's arity for a
  tuple-returning one (three for `ta.macd`/`ta.bb`/`ta.kc`/`ta.dmi`, two for
  `ta.supertrend`); `MirMetadata` carries the `ta_states` declaration table through to
  CSIL. A **tuple destructuring** declaration (`[a, b, c] = ta.macd(...)`)
  lowers to one multi-result `CallTa` followed by one
  `StoreUserSeriesCurrent` per target, emitted in reverse so the top-of-stack
  last result lands in the last target and target `i` receives tuple element
  `i`. Tuples never exist as a runtime value: this is the only construct that
  consumes a multi-result `CallTa` (a user tuple function's body binds each
  element to an ordinary `StoreUserSeriesCurrent` instead).
- **Ternaries branch.** `HirExprKind::Ternary` lowers the condition,
  then ends the block with `Branch { cond, then_block, else_block }`. Each
  arm block evaluates its expression, stores it with
  `StoreUserSeriesCurrent` into a fresh non-persistent `%tN` result slot
  (appended after the HIR slots), and jumps to a join block that reads the
  slot with `ReadUserSeries { offset: 0 }`, so only the selected arm runs.
  A `na` condition takes the else arm. When both arms are literals
  (optionally under a unary operator other than negating `i64::MIN`),
  lowering keeps the single `Select { dst, cond, lhs, rhs }` instruction
  (`select` in dumps): `dst` becomes `lhs` when `cond` is true, `rhs` when it
  is false or `na`.
- **Numeric flavor is explicit.** Arithmetic `Binary`/`Unary`
  instructions carry an `is_int` flag (shown as `(int)` in dumps) so CSIL
  lowering can select the checked integer opcodes without re-inferring
  types, and HIR's `IntToFloat` coercions become a dedicated
  `IntToFloat` instruction (`int_to_float` in dumps).
- **Metadata rides along.** `MirMetadata` carries the indicator title,
  the series-slot table (`Vec<SlotDef>`, where
  `SlotDef { name, persistent }` records each slot's source name and
  whether it is a `var` slot — `slot 0 `n` var` in dumps), and plot titles
  for debug dumps and runtime output.

MIR is the last IR where structured analysis is convenient, and the
direct source of the stack CSIL.

## CSIL

CSIL (the **Chart Script Intermediate Language**) is a compact, versioned,
stack-based bytecode executed once per bar by the VM. It is the
compiler's durable artifact: hosts can cache the serialized program and
re-run it without recompiling.

### Header and versioning policy

Every `CsilProgram` begins with a `CsilHeader`:

| Rust field (`JSON key`)       | Value    | Meaning                                             |
|-------------------------------|----------|-----------------------------------------------------|
| `magic`                       | `"CSIL"` | identifies a CSIL program                           |
| `csil_version` (`il_version`) | `2`      | bytecode format version                             |
| `language_version`            | `6`      | source-language semantics implemented (the implemented subset of the Pine v6 language) |
| `runtime_abi_version`         | `0` or `1` | VM ↔ CSIL contract; programs with TA input bindings require `1`, so older readers reject them instead of silently running defaults. |

The stable CSIL v2 JSON representation keeps the `il_version` key so existing
serialized programs remain readable. Rust APIs and PineWASM-facing terminology
use `csil_version`; deserialization also accepts that spelling.

The validator rejects any program whose magic is not `CSIL` or whose
`csil_version` is not the supported `CSIL_VERSION` (currently 2). Format changes
bump `csil_version`; changes to opcode semantics or the slot model bump
`runtime_abi_version`; and `language_version` records which source-language
semantics the program encodes. A compact binary encoding can be added behind
the same header without changing semantics.

The `ORDER` operand layouts are fixed per primitive kind and cover the strategy
order arguments PineWASM supports (see the `ORDER` table below). Validators
reject artifacts that predate these layouts rather than reinterpreting their
shorter order stacks.

The string feature (`+` concatenation and the `str.*` namespace) uses the
single `STR_CONCAT` opcode for string `+` (na-propagating). Scalar
`str.*` functions reuse the `CALL_PURE` opcode (`PureFunc` tags:
`str.tostring`, `str.tonumber`, `str.format`, `str.length`,
`str.substring`, `str.replace`, `str.replace_all`, `str.contains`,
`str.startswith`, `str.endswith`, `str.pos`, `str.upper`, `str.lower`,
`str.trim`, `str.repeat`, `str.match`, `str.format_time`); string literals
lower to `PUSH_STR` and the constant pool. `str.split`
returns `array<string>`, so it uses `CALL_COLLECTION` with the `StrSplit` tag.
String work is metered by inspected or
projected UTF-8 bytes and reserves the cumulative run-wide string budget; it
is not a flat-cost pure surface.

The **color** type uses the single `PUSH_COLOR r g b t` opcode. `#RRGGBB` /
`#RRGGBBAA` literals and the 17 `color.*` named constants both lower to
`PUSH_COLOR`; the color literal's optional alpha byte becomes Pine transparency
`t = 100 * (1 − alpha / 255)` at parse time, so the runtime carries
`t` in `0..=100` (0 = opaque). Colors are a distinct abstract-stack type that
only equality and the value-generic opcodes accept; there is no color pool
(the four channels fit in the instruction). The color-producing functions
(`color.new`/`color.rgb`/`color.from_gradient`) and the accessors
(`color.r/g/b/t`) reuse the `CALL_PURE` opcode through `PureFunc` tags,
**no new opcode**, with `Color` operand/result requirements
in the abstract-stack validator. The constructors clamp out-of-range channels
(`0..=255`) and transparency (`0..=100`) rather than aborting; the accessors
emit float-returning tags; the integer-returning tags remain readable.
Transparency is an `F64Bits` percentage in CSIL;
whole percentages retain integer JSON encoding. The packed native RGBT ABI
admits only proven whole percentages, rerouting other valid
colors to the interpreter.

The **dynamic history offset** feature (`x[n]` with `n` a series/simple `int`
expression) uses two opcodes, `READ_BUILTIN_DYNAMIC source` and
`READ_USER_SLOT_DYNAMIC slot`, both popping an `int` offset from the stack (the
literal-offset `READ_BUILTIN`/`READ_USER_SLOT` fast paths are separate). The VM maps
the popped offset to a bar index (`na`, negative, or before bar 0 → `na`, never a
panic) after checking the program's effective `max_bars_back` ceiling; a
non-negative runtime offset beyond that ceiling raises
`HistoryBufferLimitExceeded`. See
docs/pine-v6-coverage.md (§1.9) and docs/runtime.md.

The **`input.*` interface** uses the single `READ_INPUT input` opcode and
the program's `inputs` declaration table. Each `input.int/float/bool/string/color/
source(...)` declaration records one `InputDecl { id, kind, title, tooltip,
inline, group, confirm, display, active, defval, constraints }` in `inputs`
(in syntactic order; `id` is the entry index) and
lowers its every read to `READ_INPUT id`. The VM resolves the value from the
host override map (`RunOptions.inputs`, keyed by id) or the declared default;
`input.source` reads the selected builtin series' current-bar sample. Its
validated domain is exactly open/high/low/close/hl2/hlc3/ohlc4/hlcc4; the four
derived averages retain their first-class `BuiltinSeries` identities through
HIR, CSIL validation, interpreter execution, and native AOT source lanes. A host
may provide a matching external plot lane in `RunOptions.external_sources`;
that is runtime data, never an `InputDecl` default or constraint. Native AOT
uses its own source lanes for the eight declared defaults and reroutes a
selected external lane to the interpreter. The
`input.time`/`input.price`/`input.enum` kinds and the generic
`input(defval, ...)` overload reuse the **same** `READ_INPUT` opcode and
`InputDecl` shape — no new opcode. `input.enum` records `InputKind::Enum` with
an `InputValue::Enum { name, index, member, title }` default/options entry; its read pushes the member's
variant index (an `Int`), and the generic `input()` is a pure analyzer-side
desugar to the matching namespaced kind (nothing distinct reaches the CSIL).
`active` carries its declared-default result, referenced prior-input ids, an
optional direct bool-dependency fast path, and a host-evaluable scalar
expression tree. It is presentation metadata only; no opcode is introduced.
`inputs` deserializes as empty when absent.

**Collections** (the reference-typed `array<T>` core) use a single generic
`CALL_COLLECTION { func, argc }` opcode, following the
`CALL_PURE` pattern: `func` is a `CollectionFunc` tag (`array.new_float`,
`array.push`, `array.get`, ...), not one opcode per function. Like `CALL_PURE`
it pops `argc` operands (the last on top) and pushes **exactly one** result;
void mutators (`array.push`/`set`/`clear`/`unshift`/`insert`/`reverse`/`fill`/
`sort`) push a placeholder `na` (a `POP`/`DISCARD` balances the stack). Its operands and
result may be **references** — a `Value::Ref(HandleId)` into the per-run heap
(see [runtime.md](runtime.md) *Collections*). Because the receiver handle is the
first argument, it is deepest on the stack.

A data table keyed by `CollectionFunc` (in `pinewasm_hir`) gives each
function its arity bounds (`argc_bounds`), abstract operand categories
(`operand_reqs` — `Ref`/`Elem`/`Int`/`Str`, top-of-stack first), abstract
result (`result`), and metering class (`is_linear_cost`). The validator
consults `argc_bounds`/`operand_reqs`/`result` for its structural and
abstract-stack checks (a new abstract type, `Ref`, never unifies with a
scalar). The **metering** is: O(1) operations cost 1 unit
(`push`/`pop`/`get`/`set`/`size`/`first`/`last`); O(n) operations that scan or
duplicate the array cost their element count
(`from`/`new_*`/`slice`/`copy`/`concat`/`fill`/`reverse`/`clear`/`shift`/
`unshift`/`insert`/`remove`/`includes`/`indexof`/`lastindexof`/`join`, plus the
aggregate, transform, and sort functions `sum`/`avg`/`min`/`max`/`median`/`range`/`mode`/
`variance`/`stdev`/`abs`/`standardize`/`sort`/`sort_indices`, each charged the
receiver's element count). The numeric aggregates skip `na` elements
and return `float` regardless of element type (an `array<int>` sum is a float);
`array.sum` of an empty array is `0`, while `avg`/`min`/`max`/`median`/`range`/
`mode` of an empty (or all-`na`) array are `na`. The `mode` tie-break chooses the smallest of the equally-frequent values.
`variance`/`stdev` use the population
form (denominator `n`); `sort`/`sort_indices` order `na` elements last
and take an `order.ascending` (0) / `order.descending` (1) argument.
The boolean reductions `array.every`/`array.some` also reduce over every element
(linear cost) but return `bool`, not a callback result; they take no
function argument. An element counts as "true" when it is a `true` bool or a
non-zero, non-`na` number; `array.every` is `true` iff all elements are true and
`array.some` iff at least one is. An empty array is the vacuous case (`every` →
`true`, `some` → `false`); these `na`/empty rules are how PineWASM defines them. Both accept a bool or numeric receiver.
Constructing/growing an array charges the new elements against the run's
`max_collection_elements` budget (`RunOptions`, serde default 100 000) and
raises a structured `CollectionLimitExceeded` runtime error past the cap, never
a panic. Out-of-bounds indices → `CollectionIndexOutOfBounds`; a method on an
`na` receiver → `NaMethodReceiver`.

The text dump renders `CALL_COLLECTION array.push 2`. Method syntax
(`a.push(x)`) and `<type>` generics (`array.new<float>()`) are front-end sugar:
the method form desugars to the namespace call with the receiver as the
first argument, and the generic form rewrites to the typed constructor name
(`array.new_float`), so both lower to the *same* `CALL_COLLECTION` — the opcode
does not change and the sugar produces byte-identical CSIL to the base form.

**Collections of user-defined types** ride the same opcode. `array.new<MyType>()`
lowers to a `CALL_COLLECTION array.new_object 3` whose three operands are
`(size, initial, type_tag)`; `type_tag` is a `ConstInt` encoding the element
UDT's `type_id` via `ElemType::to_tag` (object element types are
`1000 + type_id`, above the scalar range 0..=4, and decoded by
`ElemType::from_tag`). `map.new<K, MyType>()` reuses the existing `map.new`
tag with the value type tag set to the object tag. Elements are `Value::Ref`
object handles; a field mutation of an element retrieved via `array.get` /
`map.get` is visible through the collection (reference semantics). This is a
new `CollectionFunc` tag (and heap state
deserializes as empty when absent).

**The `map.*` surface rides the same opcode** — no new opcode. The
map functions are additional `CollectionFunc` tags in the same data table:
`map.put`/`get`/`contains`/`remove`/`size`/`clear`/`keys`/`values`/`copy`/
`put_all`, plus the constructor `map.new`. The parser rewrites an all-scalar
`map.new<K, V>()` to its typed registry name, which the checker materializes
as `CALL_COLLECTION map.new 2`, where the two operands are **`ConstInt` type
tags** — the key type tag (deepest) and the value type tag (top) — encoded by
`ElemType::to_tag` (Float=0, Int=1, Bool=2, String=3, Color=4, and an object
value type as `1000 + type_id`) and decoded by the VM with `ElemType::from_tag`. The registry exposes one surface name per
key/value pairing (`map.new_string_float`, `map.new_int_int`, ...), each of
which lowers to the same `map.new` tag with its two tags. `map.put` returns the
**previous value** for the key (or `na`), so — unlike `array.push` — it is
value-producing (a bare `map.put(...)` statement raises the `W0002` discard
warning); `map.clear`/`map.put_all` are void mutators (push a placeholder `na`).
`map.keys`/`map.values` allocate and push a fresh `array<K>`/`array<V>` handle in
**insertion order** (maps are insertion-ordered); `map.get`/`map.remove`
return the value type or `na`. **Metering:** `map.size` is O(1) (cost 1); the
scanning/duplicating operations `put`/`get`/`contains`/`remove`/`clear`/`keys`/
`values`/`copy`/`put_all` are O(n), charged the receiver's entry count (a Vec-
backed lookup scans entries, so their cost is honest), and `put_all` additionally
charges the source map's entries. Constructing/growing a map charges its new
entries against the same `max_collection_elements` budget; a method on an `na`
map is a structured `NaMethodReceiver` error, never a panic.

**The `matrix.*` surface also rides the same opcode** — no new opcode. The matrix functions are additional `CollectionFunc` tags in the same
data table (`matrix.get`/`set`/`rows`/`columns`/`elements_count`/`fill`/`copy`/
`add_row`/`add_col`/`remove_row`/`remove_col`/`swap_rows`/`swap_columns`/
`reverse`/`transpose`/`reshape`/`submatrix`/`concat`/`sum`/`diff`/`mult`/`det`/
`inv`/`trace`/`pow`/`kron`/`avg`/`min`/`max`/`median`/`mode`/the `is_*`
predicates, plus the constructor `matrix.new`). The typed constructor
`matrix.new_<T>(rows, columns, initial_value)` lowers to
`CALL_COLLECTION matrix.new 4`, whose four operands are
`(rows, columns, initial_value, elem_ty_tag)` — the
element-type tag (top of stack) is a `ConstInt` encoded by `ElemType::to_tag`,
the same scheme `map.new` uses. Cells are stored **row-major** in
`HeapObj::Matrix { elem_ty, rows, cols, items }`. The elementwise/linear-algebra
ops that take a second operand (`sum`/`diff`/`mult`) accept **either** a matrix
handle **or** a scalar — the CSIL-level operand category is `Elem` (any value), and
the checker owns the static disambiguation (same-shape matrix, or a scalar of the
element type). Structural `transpose`/`submatrix` results preserve `matrix<T>`;
`row`/`col` preserve `array<T>`; and explicit-index `remove_row`/`remove_col`
mutate the receiver while returning the removed `array<T>`. Those arrays are
shallow for reference-valued cells: their `Value::Ref` handles are copied or
moved without cloning the target object. The four `CollectionFunc` tags keep their one-result stack discipline.
`row`/`col` keep the cell type instead of widening to float, and the two
removals return the detached array rather than a placeholder `na`. A removal
whose result is discarded stays stack-compatible. The numeric
`sum`/`diff`/`mult`/`inv`/`pow`/`kron` operations allocate a **fresh float
matrix**; `det`/`trace`/the aggregates reduce to a scalar `float`; the `is_*`
predicates push a `bool`. **Metering:** every
`matrix.*` op is in the linear-cost class, charged the receiver's cell count
(`matrix.new` charges `rows * columns`; `matrix.concat`/`matrix.kron` additionally
charge the second matrix). **Structured errors, never panics:** out-of-bounds
cell coordinates and row/column indices → `CollectionIndexOutOfBounds`; a shape
disagreement (`mult` inner dimensions, elementwise `sum`/`diff` shapes, `concat`
column counts, non-square `det`/`inv`, bad `reshape` element count) → the new
`MatrixDimensionMismatch`; a singular `inv` → the new `MatrixSingular`; a method
on an `na` matrix → `NaMethodReceiver`; growth past the budget →
`CollectionLimitExceeded`. Linear-algebra kernels use LU / Gauss-Jordan with
partial pivoting; `matrix.eigenvalues`/`eigenvectors` use the cyclic Jacobi
algorithm for **symmetric** matrices and `matrix.pinv` the normal equations for
**full-rank** matrices (non-symmetric eigen / rank-deficient pinv →
`MatrixDimensionMismatch`/`MatrixSingular`, never a panic). Optional removal
indices, in-place `matrix.sort`, numeric type-preserving overloads, and the
broader numerical algorithms remain deferred, along with the `matrix.fill`
invalid-range runtime errors and the `matrix.mult` matrix-vector overload (see
the coverage matrix).

**User-defined types** use **four** small opcodes — `NEW_OBJECT
{ type_id, field_count }`, `COPY_OBJECT { type_id }`, `GET_FIELD { field_idx }`,
`SET_FIELD { field_idx }` — plus the program's `object_types` table (one
`ObjectTypeDecl { name, field_count }` per declared `type`, in declaration order;
`type_id` is the entry index). `object_types` deserializes as empty when
absent. Objects are **reference values**:
`NEW_OBJECT` allocates a `HeapObj::Object { type_id, fields }` on the same per-run
`Heap` the collections use and pushes a `Value::Ref`, so assignment copies the
handle (aliasing observable), `var` objects persist across bars, and realtime
rollback restores them via the heap snapshot — all for free from the existing
reference machinery. `Name.new(...)` lowers to `NEW_OBJECT` after the checker
materializes each field's initializer (omitted fields become the declared default
or `PUSH_NA`); `Name.copy(obj)` lowers to `COPY_OBJECT`; field read `obj.field`
to `GET_FIELD`, field write `obj.field := value` to `SET_FIELD`. The validator
adds a new abstract type only implicitly — object handles reuse the `Ref`
category, so `NEW_OBJECT`/`COPY_OBJECT` push `Ref`, `GET_FIELD` pushes `Any` (the
checker owns field typing), and `SET_FIELD` consumes a `Ref` plus any value. A
field access on an `na` object is the structured `NaFieldAccess` runtime error
(never a panic). **Metering:** `NEW_OBJECT`/`COPY_OBJECT` cost the field count;
`GET_FIELD`/`SET_FIELD` cost 1.

**Drawing objects** reuse the user-defined-type object machinery for storage
and field access. Each drawing kind (`line`/`label`/`box`/`chart.point`, plus
`table`/`polyline`/`linefill`) is a synthetic
object type seeded into `object_types`, so `<ns>.new(...)` lowers to `NEW_OBJECT`,
setters to `SET_FIELD`, getters to `GET_FIELD`, and `.copy` to `COPY_OBJECT`.
The drawing namespaces add **no new opcodes** — they are new `DrawKind`
variants flowing through the existing `REGISTER_DRAWING`/`DELETE_DRAWING`
tags, so `table.new`/`polyline.new`/`linefill.new` and their setters/getters
lower exactly like `line`/`box`. (`polyline.points` is an `array<chart.point>`
handle, `linefill.line1`/`line2` are `line` handles — ordinary reference-valued
fields.)
Only two opcodes carry a `DrawKind` tag:
`REGISTER_DRAWING { kind }` (pop the freshly built handle, insert it into the
kind's per-run live registry — deterministic **oldest-first GC** drops the front
entry once the list exceeds the kind's max count, default 50 — then push the same
handle) and `DELETE_DRAWING { kind }` (pop a handle, remove it from the registry,
and push a placeholder `na` for the void mutator's discard; deleting an absent id
is a no-op, and so is an `na` receiver). `chart.point` is a
coordinate object and is **not** registered/GC'd. The live registry (and its
oldest-first eviction) is snapshotted/restored with the heap for realtime
rollback, and its final state is serialized into the run outcome's `drawings`
section (see browser.md). **Metering:** both cost 1 unit.

**Table cells** add three dedicated void-effect opcodes for the
per-cell 2D grid, which is stored **out-of-line** keyed by the table handle
(not in the table object's declared fields, because cells are addressed by
`(column, row)` and bounded by `columns × rows`, validated at `table.new`).
`SET_CELL { attrs }` pops one value per attribute (last on top), then `row`,
`column`, then the table handle, and writes each [`CellAttr`] of the cell at
`(column, row)` — the runtime of both `table.cell(...)` (multiple attributes)
and each `table.cell_set_<attr>(...)` (one attribute). `CLEAR_CELLS` and
`MERGE_CELLS` each pop `end_row`, `end_column`, `start_row`, `start_column`,
then the table handle, and respectively remove or merge-flag every cell in the
inclusive range. An `na` receiver raises `NaFieldAccess`; an out-of-range
`(column, row)` raises `CollectionIndexOutOfBounds` — never a panic. Each pushes
a placeholder `na` for the void mutator's discard. The cell grid rides along
with the drawing registry through realtime snapshot/rollback and is serialized
under `drawings.tables[i].cells` (see runtime.md). **Metering:** `SET_CELL` costs `attrs.len()` units (min 1); the range
ops cost 1 unit.

The **event-stream** opcodes are also dedicated effects, appending to the
run outcome's `logs`/`alerts` sections rather than pushing a value.
`LOG { level, argc }` pops `argc` operands (the format string was pushed first,
then substitution args in order — last arg on top), builds the message with the
same placeholder rules as `str.format`, and appends a `LogEntry` (`level`, the
current `bar`, `message`). `ALERT` pops `freq` (top) then `message` and appends
an `AlertEvent`, honoring the `alert.freq_*` firing rules (`freq_once_per_bar*`
dedups once per bar PER CALL SITE, keyed by the `ALERT` instruction's pc
(a distinct `alert()` call is a distinct `ALERT` opcode); `freq_all` fires every call).
`ALERT_CONDITION` pops `message` (top), `title`, then a bool `condition`, and
appends an event only when `condition` is true (freq `once_per_bar_close`). All
three push nothing; the `logs`/`alerts` vectors and the per-bar alert flag are
snapshotted/restored for realtime rollback. **Metering:** `LOG` costs `argc`
units; `ALERT`/`ALERT_CONDITION` cost 1 unit each.

The **pivot-point levels** opcode (PPL, `ta.pivot_point_levels(type, anchor)`)
is the single `READ_PIVOT_LEVELS { system, anchor_seconds }` op.
It sidesteps the array-returning-stateful
engine plumbing entirely by riding the RESAMPLE machinery, exactly like
`READ_SECURITY`: the VM resamples the chart feed to `anchor_seconds` (the same
`pinewasm_vm::resample` used by `request.security`, memoized once per run per
anchor), reads the anchor period STRICTLY BEFORE the current forming one
(`lookahead_off` — never the forming bucket, so no future information leaks),
computes the [`PivotSystem`]'s levels from that period's High/Low/Close, and
allocates a fresh `HeapObj::Array { elem_ty: Float, items }`, pushing the
`Value::Ref` (like `READ_SECURITY_LOWER_TF`, which also builds an
`array<float>`). The result is the 11-slot order
`[P, R1, S1, R2, S2, R3, S3, R4, S4, R5, S5]`; a level a system omits is `na`
(Fibonacci's R4/S4/R5/S5). Before any prior anchor period exists (the first
period), the array is EMPTY. `system` (a [`PivotSystem`] tag folded from the
const `type` string) and `anchor_seconds` (folded from the const `anchor`
timeframe, default `"1D"`) are baked at analysis (a non-const or unknown system
is an `E0247` diagnostic, never reaching the CSIL). The abstract-stack validator
treats it like the other array-builders (pushes `Ref`). Allocating the array
charges its elements against the run's `max_collection_elements` budget (a
structured `CollectionLimitExceeded` past the cap, never a panic).
**Metering:** costs 1 unit (the resample is memoized per run).

The **timeframe-time** function forms
`time(timeframe, session, timezone, bars_back, timeframe_bars_back)` and
`time_close(...)` lower through the existing `CALL_PURE` opcode as
`PureFunc::TimeOpen` / `PureFunc::TimeClose`. Each call carries five runtime
operands (timeframe string, session string, timezone string, chart-bars-back int,
timeframe-bars-back int). The VM validates those strings at runtime, applies the
same RESAMPLE machinery as `request.security`, filters by the requested session
in the requested fixed-offset timezone, and pushes the selected bucket's open or
close epoch ms. Invalid runtime strings or out-of-range offsets yield `na`.
`time_close()` clips to the session close when the selected bucket extends past
the session window.

The baked-timeframe `READ_TIME_TF { close, tf_seconds }` opcode remains in
the CSIL for compatibility with serialized programs that lack runtime arguments.
It uses the same helper with empty session/timezone and zero offsets.
**Metering:** both paths cost 1 unit (the resample is memoized per run).

### Program structure

An `CsilProgram` contains:

- `constants` — string constant pool (`PUSH_STR` indexes into it;
  strings are interned during lowering).
- `series_slots` — user series slot definitions, indexed by slot number:
  `SlotDef { name, persistent }`. Persistent (`var`) slots carry the
  previous bar's final value forward at the start of each bar and are
  rendered as `SERIES_SLOT i "name" var` in the text dump.
- `plots` — plot slot declarations (`CsilPlot { title }`), indexed by plot
  number.
- `visuals` — color-only `bgcolor`/`barcolor` declarations, indexed by visual
  number. Their per-bar color lanes are separate from numeric plots.
- `fills` — typed plot/hline endpoint pairs from `fill(...)`, indexed by fill
  number. A corresponding color lane records the band color for each bar.
- `ta_input_bindings` — optional run-constant integer dependencies. Each entry
  carries an input id, a sign inversion flag, and a target: `Sma { slot }`,
  `Change { slot }`, or `State { state, param }`. Validation checks integer
  input types, bounds, unique targets, and agreement between the declared
  input default and the target's default. The VM admits the selected state
  sizes before specializing an owned clone; AOT reads bindings from the input table, validates
  bounds, installs actual engine parameters, and meters actual window sizes.
  Serialized programs with bindings require runtime ABI 1. Literal-only
  programs keep ABI 0, and programs without this optional field still load.
- `ta_states` — engine-backed stateful call-site declarations
  (`TaStateDecl { func, params }`), indexed by the state id that
  `CALL_TA { state }` carries; rendered as `TA_STATE i func [params]` in
  the text dump. One entry per syntactic call site, in syntactic order;
  the validator requires each entry to be referenced by **at most one**
  instruction (per-call-site state is never shared). `params` holds the
  function's compile-time const parameters in signature order: window
  lengths and int arguments (`ta.pivothigh`/`ta.pivotlow`'s
  leftbars/rightbars, ...) as plain `i64`s, boolean flags as 0/1, and
  float parameters (`ta.alma`'s offset and sigma, `ta.sar`'s
  start/inc/max, `ta.bbw`/`ta.kcw`'s mult, the percentile functions'
  percentage) as raw `f64` bit patterns (`f64::to_bits` reinterpreted as
  `i64`). On the wire each param is encoded as a **decimal string**, not a
  JSON number: a bit pattern like `2.0f64.to_bits()` is `2^62`, far above
  JavaScript's `Number.MAX_SAFE_INTEGER` (`2^53`), so emitting it as a number
  breaks JS hosts (`serde_wasm_bindgen` rejects it). The string form survives
  JSON exactly and round-trips through both `serde_json` and the wasm boundary;
  deserialization also accepts a plain JSON number. `params` is opaque to
  hosts (only `inputs`/`metadata` are host-readable).
- `inputs` — `input.*` declarations (`InputDecl { id, kind, widget, title, tooltip,
  inline, group, confirm, display, active, defval, constraints }`), indexed by
  the input id that `READ_INPUT { input }` carries;
  rendered as `INPUT i input.kind title=… defval=…` in the text dump. One entry
  per syntactic `input.*` call, in syntactic order. The validator checks that
  each `id` equals its index and that `defval` (and every constraint value)
  matches the declaration's `kind`. Serialized for hosts and read by the VM to
  resolve the override-or-default value; see
  [`browser.md`](browser.md#inputs-host-overridable-defaults).
- `object_types` — user-defined-type layouts (`ObjectTypeDecl { name,
  field_count }`), indexed by the `type_id` carried by object construction and
  copy instructions.
- `functions` — `functions[0]` is `main`, executed once per bar.
  **User-defined functions do not add CSIL functions:** a `name(...) => …`
  definition is inlined at every call site during semantic analysis
  (per-call-site instantiation — see
  [`language-support.md`](language-support.md#functions-user-defined) and
  [`runtime.md`](runtime.md)), so no function or call node reaches HIR, MIR, or
  CSIL and no new opcode is introduced. **Imported library functions reuse
  the same path:** `compile_with_libraries` resolves each `import` against a
  provided source map, registers the library's exported functions under
  `alias.name` keys, and an `alias.fn(...)` call inlines exactly like a local
  function — so linking a library likewise adds no CSIL function and no opcode.
  Compound `request.security` expressions also do not add functions: their
  checked subset lowers to the serializable `HtfExpr` tree carried by
  `READ_SECURITY_EXPR`, which the VM evaluates over a resampled context (see
  [`runtime.md`](runtime.md#requestsecurity-and-secondary-contexts)). The
  vector remains extensible for future independently executable CSIL functions,
  but current compiler output contains only `main`. Each `CsilFunction` carries `code`, a
  `source_map` with **exactly one span per instruction**, and `max_stack`, the
  maximum operand-stack depth computed during lowering and re-checked by the
  validator.
- `metadata` — the program title from `indicator(...)`, plus the
  `strategy` field: `Some(StrategyInfo { … })` const-folded from a
  `strategy(...)` declaration, `None` for indicators. It carries the
  honored broker parameters (initial capital, qty type/value, commission,
  slippage, pyramiding, ...); the VM reads it to seed the run outcome's
  `strategy` section. No opcode accompanies the declaration — it is pure
  metadata, so `csil_version` and `runtime_abi_version` are both unchanged.

The **`request.security`** call site uses the single
`READ_SECURITY { selector, source, htf_seconds, gaps_on, lookahead_on }` opcode.
It backs the secondary-context feature (docs/runtime.md): the analyzer
restricts `expression` to a plain builtin price series and bakes the requested
higher-timeframe length (`htf_seconds`) and the two `barmerge.*` modes into the
instruction. For `selector=None`, the VM resamples the chart feed once per site
(memoized), evaluates `source` over each HTF bar, and back-maps the per-bucket
result onto the chart bars. For `selector=Some("security/<symbol>/<timeframe>/<source>")`,
the instruction reads the optional host table `MarketData::request_data`
directly and defaults to `na`.

Compound higher-timeframe expressions use `READ_SECURITY_EXPR { expr, htf_seconds, gaps_on,
lookahead_on }` for a COMPOUND `expression` (e.g. `ta.sma(close, 2)` or
`(high + low) / 2` inside `request.security`). Rather than compiling the sub-AST into a separate
`CsilFunction`, the analyzer lowers the checked sub-expression into a compact,
serializable `HtfExpr` tree (OHLCV reads, `barstate.islast` and
`barstate.isrealtime`, literals, unary neg, binary arithmetic/comparison,
slot-based `ta.sma`/`ta.change`, and engine-backed `ta.ema`, `ta.hma`,
`ta.vwma`, and `ta.atr` with per-site state-slot indices in the secondary
context's OWN state space).
Boolean expression roots use `HtfExpr::BoolResult` to preserve the Pine type at
the VM and typed-CSIL boundary; the secondary numeric cells encode false/true
as exact `0.0`/`1.0` values.
`HtfExpr::InputSource` and `HtfExpr::TaInput` preserve direct source and length
input references; the VM binds their run-selected values before costing and
evaluating the same-feed tree. The VM runs that
tree over the resampled bars as a mini per-context "bar-loop": one pass in bucket
order, advancing each `ta` slot's source history so the secondary context's state
is independent of `main`'s. `HtfExpr::History` gives each history site its own
secondary-context ring and supports direct source history plus history of a
stateful expression. This is functionally the multi-function-execution model
the resample layer was built for, expressed as a self-contained tree the VM
interprets rather than a second bytecode function. Constructs outside the
documented tree subset stay `E0244`; external-symbol compound trees instead
lower to a canonical `READ_REQUEST SecurityExpression` host selector.
An `HtfExpr::Binary`
carries an `int_result` flag: the secondary context evaluates on `f64`, but an
integer-typed `%` keeps integer semantics; `/` is statically int for two int
operands but keeps its fraction (`7 / 2 == 3.5`), and `+`/`-`/`*` on integers
are already exact. The
validator bounds every `HtfExpr::Ta.ta_slot` to the tree's `ta`-node count (the
analyzer assigns slots densely as `0..N`), so a malformed serialized program
cannot drive the per-context history vector out of range (no overflow, OOB, or
oversized allocation).

The scalar request interfaces use the single `READ_REQUEST { func, selector }` opcode for
the scalar `request.*` external-data interfaces
(`request.dividends`/`splits`/`earnings`/`financial`/`economic`/`currency_rate`).
Unlike
`READ_SECURITY` these
have no second feed and no sub-expression: the analyzer folds the call's constant
string arguments into a stable `selector` key and the VM reads the per-bar value
from the optional host table `MarketData::request_data` (default `na`). They are
modeled as INTERFACES — the language surface (signature, typing) is real; the
data sourcing is a host concern — so the whole feature is **one opcode plus an
optional-table lookup**. `request.seed` uses the same host-table interface.
The dedicated `READ_SECURITY_LOWER_TF` opcode builds its `array<float>` from
`MarketData::security_lower_tf_data`; this network-free VM does not invent
intrabar decomposition, so a missing host entry yields an empty array.

### Opcode table (version 2)

Conventions: binary operators pop `rhs` then `lhs` and push the result;
`offset` operands are history offsets in bars (0 = current bar); jump
targets are absolute instruction indices within the function. Stack
effect is written pops→pushes, matching `CsilInstr::resolve_stack_effect()`.
Semantics are as implemented by `pinewasm_vm`.

| Opcode | Operands | Stack | Semantics |
|---|---|---|---|
| `PUSH_NA` | — | 0→1 | Push `na`. |
| `PUSH_BOOL b` | bool literal | 0→1 | Push a bool. |
| `PUSH_I64 v` | i64 literal | 0→1 | Push an int (exact 64-bit value). |
| `PUSH_F64 v` | f64 literal | 0→1 | Push a float. The validator requires `v` to be finite (use `PUSH_NA` for unavailable values). |
| `PUSH_STR id` | constant-pool index | 0→1 | Push string `constants[id]`; out-of-range is `InvalidProgram` at runtime and rejected by the validator. |
| `PUSH_COLOR r g b t` | three `u8` RGB channels and an `F64Bits` percentage | 0→1 | Push the color `{r, g, b, t}` with finite transparency in `0..=100`, including fractions; invalid percentages fail validation. Whole percentages keep the integer JSON encoding. Colors are a distinct operand type (`Color` in the abstract stack): they never unify with numeric or string operands, and only equality (`CMP_EQ`/`CMP_NE`) and the value-generic opcodes (`DUP`/`POP`/`SELECT`) accept them. Backs `#RRGGBB[AA]` literals and the `color.*` named constants. |
| `DUP` | — | 1→2 | Duplicate the top of stack. |
| `POP` | — | 1→0 | Discard the top of stack (balances `Discard`ed expressions). |
| `READ_BUILTIN source offset` | builtin series, offset | 0→1 | Push the builtin series value at bar `bar − offset`. `na` if that underflows bar 0. Non-finite input data is normalized to `na` at the read. The int-typed sources (`BAR_INDEX` and the time/calendar series, per `BuiltinSeries::is_int()`) push **int**; the `series bool` `barstate.*` predicates (per `BuiltinSeries::is_bool()`) push **bool**; the OHLCV and derived-average series push floats. Time/calendar, `barstate.*`, and `strategy.*` getters are all `BuiltinSeries` variants and reuse this opcode. The driver supplies the bar-state flags. The VM reads strategy getters from a per-bar broker snapshot (frozen at the start of each bar after prior-bar fills) rather than market data, so history (`strategy.position_size[1]`) reads the prior bar's snapshot; trade-count getters push **int**, while monetary and size getters push **float** (a flat/no-trades metric surfaces as `na`). Strategy getters are compile-gated to `strategy()` scripts (`E0241`). |
| `READ_USER_SLOT slot offset` | slot, offset | 0→1 | Push the user series value at bar `bar − offset`; `na` if before bar 0 or the cell was never written. |
| `READ_BUILTIN_DYNAMIC source` | builtin series | 1→1 | Pop an `int` bar offset and push builtin `source` read at `bar − offset` — the runtime-offset form of `READ_BUILTIN` (`close[n]` with `n` a series/simple `int` expression). A finite fractional runtime offset truncates toward zero (`-0.5` becomes zero). A negative truncated offset (future bar), an offset before bar 0, an `na`/non-numeric value, or an unrepresentable offset pushes `na` (never a panic). A non-negative offset greater than `ProgramMetadata.max_bars_back` raises `HistoryBufferLimitExceeded`. Result type mirrors `READ_BUILTIN` (int for `is_int()` sources, bool for `is_bool()`, float otherwise). |
| `READ_USER_SLOT_DYNAMIC slot` | slot | 1→1 | Pop an `int` bar offset and push the user `slot` value at `bar − offset` — the runtime-offset form of `READ_USER_SLOT`. A finite fractional runtime offset truncates toward zero. Out-of-range (negative after truncation / before bar 0 / unwritten), unrepresentable, or `na`/non-numeric offsets push `na`, except a non-negative offset greater than `ProgramMetadata.max_bars_back` raises `HistoryBufferLimitExceeded`. Compound history with a dynamic offset (`(close − open)[n]`) lowers to `STORE_USER_SLOT_CURRENT` into the site's hidden slot followed by `READ_USER_SLOT_DYNAMIC`. |
| `STORE_USER_SLOT_CURRENT slot` | slot | 1→0 | Pop a value into the current bar's cell of `slot`, and set the slot's written flag (persisted across bars; carry-forward never sets it). |
| `READ_INPUT input` | input id | 0→1 | Push the value of `inputs[input]`, resolved from the host override map (`RunOptions.inputs`, keyed by id) or the declared default. The pushed abstract type is fixed by the declaration's kind (`Int`/`F64`/`Bool`/`Str`/`Color`, `F64` for `input.source`, `Int` for `input.time` and `input.enum`, `F64` for `input.price`); the validator reads it from the `inputs` table. An out-of-range `input` is rejected by the validator and is `InvalidProgram` at runtime. For `input.source` the value is the selected builtin series' current-bar sample. **Metering:** costs a push (no window scan). |
| `READ_SECURITY selector source htf_seconds gaps_on lookahead_on` | optional host selector, builtin series, HTF length (s), two mode flags | 0→1 | Push the `request.security` value for the current bar. With no selector, read builtin series `source` over the chart feed resampled to `htf_seconds`, mapped back onto this chart bar honoring `gaps_on`/`lookahead_on`; `gaps_on` yields `na` between HTF closes and `lookahead_on` reads the still-forming bucket (repaints). With a selector, read `MarketData::request_data[selector]` for the current bar (`security/<symbol>/<timeframe>/<source>`), using `na` for missing/non-finite/out-of-range host samples. Always a float. Only a float-producing `source` reaches here (the analyzer rejects the rest). The VM memoizes same-feed back-mapped series per `(source, htf_seconds, gaps_on, lookahead_on)` site, so same-feed resampling runs once per run. **Metering:** costs 1 unit. |
| `READ_SECURITY_EXPR expr htf_seconds gaps_on lookahead_on` | `HtfExpr` tree, HTF length (s), two mode flags | 0→1 | Evaluate the checked compound tree once per resampled bucket in a secondary context with independent TA and history slots, then map the results back to chart bars. Supports direct and stateful-expression history through `HtfExpr::History`; always pushes float-or-`na`. **Metering:** costs 1 unit after the per-run memo is built. |
| `READ_REQUEST func selector` | `request.*` interface tag, selector string | 0→1 | Push the current bar's value from `MarketData::request_data` for scalar external-data calls, `request.seed`, or an external-symbol compound security expression. Selectors are canonical keys; corporate-action and financial requests may encode `syminfo.ticker`/`tickerid` for resolution against the current chart symbol per run. Missing, short, or non-finite host data yields `na`. The deprecated `request.quandl` tag is an accepted stub that always yields `na`. **Metering:** costs 1 unit. |
| `BUILD_FOOTPRINT selector` | selector string | 0→1 | Build and push the current bar's order-flow `footprint` object (the `footprint` namespace; backs `request.footprint()`). The VM materializes the object from the optional per-run host table `MarketData::footprint_data`, keyed by `selector` (a compile-time key `footprint/…` folded from the call's constant string arguments): it allocates a `volume_row` heap object per host row, the trailing `rows` array (`array<volume_row>`), then the `footprint` object (seven scalar order-flow fields + `rows`), and pushes its handle (`Ref`). A miss (the default), or a `null`/absent bar entry, yields an object whose scalar fields are all `na`, whose imbalance flags are `false`, and whose `rows` array is empty. No second data feed is involved; this value-tested host interface is deliberately narrower than the full feed API. The scalar accessors (`footprint.*`/`volume_row.*`) are plain `GET_FIELD` reads over the object, so an `na` receiver raises `NaFieldAccess` (never a panic). Allocating past `RunOptions.max_collection_elements` raises `CollectionLimitExceeded`. **Metering:** costs 1 unit. |
| `READ_SECURITY_LOWER_TF selector` | canonical host selector | 0→1 | Allocate and push the current bar's `array<float>` from `MarketData::security_lower_tf_data`. A missing selector or bar yields an empty array; the VM does not fabricate intrabar data. Allocation charges the collection budget. **Metering:** costs 1 unit. |
| `READ_PIVOT_LEVELS system anchor_seconds` | pivot system, anchor length (s) | 0→1 | Resample the chart feed, compute the requested pivot system from the prior completed anchor bucket, and push a fresh float array in `P,R1,S1,…` order (unsupported levels are `na`). Before a completed bucket exists, push an empty array. **Metering:** costs 1 unit plus the charged allocation. |
| `READ_TIME_TF close tf_seconds` | open/close selector, timeframe length (s) | 0→1 | Push the epoch-millisecond open or close of the chart bar's enclosing timeframe bucket. This baked-timeframe opcode remains serializable; Pine's runtime timeframe/session overloads use `CALL_PURE`. **Metering:** costs 1 unit. |
| `FOOTPRINT_ROW_BY_PRICE` | — | 2→1 | Pop a price (top) and a `footprint` object handle, and push the object's matching `volume_row` handle — the first row whose `[down_price, up_price]` range contains the price — or `na` when no row matches (or the footprint has no rows, or the price/bounds are non-finite). Backs `footprint.get_row_by_price(fp, price)`. An `na` footprint receiver raises `NaFieldAccess` (mirrors field access); never panics. **Metering:** costs 1 unit. |
| `WINDOW_MEAN slot length` | slot, window length | 0→1 | Push the arithmetic mean of the cells of `slot` at bars `bar − length + 1 ..= bar`; `na` while fewer than `length` bars exist, and `na` if any cell in the window is `na` (unwritten cells read as `na`). Cells are floats in compiled programs (the compiler coerces ta sources); an int cell is widened defensively, other types are `TypeMismatch`. The validator requires `1 ≤ length ≤ 100000`. **Metering:** the VM scans the window, so one `WINDOW_MEAN` costs `length` units of the per-bar instruction budget. Backs `ta.sma`. |
| `CALL_PURE func argc` | pure-function tag, argument count | argc→1 | Pop `argc` arguments (the **last** argument is on top of the stack) and push the value of builtin `func`. Most tags are stateless (`math.abs` … `math.round_to_mintick`, `na`, `nz`); `time`/`time_close` are context reads that use the current bar, chart feed, runtime timeframe/session/timezone strings, and chart/timeframe offsets. All functions are na-propagating (any `na` argument → `na` result) **except** `na` (pushes a plain bool: true iff the argument is `na`) and `nz` (pushes the source unless it is `na`, else the replacement). Result types follow the source-language rules: `math.abs`/`math.sign` preserve int/float; `math.floor`/`math.ceil` and one-argument `math.round` push an int (int input is identity; a float outside the i64 range is `na`); two-argument `math.round` pushes a float; `math.min`/`math.max` push an int when every operand is an int, a float otherwise; everything else pushes a float. Domain errors (√−1, log 0, asin 2, overflow) normalize to `na` through the non-finite → `na` rule. `math.random` draws from the VM's deterministic per-run PRNG (see docs/runtime.md); `math.round_to_mintick` rounds to a multiple of the market data's `mintick`. The **`str.*` namespace** rides the same opcode: `str.tostring`/`str.format`/`str.substring`/`str.replace`/`str.replace_all`/`str.upper`/`str.lower`/`str.trim`/`str.repeat`/`str.match` push a `Str`; `str.length`/`str.pos` push an `Int`; `str.contains`/`str.startswith`/`str.endswith` push a `Bool`; `str.tonumber` pushes an `F64`-or-`na`. All `str.*` functions are na-propagating. Operand shapes follow the surface signatures after the compiler materializes defaults (`str.replace` always has 4 operands — the occurrence defaults to 0; `str.substring`/`str.repeat` stay 2 or 3 because the absent trailing argument selects a distinct VM form). String position/length semantics count Unicode **code points**; `str.upper`/`str.lower` change ASCII letters only. `str.match` returns the first regex match and reports invalid/evaluation-failing patterns as structured errors; patterns are capped at 16 KiB, the memo at 64 entries/256 KiB of textual data, delegated NFA/DFA construction at 256 KiB each, and backtracking at 100,000 steps. The validator enforces per-function `argc` bounds (`PureFunc::argc_bounds`; the compiler materializes optional-argument defaults, so `NZ` always has 2 operands, `MATH.RANDOM` 2 or 3, and `time`/`time_close` 5) and per-function operand types; `NZ`'s operand pair must have matching types where statically known (like equality). **Metering:** numeric O(1) pure calls cost the dispatch unit; calls with string operands additionally charge the UTF-8 bytes inspected and known string-producing calls precharge a conservative output projection before allocation. Those projected scalar outputs and regex textual storage also reserve against the cumulative `RunOptions.max_string_bytes_per_run` budget (32 MiB by default); exceeding it raises `StringLimitExceeded`. Interpreter and AOT `host.vm` use this same path. |
| `CALL_TA state` | ta_states index | arity→N | Pop the call site's runtime source operands — the **arity** and each operand's type come from `ta_states[state].func`'s data table (`TaFunc::engine_operands`; the last operand is on top of the stack) — advance the site's VM-owned state, and push the function's **N results**, where `N = TaFunc::engine_result_arity()`. Scalar functions push one value (`N = 1`). The **tuple-returning** functions push their arity, **left to right in tuple order**: `ta.macd`, `ta.bb`, `ta.kc`, `ta.dmi` push three (`[a, b, c]` — `a` is pushed first, deepest on the stack, and `c` ends up on top), and `ta.supertrend` pushes two (`[supertrend, direction]`). A destructuring declaration then pops them into its target slots, popping in reverse (the top-of-stack last element into the last target first), so target `i` receives tuple element `i`. Both the arity and `N` come from the `ta_states` table, so `CsilInstr::stack_effect_rule` reports only the state id; `CsilInstr::resolve_stack_effect` looks both counts up in the table (rejecting an out-of-range state), and the validator and AOT analysis use it. Numeric operands are floats (the compiler coerces ints); `ta.barssince`/`ta.valuewhen` take bool condition operands; the cross family, `ta.correlation` (two sources), and `ta.stoch` (three) pop several; `ta.atr`/`ta.tr`/`ta.wpr`/`ta.sar`/`ta.dmi`/`ta.supertrend` and the variable-style builtins (`ta.obv`, `ta.accdist`, `ta.nvi`, `ta.pvi`, `ta.pvt`, `ta.wad`, `ta.wvad`, `ta.iii`, `ta.vwap`) pop nothing (they read market data directly). The result type of each pushed value is per function (`TaFunc::engine_result`): float for most, **int** for `ta.highestbars`/`ta.lowestbars`/`ta.barssince`, **bool** for `ta.crossover`/`ta.crossunder`/`ta.cross`/`ta.rising`/`ta.falling`; the tuple functions push **floats** (three each for `ta.macd`/`ta.bb`/`ta.kc`/`ta.dmi`, two for `ta.supertrend`). The state advances **only** when the instruction executes: on bars where control flow skips the site, the site's window/recursion simply does not see that bar (see docs/runtime.md; `W0004` warns at compile time). Per-function warm-up/na semantics are specified in docs/language-support.md. The validator requires `state` in range, each state id used by at most one instruction site, and well-formed `ta_states` declarations. **Metering:** costs `TaFunc::engine_cost(params)` units — the window length for window-rescanning functions (`math.sum`, `ta.wma`, `ta.vwma`, `ta.alma`, `ta.stdev`, `ta.dev`, `ta.variance`, `ta.highest(...bars)`, `ta.lowest(...bars)`, `ta.linreg`, `ta.cci`, `ta.cog`, `ta.correlation`, `ta.range`, `ta.mfi`, `ta.wpr`, `ta.stoch`, `ta.bbw`, `ta.bb` (its population stdev rescan), and the sort-based `ta.median`/`ta.mode`/`ta.percentile_*`/`ta.rci`, whose per-cell sort log-factor is absorbed into the unit; `ta.cmo`/`ta.percentrank`/`ta.rising`/`ta.falling` cost `length + 1` for their length-plus-one windows; `ta.pivothigh`/`ta.pivotlow` cost `leftbars + rightbars + 1`; `ta.swma` costs 4; `ta.hma` costs `length + length/2 + round(sqrt(length))` for its three wma rescans), and 1 for the O(1)-state/step functions (`fixnan`, `ta.ema`, `ta.rma`, `ta.rsi`, `ta.atr`, `ta.tr`, `ta.mom`, `ta.roc`, `ta.cum`, the cross family, `ta.barssince`, `ta.valuewhen`, `ta.sar`, `ta.tsi`, `ta.kcw`, the ema-based `ta.macd`/`ta.kc`, the Wilder-recursion `ta.dmi`, the atr-recursion `ta.supertrend`, and the variable-style builtins). |
| `CALL_COLLECTION func argc` | collection-function tag, argument count | argc→1 | Pop `argc` operands (the **last** on top; the receiver handle is the first argument, so it is deepest) and push **exactly one** result of the reference-typed collection builtin `func` (`str.split`, `array.*`, `map.*`, and `matrix.*`). Operands and result may be `Value::Ref(HandleId)` into the per-run heap. Void mutators push a placeholder `na` that a following `POP` balances; `map.put` returns the previous element or `na`, while matrix row/column access and removal push an `array<T>` handle. The validator consults the per-function argument, operand, and result tables. Bounds, `na` receivers, heap caps, dimensions, and singularity failures are structured runtime errors. **Metering:** O(1) operations cost 1; scanning/copying operations charge elements touched; superlinear matrix operations charge their work estimate. |
| `INT_ADD` / `INT_SUB` / `INT_MUL` | — | 2→1 | Checked exact integer arithmetic when both values are whole; a fractional value originating from `PINE_INT_DIV` continues through the operation as a float. Any `na` operand yields `na`; exact integer overflow yields `na`. |
| `INT_DIV` | — | 2→1 | Integer division truncating toward zero; division by zero and overflow (`i64::MIN / -1`) yield `na`. Source division emits `PINE_INT_DIV`; this opcode remains so previously serialized programs still validate and run. |
| `PINE_INT_DIV` | — | 2→1 | Int/int division: statically int, with an exact whole quotient or a fractional runtime value. Zero divisor and checked overflow yield `na`. Lowering inserts `int(...)` (`CALL_PURE int 1`) after a fractional int that only operands on the whole-part list read (`reads_whole_part` in `crates/pinewasm_csil/src/fraction.rs`; an unlisted operand is left alone), before the budget reference is taken and before the optimization passes, so those consumers read its whole part and the cast is charged like any instruction. The VM executes it. Native typed integer lanes cannot hold its fractional result, so the native backend lowers it only in the shapes its regression tests cover and otherwise attempts a guarded rewrite of a supported kept quotient into float code (`crates/pinewasm_aot/src/fraction.rs`). The carry canonicalizes zero to positive zero, so it is not an unrestricted bit-equivalence claim for VM float paths producing negative zero. When two whole operands give a result `f64` cannot hold exactly (possible from 2^53 up), a standalone module stops the run with status 15 and a chart-tier module returns the reroute status 6. A chart program that passes such a value to the VM host (text formatting, logging) stays interpreter-hosted. |
| `INT_REM` | — | 2→1 | Integer remainder for whole values, or float remainder when a fractional int value is present; the result takes the sign of the dividend. Remainder by zero and exact integer overflow yield `na`. |
| `INT_NEG` | — | 1→1 | Checked exact integer negation, or float negation for a fractional int value; `na` yields `na`; negating `i64::MIN` overflows to `na`. |
| `INT_TO_F64` | — | 1→1 | Widen an exact int to a float, or pass through a fractional int's physical float value; `na` passes through. Emitted for the explicit `IntToFloat` coercions the type checker inserts. |
| `STR_CONCAT` | — | 2→1 | String `+` concatenation, na-propagating: pop `rhs` then `lhs` (both strings), push their concatenation; an `na` operand yields `na`. The validator requires two `Str`-or-`Any` operands and pushes `Str`. **Metering:** charges the combined UTF-8 byte length and reserves that allocation against the run-wide string budget. Values over the per-string cap yield `na`. |
| `SELECT` | — | 3→1 | Ternary conditional select: pop `rhs`, then `lhs`, then a bool `cond`; push `lhs` when `cond` is true, `rhs` when it is false **or `na`** (na-condition-is-false, consistent with `JUMP_IF_FALSE`). Both branch values are computed before the select; lowering uses `SELECT` only when both arms are literals, so no side effect is skipped. The validator requires a `Bool`-or-`Any` condition and pushes the merge of the two branch types. **Metering:** costs 1 unit. Backs the ternary `?:`. |
| `F64_ADD` / `F64_SUB` / `F64_MUL` / `F64_DIV` | — | 2→1 | Float arithmetic, na-propagating: any `na` operand yields `na`. Division by zero and overflow produce non-finite raw results, which become `na`. |
| `F64_REM` | — | 2→1 | Float remainder; the result takes the sign of the dividend; `x % 0` is NaN, which becomes `na`. |
| `F64_NEG` | — | 1→1 | Float negation; `na` yields `na`. |
| `CMP_GT` / `CMP_GE` / `CMP_LT` / `CMP_LE` | — | 2→1 | Numeric comparison pushing a bool; any `na` operand yields `na`. Int-int compares exactly as i64, float-float as f64; a mixed pair promotes the int side (defense in depth — the type checker coerces statically, so compiled programs never mix). Non-numeric operands are a `TypeMismatch` runtime error. |
| `CMP_EQ` / `CMP_NE` | — | 2→1 | Equality over matching runtime types (bool, int, float, or string; int equality is exact i64); any `na` operand yields `na`; mismatched types are a `TypeMismatch` runtime error (statically rejected where knowable — mixed int/float equality is promoted to float at compile time). |
| `BOOL_AND` / `BOOL_OR` | — | 2→1 | Kleene three-valued logic over `na`: `false and _ = false`, `true or _ = true`, otherwise `na` when an operand is `na`. The opcode itself pops two already-evaluated operands; source-level `and`/`or` short-circuit because lowering brackets the rhs with `DUP` + `JUMP_IF_EXACT_*` (see below), so the combine only runs when the lhs did not decide alone. |
| `BOOL_NOT` | — | 1→1 | Boolean negation; `na` yields `na`. |
| `JUMP target` | instruction index | 0→0 | Unconditional jump. |
| `JUMP_IF_FALSE target` | instruction index | 1→0 | Pop a bool; jump when it is `false` **or `na`** (Pine-style conditions treat `na` as false). |
| `JUMP_IF_EXACT_TRUE target` / `JUMP_IF_EXACT_FALSE target` | instruction index | 1→0 | Pop a bool; jump only when it is exactly `true` / exactly `false` — `na` never jumps (unlike `JUMP_IF_FALSE`). Emitted after a `DUP` of a lazy boolean's lhs to short-circuit `or` / `and` (lazy evaluation): the jump lands just past the combining `BOOL_OR`/`BOOL_AND`, the retained lhs copy is the expression's result on the skip path, and an `na` lhs falls through so the strong-Kleene combine still sees the rhs (`na and false = false`, `na or true = true`). **Metering:** costs 1 unit. |
| `JUMP_IF_SLOT_WRITTEN slot target` | slot, instruction index | 0→0 | Jump when `slot`'s written flag is set (i.e. the slot has ever been stored to during this run). No stack effect. Emitted for `var` declarations so the initializer on the fall-through path runs exactly once. |
| `PLOT_F64 plot_slot` | plot slot | 1→0 | Pop a float-or-`na` and write it into the current bar's cell of the plot slot. Non-float, non-na values are a `TypeMismatch` runtime error. |
| `PLOT_STYLED_F64 plot_slot color_count` | plot slot, declared color-channel count | (1+N)→0 | Pop N color-or-`na` channels and the float-or-`na` value, then record both for the current bar. The validator checks the declared plot and exact channel count. |
| `VISUAL_COLOR visual_slot color_count` | visual slot, declared color-channel count | N→0 | Pop and record N color-or-`na` channels for a color-only chart visual such as `bgcolor` or `barcolor`. |
| `FILL_COLOR fill_slot color_count` | fill slot, declared color-channel count | N→0 | Pop and record N color-or-`na` channels for the declared `fill(...)` band. |
| `RUNTIME_ERROR` | — | 1→0 | Pop a string message and abort at this bar/instruction with `RuntimeErrorKind::UserError`; an `na` message becomes empty text. Earlier completed bars retain their output. |
| `NEW_OBJECT type_id field_count` | object-type id, field count | field_count→1 | Pop `field_count` field values (declaration order; the last field on top), allocate a fresh `HeapObj::Object { type_id, fields }`, and push its `Value::Ref` handle. The validator checks `field_count` against `object_types[type_id].field_count`. Growth past the budget raises `CollectionLimitExceeded`. **Metering:** costs `field_count` units. |
| `COPY_OBJECT type_id` | object-type id | 1→1 | Pop an object handle and push a NEW handle whose field values are shallow-copied. An `na` source raises `NaFieldAccess`. **Metering:** costs the source's field count. |
| `GET_FIELD field_idx` | field index | 1→1 | Pop an object handle and push field `field_idx`. An `na` object raises `NaFieldAccess`. **Metering:** costs 1 unit. |
| `SET_FIELD field_idx` | field index | 2→0 | Pop the value (top) then the object handle, and write field `field_idx`. An `na` object raises `NaFieldAccess`. **Metering:** costs 1 unit. |
| `REGISTER_DRAWING kind` | drawing kind | 1→1 | Pop a freshly built drawing handle, register it live for `kind` (oldest-first GC past the kind's max count), and push the same handle. **Metering:** costs 1 unit. |
| `DELETE_DRAWING kind` | drawing kind | 1→0 (+1 `na`) | Pop a drawing handle, remove it from `kind`'s live registry (no-op if absent), and push a placeholder `na`. An `na` receiver is a no-op. **Metering:** costs 1 unit. |
| `READ_DRAWINGS_ALL kind` | drawing kind | 0→1 | Take no operand; build and push a fresh `array<drawing>` (a heap `Ref`) of `kind`'s live objects in creation order — the runtime of the `line.all`/`label.all`/`box.all`/`table.all`/`polyline.all`/`linefill.all` accessors. **Metering:** costs the live-object count (it copies that many handles); the heap cap bounds the array. |
| `SET_CELL attrs` | cell-attribute list | (3+N)→0 (+1 `na`) | Pop N attribute values (last on top), then `row`, `column`, then the table handle; write each attribute of the cell at `(column, row)` of the out-of-line grid. `na` receiver → `NaFieldAccess`; out-of-range coordinate → `CollectionIndexOutOfBounds`. **Metering:** costs N units (min 1). |
| `CLEAR_CELLS` | — | 5→0 (+1 `na`) | Pop `end_row`, `end_column`, `start_row`, `start_column`, then the table handle; remove every cell in the inclusive range. `na`/out-of-range as above. **Metering:** costs 1 unit. |
| `MERGE_CELLS` | — | 5→0 (+1 `na`) | Pop `end_row`, `end_column`, `start_row`, `start_column`, then the table handle; mark every cell in the span merged. `na`/out-of-range as above. **Metering:** costs 1 unit. |
| `LOG level argc` | log level, arg count | argc→0 | Pop `argc` operands (format string deepest, args on top), render via `str.format` rules, and append a `LogEntry` (`level`, current bar, message). **Metering:** costs `argc` units. |
| `ALERT` | — | 2→0 | Pop `freq` then `message`; append an `AlertEvent` honoring the `alert.freq_*` firing rules (once-per-bar dedup is per call site, keyed by pc). **Metering:** costs 1 unit. |
| `ALERT_CONDITION` | — | 3→0 | Pop `message`, `title`, then a bool `condition`; append an `AlertEvent` (freq `once_per_bar_close`) only when `condition` is true. **Metering:** costs 1 unit. |
| `ORDER kind` | order-primitive kind | k→0 | Pop the primitive's fixed operand layout (`k = kind.operand_count()`, slot order — the last slot is on top), and place the order with the run's broker emulator. Pushes nothing. **Metering:** costs 1 unit. |
| `RETURN` | — | 0→0 | End of the per-bar program. The operand stack must be empty. |

#### `ORDER` operand layouts

Each `strategy.*` order primitive carries a fixed, kind-specific operand
layout that MIR pushes in slot order and the VM broker loop pops. String slots
(`id`, `comment`, `from_entry`) and the direction operand are string/token
values; price and quantity slots are floats. A missing optional slot is `na`.
Direction retains its nominal `strategy_direction` token through HIR, MIR, and
CSIL, including series-capable conditional direction expressions; the broker
resolves the effective `long` / `short` token when it places the order.

| Kind | `operand_count()` | Slots (in order) |
|---|---|---|
| `strategy.entry` | 10 | `id`, `direction`, `qty`, `limit`, `stop`, `oca_name`, `oca_type`, `comment`, `alert_message`, `disable_alert` |
| `strategy.order` | 10 | `id`, `direction`, `qty`, `limit`, `stop`, `oca_name`, `oca_type`, `comment`, `alert_message`, `disable_alert` |
| `strategy.exit` | 21 | `id`, `from_entry`, `qty`, `qty_percent`, `profit`, `limit`, `loss`, `stop`, `trail_price`, `trail_points`, `trail_offset`, `oca_name`, `comment`, `comment_profit`, `comment_loss`, `comment_trailing`, `alert_message`, `alert_profit`, `alert_loss`, `alert_trailing`, `disable_alert` |
| `strategy.close` | 7 | `id`, `comment`, `qty`, `qty_percent`, `alert_message`, `immediately`, `disable_alert` |
| `strategy.close_all` | 4 | `comment`, `alert_message`, `immediately`, `disable_alert` |
| `strategy.cancel` | 1 | `id` |
| `strategy.cancel_all` | 0 | — |
| `strategy.risk.max_position_size` | 1 | `contracts` |
| `strategy.risk.allow_entry_in` | 1 | `value` |
| `strategy.risk.max_drawdown` | 3 | `value`, `type`, `alert_message` |
| `strategy.risk.max_intraday_loss` | 3 | `value`, `type`, `alert_message` |
| `strategy.risk.max_intraday_filled_orders` | 2 | `count`, `alert_message` |
| `strategy.risk.max_cons_loss_days` | 2 | `count`, `alert_message` |

All six `strategy.risk.*` tags ride the same `ORDER` opcode but are emitted into
an unconditional, source-ordered MIR/CSIL prelude. They therefore execute on every
script calculation regardless of their source branch, loop, local block, or
expanded function. The instruction offset is the stable call-site identity:
recalculations update that site's rule without duplicating it, while separate
sites remain simultaneously active. Position/count rules use the smallest
value, entry-direction rules intersect, and mixed cash/percent loss thresholds
are normalized and checked together. Risk orders are registrations, not fills,
so they push and realize nothing.

The **per-trade accessors** (`strategy.closedtrades.<field>(trade_num)` /
`strategy.opentrades.<field>(trade_num)`) and the currency conversions
(`strategy.convert_to_account/symbol(value)`) do **not** add opcodes — they are
new `PureFunc` tags on the existing `CALL_PURE` opcode (a `TradeAccessor` payload
names the field). Each pops its single argument and reads live broker state: an
out-of-range `trade_num` (or a missing broker) pushes `na`, never panicking. The
conversions are the identity unless a `currency_rate/<from>/<to>` host table is
supplied (mirroring `request.currency_rate`).

The broker fill rules these opcodes feed are documented in `docs/runtime.md`
(broker emulator).

### Source map

`source_map[i]` is the span for `code[i]` — one span per instruction, no
gaps. Lowering emits both together; the validator rejects any length
mismatch. Runtime errors use it to report `span` alongside
`instruction_offset` and `bar_index`.

### Serialization

Serialization is `serde`-based: `CsilProgram::to_json` / `from_json` for
Rust hosts, and a direct JavaScript object serializer across the Wasm boundary
(`Value::Na` and non-skipped `Option::None` fields map to `null`, fields with a
Serde omission rule may be absent, and wide integers become `bigint`, so JSON
persistence requires an explicit codec). `PUSH_I64` operands survive the Rust
JSON format exactly as 64-bit integers. A compact binary encoding is deferred;
it can be introduced behind the same header without changing semantics.
Round-tripping is tested (`json_round_trip_preserves_program`,
`int_program_round_trips_through_json`,
`var_program_round_trips_through_json` — which pins `SlotDef.persistent`
— `pure_program_round_trips_through_json` — which pins `CALL_PURE`'s
function tag and argc — and `ta_engine_program_round_trips_through_json`
— which pins `ta_states` including `ta.alma`'s `f64` bit patterns — in
`crates/pinewasm_csil/src/tests.rs`).

### MIR → CSIL lowering

MIR is a register machine; CSIL is a stack machine. Because MIR is
emitted from expression trees in evaluation order, every register is
defined immediately before its single use, so the operand stack mirrors
the register definitions exactly. Lowering tracks that correspondence
with a simulated stack of `ValueId`s and reports an internal error
(`LowerError`, surfaced as an `E9999` diagnostic) if the discipline is
ever violated — a compiler bug, never a user error.

Blocks are analyzed in CFG readiness order: a worklist carries the exact
ordered register stack (and any open lazy-boolean guards) along every edge,
so a ternary nested in a larger expression can keep live outer operands
across its branch. A block is lowered once its first incoming state is
known; every later predecessor must arrive with the identical stack, or
lowering fails with `StackMergeMismatch` (`GuardMergeMismatch` for guards).
Equal heights alone are not enough. The emitted code is then laid out in
block-id order; jump targets are patched by fixups once every block's
start offset is known. A `Branch` terminator lowers to
`JUMP_IF_FALSE else_target` followed by `JUMP then_target`, and a
`BranchIfSlotWritten` terminator to
`JUMP_IF_SLOT_WRITTEN slot written_target` followed by
`JUMP unwritten_target` — the second jump is usually to the next
instruction, kept for uniformity. The operand stack must be empty at
`RETURN`.

### Validator rules

`pinewasm_csil::validate` rejects malformed bytecode before it reaches
the VM:

1. **Header checks** — magic must be `CSIL`, `csil_version` must be
   supported; at least one function (`main`) must exist.
2. **Structural bounds**, for every instruction, reachable or not:
   string-constant indices, series slots, plot slots, and jump targets
   in range (`JUMP_IF_SLOT_WRITTEN` has both a slot and a target bound
   check; `WINDOW_MEAN` has a slot bound check **and** a window-length
   check, `1 ≤ length ≤ 100000`; `CALL_PURE` has a per-function `argc`
   bounds check; `CALL_TA` has a state bound check); `PUSH_F64`
   constants finite; non-empty body; source map length equal to code
   length. In addition, the `ta_states` table itself is validated before
   anything else: every declaration must name an **engine** function
   (`ta.change`/`ta.sma` are slot-based and rejected here) with exactly
   its declared const-parameter count, window lengths in `1..=100000`
   (`ta.hma` requires ≥ 2), int parameters in their declared ranges,
   float bit patterns decoding to finite values (`ta.alma`'s sigma and
   `ta.sar`'s start/inc/max strictly positive; the percentile functions'
   percentage within `0..=100`), and boolean flags 0/1 — and each state
   id may be referenced by **at most one** `CALL_TA` site across the
   whole program (duplicate use is a validation error; unused entries
   are permitted).
3. **Abstract stack interpretation** over all execution paths (worklist
   over abstract types `Int`/`F64`/`Bool`/`Str`/`Any`, where `Any` is
   the join for statically unknown values such as `PUSH_NA` and
   `READ_USER_SLOT`): no underflow; operand types correct where
   statically knowable (`F64_*` arithmetic needs `F64`; `INT_*`
   arithmetic and `INT_TO_F64` need `Int`; the `CMP_GT`..`CMP_LE`
   comparisons accept `F64`, `Int`, or `Any` operands; logic and
   `JUMP_IF_FALSE` need `Bool`; `PLOT_F64` needs `F64`; equality
   operands must match unless one is `Any`; `CALL_PURE` builds its
   operand requirements per function — `F64` for float-only functions,
   numeric for the int-preserving ones, `Int` for `math.round`'s
   precision and `math.random`'s seed, int/float/bool for `NZ`'s pair
   (which must additionally match, like equality), anything for `NA`;
   `SELECT` needs a `Bool` condition under two branch operands of any
   value type);
   pushes are typed
   (`PUSH_I64` and the `INT_*` ops push `Int`; `READ_BUILTIN` pushes
   `Int` for `BAR_INDEX` and the time/calendar series, `Bool` for the
   `barstate.*` predicates, and `F64` otherwise; `INT_TO_F64` and
   `WINDOW_MEAN` push `F64`; `CALL_PURE` pushes its per-function result
   type as described in the opcode table; `CALL_TA` pops its
   per-function operand list — `F64` sources, `Bool` conditions — and
   pushes its per-function result type (`F64`/`Int`/`Bool`) **once per
   tuple element** (`engine_result_arity()` times — one for scalars, three
   `F64`s for `ta.macd`/`ta.bb`/`ta.kc`/`ta.dmi`, two for `ta.supertrend`), both looked
   up in `ta_states[state].func`'s data table because the instruction
   alone does not determine them; `SELECT` pushes the merge of
   its two branch operand types); consistent stack heights at merge points, with per-cell type
   join (`JUMP_IF_SLOT_WRITTEN` has no stack effect and merges both of
   its successor paths exactly like `JUMP_IF_FALSE`); the stack empty at
   `RETURN`; execution never falls off the end of the function. A linear
   height-only preflight computes the exact reachable stack shape before the
   typed snapshots allocate and rejects functions whose snapshots would retain
   more than 8,000,000 abstract cells. This prevents a compact push/pop
   triangle from turning validation into quadratic, multi-gigabyte work.
4. **`max_stack` re-check** — the declared `max_stack` must not
   understate the maximum depth observed by the abstract interpreter,
   and must not exceed the sanity cap of 65,536 (hosts size buffers from
   the declared value, so a huge declaration is rejected even when actual
   usage is small).

Each rule has a corresponding rejection test in
`crates/pinewasm_csil/src/tests.rs` (bad magic, unknown version, underflow,
out-of-range jump/slots — including `JUMP_IF_SLOT_WRITTEN` slot/target
bounds and merge behavior in `validator_checks_jump_if_slot_written_bounds`
and `jump_if_slot_written_has_no_stack_effect_and_merges_paths`, and
`WINDOW_MEAN` slot/length bounds in
`validator_rejects_window_mean_bad_length_and_slot` — missing
return, type confusion — including `Int` where `F64` is required and vice
versa, and `WINDOW_MEAN`'s `F64` push in
`window_mean_pushes_f64_for_the_abstract_interpreter`, and `CALL_PURE`'s
argc bounds, operand types, `NZ` operand matching, and result typing in
`validator_rejects_call_pure_argc_out_of_bounds`,
`validator_rejects_call_pure_operand_type_confusion`,
`validator_rejects_mismatched_nz_operands`, and
`call_pure_result_types_feed_the_abstract_interpreter`, and `SELECT`'s
condition typing, underflow, and result-type merge in
`validator_rejects_select_on_non_bool_condition`,
`validator_rejects_select_underflow`, and
`select_result_type_feeds_the_abstract_interpreter`, and `CALL_TA`'s
state bounds, duplicate-use rule, declaration checks, operand typing,
and result typing in `validator_rejects_call_ta_state_out_of_range`,
`validator_rejects_duplicate_call_ta_state_use`,
`validator_rejects_malformed_ta_state_declarations`,
`validator_rejects_call_ta_operand_type_confusion`, and
`call_ta_result_types_feed_the_abstract_interpreter` — unbalanced return,
short source map, understated and overdeclared `max_stack`, non-finite
constants).

## Worked example: the golden program

Source (byte offsets shown for the spans referenced below):

```pine
//@version=6
indicator("Example")
x = close - close[1]
plot(x)
```

Line 3 starts at byte 34: `close` is 38..43, `close[1]` is 46..54.
Line 4 starts at byte 55: `x` is 60..61.

**HIR** (snapshot
`pinewasm_semantics__tests__core_example_hir_snapshot.snap`) —
identifiers resolved, history desugared to offset reads, `plot`
desugared to an effect statement with an allocated slot, every
expression typed and qualified:

```text
HirProgram indicator="Example"
  symbol 0 `x`: series float (slot 0)
  plot slot 0 title="<none>"
  Assign `x` (sym 0)
    Binary `-` : series float
      ReadBuiltin close[0] : series float
      ReadBuiltin close[1] : series float
  Plot slot 0
    ReadVar `x`[0] (sym 0) : series float
```

**MIR** (snapshot
`pinewasm_mir__tests__core_example_mir_snapshot.snap`) — the
expression tree flattened into registers in evaluation order, effects as
instructions, one block because there is no control flow:

```text
MirBody title="Example" slots=1 plots=1 values=4
  slot 0 `x`
  plot 0 title="<none>"
bb0:
  v0 = read_builtin close[0]
  v1 = read_builtin close[1]
  v2 = binary `-` v0, v1
  store_user_current slot0, v2
  v3 = read_user slot0[0]
  plot p0, v3
  return
```

**CSIL** (snapshot
`pinewasm_csil__tests__core_example_csil_snapshot.snap`) — registers
replaced by stack positions, with one source span per instruction:

```text
CSIL version 2 lang 6 abi 0
PROGRAM "Example"
SERIES_SLOT 0 "x"
PLOT_SLOT 0

FUNC main (max_stack 2)
  0000 READ_BUILTIN CLOSE 0             ; 38..43
  0001 READ_BUILTIN CLOSE 1             ; 46..54
  0002 F64_SUB                          ; 38..54
  0003 STORE_USER_SLOT_CURRENT 0        ; 34..54
  0004 READ_USER_SLOT 0 0               ; 60..61
  0005 PLOT_F64 0                       ; 55..62
  0006 RETURN                           ; 55..62
END
```

Executed over `close = [10, 12, 11]`, this plots `[na, 2.0, -1.0]`
(`na` serializes to JSON `null`); the end-to-end assertion lives in
`golden_close_difference_example` in
`crates/pinewasm_compiler/tests/pipeline.rs`.

For a branching example (the `if`/`else` diamond and its
`JUMP_IF_FALSE`/`JUMP` layout), see the `if_else` snapshots in the same
three crates.

## Ordered distance scan

The compiler adds a guarded optimization for one structural class of
ordered, stateful array searches: a fixed-shape ordered search loop over arrays,
run by a bounded-pruning kernel with a guarded fallback to the original loop.
It recognizes resolved, typed operations,
not script names or source text. The recognized loop computes the left-associated
sum of `log(1 + abs(query[j] - feature[j][i]))` for two through five features,
accepts candidates by the threshold comparison `distance >= threshold`, pushes a
rounded integer label and distance into persistent arrays, then reads the
neighbor position **before** shifting the arrays.

The compiler must match the entire body, including an inlined pure distance
function and its feature-count switch. It rejects changed arithmetic, predicates,
queue updates, effects, history reads, persistent local or threshold variables, and observers
of erased local writes. Captured bounds and operands must be invariant. A
runtime guard additionally checks integer bounds, feature count 2–5, neighbor
count 1–100, object/array layouts, array coverage, queue lengths, and aliasing.
A failed guard executes the original loop. Labels come from the existing Pine
array, so the selected input series keeps its meaning. The guard reads optional object fields without raising an error, so it cannot
introduce a dereference on a loop that would not execute. The following Pine
`array.sum` remains unchanged, including its missing result for an empty queue.

The internal collection operations are `InternalDistanceScanEligible` and
`InternalDistanceScan`. They are not registered Pine functions. Programs using
either require runtime ABI 2; old readers must reject them. Existing ABI 0/1
programs remain accepted. `renderingContract().programRuntimeAbi.distanceScans`
publishes that version for result-admission consumers.

The scan's ten operands are start, end, feature count, query object, feature-array
object, label array, distance queue, prediction queue, neighbor count and initial
threshold, in that order. It returns a scalar and mutates only operands 6 and 7.
The guard adds two field descriptors for operands 2 and 8: `-1` means a direct
value; a nonnegative descriptor safely reads that field of the supplied receiver.
`InternalDistanceScanEligible` has target-chosen boolean semantics: false is
always allowed, including when the target could execute the kernel. The false
arm defines the program's behavior; the true arm must be an exact refinement of
it. This is a producer obligation that bytecode validation cannot prove. Pine
source cannot name or observe this compiler-owned guard; the source compiler
retains the original literal calculation in its false arm. A foreign or modified
cached program whose arms differ can therefore behave differently on VM/chart
and standalone targets. A VM backtest alone does not certify its standalone
export. Consumers crossing that trust boundary should recompile trusted source
and check input and constant tables, or refuse the foreign program. The
standalone lowering neutralizes the true arm only after proving its single-entry
structured guard/kernel role, and refuses unfamiliar or exposed uses. Both operations return scalars and forward no heap handles. Dependency and signal
slicing analyses must model those two queue effects without inventing aliases
into readonly query objects or settings. The scan returns the final threshold and mutates the
existing queue handles. It preserves inclusive ascending and descending order,
ties, integer division with a fractional result followed by `math.round`, missing values,
and the push/read/shift ordering. Descending searches use the uncached Rust
kernel. Profiling and realtime execute the same operation as the interpreter.
AOT lowers the surrounding control flow and uses the same Rust kernel through
its residual host interface; a linked chart module contains that host and imports
nothing. Set compile option `disableDistanceScanOptimization: true` (Rust:
`disable_distance_scan_optimization`) to keep the unoptimized literal program, for a target without the residual host. This does not make the Rust-heap kernel part of the standalone native-heap
subset. The standalone signal compiler selects the equivalent literal arm of
recognized compiler-owned guard/kernel macros before slicing and heap planning,
so previously eligible signal exports remain eligible without a caller opt-out.
Its finite instruction accounting follows that literal target's work, rather
than the indexed VM/chart target's work. Unpaired or unfamiliar kernel uses retain
an explicit refusal; standalone import-free audits are unchanged. A compiled-Wasm label alone does not establish this distinction.

### Index construction and invalidation

During execution, complete groups of 32 already-available rows are summarized
with a minimum and maximum for every feature. Parent nodes combine their
children's extrema. A partial final group is scanned directly. Each new complete
group is added once while the arrays only append. Queries recompute a bound for
the current feature vector and threshold; they reuse the stored extrema.
Only a group wholly inside the current inclusive search interval may be skipped.
Children are visited left before right. No unavailable or future row contributes
to a pruning decision.

Only finite feature/query values in `[0,1]` admit pruning. Other finite values
use exhaustive evaluation, and nonfinite numeric intermediates follow the VM's
missing-value normalization. A group with missing values in every row of one
active feature cannot accept a candidate. Groups containing out-of-domain finite
values remain unprunable. For a bounded group the largest coordinate separation
is attained at an endpoint, giving the real-number upper bound
`sum(log(1 + max(abs(q-min), abs(q-max))))`.

The implementation uses `math.log`'s correctly rounded logarithm from
`pinewasm_num`, whose error is at most half a unit in the last place (see
[`math.log` rounding](runtime.md#mathlog-rounding)). On `[1,2]`, that error and the
rounding of at most five positive terms are conservatively below `1e-14`.
The rejection comparison adds `1e-10 * (1 + abs(bound))` and uses strict `<`;
equalities and adjacent floating-point values therefore stay on the exhaustive
path. This argument relies on the logarithm's correct rounding, correctly rounded
IEEE operations, and absence of reassociation/fast-math. It is an informal argument,
not a formal proof of the numerical library or the compiler transformation.

The cache holds one index, with at most 32,768 leaves. A heap watches at most
32 array prefixes. Any unrestricted mutable access invalidates a watched prefix;
the dedicated append helper can only append and preserves the old prefix.
Handle changes, revision changes, shrinking arrays, and realtime rollback prevent
reuse. Revision saturation disables reuse. Cache metadata is not serialized and
does not participate in semantic heap equality. New sessions start with no cache.
Index storage and bookkeeping are reserved against the VM memory budget before
allocation. A VM-level high-water mark survives invalidation, rollback and AOT
state transfers; rebuilds charge only growth beyond that peak. Vector growth
accounts for simultaneous old and new storage. Repeated ticks or alternating
scan sites therefore cannot consume an unbounded cumulative cache reservation. If optional storage will not fit, the scan uses its uncached kernel.

### Work accounting

The new operation charges generated work, not the erased source loop's original
logical instruction count. Finite-budget failure points may therefore change
when a source program is recompiled. This is intentional and versioned; it is not
a raised run quota or an AOT-only exemption. The interpreter, profiled execution,
realtime, hosted AOT and linked AOT share these charges:

- normal dispatch and argument evaluation;
- one unit per candidate eligibility check, then nine per active feature for a
  full distance calculation;
- one plus nine per active feature for a visited node;
- allocated node count for tree growth, and 32 times the feature count plus tree
  height for a newly summarized group;
- eight units for an accepted candidate, plus twice the queue length and four
  units when the two queues shift.

Charges precede their corresponding work/effects, and both the per-bar and
cumulative counters apply. AOT preserves its existing structured-error or replay
protocol at exhausted host sites. Cache memory reservations cover their bounded high-water peak; other dynamic
VM reservations retain their existing cumulative policy. These units bound fixed-size kernel work;
they are not a count of CPU or WebAssembly instructions. Performance acceptance
must include construction, bounds, queue maintenance, and fallback overhead,
and must compare final executable artifacts separately from profiling runs.
