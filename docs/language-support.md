# PineWASM Language Reference

PineWASM compiles and executes a documented subset of Pine Script® source; the coverage matrix lists the scope.
This document is the normative reference for the core language semantics
implemented by the current compiler.
It is intentionally scoped: it explains syntax, typing, execution-facing
semantics, and diagnostics, but it is not an exhaustive inventory of every
builtin overload. This guide describes the behavior implemented by
PineWASM. For each claim, the coverage matrix links the relevant
tests and identifies the cases they cover. This
document describes PineWASM's implementation in project-maintained terms;
it is not an official or replacement reference for the third-party language.
The authoritative, test-linked inventory of supported, partial, and unavailable
language surfaces is [`pine-v6-coverage.md`](pine-v6-coverage.md). If a coverage
claim here and that matrix ever disagree, use the matrix and its linked current
tests; this reference should then be corrected.

Sources are UTF-8 text. All spans in diagnostics are **UTF-8 byte offsets**
into the source buffer (half-open ranges `[start, end)`), not character or
UTF-16 offsets.

## Contents

- [Grammar](#grammar)
- [Lexical structure](#lexical-structure)
- [Expressions and precedence](#expressions-and-precedence)
- [History references](#history-references)
- [Statements and scoping](#statements-and-scoping)
- [Built-ins](#built-ins)
- [Worked example](#worked-example)
- [Diagnostic codes](#diagnostic-codes)
- [Current coverage boundaries](#current-coverage-boundaries)
- [PineWASM-specific semantic rules](#pinewasm-specific-semantic-rules)

## Grammar

EBNF-ish notation. `NEWLINE`, `INDENT`, and `DEDENT` are synthetic tokens
produced by the indentation-aware lexer (see
[Indentation](#indentation-and-blocks) below); terminals are quoted.

```
program          ::= { version-directive NEWLINE | top-item } EOF
version-directive::= "//@version=" INTEGER            (* on a top-level line; a repeat
                                                           is ignored with W0005 *)

top-item         ::= indicator-decl | strategy-decl | library-decl | import-decl
                   | func-def | method-def | enum-decl | type-decl | statement

indicator-decl   ::= "indicator" "(" [ call-arg { "," call-arg } ] ")" NEWLINE
                     (* semantics: const title plus the supported metadata /
                        runtime parameters; see the coverage matrix *)

strategy-decl    ::= "strategy" "(" [ call-arg { "," call-arg } ] ")" NEWLINE

library-decl     ::= "library" "(" [ call-arg { "," call-arg } ] ")" NEWLINE
                     (* semantics: a string-literal title + optional overlay bool;
                        marks the script a *library* so `export` is allowed *)

import-decl      ::= "import" IDENT "/" IDENT "/" ( IDENT | INTEGER )
                     [ "as" IDENT ] NEWLINE
                     (* Publisher/LibName/Version [as alias]; resolves against
                        the provided source map only — never the network *)

enum-decl        ::= [ "export" ] "enum" IDENT NEWLINE INDENT enum-member+ DEDENT
enum-member      ::= IDENT [ "=" STRING ] NEWLINE       (* optional title *)

type-decl        ::= [ "export" ] "type" IDENT NEWLINE INDENT type-field+ DEDENT
type-field       ::= field-type IDENT [ "=" expr ] NEWLINE
field-type       ::= type-base [ "[" "]" ]
type-base        ::= "int" | "float" | "bool" | "string" | "color"
                   | "array" "<" field-type ">"
                   | "matrix" "<" field-type ">"
                   | "map" "<" field-type "," field-type ">"
                   | dotted-type
                     (* dotted-type names a UDT/enum in fields; declaration
                        annotations additionally recognize drawing types.
                        T[] is array<T> sugar *)
dotted-type      ::= IDENT { "." IDENT }

func-def         ::= [ "export" ] IDENT "(" [ param { "," param } ] ")" "=>"
                     ( statement                       (* single-line body *)
                     | NEWLINE block )                 (* multi-line body   *)
                     (* top-level only; the body's last statement is the
                        return value; monomorphized per call site.
                        `export` is only valid in a library script *)
method-def       ::= [ "export" ] "method" method-name
                     "(" param [ "," param { "," param } ] ")" "=>"
                     ( statement | NEWLINE block )
                     (* the first parameter must carry a receiver type; like
                        any parameter it may have a qualifier and a default.
                        Exported methods parse but are not yet importable *)
method-name      ::= IDENT | "type" | "int" | "float" | "bool" | "string"
param            ::= [ qualifier ] [ field-type ] IDENT [ "=" expr ]
qualifier        ::= "const" | "input" | "simple" | "series"

statement        ::= assignment
                   | reassignment
                   | destructuring
                   | if-statement
                   | once-statement
                   | switch-statement
                   | for-statement
                   | while-statement
                   | break-statement
                   | continue-statement
                   | expression-statement
                     (* assignment, reassignment, destructuring and
                        expression-statement end at NEWLINE, or at a ","
                        followed by another statement on the same line
                        (`a = 1, b = 2`); a "," right before NEWLINE just
                        ends the statement. An inline body (after `=>` in a
                        function, method or switch arm) may likewise be a
                        comma-separated list whose last statement is the
                        value *)

assignment       ::= [ "var" | "varip" ] [ type-annot ] IDENT "=" expr NEWLINE
reassignment     ::= assign-target ":=" expr NEWLINE
assign-target    ::= IDENT { "." IDENT }             (* variable or UDT field *)
destructuring    ::= "[" IDENT { "," IDENT } "]" "=" expr NEWLINE
                     (* the RHS must be a tuple-producing call; the target
                        count must match the tuple arity — E0225 otherwise *)
expression-statement ::= expr NEWLINE

type-annot       ::= field-type

if-statement     ::= if-form
if-form          ::= "if" expr NEWLINE block
                     [ "else" ( if-form               (* "else if ..." chain *)
                              | NEWLINE block ) ]
once-statement   ::= "once" [ expr ] NEWLINE block      (* contextual; version 6 only *)
switch-statement ::= switch-form
switch-form      ::= "switch" [ expr ] NEWLINE
                     INDENT { switch-arm } DEDENT
switch-arm       ::= [ expr ] "=>" ( statement            (* inline body *)
                                   | NEWLINE block )       (* block body *)

for-statement    ::= for-counted | for-in
for-counted      ::= "for" [ "int" ] IDENT "=" expr "to" expr [ "by" expr ] NEWLINE block
                     (* the optional `int` is dropped: the counter's type
                        comes from the bounds and step *)
for-in           ::= "for" for-in-pattern "in" expr NEWLINE block
for-in-pattern   ::= IDENT                            (* element only        *)
                   | "[" IDENT "," IDENT "]"          (* [index, element]    *)
                   (* the iterable must be an array<T> (E0235 otherwise);
                      desugars to a counted loop over array.size with
                      array.get per iteration — see the notes below *)
while-statement  ::= "while" expr NEWLINE block
break-statement  ::= "break" NEWLINE
continue-statement ::= "continue" NEWLINE

block            ::= INDENT { statement } DEDENT

expr             ::= ternary-expr
ternary-expr     ::= or-expr [ "?" expr ":" expr ]     (* right-associative *)
or-expr          ::= and-expr  { "or"  and-expr }
and-expr         ::= not-expr  { "and" not-expr }
not-expr         ::= "not" not-expr | eq-expr
eq-expr          ::= rel-expr  { ( "==" | "!=" ) rel-expr }
rel-expr         ::= add-expr  { ( "<" | "<=" | ">" | ">=" ) add-expr }
add-expr         ::= mul-expr  { ( "+" | "-" ) mul-expr }
mul-expr         ::= unary     { ( "*" | "/" | "%" ) unary }
unary            ::= ( "-" | "+" ) unary | postfix
postfix          ::= primary { "(" [ call-arg { "," call-arg } ] ")"  (* call *)
                             | "[" expr "]" }                  (* history *)
call-arg         ::= [ IDENT "=" ] expr    (* named arg when `IDENT =` leads;
                                              a positional arg after a named
                                              one is a parse error *)
primary          ::= NUMBER | STRING | "true" | "false" | "na"
                   | generic-ctor | name | "(" expr ")"
                   | if-form          (* if used as a value *)
                   | switch-form      (* switch used as a value *)
                   | tuple            (* only legal as a destructuring RHS
                                         or a function body's last statement *)
tuple            ::= "[" expr { "," expr } "]"
name             ::= IDENT { "." IDENT }
generic-ctor     ::= ctor-base "<" generic-type { "," generic-type } ">"
ctor-base        ::= "array.new" | "matrix.new" | "map.new"
                     (* the `<...>` type-argument list is parsed ONLY after one
                        of these registered constructor bases; everywhere else
                        `<`/`>` are the comparison operators. An all-scalar
                        list rewrites to the typed constructor name —
                        `array.new<float>` -> `array.new_float`,
                        `map.new<string, float>` -> `map.new_string_float`. A
                        list that names a dotted-type keeps the base name and
                        carries its type arguments on the call; semantic
                        analysis validates them. A bad/missing/wrong-arity
                        type argument is E0234. *)
generic-type     ::= "int" | "float" | "bool" | "string" | "color"
                   | dotted-type
```

Notes on the grammar:

- **Dotted names and fields.** In primary position,
  `IDENT ("." IDENT)*` folds into one syntactic path whose span covers the
  whole path. Semantic analysis distinguishes namespace members
  (`ta.sma`, `color.red`), imported-library members, UDT field reads
  (`point.x`), and method calls from the receiver type. A field read may also
  follow an object-valued expression, e.g. `array.get(points, 0).price`. A UDT field is written
  with `:=` (`point.x := 1.0`); namespace members are never assignable, and a
  dotted target using declaration `=` is rejected. An unresolved path gets the
  appropriate unknown-name/member diagnostic. A dot not followed by an
  identifier (`x.5`) is `E0110`.

- **Collection method-call syntax.** A postfix call whose callee is a
  dotted `receiver.method` where `receiver` is an array/matrix/map-typed
  *variable* is a method call: it desugars to the namespace form with the
  receiver as the first argument (`a.push(x)` ≡ `array.push(a, x)`,
  `m.get(k)` ≡ `map.get(m, k)`, `mat.get(r, c)` ≡ `matrix.get(mat, r, c)`),
  resolving to the *same* registry builtin and lowering to byte-identical CSIL.
  Syntactically these are indistinguishable from a namespaced builtin name
  (both fold into a dotted identifier), so the distinction is made during
  semantic analysis: the namespace form wins whenever the dotted name is
  itself a registered builtin; otherwise, if the prefix resolves to a
  collection variable, it is a method call. An unknown method on a collection
  receiver is `E0235` (with a did-you-mean over that type's methods); a
  method-style call on a non-collection value falls through to the normal
  unknown-function handling.
- **`for … in`.** `for element in arr` / `for [index, element] in arr`
  iterates an `array<T>`. It desugars to a counted loop over
  `array.size(arr)`: the iterable is evaluated **once** into a hidden temp (so
  a side-effecting iterable runs once). The loop end is re-evaluated before
  each iteration, as in the counted `for`, so a body that resizes the array or
  matrix is walked to its updated size
  ([`for_in_over_a_resized_array_or_matrix_walks_the_updated_size`](../crates/pinewasm_vm/src/tests.rs)).
  Each iteration binds `element` via `array.get(temp, i)` (and, in
  the tuple form, `index = i`). `break`/`continue` and nesting work exactly as
  in the counted `for`. An empty array iterates zero times because an
  `array.size(temp) > 0` guard skips the counted loop when `end = -1`.
  A matrix iterates by rows: `for row in m` and
  `for [index, row] in m` bind each row, top to bottom, as a new array
  (`matrix.row(temp, i)`, bounded by `matrix.rows(temp)`). A row the body
  uses only as the receiver of builtin `array.*` calls and methods that
  neither return, store nor view it (not `array.concat`, `array.slice` or
  `array.from`)
  cannot outlive its iteration, so the loop refills one hidden row array with
  `array.clear` and `matrix.get` instead, and holds one row against the
  collection budget
  ([`for_in_over_matrix_binds_rows`](../crates/pinewasm_semantics/src/tests.rs),
  [`for_in_over_a_matrix_reuses_one_row_when_the_row_stays_in_its_iteration`](../crates/pinewasm_vm/src/tests.rs),
  [`for_in_over_matrix_visits_rows_in_order`](../crates/pinewasm_vm/src/tests.rs)).
  A map iterates by entries in insertion order with `for [key, value] in m`;
  each iteration binds the value its entry had at loop entry, so a value the
  body replaces for a later key is not seen by that key's iteration
  ([`for_in_over_a_map_binds_each_value_from_the_entry_snapshot`](../crates/pinewasm_vm/src/tests.rs)).
  This rule is PineWASM's own. Looping over
  `m.keys()` and calling `m.get(k)` reads live values instead
  ([`for_in_over_a_map_keys_array_reads_live_values_with_map_get`](../crates/pinewasm_vm/src/tests.rs)).
  The keys are snapshotted with `map.keys`; the values are snapshotted with
  `map.values` only when the body may write a map (a `map.put`, `remove`,
  `clear` or `put_all`, or a user function or other call the analyzer cannot
  see into). Otherwise each value is read with `map.get`, which then equals
  the snapshot, and the loop holds one copy of the keys, not two snapshots,
  against the collection budget
  ([`for_in_over_map_binds_keys_and_values`](../crates/pinewasm_semantics/src/tests.rs),
  [`for_in_over_a_map_holds_one_snapshot_against_the_budget`](../crates/pinewasm_vm/src/tests.rs),
  [`for_in_over_a_map_snapshots_values_only_when_the_body_may_write_a_map`](../crates/pinewasm_vm/src/tests.rs),
  [`for_in_over_map_visits_entries_in_insertion_order`](../crates/pinewasm_vm/src/tests.rs)).
  Adding or removing a map's entries while looping through it is detected:
  each iteration first compares `map.size` with the snapshot and stops with a
  runtime error when they differ
  ([`for_in_over_map_stops_when_the_body_adds_or_removes_entries`](../crates/pinewasm_vm/src/tests.rs),
  [`for_in_over_a_map_the_body_resizes_stops_in_vm_and_wasm`](../crates/pinewasm_aot/tests/differential.rs)).
  A change in the last iteration, or a remove and re-put that keeps the size,
  is not detected. Replacing an existing key's value is allowed; the loop
  still binds the value from the entry snapshot.
  A map has no single-binding form (`E0235`,
  [`for_in_over_map_without_key_binding_is_e0235`](../crates/pinewasm_semantics/src/tests.rs)).
  Any other iterable is `E0235`.
- The **version directive** may appear on any top-level line, including after
  imports or executable statements. A repeated directive is ignored with
  warning `W0005` (the first one applies); an *indented* directive is currently rejected as unexpected
  indentation. A `//@version=` comment trailing a
  statement on the same line is treated as a plain comment. A malformed
  directive (e.g. `//@version=abc`) is `E0110`. A version below `6` is
  rejected with `E0249`, because PineWASM implements only version 6
  semantics. Rewrite older source with [`pinewasm migrate`](migration.md). A version above `6`
  compiles with warning `W0003` and version-6 semantics.
- `indicator(...)` is a top-level declaration item. Its required title is a
  string literal (`E0204`/`E0205`), and the supported optional parameters are
  accepted by name and type-checked. Some are presentation metadata, while
  `max_bars_back` and the `max_*_count` limits affect runtime storage; the
  coverage matrix records the status of each parameter. A script has at most
  one declaration (`E0213`), and a declaration inside a block is `E0214`.
- `strategy(...)` is the strategy analog of `indicator(...)`: a
  top-level declaration item whose first argument is a string-literal title
  and whose remaining arguments are the supported strategy parameters
  (`initial_capital`, `default_qty_type`, `commission_type`, `slippage`,
  `pyramiding`, `process_orders_on_close`, …), const-folded into metadata.
  A script may declare **either** `indicator(...)` **or** `strategy(...)`,
  never both — the two are mutually exclusive top-level declarations
  (`E0240`). A duplicate `strategy(...)` is `E0213`. See runtime.md's
  broker-emulator section for the honored vs. metadata parameters.
- **Order primitives.** A strategy places orders with
  `strategy.entry/order/exit/close/close_all/cancel/cancel_all`, directed by
  the `strategy.long`/`strategy.short` constants. They are effect calls that
  may execute conditionally inside `if`/`switch` blocks and require a
  `strategy(...)` declaration; using one in an
  indicator (or with no strategy decl) is `E0241`. The broker emulator fills
  them — market orders at the next bar's open, limit/stop orders intrabar
  against the bar's OHLC — and reports the resulting trades, signed position,
  and per-bar equity in `RunOutcome.strategy`. Stop-limit activation, trailing
  exits, OCA groups, named/reserved partial exits, and strategy execution over
  appended realtime ticks are implemented; see
  [`stop_limit_activates_before_limit_and_persists_across_bars`](../crates/pinewasm_vm/src/tests.rs),
  [`strategy_short_trailing_exit_ratchets_across_bars`](../crates/pinewasm_vm/src/tests.rs),
  [`strategy_oca_cancel_none_and_reduce_apply_to_entry_order_groups`](../crates/pinewasm_vm/src/tests.rs),
  [`separately_reserved_partial_exits_survive_each_others_fills`](../crates/pinewasm_vm/src/tests.rs),
  and
  [`strategy_with_realtime_ticks_executes_against_the_appended_feed`](../crates/pinewasm_vm/src/tests.rs).
  The fill-rule assumptions table remains in [`runtime.md`](runtime.md), and
  [`ir.md`](ir.md) specifies the `ORDER` opcode.
- **Strategy getters.** The broker state is readable back into the
  script through the `strategy.*` getter series — the position
  (`strategy.position_size`/`position_avg_price`/`position_entry_name`), money
  and margin (`strategy.equity`/`openprofit`/`netprofit`/`grossprofit`/
  `grossloss`, `strategy.initial_capital`, `strategy.margin_liquidation_price`,
  `strategy.opentrades.capital_held`), trade counts and retention
  (`strategy.closedtrades`/`wintrades`/`losstrades`/`eventrades`/`opentrades`,
  `strategy.closedtrades.first_index`), and the derived metrics
  (`strategy.avg_trade`/`avg_winning_trade`/`avg_losing_trade`,
  `strategy.max_drawdown`/`max_runup`/`max_contracts_held_all`). The corresponding
  percent series are `strategy.avg_trade_percent`,
  `avg_winning_trade_percent`, `avg_losing_trade_percent`,
  `netprofit_percent`, `grossprofit_percent`, `grossloss_percent`,
  `openprofit_percent`, `max_drawdown_percent`, and `max_runup_percent`.
  Numeric values are builtin `series float`/`series int` reads, while
  `position_entry_name` is a `series string`; all read a per-bar broker
  snapshot, so history (`strategy.position_size[1]`) and `na` on empty metrics
  behave like any other series. They require a `strategy(...)` declaration
  (`E0241`). See ir.md's `READ_BUILTIN` for the getter opcode reuse.
- **Per-trade accessors, risk rules, and conversions.** The broker retains
  its closed trades and open lots, read through indexed accessor functions:
  `strategy.closedtrades.<field>(trade_num)` and
  `strategy.opentrades.<field>(trade_num)` (`trade_num` is a `series int`,
  0 = oldest; an out-of-range index reads `na`, never a panic). Fields cover
  `entry_price`/`exit_price`/`size`/`profit`/`commission`, the bar-index and
  time of entry/exit, and the entry/exit id/comment; `profit_percent`,
  `max_runup_percent`, and `max_drawdown_percent` divide the corresponding
  account-currency value by entry notional and multiply by 100.
  `max_runup`/`max_drawdown` track per-bar marked excursions.
  `strategy.default_entry_qty(fill_price)` estimates the declaration's default
  order quantity at a prospective fill price.
  `strategy.risk.max_position_size` caps the opened same-direction exposure,
  `strategy.risk.allow_entry_in` restricts entry
  direction, and the drawdown/intraday/loss-day halt rules are enforced by the
  broker. `strategy.convert_to_account`/`strategy.convert_to_symbol` are
  host-rate currency interfaces (identity by default). All require a
  `strategy(...)` declaration (`E0241`). The accessors and conversions are
  `CALL_PURE` tags; risk rules are `ORDER` primitives. See runtime.md
  and ir.md.
- **Libraries, `export`, and `import`.** A script may declare
  `library("Title", overlay?)` instead of `indicator(...)`/`strategy(...)`; the
  three script kinds are mutually exclusive (`E0240`), and a duplicate is
  `E0213`. Only in a `library(...)` script may a top-level definition carry the
  `export` modifier — `export dbl(x) => x * 2` — which marks it callable by an
  importer; `export` in an indicator/strategy is `E0243`, and so is a library
  that exports nothing. `library()` takes `title`, `overlay` and
  `dynamic_requests` by position or by name. An
  `import Publisher/LibName/Version [as alias]` statement makes another
  library's exports available under `alias` (defaulting to the library name when
  `as` is omitted): `alias.dbl(21)` calls the exported `dbl`. Imports resolve
  **only** against a caller-provided source map (`compile_with_libraries`,
  docs/browser.md) — never the network; an import path absent from the map is
  `E0243`, and calling a private (non-exported) function through an alias, or an
  unknown exported name, is `E0243`. A duplicate `import` alias is `E0243` (last
  binding never silently wins). Each library is compiled as an **independent
  unit in its own namespace**: an exported function's body and its parameter
  defaults resolve their free names — sibling/private helper calls, top-level
  library globals, and the library's own imports — against the *library's*
  namespace, never the importer's. An importer that defines a same-named
  function or global therefore
  cannot hijack a library-internal name (no silent wrong value), and a
  library-internal name never leaks into the importer. A library global is
  analyzed once (its init runs before any call site), giving it library-scoped
  state shared across sites. The exported function still inlines at each call
  site exactly like a local one (per-call-site instantiation, so a `series`
  argument recomputes per bar and `var`/`ta.*` state is per site) — linking adds
  no CSIL opcode. Imports resolve **transitively** (a library may itself `import`
  another; deduplicated by path) and an import **cycle** terminates. A duplicate
  import **path** in the caller-provided library map is `E0243` (the last entry
  never silently wins), matching the duplicate-alias rule.
  **Diagnostic attribution.** Each library is parsed under its **own** source id
  — `libraries[i]` is source id `i + 1`; the main source is `0` — so a library's
  parse *and* semantic diagnostics carry the library's own source id and byte
  offsets and are never clamped onto the main buffer (a host maps the marker to
  the right library editor; see docs/browser.md). A library that fails to parse
  also emits a summary `E0243` anchored to the `import` statement so a
  main-buffer-only host still surfaces the failure.
  **Overloads and methods.** Every overload of a library function links, and
  `alias.f(...)` picks among the *exported* ones like a local call (see
  [Functions](#functions-user-defined)). A library's methods link too: its own
  bodies call any of them, and an importer calls an exported method on a value
  of its receiver type (`x.m(...)`, also off a call result) or through the alias
  with the receiver first (`alias.m(x, ...)`, where the receiver's type picks
  the method). The importer's own methods are searched first. An imported
  method's body runs in the library's namespace, like an exported function's.
  A library's repeated definitions are `E0224` at the library's definition,
  whether or not the importer calls them: a second function with the same
  signature, or a second method with the same receiver type and signature. A
  method overloaded by its other parameters links every overload, and a call
  picks one by its arguments, as for an overloaded function.
  **Subset (matrix notes):** exported UDTs/enums parse (and carry the `export`
  flag) but are not yet importable, so a library method whose receiver is a type
  the library declares itself does not link; `overlay` is validated for type but
  carries no runtime effect.
- Calls chain only directly off an identifier: `(f)(x)` is a parse error.
- **Named arguments.** Registry-bound built-ins, declaration calls, scalar
  `request.*` interfaces, typed matrix constructors, and user functions may
  write `name = value` after positional arguments
  (`ta.sma(close, length = 14)`, `indicator(title = "X")`,
  `matrix.new<float>(rows = 2, columns = 3)`). A positional argument
  following a named one is a parse error. Semantic analysis binds arguments by
  position first, then by the canonical parameter name. Drawing constructors
  and members bind their parameter names (`table_id` on table
  members, `id` elsewhere; see **Drawing objects** under
  [Series](#series)). The type casts bind `x` (`float(x = 7)`), `timestamp`
  binds each overload's names (`dateString`, `timezone`, `year`, ...),
  `alertcondition`'s `message` may skip `title`, a named `timezone` may skip
  `str.format_time`'s `format`, a single `log.*` argument may be named
  `message` or `formatString`, and the `footprint.*`/`volume_row.*`
  accessors bind `id` (and `price`). `request.footprint` and a few bespoke
  host interfaces remain explicitly positional. The generic
  `array.new<UDT>`, `array.new<MyEnum>` and drawing array constructors bind
  `size` and `initial_value` by name
  ([`typed_and_object_array_constructors_bind_named_arguments`](../crates/pinewasm_vm/src/tests.rs)). Generic
  matrix constructors bind their supported names. A parameter bound twice (once
  positionally and once by name, or the same name twice) and an unknown
  parameter name are both `E0228` (unknown names carry a did-you-mean over the
  signature); a named argument that leaves an earlier required parameter
  unbound is `E0204`. Named binding is a purely front-end rewrite to the
  positional form, so a named call and the equivalent positional call produce
  identical HIR/MIR/CSIL and identical runtime output.
- `var` always declares with `=`; `var x := 1` is a parse error (`E0110`,
  "`var` declares with `=`, not `:=`"). Conversely, `x := 1` always
  *parses* — whether `x` exists is a semantic question (`E0217`), not a
  parse error.
- **Type annotations.** A declaration may be prefixed with a scalar or
  `color` type, `array<T>`/`T[]`, a drawing-object type, a UDT name, or an enum
  name. The annotation fixes the variable's declared type and supplies the
  expected type for `na`: `float x = na`, `Point p = na`, `Signal s = na`.
  It may follow `var` or `varip`; an annotated declaration always uses `=`,
  while later reassignment uses `:=`. The declared type governs later `:=`
  checks (see [Type annotations](#type-annotations) under Semantics for the
  `E0226` compatibility rules). `matrix<T>` and `map<K, V>` annotations are
  accepted too, so far with compile-time tests only; that section lists the
  forms those tests cover.
- **`varip` declares an intrabar-persistent variable.** It parses
  exactly like `var` (including `varip int x = 1` and the `varip x := e`
  parse error) and lowers to a persistent slot; the difference is at
  runtime — a `varip` slot is excluded from realtime-tick rollback, so it
  persists across ticks (see [Realtime execution](runtime.md)). On
  historical bars `varip` and `var` are indistinguishable.
- Statements end at the newline; extra tokens after a complete statement
  (e.g. `plot(x) plot(y)`) are a parse error (`E0110`).
- The **ternary conditional** `cond ? a : b` is the loosest-binding
  expression form and is right-associative: `a ? b : c ? d : e` is
  `a ? b : (c ? d : e)`. The then-branch is a full expression delimited by
  the `:` separator, so `a ? b ? c : d : e` is `a ? (b ? c : d) : e`.
  `:=` always lexes as one token, never as `:` `=`. A `?` without a
  matching `:` and a `:` outside a ternary (including `x ?: y`, which is
  missing its then-branch) are parse errors (`E0110`) with targeted
  messages. Ternary nesting counts against the depth cap below.
- Expression nesting is capped at depth 200; deeper nesting is rejected
  with `E0110` rather than overflowing the parser stack. A declared type has
  the same cap on its own axis: 200 nested `<...>` argument lists (see
  *Nesting limits* below).
- **`if`/`switch` as expressions.** The same `if`/`switch` forms appear both
  as statements and, when consumed as a value, as expressions (`x = if cond
  ... else ...`, `y = switch subject ... => ...`). The two share syntax; the
  parser decides by position (an `if`/`switch` in expression position is an
  expression). A block-form value `if`/`switch` at the tail of a statement is
  terminated by its closing `DEDENT`, so no trailing `NEWLINE` is required
  after it. See [the `if`/`switch` conditional
  forms](#if-and-switch-as-expressions) under Semantics for value rules.
- **`switch`.** A `switch` has an optional subject on its opening line; each
  arm is `label => body` (or `=> body` for the lone `default`). The body is a
  single inline statement (`=> expr`, `=> x := 1`) or an indented block. A
  `switch` arm's `=>` is the `FatArrow` token; a missing `=>` is a parse
  error (`E0110`), and later arms/statements still parse.
- **`once`.** `once` followed by an optional Boolean condition and an
  indented block is an effect statement. Outside that header it remains an
  ordinary identifier (`once = 1`); it is not globally reserved. A Once block
  in a script whose version is below 6 is a parse error (`E0110`). See
  [Once statements](#once-statements) for activation and rollback behavior.
- **Destructuring vs. tuple expressions.** A statement that begins with `[` is
  a **destructuring declaration** (`[a, b] = expr`) when a `=` follows the
  closing `]`, and a **tuple-expression statement** (a function body's trailing
  `[hi, lo]` return value) otherwise. The `target[offset]` history postfix
  needs a preceding primary, so a `[` at a statement or primary start is never a
  history reference. A destructuring pattern's targets are plain identifiers (a
  namespaced or non-identifier target is a parse error), and it uses `=`, not
  `:=` (an `E0110` parse error otherwise). A tuple expression is only
  *semantically* legal as a destructuring RHS or a function body's last
  statement (`E0225` elsewhere), but it always parses so those positions can
  consume it. See [Tuples](#tuples) under Semantics.
- **Function definitions.** A `name(p1, p2) => …` header at the top level is a
  user-defined function definition (the `=>` after the balanced parameter list
  is what distinguishes it from a call: `f(x)` is a call, `f(x) => …` is a
  definition). The body is a single inline statement (`=> expr`) or a NEWLINE
  followed by an indented block; the body's **last statement** supplies the
  return value. Parameters may carry an optional qualifier and type plus an
  optional default (`series float src`, `simple int n = 14`, `p = 1`). A
  definition **inside a block** is `E0224` (definitions are
  top-level only). See [Functions](#functions-user-defined) under Semantics for
  per-call-site instantiation.

### Indentation and blocks

Blocks are delimited by indentation, Python-style:

- The lexer tracks a stack of indentation widths and emits `INDENT` /
  `DEDENT` tokens at line starts, plus a `NEWLINE` at the end of every
  logical line that carried at least one real token.
- A **tab counts as 4 columns**; spaces count as 1. The canonical style is
  4-space indentation per block level.
- Blank lines and comment-only lines produce no tokens and never affect
  indentation.
- A dedent must land exactly on an enclosing block's width; anything else is
  `E0100` (inconsistent indentation). For example, dedenting from 4 columns
  to 2 when the enclosing levels are 0 and 4 is an error.

## Lexical structure

**Identifiers** — `[A-Za-z_][A-Za-z0-9_]*`. Namespaced built-in references
join identifiers with `.` (`ta.sma`); see the grammar notes. User variables
cannot contain dots.

**Keywords** — `if`, `else`, `true`, `false`, `and`, `or`, `not`, `var`,
`varip`, `int`, `float`, `bool`, `string`, `na`, `for`, `to`, `by`, `in`,
`while`, `break`, `continue`, `switch`, `enum`, `type`, and `method` (the exact
set of `TokenKind::keyword_from_str` in
`crates/pinewasm_syntax/src/token.rs`). These cannot be used
as identifiers, except that `method` is a keyword only in `method name(` and is
otherwise an ordinary name (`method = input.string(...)`). The four scalar type names `int`/`float`/`bool`/`string`
introduce type-annotated declarations (`float x = na`); `varip` declares a
run-once persistent slot whose current value survives realtime rollback, while
remaining identical to `var` on historical bars — verified by
[`varip_counts_ticks_var_counts_bars`](../crates/pinewasm_vm/src/tests.rs).
`na` doubles as the callee of
the `na(x)` builtin: `na` immediately followed by `(` parses as a call,
any other `na` is the literal, and `na = 1` / `na := 1` are targeted
parse errors ("cannot assign to `na`").

`once` is recognized contextually from an ordinary identifier when it heads
an indented effect block; it does not add a globally reserved token. See
[Once statements](#once-statements).

**Comments** — `//` to end of line. Ordinary comments produce no tokens.
`//@version=N` and `//@strategy_alert_message=...` annotations are recognized
on top-level lines; the version can also follow imports or statements.

**Number literals** — decimal only:

```
NUMBER   ::= DIGITS [ "." [ DIGITS ] ] [ EXPONENT ]
EXPONENT ::= ( "e" | "E" ) [ "+" | "-" ] DIGITS
```

- *Int-form* (`0`, `42`) has no decimal point and has type `int`;
  *float-form* contains a decimal point or exponent (`1.5`, `8.`, `10e10`,
  `2.5e-3`). A leading digit is required, so `.5` is not a number literal.
  A trailing dot is consumed only when the following byte cannot begin an
  identifier and is not another dot; this keeps the member separator in
  `1.member` distinct.
- An int-form literal must fit a signed 64-bit integer
  (`9223372036854775807` at most); anything larger is a parse error
  (`E0110`, "integer literal out of range"). The value is carried exactly
  as an `i64` from the parser to the runtime — literals above 2⁵³ do not
  round (e.g. `9007199254740993 % 2` is `1`). Float-form literals are not
  range-checked.
- There are no hex/binary number literals or digit separators.
- History offsets require an `int`: an int-form literal (`close[1]`) or an
  `int`-typed expression (`close[n]`); a `float`-form offset (`close[1.0]`)
  is `E0210`. See [History references](#history-references).

**String literals**: single- or double-quoted, or triple-quoted multiline:

```
STRING ::= '"' { character | escape } '"'
         | "'" { character | escape } "'"
         | '"""' { character | newline | escape } '"""'
         | "'''" { character | newline | escape } "'''"
escape ::= "\n" | "\t" | "\"" | "\'" | "\\"
```

- A multiline string (`"""..."""` or `'''...'''`) keeps every line break and
  all leading spaces of its lines as text, from column zero (a CRLF break
  becomes `\n`). A missing closing
  delimiter is `E0102`.

- `\n`, `\t`, `\"`, `\'`, and `\\` have special meanings. A backslash before
  any other character preserves both characters literally (e.g. `"\s"`
  contains a backslash followed by `s`).
  The closing quote must match the opening quote; the other quote kind may
  appear unescaped. A source-wrapped single-line string may continue over a
  physical newline when the next line is a valid continuation (inside
  parentheses, any indentation; otherwise, indentation not divisible by four).
  Each wrapped line contributes one space, regardless of its source indentation.
  An ordinary statement newline or end of input before the closing quote is
  `E0102` (unterminated string literal).

**String operators and the `str.*` namespace.** The `+` operator
concatenates two strings (series-capable, na-propagating; `string + number`
is `E0207`). Equality (`==`/`!=`) works on strings, but strings have **no**
lexicographic ordering, so `<`/`<=`/`>`/`>=` on strings are rejected
(`E0207`). The `str.*` namespace is implemented on the pure-builtin engine:
`str.tostring`, `str.tonumber`, `str.format`, `str.length`, `str.substring`,
`str.replace`, `str.replace_all`, `str.contains`, `str.startswith`,
`str.endswith`, `str.pos`, `str.upper`, `str.lower`, `str.trim`, `str.repeat`,
`str.match`, `str.split`, and `str.format_time`. All are na-propagating, except
`str.trim`, which returns an empty string for `na`. String
length/position count Unicode **code points**; `str.upper`/`str.lower` change
ASCII letters only. Notable subsets and limits (also tracked in
`docs/pine-v6-coverage.md`): `str.format` handles plain `{N}` index placeholders
(no typed `{0,number,...}`/date modifiers); `str.match` uses Rust's
`fancy-regex` engine to return the first matched substring, including
lookbehind patterns (see the `str.match` tests);
`str.tonumber` accepts sign + digits + one `.` (no exponents). `str.split(source,
separator) → array<string>` builds a fresh heap `array<string>` (an empty
separator splits into characters; consecutive/trailing separators yield
empty-string elements — assumed, pinned by test), riding `CALL_COLLECTION` like
`array.from`. `str.format_time(time, format, timezone) → string` formats an
epoch-ms int against the time axis + timezone handling (fixed offsets and
IANA names). It writes these tokens: `yy` (two-digit
year) and `yyyy`, `M`/`MM`, `d`/`dd`, `H`/`HH`, `h`/`hh` (12-hour, 1 to 12),
`m`/`mm`, `s`/`ss`, `a` (`AM`/`PM`), an `S` run (milliseconds, zero-padded to
the run's length) and `Z` (`+HHmm`), plus single-quoted literal spans; other
field letters pass through. The `format.mintick`,
`format.percent`, `format.volume`, and `format.inherit` display constants are
usable as `str.tostring`'s format argument.

**The `color.*` constructor/accessor functions.** Also on the pure-builtin
engine, all operating on the `color` value type (runtime `Value::Color { r,
g, b, t }`, transparency `t` in `0..=100`; see `docs/runtime.md`):

- `color.new(color, transp) → color` — the color with its transparency
  replaced by `transp` (`0..=100`, **clamped**; 0 opaque, 100 transparent). A
  `color.new(c, 0)` value is bit-identical to the equivalent named constant,
  so it plots the same.
- `color.rgb(red, green, blue, transp = 0) → color` — a color from `0..=255`
  RGB channels and `0..=100` transparency. Each argument accepts an int **or a
  float** (for example `color.rgb(math.random(0, 255), …)`). RGB channels
  round to the nearest integer; transparency retains its fractional percentage.
  Every argument is **clamped** into range,
  so an out-of-range channel saturates rather than wrapping or panicking.
- `color.from_gradient(value, bottom_value, top_value, bottom_color,
  top_color) → color` — linear interpolation of all four RGBA channels by
  `value`'s position in `[bottom_value, top_value]`, **clamped** to the
  endpoints outside that range (no extrapolation). The midpoint blends 50/50.
- `color.r/g/b(color) → float` (channel `0..=255`) and `color.t(color) → float`
  (transparency `0..=100`).

All are na-propagating (an `na` color or numeric argument yields `na`). The
argument types are checked (`E0205` on a non-color where a color is required,
and vice-versa).

**Boolean literals** — `true`, `false`, type `bool`.

**The `na` literal** — the keyword `na` denotes the not-available value.
It has no type of its own: it **adopts the type its context expects**,
and the inference is deliberately local and simple. Exactly these
positions are inferable:

- **A builtin function argument** — `na` adopts the parameter's declared
  type (float for the numeric parameters of `plot`, `ta.*`, and the
  `math.*`/`nz` family): `plot(na)` is a float `na`; `nz(na)` is `0.0`.
- **A binary operand whose sibling types** — `na` adopts the other
  operand's type: `close + na` is float, `1 + na` is int (value `na`),
  `s == na` compares strings when `s` is a string. The logical operators
  expect bool, so a bare `na` operand has no expected type: `na and x` / `not na`
  are rejected with `E0220`.
- **The value of a `:=` reassignment** — `na` adopts the target
  variable's declared type: `x := na` stores an `na` of `x`'s nullable
  type. Boolean targets reject this with `E0220`.
- **A type-annotated declaration** — the annotation supplies the type the
  initializer's bare `na` adopts: `float x = na` types `x` as float, `int n
  = na` as int. This is how to declare an `na`-initialized
  variable (see [Type annotations](#type-annotations)).
- **A ternary branch** — the condition of `cond ? a : b` requires bool;
  a bare `na` condition is rejected. An `na` branch adopts the *other*
  branch's nullable type (boolean branches reject `na` with `E0220`)
  (`close > 0 ? close : na` is float, `c ? na : 2` is int). When **both**
  branches are `na`, the surrounding context's expected type applies, as
  for a parenthesized `na`: `plot(c ? na : na)` is float, while
  `x = c ? na : na` is `E0220` on each side.

Everywhere else — an *unannotated* declaration `x = na`, a `var`
initializer without an annotation, a bare `na` statement, unary `-na`, an
`if` condition — there is no expected type and the literal is rejected with
`E0220` (with a note pointing to type annotations, e.g. `float x = na`). At
runtime the literal is the ordinary `na` value and follows every na
rule in [`runtime.md`](runtime.md).

## Expressions and precedence

Binding powers, loosest to tightest (higher binds tighter):

| Level | Operators            | Meaning                        | Operand types → result       |
|------:|----------------------|--------------------------------|------------------------------|
| 0     | `cond ? a : b`       | ternary conditional            | `bool, T, T → T` (mixed int/float branches → float) |
| 1     | `or`                 | logical or                     | `bool, bool → bool`          |
| 3     | `and`                | logical and                    | `bool, bool → bool`          |
| 5     | `==` `!=`            | equality                       | numeric/numeric or same type (bool/string) → `bool` |
| 7     | `<` `<=` `>` `>=`    | comparison                     | numeric, numeric → `bool`    |
| 9     | `+` `-` (binary)     | addition, subtraction          | numeric, numeric → numeric   |
| 11    | `*` `/` `%`          | multiplication, division, remainder | numeric, numeric → numeric |
| —     | unary `-`, `+`, `not` | negation, identity, logical not | `int → int` / `float → float`, `bool → bool` |
| —     | postfix `f(...)`, `e[n]` | call, history reference    | see below                    |

- All binary operators are left-associative; the ternary `?:` is
  right-associative (see
  [the ternary conditional operator](#the-ternary-conditional-operator)).
- Unary `-` binds tighter than `*`, `/`, and `%`: `-a * b` is `(-a) * b`.
- Unary `+` is a numeric identity (`+x == x`, `+close` reads `close`); it binds
  like unary `-` and folds away at compile time (no runtime op). A non-numeric
  operand is `E0207`.
- **`not` is the exception**: it binds *looser* than comparisons and
  equality but tighter than `and`/`or`. `not a > b` parses as `not (a > b)`;
  `not a and b` parses as `(not a) and b`. This is a PineWASM rule; see
  [PineWASM-specific semantic rules](#pinewasm-specific-semantic-rules).
- **`and`/`or` short-circuit:** the rhs is evaluated only when
  the lhs does not decide the result alone (`false and …` and `true or …`
  skip it), so `array.size(a) == 0 or array.get(a, array.size(a) - 1) != x`
  never faults on an empty array. An `na` lhs still evaluates the rhs and
  combines with strong-Kleene three-valued logic — see
  [PineWASM-specific semantic rules](#pinewasm-specific-semantic-rules) and
  [`runtime.md`](runtime.md#na-semantics).
- Postfix call and history bind tightest and may chain syntactically
  (semantic restrictions on history targets apply, see below).
- Parentheses group as usual.
- Operand type errors are reported as `E0207`.

### The ternary conditional operator

`cond ? a : b` evaluates to `a` when `cond` is true and to `b` when it is
false **or `na`** (an `na` condition is treated as false, consistent with
`if` — an assumed rule). It is an expression and is usable anywhere
an expression is: function arguments, history targets, `:=` values, plot
values, and inside other ternaries.

- **Typing.** `cond` must be `bool` (`E0207` otherwise; there is no
  implicit float→bool conversion). The branch types unify: identical
  types stand; a mixed int/float pair coerces the int side to float
  through an explicit `IntToFloat` node; an `na` literal branch adopts
  the other branch's type (see [the na literal](#lexical-structure));
  anything else is `E0207` ("branches have mismatched types"). The result
  qualifier is the join of the condition and both branches.
- **Lazy evaluation.** Only the selected arm runs. A
  ternary with a non-literal arm lowers to real branch blocks: each arm
  stores its value in a hidden non-persistent result slot, and the join
  reads it back (see [`ir.md`](ir.md)). An error or side effect in the
  unselected arm therefore never happens: `i < array.size(a) ?
  array.get(a, i) : na` is safe, pinned by
  [`ternary_size_guard_skips_out_of_bounds_get`](../crates/pinewasm_vm/src/tests.rs)
  and [`ternary_does_not_advance_unselected_ta_state`](../crates/pinewasm_vm/src/tests.rs).
  When both arms are literals (optionally under a unary operator), the
  cheaper `SELECT` opcode is kept; evaluating literals is total and
  side-effect free, so the result is the same. A literal-bound integer
  equality guarding `ta.valuewhen` selects one arm at compile time, so an
  unreachable invalid occurrence is not analyzed.
- **`W0004` fires for series-controlled ternary branches.** A
  series-materializing construct inside an arm
  (`c ? ta.sma(close, 2) : x`, `c ? (close - open)[1] : x`) records nothing
  on bars where the other arm is selected, exactly like an `if`/`switch`
  expression branch, pinned by
  [`series_ternary_branch_history_sites_warn_w0004`](../crates/pinewasm_semantics/src/tests.rs).
  An input- or simple-controlled condition selects the same arm on every
  bar and does not warn. A stateful call in an arm also warns `W0009`.

### `if` and `switch` as expressions

The `if` and `switch` forms double as **value-producing expressions**. An
`if`/`switch` consumed as a value is an expression; in statement position it
keeps its statement behavior.

```text
x = if close > open
    close
else
    open

label = switch bar_index
    0 => "first"
    1 => "second"
    => "other"       // default arm

sign = switch          // subjectless: arms are bool conditions
    close > open =>  1
    close < open => -1
    =>               0
```

- **Branch values.** Each branch is an indented block whose **last statement**
  supplies the value (leading statements run for effect first). A nested
  `if`/`switch` written in statement position as that last statement is
  treated as a nested value form. A branch that is empty or does not end in a
  value is `E0227`.
- **Type unification.** Branch/arm values unify exactly like the ternary:
  identical types stand; a mixed int/float pair widens the int side to float;
  an `na`-literal branch adopts a sibling's type; anything else is `E0207`.
  A missing `else` (or an unmatched `switch`) yields false for a boolean
  result, and an internal missing value for other types. Empty-string defaults
  and other non-boolean cases are not covered by tests. When *every* branch is `na` and no outer type is
  expected, the value has no type (`E0220`).
- **`if` conditions** must be `bool` (`E0208`).
- **`switch` matching.** With a subject, each case label must be
  constant-comparable to the subject and is compared with `==`; a label whose
  type is not comparable to the subject is `E0207`. Subjectless `switch` arms
  are `bool` conditions (`E0208` otherwise). The **subject is evaluated
  exactly once** (into a hidden temp), then reused for every comparison. At
  most one `default` arm, which must follow every labeled arm (`E0225`); there is **no fallthrough** — arms are
  tested top to bottom and the first match wins.
- **Lazy evaluation.** Like the ternary, `if`/`switch` expressions are
  **lazy**: they lower to real branch blocks, so
  only the taken branch's value and side effects run. A `ta.*` call or
  compound history in a branch therefore *does* trigger `W0004` (it skips bars
  where the branch is not taken), just like an `if` statement branch. See
  [PineWASM-specific semantic rules](#pinewasm-specific-semantic-rules).
- **Desugaring.** An `if`/`switch` expression allocates a hidden
  non-persistent result slot; each branch stores its (unified, coerced) value
  into the slot, and the expression reads the slot at offset 0. A `switch`
  desugars to an if/else-if chain on the `==` comparisons. This reuses the
  existing hidden-slot (`%hN`) machinery and adds **no new opcodes**.

### Numeric types and promotion

"Numeric" above means `int` or `float`. The rules:

- **`int OP int` is statically `int`** for `+ - * / %`. Integer arithmetic on
  whole values is exact 64-bit and *checked*: any overflow produces `na` at runtime rather than
  wrapping (assumed behavior, pinned by tests).
- **`int / int` is statically `int` and keeps its fraction** (`7 / 2` is `3.5`,
  `-7 / 2` is `-3.5`), whatever the operands' qualifiers. Version 6 source keeps the
  fraction unconditionally. To discard the fraction, use `int(a / b)` (truncates toward zero) or
  `math.floor`/`math.round`. The quotient is accepted in
  an `int` variable or argument; the VM preserves the fraction until an operation
  explicitly discards it. Anything that can only use a whole number reads
  the whole part (truncated toward zero): a history offset, an array or
  matrix index or size, a length, a value stored in an `int` array, matrix,
  map or object field, a drawing coordinate, a precision, a table cell (see
  "Integer-only consumers" below). A constant folded for an integer-only
  setting (an `input.int` default) is whole in the same way. Native WebAssembly lowers the quotient
  exactly in some shapes and carries any other kept quotient as float code,
  on the chart tier and in standalone export alike
  (local regression tests); on the chart
  tier, a script that formats such a value as text stays interpreter-hosted. A
  fixed-length-only builtin such as `ta.wma` requires a literal, a known int
  binding or an `input.int`, so `ta.wma(src, int(len / 2))` is `E0219`.
  Documented runtime-length forms, including `ta.sma`, follow their own
  current-bar integer-length rules below.
- **Division or remainder by zero produces `na`**, for both int and float
  operands.
- **`%` follows the Rust remainder rule**: the result takes the sign of
  the dividend (`-7 % 3` is `-1`, `-7.5 % 2` is `-1.5`). Negative-operand
  semantics beyond these examples are **assumed**.
- **Mixed int/float operands promote the int side to float**, and the
  operation is float (`7.0 / 2` is `3.5`). The checker inserts an
  explicit coercion node (visible as `IntToFloat` in HIR dumps); the same
  coercion applies wherever a float is required but an int is given —
  `plot(...)` values, mixed comparisons, and mixed equality.
- **Main-program comparisons on two ints compare exactly as i64.** Float comparisons
  treat values as equal when IEEE equality holds or
  `abs(lhs - rhs) <= 1e-10`, so `<` and `>` are false and `<=` and `>=` are
  true inside that tolerance. This applies to script comparisons and the
  supported finite-literal equality guards used by persistent-write analysis;
  it does not round the operands to nine decimal places. The
  [comparison test](../crates/pinewasm_aot/tests/float_comparison_tolerance.rs)
  checks all six operators on 48 finite operand pairs in both the bytecode
  interpreter and standalone Wasm. The
  [persistent-write tests](../crates/pinewasm_compiler/tests/persistent_write_qualifiers.rs)
  separately check how constant equality and inequality guards affect whether a
  write is reachable.
  Missing operands produce ordinary `false` for all six comparison operators.
- **Requested-context comparisons.** Admitted same-timeframe and
  higher-timeframe numeric expression trees use the same float tolerance.
  The [requested-comparison tests](../crates/pinewasm_compiler/tests/requested_float_comparison_policy.rs)
  separately cover all six operators, missing operands and publication clocks.
  This does not widen expression admission: scalar `input.float` operands in
  a request source still produce `E0244`. The requested-expression
  representation remains `f64`, so it does not preserve every large-integer
  comparison that is exact in the main program.
- **Crossing and sign built-ins have separate numeric rules.** The
  `ta.cross`, `ta.crossover`, `ta.crossunder` and `math.sign` built-ins use
  raw float results at `-5e-11` and `5e-11`, while script comparisons use the
  `1e-10` tolerance and treat each as equal to zero. See
  [`cross_and_sign_policy_is_tested_separately_from_comparison_operators`](../crates/pinewasm_aot/tests/cross_comparison_policy.rs)
  for the fixed VM/native vectors; other builtin boundary combinations are not
  covered by tests.
- **Equality requires matching types after promotion**: int==int (exact),
  float==float, bool==bool, string==string. `1 == 1.0` works via
  promotion; `true == 1` is `E0207`.
- **Unary `-`** maps int → int (checked; negating the most negative i64
  yields `na`) and float → float.

## History references

`expr[n]` reads the value an eligible target had `n` bars ago (`n = 0` is
the current bar). Variables and supported computed expressions can have
history regardless of their value qualifier:

- The offset is **numeric**: either a **non-negative integer literal**
  (`close[1]`, `x[0]`, `bar_index[2]`) — the compile-time fast path, which
  reaches up to 5000 bars back with nothing declared (`E0233` beyond) and
  raises the effective `max_bars_back` ceiling to its own depth — or an
  `int`- or `float`-typed expression (`close[n]` with `n` a numeric
  variable, `close[bar_index - 1]`, `close[1.5]`) resolved at run time;
  fractional values truncate toward zero. A nonnumeric offset such as
  `close["x"]` is `E0210`. For a dynamic offset the
  referenced bar is `current_bar - n`. A negative offset (future bar), an
  offset beyond the available history, or an `na` offset is unavailable:
  Boolean history yields `false`, while non-Boolean history yields `na`
  (the negative/`na` cases remain **assumed**, pinned by test). A non-negative
  dynamic offset greater than the effective `max_bars_back` ceiling is a
  structured runtime error. See
  `READ_BUILTIN_DYNAMIC`/`READ_USER_SLOT_DYNAMIC` in [ir.md](ir.md).
- **Identifier targets take a fast path.** A plain identifier naming a
  built-in series or a user variable (parenthesized identifiers are
  unwrapped, so `(close)[1]` works) compiles to a direct offset read of
  that series — no extra storage.
- **Compound targets are materialized implicitly.** History on supported
  computed expressions — `(close - open)[1]`, `(1 + 2)[1]`, `(-2.0)[1]`,
  `float(2)[1]`, `close[1][2]` — allocates a **hidden series slot** for the syntactic
  site: the expression is evaluated, stored into the hidden slot for the
  current bar, and the slot is read back at offset `n` (a literal read, or
  a dynamic `READ_USER_SLOT_DYNAMIC` when `n` is an int expression —
  `(close - open)[i]`). Hidden slots carry compiler-generated names (`%h0`,
  `%h1`, ...) that no source identifier can collide with; they appear in
  slot tables and debug dumps but never participate in name lookup, and
  they are never persistent. Computed `const`, `input` and `simple` targets
  use the same mechanism. A computed target that folds to a variable read
  still captures that variable at the expression site; it does not read a
  later assignment through the variable's final per-bar slot value.
  Drawing `.all` accessors also materialize their fresh array at the site,
  so literal and dynamic offsets refer to the array captured on that bar.
- **Composition** follows from materialization: `close[1][2]` behaves like
  `close[3]`. The hidden slot for `close[1]` holds, at bar `b`, the value
  `close[b-1]`; reading it at offset 2 yields `close[b-3]`. Nesting works
  to any depth (each site gets its own slot).
- History written directly on a literal, a built-in value without direct-history
  capability, or an enum member (`2[1]`, `(2)[1]`, `color.red[0]`) is `E0209`.
  This does not reject a computed expression merely because its value is constant.
  The seven `chart.is_*` chart-type predicates are Simple context variables with
  direct history; their history results are Series, including offset zero.
  `chart.bg_color` and `chart.fg_color` remain Input colors without direct history.
- Unavailable non-Boolean history yields `na` — e.g. `close[1]` is
  `na` on bar 0, and a materialized site's warm-up mirrors the composed
  offset (`close[1][2]` is `na` until bar 3). Boolean history yields `false`
  when its referenced value is unavailable.
- A history read is always a `series` value regardless of the target's
  qualifier.
- **Sites that don't execute every bar warn with `W0004`.** A materialized
  history site inside an `if` branch stores its hidden slot only on bars
  where the branch executes; unavailable reads across a skipped bar yield
  `false` for Boolean history and `na` otherwise. The compiler warns at the
  site. This matches the branch-local behavior of `if` statements
  (**assumed**; the gap behavior is pinned by
  [`compound_history_in_a_branch_reads_gaps_as_na`](../crates/pinewasm_vm/src/tests.rs)).
  A site inside a **`var` initializer** is the limiting case: the
  initializer executes only once, so the hidden slot records that evaluation.
  The variable retains the initial history result unless reassigned; that
  result depends on the offset and value type. `W0004` fires with run-once wording
  ([`compound_history_in_var_initializer_warns_w0004`](../crates/pinewasm_semantics/src/tests.rs)).
  Direct history reads that reuse a variable or built-in series slot never
  warn: they materialize nothing.

## Statements and scoping

**Assignment / declaration** — `x = expr` declares a new variable and
initializes it. The initializer runs **every bar**; on bars where control
flow skips the statement the variable's cell for that bar stays `na`.

- Declaring the same name twice **in the same scope** is `E0211` (use `:=`
  to change an existing variable).
- A declaration may shadow a built-in variable (`float close = open`): the
  name then refers to the variable for the rest of its scope, with warning
  `W0006`. If the script also uses the name for the built-in, in any scope,
  the declaration is `E0211`. Reads the
  compiler adds itself, such as `chart.point.now()`'s `close`, still read
  the built-in.
- Assigning a genuinely `void` call (for example, `x = array.clear(a)`) is
  `E0212`; `plot` and `hline` instead return compile-time handles that may be
  bound and passed to `fill`.
- Without an explicit annotation, a variable's type and qualifier are inferred
  from its initializer.
- A variable named `_` is unused: any number may be declared, several in
  one scope, in a tuple (`[_, b, _] = f()`), as a loop counter or as a
  function parameter. Its
  initializer still runs, but the variable never enters a scope, so reading
  it is `E0201` and reassigning it is `E0217`. A helper called as a statement
  may end with a `_` declaration; where a value is needed, such a tail is
  `E0227`.
- A helper called as a statement may end in a void effect call
  (`strategy.*`, `log.*`, `alert`, `runtime.error`) or in a call to a user
  function, method or library function, which is then also called as a
  statement; using its result is `E0212`.

**`var` declaration** — `var x = expr` declares exactly like `=` (same
scoping, duplicate, and built-in collision rules; `E0211` on redeclare) but
makes the variable **persistent**:

- **Carry-forward**: at the start of each bar after the first, the
  variable's current cell starts as the previous bar's final value instead
  of `na`.
- **Initialize-once**: the initializer expression is evaluated only the
  *first time the declaration statement actually executes*. If the
  declaration sits inside an `if` branch, that may be a later bar; bars
  before the first initialization have an unwritten slot. Unavailable history
  yields `false` for Boolean results and `na` otherwise. Once initialized,
  the value persists and is never re-initialized
  — the runtime tracks a per-slot "ever written" flag (see
  [`runtime.md`](runtime.md)).
- A series-materializing construct inside an initializer
  (`var x = ta.sma(close, 2)`, `var d = (close - open)[1]`) executes only
  when the variable initializes, and the compiler warns `W0004` at the site.
  The variable retains that computed result unless reassigned; it is not
  necessarily `na` (see
  [History references](#history-references) and the diagnostics table).
- `var x := expr` is a parse error: `var` declares with `=`.
- Initialize-once and carry-forward semantics are PineWASM's own model of
  `var`; the conditional-first-execution nuance (a `var` inside a
  branch initializing on the first bar the branch runs) is **assumed** and
  pinned by tests. `varip` shares this machinery, adding intrabar
  persistence (see below).

### Type annotations

A declaration may name the variable's type explicitly with a leading scalar
or `color` type, `array<T>`/`T[]`, drawing-object type, UDT name, or enum name.
The annotation combines with `=` and optionally `var` or `varip`, for example
`int count = 0`, `float x = na`, `var array<float> samples = na`,
`varip line guide = na`, `Point p = na`, or `Signal s = na`. `matrix<T>` and
`map<K, V>` annotations are accepted too; they have compile-time tests only
and are described after the rules below.

- The annotation **is** the declared type used for every later `:=` check
  and for history typing — it does not depend on the initializer.
- The initializer must be compatible with the annotation:
  - the same type is accepted as-is;
  - a **`float` annotation accepts an `int` initializer**, coercing it with
    `IntToFloat` (`float x = 3` stores `3.0`) — this is the only widening,
    matching the `:=` rule;
  - a bare `na` initializer adopts the annotated type, including reference and
    nominal types, which is how to declare an
    `na`-initialized variable;
  - any other disagreement — `int n = 1.5`, `bool b = 1`, `string s = 3`,
    `float f = true` — is **`E0226`**.
- An annotated header uses `=`; `int n := 1` is a parse error. Scalar type
  keywords (and `varip`) are reserved and cannot be identifiers.

**`matrix<T>` and `map<K, V>` annotations** are implemented, so far with
compile-time tests only: the tests below stop at semantic analysis or at
compilation, and no test runs a script that declares a variable, field, or
parameter through one. The rules above are therefore stated for them only
where one of these tests covers the case:

- **Accepted:** a `matrix<float>` variable declaration and a
  `var map<string, float>` declaration
  ([`matrix_and_map_annotations_resolve_to_collection_types`](../crates/pinewasm_semantics/src/tests.rs));
  a `map<string, float>` field of a user-defined type and a `matrix<float>`
  function parameter
  ([`matrix_and_map_types_in_fields_and_parameters`](../crates/pinewasm_semantics/src/tests.rs));
  and a function with `matrix<float>` and `map<string, float>` parameters
  ([`generic_matrix_and_map_function_parameters_survive_v5_conversion`](../crates/pinewasm_migrate/tests/golden.rs)).
- **Rejected:** a `matrix<int>` annotation on a `matrix<float>` initializer
  ([`mismatched_matrix_annotation_is_an_error`](../crates/pinewasm_semantics/src/tests.rs));
  a non-matrix argument to a `matrix<float>` parameter
  ([`declared_user_parameter_types_reject_wrong_argument_types`](../crates/pinewasm_compiler/tests/v6_call_binding.rs));
  an unknown map value type, which is `E0236`
  ([`unknown_map_value_type_is_e0236`](../crates/pinewasm_semantics/src/tests.rs));
  a map key type that is a user-defined type or a collection, which is
  `E0234`
  ([`non_scalar_map_key_annotation_is_e0234`](../crates/pinewasm_semantics/src/tests.rs));
  and a `map<MyEnum, V>` variable or field annotation, which is accepted
  ([`map_enum_key_annotation_is_accepted`](../crates/pinewasm_semantics/src/tests.rs)).

Other annotation forms have no test, so no claim is made for them; examples
are a user-defined type as the element or value type (`matrix<MyType>`), a bare
`na` initializer (`matrix<float> m = na`), and a `matrix<T>` field. The
constructors behind these annotations accept enum element, key, and value
types: `matrix.new<MyEnum>()`
([`matrix_new_accepts_enum_element_type`](../crates/pinewasm_semantics/src/tests.rs)),
`map.new<MyEnum, V>()`
([`map_new_accepts_enum_key_type`](../crates/pinewasm_semantics/src/tests.rs)),
and `map.new<K, MyEnum>()`
([`map_new_accepts_enum_value_type`](../crates/pinewasm_semantics/src/tests.rs)),
with run-time values pinned by
[`enum_collections_store_and_return_enum_members`](../crates/pinewasm_vm/src/tests.rs).
`for … in` iterates a matrix by rows and a map by entries
([`for_in_over_matrix_binds_rows`](../crates/pinewasm_semantics/src/tests.rs),
[`for_in_over_map_binds_keys_and_values`](../crates/pinewasm_semantics/src/tests.rs)).

**`varip`** — an intrabar-persistent declaration. It is a reserved
keyword (never an identifier) and parses exactly like `var`: `varip x = e`,
`varip int x = 1`, and the `varip x := e` parse error all mirror the `var`
forms. It lowers to a persistent slot, so on historical bars it is
indistinguishable from `var` (initialize-once + carry-forward). The one
difference is realtime execution: a `varip` slot's current cell is excluded
from the per-tick rollback, so it persists across the ticks of a realtime bar
while a plain `var` rolls back to its bar-start value each tick. See
[Realtime execution](runtime.md).

**Reassignment** — `x := expr` assigns a new value to an **existing**
variable:

- The target must resolve to a user variable in the current or an enclosing
  scope within its permitted function frame — `:=` never declares. An `if`
  branch can update an outer binding in that frame; lookup is innermost-first,
  so a shadowing inner declaration takes precedence. A function or method
  cannot rebind a global, or one of its own parameters (`E0217`; assign a
  local copy instead), but may still mutate the contents of a referenced
  array or object. A caller's bindings are not visible to it at all.
- Reassigning an undeclared name is `E0217` ("declare it first with `=`");
  reassigning a built-in series (`close := 1`) or a function name
  (`plot := 1`) is `E0217` with a tailored message.
- **Type rule**: the value's type must equal the variable's type, except
  that an `int` value may be assigned to a `float` variable (the checker
  inserts an explicit `IntToFloat` coercion). Anything else is `E0218`.
  The variable's declared type never changes.
- **Qualifier rule**: reassignment joins the current qualifier, the value's
  qualifier, and conditions controlling writes outside the variable's own
  declaration scope. Input/simple controls retain that strength; series
  controls retain `series` and their intermittent-call warnings.
- For inferred persistent value scalars (`var`/`varip`), active independent
  stable writes before every read have a minimum `simple` qualifier.
  Source-identified carried reads, history, series values and series controls
  remain `series`; a read preceding a later active write is qualified before
  earlier consumers are checked. Series strength is never erased by a later
  literal assignment. Reference values retain their existing rules. An explicit
  qualifier still constrains the initializer and each write; it does not prove
  that a persistent read is fixed at an earlier use.
- This persistent summary is bounded. Unsupported closed guards retain the
  previous early qualifier and series-on-write treatment, and helper/loop
  handling is not a general interprocedural solver. Some Boolean recurrence
  diagnostics remain conservatively `series`. Exact source cases
  and runtime offset cases are covered in
  [persistent write regressions](../crates/pinewasm_compiler/tests/persistent_write_qualifiers.rs).
- When a complete summary proves a potentially carried read before a later write
  in the main program, an
  explicitly qualified `var`/`varip` value scalar also has `series` strength
  before consumers are checked. Its annotation cannot preserve a literal or
  input origin for a fixed display setting. Input initializers still register
  their IDs, defaults and order and execute normally; fields that admit series
  values, such as plot colors, retain per-bar values. The
  [local fixedness regressions](../crates/pinewasm_compiler/tests/persistent_fixedness.rs) cover these producer/consumer
  boundaries separately from the other admission tests.
- Helper guard facts are deliberately narrow. A unique, local, non-exported
  helper with no parameters and one expression can supply Boolean
  literal/group/`not` truth or a numeric comparison. Both comparison operands
  must be supported numeric literals or direct, unshadowed bare numeric built-in
  series leaves (grouping is allowed), with at least one series leaf. Arithmetic,
  dotted names and calls are not comparison operands in this proof.
  A series dependency is not proof that the comparison changes truth value
  between bars. Parameters, captures, methods, generic forms, duplicate
  definitions, transitive calls, statements and excessive nesting stay opaque.
  Helper truth is used for conditions, not general scalar constant folding;
  known-false and lazy conditions retain their effect on reachable writes
  ([`false_lazy_and_opaque_helper_controls_keep_their_safe_values_and_offsets`](../crates/pinewasm_compiler/tests/persistent_fixedness.rs)).
- Works on both plain and `var` variables. Reassigning a plain variable
  still only writes the current bar's cell; persistence comes from `var`.

**Expression statement** — an expression on its own line. Effect calls
(`plot(...)`) are the intended use. A pure or otherwise meaningfully
value-producing expression whose result is discarded produces warning `W0002`.
Side-effect-first calls whose return value is optional — drawing
creation and `array.pop`/`array.shift`/`array.remove` — may stand alone without
the warning. The last expression of an `if`, `switch`, `for` or `while` block is
that block's value, not a discarded one, and never warns.

**`if` / `else if` / `else`** — the condition must be `bool` (`E0208`;
there is no implicit float→bool conversion, so `if close` is an error).
Each branch body is an indented block. `else if` chains desugar to an
`else` branch containing a nested `if`.

**Scoping rules:**

- There is one global scope; every `if` branch introduces a new nested
  scope.
- Variables declared inside a branch **do not leak**: they are not visible
  after the block ends.
- **Shadowing is allowed**: a branch may declare a variable with the same
  name as one in an enclosing scope; the inner declaration wins inside the
  block.
- Redeclaring within the *same* scope is `E0211` (see above).
- Name lookup searches scopes innermost-first.
- Plot-family calls and `alertcondition()` are global-scope-only. Using one in
  an `if` branch, loop, or user function, or a `plot()`/`hline()` call as a
  ternary operand, is `E0254`; compute a conditional series locally and pass
  that value to one top-level plot call.
- A scope may declare at most 1,000 variables, and a script may use at most 64
  plot counts (one per plotted value series, plus one per color argument in
  the series form; see the [coverage matrix](pine-v6-coverage.md)). Going over
  either limit is `E0253`.

**`for` loop** — `for counter = start to end [by step]` runs its indented
body once for each value the counter takes. A loop header may also be used as
an expression, e.g. `result = for item in values`, with the loop's final body
expression supplying the result. The migrator parses and preserves these value
loops; the compiler currently reports `E0250` because it does not lower them.

- **A counted loop at the end of a value block** (including a user function)
  returns its final expression or assignment from the latest iteration that
  reaches it. If no iteration reaches that line, the result is `na`. A
  `continue` skips the line; a `break` retains the previous result. Focused
  local regression tests and
  [`loop_tailed_function_returns_last_evaluated_value_or_na`](../crates/pinewasm_vm/src/tests.rs)
  cover this form. Direct loop-result declarations and `while` result
  evaluation remain unsupported by the compiler; loop-body tails that are
  nested conditionals also remain untested.

- **Bounds and evaluation timing.** `start`, `end`, and `step` are `int` or
  `float` expressions (`E0222` otherwise). `start` and `step` are evaluated
  once at loop entry; `end` is evaluated once to choose the direction for an
  implicit step, then re-evaluated before every iteration.
  Mutating an end-bound variable in the body can therefore extend or shorten
  the loop. The regression
  [`for_end_boundary_is_reevaluated_each_iteration`](../crates/pinewasm_vm/src/tests.rs)
  covers this behavior.
- **The counter is a fresh loop-local variable.** It lives in a new scope for
  the body only (the same scoping as an `if` branch — see below), does not
  leak past the loop, and may shadow an outer name. Assigning to the counter
  with `:=` inside the body is **`E0217`** ("cannot reassign the loop
  counter"): the loop controls it.
- **The counter's type** is `int` when `start` and the explicit or implicit
  step are `int`; a fractional `end` remains fractional in the comparison
  without widening that counter. A float start or step widens the counter.
- **Step and direction.** An explicit `by step` is a positive magnitude; the
  initial bounds select addition when `start <= end` and subtraction when
  `start > end`. The last eligible `end` value is inclusive. With no `by`,
  the magnitude is 1. A zero or negative explicit step compiles but aborts
  at runtime on the first executed bar with a `UserError` saying the step
  must be greater than zero.
- An `na` bound makes the loop's comparison `na`, which — like an `na` `if`
  condition — is treated as false, so the loop exits (assumed, consistent
  with `if`).

**`while` loop** — `while cond` runs its body while `cond` is `true`. The
condition must be `bool` (`E0208`, like `if`; an `na` condition is treated as
false and stops the loop) and is re-checked every iteration. A `while true`
runs until the per-bar instruction budget cuts it with a structured
`InstructionLimitExceeded` error — never a hang (pinned by
[`infinite_while_true_hits_the_instruction_budget`](../crates/pinewasm_vm/src/tests.rs)).

**`break` / `continue`** — valid only inside a loop body (**`E0223`**
otherwise, including inside an `if` that is not itself inside a loop). They
affect the **innermost** enclosing loop: `break` exits it, `continue` skips
to its next step (the counter advance for a `for`, the condition re-check for
a `while`).

**Loop body scoping and history (assumed).** A loop body is a nested scope,
exactly like an `if` branch: declarations do not leak, shadowing is allowed,
and a `:=` inside the body joins `series` onto the reassigned variable (a
loop-carried variable is path/time-dependent). This matches the
branch-local behavior of `if` statements; the precise loop-local history
semantics are **assumed**.

**Stateful loop calls remain a frontend restriction.** Calling a
stateful `ta.*` function **or** materializing compound history
(`(expr)[n]`, a `ta.obv`-family bare name) inside a loop body is a compile
error (**`E0222`**). The SMA storage supports repeated same-bar replacement, but the compiler still rejects the source-level SMA loop
form. Other stateful loop combinations are not covered by tests. The same
constructs are accepted at global scope or inside an `if` branch. See
[PineWASM-specific semantic rules](#pinewasm-specific-semantic-rules).

### Once statements

`once [condition]` introduces an indented effect block. An omitted condition
is `true`; an explicit condition must be Boolean (`E0208`). The block has its
own scope and no result value. It may be the final statement of a helper,
but a caller cannot bind its `void` result
([`helper_call_sites_have_independent_flags_and_void_results`](../crates/pinewasm_compiler/tests/once.rs)).

Each syntactic instance has its own persistent activation flag. It executes
when first reached with a true condition, then remains inactive after that
execution commits. Ordinary realtime rollback also restores the activation:
an open-bar execution can run again on the next update, and only an
execution committed on the closing update deactivates it for later bars.
Effects retain their own policies: `varip` changes and emitted logs survive
the rollback. A new run starts with fresh activation flags.

The [compiler tests](../crates/pinewasm_compiler/tests/once.rs) cover omitted,
delayed and nested conditions, independent helper call sites, run resets,
errors, carried write qualifiers and open/closing ticks. The
[native test](../crates/pinewasm_aot/tests/once.rs) exercises standalone
activation. Coverage is limited to the helper, loop, and recalculation cases
exercised by the linked tests. Existing
restrictions on stateful calls inside loops still apply.

## Functions (user-defined)

A top-level `name(p1, p2) => body` defines a user function. The headline
semantic is **per-call-site instantiation** (auto-instantiation): a function is an AST *template*, and every call site
**inlines** a fresh copy of it during semantic analysis. There is no call
frame at runtime and no function or call node in HIR/MIR/CSIL — the compiler
expands the call before any later stage sees it (see
[`runtime.md`](runtime.md)).

At each call site the expansion:

1. **Evaluates every argument exactly once** into a hidden temp bound to the
   parameter — so `dbl(ta.sma(close, 2))` for `dbl(x) => x + x` computes the
   `ta.sma` once and adds the temp to itself (verified by
   [`function_arguments_are_evaluated_once`](../crates/pinewasm_semantics/src/tests.rs)).
   A parameter reads its temp's slot, so history on a parameter (`p[1]`) works.
2. **Analyzes the body in a fresh scope** where the parameters resolve to those
   temps; leading statements run for effect and the last statement supplies the
   return value (which the caller reads). Names in the body resolve where the
   function is defined, not where it is called: its own parameters and locals
   first, then the globals declared above the definition. A caller's
   parameters and locals stay invisible, even though the body is inlined among
   them, so `g(x) => f()` cannot change what `x` means in `f() => x`, and a
   name that only a caller declares is unknown in the body (`E0201`). A global
   declared below the definition is invisible too, even when every call comes
   after it: `f() => y` then `y = 3.0` then `plot(f())` is `E0201`, and a body
   that names a built-in shadowed only by a later global reads the built-in,
   so the script both shadows and uses it (`E0211`). See
   [`function_lexical_scope.rs`](../crates/pinewasm_compiler/tests/function_lexical_scope.rs).
3. Gives **every stateful construct in the body fresh per-site state**:
   `ta.*` call sites, materialized history, `var` declarations, and
   `if`/`switch` result slots are all allocated anew for that instantiation.
   Two call sites of a function containing `var c = 0` therefore keep
   **independent counters**, and two call sites of a function containing
   `ta.sma` keep **independent windows** (verified in the semantics tests and
   the [`golden_user_defined_functions`](../crates/pinewasm_compiler/tests/pipeline.rs)
   end-to-end run). Each call site gets independent state, as described above.

Other rules:

- **Parameters** may be untyped identifiers or carry a qualifier
  (`const`/`input`/`simple`/`series`) and declared type. The declared type is
  checked at every call: an argument of another type is **`E0205`**
  (`` `f` argument `a` must be int, found float ``), except that an `int`
  argument widens to a `float` parameter. The type decides, not the value:
  `int / int` is statically `int`, so `f(7 / 2)` binds an `int` parameter
  and the parameter holds 3.5, while `f(7 / 2.0)` and `f(3.5)` are `E0205`
  ([`an_int_quotient_is_an_int_argument_and_keeps_its_fraction`](../crates/pinewasm_compiler/tests/typed_user_parameters.rs),
  [`a_float_is_not_an_int_argument`](../crates/pinewasm_compiler/tests/typed_user_parameters.rs)).
  Per-call-site inlining derives the effective qualifier from the
  argument. **Default values** (`p = 1`) let a call omit trailing arguments.
  A typed parameter's default must have the declared type too, with the same
  widening. A call that omits the argument evaluates the default and reports
  a mismatch as `E0205` at the default (`` `f` default for parameter `a` must
  be int, found float ``). A default that no call evaluates is checked when
  it is built from literals alone (`int a = 2.5`, `string s = 1`,
  `int a = 7 / 2.0`): such a definition is `E0205` also when `f` is never
  called, or always called with the argument, in a library as in a script,
  and so is a method's receiver default, which no call can omit
  ([`a_mistyped_default_is_an_error_at_the_definition`](../crates/pinewasm_compiler/tests/typed_user_parameters.rs),
  [`a_method_receiver_default_is_checked_though_no_call_can_omit_it`](../crates/pinewasm_compiler/tests/typed_user_parameters.rs),
  [`a_library_default_is_checked_whether_or_not_the_importer_calls_it`](../crates/pinewasm_compiler/tests/typed_user_parameters.rs)).
  That check runs after the statements, so its error follows the script's
  other errors. Any other default (one that uses `na`, names a variable, a
  built-in or a function, or applies an operator to operands it does not
  take) has no type until a call evaluates it, so a function that is never
  called without that argument is not checked for it; neither is a default
  whose parameter declares a type that does not exist
  ([`a_default_that_names_a_value_is_checked_where_a_call_omits_it`](../crates/pinewasm_compiler/tests/typed_user_parameters.rs),
  [`a_default_that_is_not_well_typed_is_left_to_the_call_that_evaluates_it`](../crates/pinewasm_compiler/tests/typed_user_parameters.rs)).
  A default is part of the definition: it resolves names as the body does,
  never in the caller's scope, and it is evaluated at each call that omits
  the argument. Arguments the caller writes resolve in the caller's scope.
  Wrong argument count is **`E0224`**, and so is omitting a parameter that
  has no default: in `f(a = 1.0, b) => a + b`, the call `f(5.0)` has an
  allowed count but binds `a`, so it reports `` `f` is missing required
  argument `b` ``.
- **Auto-qualifier.** Each parameter takes the join of its argument's
  qualifier; the return qualifier is the body result's qualifier. Type-checking
  is per instantiation — a function called with a `float` at one site and an
  `int` at another produces two independently checked instantiations.
- **Recursion is `E0224`.** Direct (`f` calls `f`) or mutual (`a` calls `b`
  calls `a`) recursion cannot be inlined and is rejected. Functions may call
  other **acyclic** functions, which are expanded transitively.
- **Calls do not depend on definition order.** Every definition is collected
  before analysis, so a call may name a function or method defined below it,
  at the top level or in another body.
- **Overloads.** Several definitions may share a name when their parameter
  counts or declared types differ. A call picks the one whose parameters can
  bind its arguments (positional, named and defaulted); when more than one can,
  the declared parameter types decide against the argument types known before
  analysis: literals, variables, built-in series and constants, casts, pure
  built-ins with a fixed result type, history references, and arithmetic,
  comparisons, logic and ternaries over those. A known mismatch disqualifies,
  and an exact match outranks an `int` argument for a `float` parameter. The
  choice is never a guess: a tie, or candidates that declare different types
  for an argument whose type is not known before analysis (a `ta.*` call, say),
  is `E0224` ("cannot tell which overload"); assigning that argument to a
  typed variable first resolves it. One overload may call another of the same
  name; that is not recursion. A call no overload can take is `E0224`.
- **Definitions are top-level only** (`E0224` inside a block); a **duplicate
  parameter name**, or a second definition with the **same name and
  signature** (parameter count and declared types), is `E0224`.
- **Plot-family calls inside a function body are `E0254`; an
  `indicator(...)` declaration there is `E0214`.** Output declarations are
  global-scope-only, so use them at the top level.
- **Instantiation-size guard.** A pathological call graph (a deep or
  fan-out chain that inlines to a huge body) is cut by a hard cap on the total
  number of expanded body statements
  (`MAX_EXPANSION_NODES` = 10 000), reported once as a structured `E0224`
  ("function expansion exceeded the …-node limit") — never a hang or OOM
  (verified by
  [`expansion_guard_reports_a_structured_error_on_a_bomb`](../crates/pinewasm_semantics/src/tests.rs)).
- **Nesting limits.** One expression's tree may be at most 200 levels tall
  (`MAX_EXPR_DEPTH`), counting every way of adding a level: parentheses,
  call arguments, prefix operators, ternaries, each further term of an
  operator chain (`a + b + c ...`) and each further link of a postfix chain
  (`a.b().c() ...`, `a[1][1] ...`). An assignment such as `x = a + a + ...`
  therefore holds up to 198 terms; a longer one is `E0110`
  ("expression is nested too deeply"). Argument lists and tuple elements are
  siblings and do not count. A declared type may nest at most 200 `<...>`
  argument lists (`MAX_TYPE_DEPTH`), wherever it is written (a `type` field,
  a parameter, a method receiver, a variable annotation): each `array<...>`,
  `matrix<...>` and `map<...>` counts one level, the two arguments of a map
  sit at the same level, and the `T[]` sugar adds an array level without
  counting. A deeper type is `E0110` ("type is nested too deeply"); holding
  an inner collection in a field of a user-defined type avoids it.
  Across a whole call path the analyzer allows 200
  units of nesting (`MAX_ANALYSIS_NESTING`): one per expression level, two
  per method call and per `if`/`for`/`while`/`switch` block, summed through
  the user functions inlined along the way. Each `else if` link of a statement
  chain is charged one unit, so links and the expressions they hold share the
  one budget; a flat chain of up to 197 arms still compiles, and a chain of
  198 or more, which base accepted up to 199, is `E0224`.
  An `if ... else if` used as a value (`x = if ...`) is charged as a nested
  `if` expression, so it is limited well below the statement form; a 120-arm
  value chain is `E0224`. More is `E0224`
  ("code is nested too deeply"). The expression and analysis limits keep the
  compiler inside the browser package's 1 MiB stack (the measured worst
  accepted inputs compile there; native release builds need more stack, so the test thread is 2 MiB), where an overflow is a trap that the host
  cannot recover from; assigning part of a long expression to a variable
  avoids either. The type limit is the same guard on its own axis: measured
  natively, parsing a type at its cap needs about a third of the stack that
  parsing an expression at its cap does, and a type at its cap compiles at
  the bottom of 90 nested blocks on that 2 MiB thread (all verified by
  [`long_expression_chains.rs`](../crates/pinewasm_compiler/tests/long_expression_chains.rs)).
- **`W0004` inheritance.** Calling a function that contains a stateful
  construct from inside a **conditional branch** warns `W0004`: the
  instantiation inherits the branch's conditional context, so the inlined
  stateful site skips bars where the branch does not execute. The same call at
  the top level does not warn (verified by
  [`calling_a_stateful_function_from_a_branch_warns_w0004`](../crates/pinewasm_semantics/src/tests.rs)).

## Enums (user-defined)

A top-level `enum Name` followed by an indented member list defines an
enumeration:

```
enum Signal
    buy = "Buy signal"
    sell = "Sell signal"
    neutral
```

Each member is referenced by dot notation, `Signal.buy`, and is a value of the
distinct type `Signal`. An optional `= "title"` string supplies the member's
display title (for an `input.enum` dropdown); when omitted, the title
defaults to the member name (`neutral`'s title is `"neutral"`).

Semantics:

- **Distinct value type.** An enum value is *nominally* typed: `Signal` is not
  `int` and not any other enum, even one with identical members. The checker
  permits only `==` / `!=` and `switch` between values of the **same** enum
  type; comparing an enum to an `int` or to a different enum is **`E0207`**.
- **Runtime representation.** A member lowers to its zero-based declaration
  index as a constant integer, carrying the enum type through the checker. `==`
  / `!=` and `switch` therefore reduce to integer comparison in the VM — no new
  opcode or heap object is involved (an enum value is not a reference value).
- **`switch` over an enum subject** matches each arm's member label against the
  subject; a `default` (`=>`) arm covers the rest (see the switch rules under
  [Statements](#statements-and-scoping)).
- **Persistence.** A `var`-declared enum variable persists its member across
  bars like any `var`, and is restored on realtime-tick rollback like any
  scalar (see [`runtime.md`](runtime.md)).
- **Errors (`E0239`).** A **duplicate enum name**, a **duplicate member** within
  one enum, an enum with the **name of a built-in type or namespace** (`enum
  ta`; unlike a user-defined type, an enum may not take one), or a reference to
  a **member that does not exist** on the enum is `E0239`. An `enum` with no members is a parse error.

Verified end-to-end (member equality, distinct-member inequality, `switch`
selection, `default` arms, and `var` persistence across bars) by the
[enum VM tests](../crates/pinewasm_vm/src/tests.rs) and the
[`enums.pine`](../tests/compile-pass/enums.pine) fixture.

An enum member is also usable as the `defval` of an
[`input.enum(...)`](#the-input-interface--host-overrides-and-controls)
declaration.

Enum names are valid type annotations, including `Signal s = na`; the
annotation supplies the nominal enum type and governs later reassignment —
verified by the enum/type-annotation rows in the coverage matrix.

## User-defined types

A top-level `type Name` followed by an indented list of typed field declarations
defines a **record** — a user-defined type whose instances carry a fixed set of
named fields:

```
type Point
    float x
    int   y = 5
    string label
    Point next
    array<float> samples
```

Each field declares a type, a name, and an optional `= default` value. Field
types may be a scalar (`int`/`float`/`bool`/`string`), `color`, `array<T>`, or a
bare name referring to another declared type or enum — **including the owning
type itself**, so self-referential structures (a linked-list `Node next`) are
legal. A `map<K, V>` field is accepted too, with a compile-time test only (a
`map<string, float>` field in
[`matrix_and_map_types_in_fields_and_parameters`](../crates/pinewasm_semantics/src/tests.rs));
a `matrix<T>` field has no test, so no claim is made for it (see
[Type annotations](#type-annotations)). Field order is the runtime slot order.

- **Reference values on the heap.** An object is a `Value::Ref` handle into the
  per-run heap (`HeapObj::Object`), exactly like an `array`/`map`/`matrix`.
  Assignment copies the *handle*, so two variables can alias one object and
  observe each other's mutations; a `var`-held object persists across bars and
  rolls back on a discarded realtime tick. An **unassigned object variable is
  `na`**.
- **Construction.** `Name.new(...)` builds an instance. Arguments may be
  **positional** or **named** (`Point.new(3.0, y = 7)`); an omitted field takes
  its declared default, or `na` when it has none (`false` for a `bool` field,
  which is never `na`). A declared default is a literal (a sign before a
  number included), `na`, a built-in variable or constant, or an enum member;
  a function call or other expression is `E0236`. `Name.copy(obj)`, or `obj.copy()`, is a
  **shallow copy**: a NEW handle whose field values are copied from `obj`
  (independent of the original).
- **Field access.** `obj.field` reads a field; `obj.field := value` writes one.
  Both chain through nested objects (`outer.child.x`, `node.next.next.val`). A
  field write coerces `value` to the field type (an `int` widens to a `float`
  field). A field read is `series`, because a field can be reassigned on any
  bar, so it cannot be passed where PineWASM requires a `simple` value (a
  `ta.ema` length is `E0219`). A `ticker.*` argument may be `series`:
  `ticker.new("NASDAQ", s.symbol)` returns a per-bar `series string`,
  which in turn cannot fill a `simple string` parameter (with constant or input
  arguments the result is `simple string`, never `const`). A value computed from a field
  (a variable reassigned under `if s.fast`, or `s.fast ? 20 : 50`) is `series`
  as well. Object fields have **no per-bar history**: `[]`
  written directly on a field (`obj.field[1]`, `arr.get(0).field[n]`) is
  `E0209`; bind the field to a variable and take that variable's history
  instead. `[]` after a call takes the history of the call's result, so it
  stays valid when the call reads a field: a drawing getter
  (`line.get_x1(id)[1]`), a user function or a method.
- **Errors.** A **duplicate type name** (or a name already taken by an enum), a
  **duplicate field**, or an **unknown field type** is `E0236`. A
  **construction/write field-type mismatch**, an **unknown field**, or a call of
  an unsupported method (`Name.foo`) is `E0237`. Reading or writing a field of an
  **`na` object** is a structured runtime error (`NaFieldAccess`), never a panic.
  A `type` with no fields is a parse error.

Verified end-to-end (positional and named construction, defaults, field
read/write across bars via `var`, aliasing vs `copy` independence, a two-level
nested type, a self-referential linked-list walk, and the `na`-object structured
error) by the [UDT VM tests](../crates/pinewasm_vm/src/tests.rs), the
[UDT semantics tests](../crates/pinewasm_semantics/src/tests.rs), and the
[`udt.pine`](../tests/compile-pass/udt.pine) fixture.

UDT names are valid annotations, including `Point p = na`. Remaining UDT
boundaries are `input`-of-UDT, complete `matrix<MyType>` operation coverage
(the tested subset is listed under
[Collections of UDTs](#collections-of-udts)), and per-field history
(`obj.field[1]` is `E0209`); see the coverage matrix's UDT row.

### Methods

A **method** is a function whose first, explicitly-typed parameter is the
**receiver** it associates with:

```
method length(Point this) =>
    math.sqrt(this.x * this.x + this.y * this.y)

method move(Point this, dx, dy) =>       // multi-line body; mutates + returns this
    this.x := this.x + dx
    this.y := this.y + dy
    this
```

- **Declaration.** The `method` keyword precedes the name. The first parameter
  carries an explicit receiver type (a UDT name, or a collection form such as
  `array<float>`); the remaining parameters use the same optional
  qualifier/type/default syntax as plain-function parameters. Single-line
  (`=> expr`) and multi-line (indented block) bodies are both allowed.
- **Call.** `obj.f(args)` resolves to the method registered for `obj`'s type and
  **inlines** it with the receiver prepended as the first argument — reusing the
  plain-function per-call-site instantiation machinery. A method body's `var` /
  `ta.*` therefore gets **fresh per-call-site state**, and reference semantics on
  the receiver object flow through (a mutating method's field writes are visible
  to the caller).
- **Chaining.** A method that returns an object enables `obj.a().b()`; the
  `.b()` chains off the result of `.a()`.
- **Function form.** `f(obj, args)` calls the method as a function, with the
  receiver as its first argument, when no function of that name exists. The
  receiver is evaluated once and its type picks the method, exactly as for
  `obj.f(args)`; a receiver of a type with no such method is `E0237`. This
  also works on the right of a tuple destructure (`[a, b] = f(obj)`).
- **Dispatch** keys on the receiver's concrete type, so the same method name may
  be declared on two different types without collision (`(type, name)` is the
  key). Overloading one name across arities on the same type is not yet
  supported.
- **Errors.** An unknown method on an object receiver is `E0237` (with a
  did-you-mean over that type's methods); a `method` whose receiver names an
  unknown type is `E0237`; a duplicate method for one receiver type is `E0224`.

### Collections of UDTs

An `array<MyType>` and a `map<K, MyType>` may hold **user-defined-type values**,
lifting the scalar-only element restriction through UDT indirection (a
`matrix<MyType>` can too; its narrower test coverage is the last bullet
below):

```
objs = array.new<Point>()
array.push(objs, Point.new(3.0, 4.0))
p = array.get(objs, 0)
p.x := 99.0                 // reference semantics: visible through the array

m = map.new<string, Point>()
map.put(m, "a", Point.new(1.0, 2.0))
```

- **Construction.** `array.new<MyType>(size = 0, initial_value = na)` and
  `map.new<K, MyType>()` — the element/value type names a collected UDT. An
  unknown type name in `array.new<T>()` is `E0234`
  ([`array_of_unknown_udt_is_e0234`](../crates/pinewasm_semantics/src/tests.rs)).
  An enum name is accepted in `array.new<MyEnum>()`
  ([`array_new_accepts_enum_element_type`](../crates/pinewasm_semantics/src/tests.rs))
  and in `map.new<K, MyEnum>()`
  ([`map_new_accepts_enum_value_type`](../crates/pinewasm_semantics/src/tests.rs)).
  An enum element is the member's index on the `int` lane at run time, while
  its static type stays the enum, so an element of another type is `E0234`
  ([`enum_collection_element_of_another_type_is_e0234`](../crates/pinewasm_semantics/src/tests.rs)).
- **Reference semantics.** Elements are object references; a field mutation of an
  element retrieved with `array.get` / `map.get` is visible through the
  collection.
- **Matrices.** `matrix.new<MyType>(...)` builds a `matrix<MyType>` (see
  [Collections (matrices)](#collections-matrices) for its arguments). For
  user-defined cells, the value tests that run from source cover construction,
  `matrix.get`/`matrix.set`, `matrix.rows`/`matrix.columns`, and sorting by a
  field
  ([`matrix_new_udt_and_sort_field_overloads`](../crates/pinewasm_vm/src/tests.rs));
  no claim is made for the other matrix operations over user-defined cells.

Methods and the array and map forms above are verified end-to-end by
[`golden_udt_methods`](../crates/pinewasm_compiler/tests/pipeline.rs),
[`golden_collections_of_udts`](../crates/pinewasm_compiler/tests/pipeline.rs),
the [user-defined type semantics tests](../crates/pinewasm_semantics/src/tests.rs), and the
[`udt_methods.pine`](../tests/compile-pass/udt_methods.pine) fixture.

## Tuples

A **destructuring declaration** `[a, b, c] = expr` declares two or more
variables at once, binding each to one element of a tuple the RHS produces.
Tuples are a **compile-time concept only** — there is no tuple runtime value and
no tuple type. Supported tuple producers include:

- a **tuple-returning `ta.*` call** (`ta.macd`, `ta.bb`, `ta.kc`, `ta.dmi`,
  `ta.supertrend`, or the three-argument `ta.vwap` band overload — see
  [the `ta` namespace](#tuple-returning-ta-functions)), or
- a **user function whose body's last statement is a tuple expression**
  (`... => [hi, lo]`), including an `if`/`switch` value tail;
- an **`if` or `switch` value structure** whose local blocks return tuples;
- a supported float/boolean tuple from **`request.security`** (see `runtime.md`).

Rules:

- **Declaration only.** A destructuring pattern always *declares* its targets
  with `=` (never `:=`); each target follows the ordinary declaration rules
  (`E0211` on a redeclaration in the same scope, `W0006` when it shadows a
  built-in variable). The
  targets get slots like any variable, so history on a destructured target
  works: `[m, u, l] = ta.bb(close, 20, 2.0)` then `u[1]` reads the previous
  bar's upper band.
- **Arity must match.** The number of targets must equal the tuple's arity;
  `[a] = ta.bb(...)` (one target, a 3-tuple), `[a, b, c, d] = ta.bb(...)`, and a
  scalar RHS (`[a, b] = close`, a 1-tuple) are all **`E0225`** with the expected
  and found counts. Each element of the tuple `ta.*` functions is
  `series float`.
- **Tuple expressions are position-restricted.** A tuple `[e0, e1, ...]` is
  legal as a function or conditional local block's final value, or inside a
  supported `request.security` expression. A direct literal declaration
  `[a, b] = [1, 2]` is rejected; use a tuple-returning function or structure.
  Anywhere else — a bare `plot([a, b])`, a nested operand `[a, b] + 1`, a
  scalar declaration `x = [a, b]` — is **`E0225`**. A tuple-returning `ta.*`
  call used as a scalar value, and a tuple-bodied user function used as a scalar
  value, are likewise `E0225`: they must be destructured.
- **Lowering (no runtime tuple).** A tuple `ta.*` call lowers to **one**
  multi-result `CALL_TA` that pushes its N results left to right, followed by
  one store per target (see [`ir.md`](ir.md)); a user tuple function's result is
  N independent element expressions, each stored into its target's slot as an
  ordinary declaration. Either way, each syntactic `ta.*` call site still owns
  its own per-call-site state, so two sites of the same tuple function — or two
  call sites of a tuple-returning user function — keep **independent** windows
  and recursions (per-call-site instantiation, exactly like scalar `ta.*`).
- **Conditional returns.** Corresponding components unify independently
  (int/float widens to float); incompatible types and different tuple sizes
  are rejected. All components share the joined qualifier. One selected block
  fills component slots, so conditions and branch effects run once. A missing
  branch supplies `na` for each non-bool component and `false` for each bool
  component. Nested conditional function tails and direct `if`/`switch`
  destructuring are covered by the `conditional_tuple_*` semantic and VM tests.

## Built-ins

### Series

The OHLCV series and the derived averages have type `float`; `bar_index`
and the time/calendar series have type `int`. All carry
qualifier `series` and cannot be assigned to with `:=`; a declaration may
shadow one (see Declarations).

| Name        | Description                                        |
|-------------|----------------------------------------------------|
| `open`      | Bar open price (`float`)                           |
| `high`      | Bar high price (`float`)                           |
| `low`       | Bar low price (`float`)                            |
| `close`     | Bar close price (`float`)                          |
| `volume`    | Bar volume (`float`)                               |
| `bar_index` | Zero-based index of the current bar (`int`)        |
| `hl2`       | `(high + low) / 2` (`float`)                        |
| `hlc3`      | `(high + low + close) / 3` (`float`)               |
| `ohlc4`     | `(open + high + low + close) / 4` (`float`)        |
| `hlcc4`     | `(high + low + close + close) / 4` (`float`)       |
| `time`      | Bar open time, epoch ms (`int`)                    |
| `time_close`| Bar close time, epoch ms (`int`)                   |
| `timenow`   | Deterministic: last bar's close time (`int`)       |
| `last_bar_index` | `bar_index` of the run's last bar (`int`)     |
| `last_bar_time`  | `time` of the run's last bar (`int`)          |
| `year` `month` `dayofmonth` | Calendar fields of `time` (`int`)  |
| `dayofweek` | Day of week, 1 = Sunday … 7 = Saturday (`int`)     |
| `hour` `minute` `second` | Intraday fields of `time` (`int`)    |

The time axis comes from `MarketData.time` (bar open times, epoch ms) or,
when absent, a synthesized daily sequence from 2020-01-01Z; the calendar
series decompose `time` in the exchange timezone (`syminfo.timezone`, a
fixed offset or an IANA name whose daylight-saving changes are followed;
an unknown name is refused). `timenow` is a deterministic
last-bar-close value rather than a wall-clock value (see `docs/runtime.md`).
The `dayofweek.*` constants (`dayofweek.sunday = 1`, …, `.saturday = 7`)
are `const int`.

The **function** forms `time(timeframe[, session][, timezone])` and
`time_close(timeframe[, session][, timezone])` (`series int`) return the
enclosing `timeframe` bucket's OPEN (`time`) or CLOSE (`time_close`) epoch-ms
time — distinct from the bare `time`/`time_close` *variables*, which read the
chart bar's own open/close time. They are modeled with the same higher-timeframe
RESAMPLE machinery as `request.security` (the `READ_TIME_TF` opcode): the chart
feed is bucketed to `timeframe` and each chart bar reports its own enclosing
bucket's open time, or that bucket's close for `time_close`. So over a daily feed
`time("1D")` equals the bar's own open, while `time("1W")` is a step function
that repeats each weekly bucket's open across its seven days. The `timeframe`,
`session`, and `timezone` arguments are runtime strings; invalid runtime strings
return `na`. `bars_back` offsets the chart bar before rebucketing and
`timeframe_bars_back` offsets the selected higher-timeframe bucket. Session
strings are evaluated in the requested timezone (fixed offset or IANA; an
unknown name stops the run) and `time_close`
clips to the active session boundary when needed (see `docs/runtime.md`). The
session-type constants `session.regular` and `session.extended` are not hours:
passed as `session`, they stand for the symbol's own session, as if the argument
were omitted.

**Context accessors: `timeframe.*` / `syminfo.*` / `session.*`.** More
builtin-series reads (no new opcode), all **per-run constants** except the
`session.is*bar*` predicates (which track the first/last bar of the run).
`BuiltinSeries` grew an `is_string()` partition beside `is_int()`/`is_bool()`.

- `timeframe.period`/`.main_period` (`string`), `.multiplier` (`int`), and the
  unit predicates `.isdaily`/`.isweekly`/`.ismonthly`/`.isdwm`/`.isintraday`/
  `.isminutes`/`.isseconds`/`.isticks` (`bool`) all derive from the chart
  timeframe (`MarketData.timeframe`, default `"1D"`). The functions
  `timeframe.in_seconds(tf)` (`int`; e.g. `"1W"` → 604800) and
  `timeframe.from_seconds(seconds)` (`string`) are pure builtins.
- `syminfo.basecurrency`/`.currency`/`.description`/`.country`/`.industry`/
  `.sector`/`.type`/`.root`/`.prefix`/`.session`/`.timezone`/`.volumetype`/
  `.main_tickerid` (`string`) and `.mintick`/`.pointvalue`/`.mincontract`
  (`float`), `.minmove`/`.pricescale` (`int`) are host-or-default reads off the
  `SymInfo` table (see `docs/browser.md`).
- `session.isfirstbar`/`.isfirstbar_regular`/`.islastbar`/`.islastbar_regular`/
  `.ismarket`/`.ispremarket`/`.ispostmarket` (`series bool`) model one
  continuous regular session, plus `time_tradingday` (`int`). The
  `session.regular`/`session.extended` `const string` tokens complete the
  namespace. `timeframe.change(tf)` (`series bool`) is `true` on the first
  chart bar of each `tf` bucket: exactly when `time(tf)` opens a new one, so
  calendar months, quarters and years start on the first trading day, weeks
  start on Monday, and fixed widths follow the exchange clock, as
  `request.security` resamples. A day opens at the exchange's midnight, not
  at a session open the evening before. On
  a timeframe no longer than the chart's, every bar is a change, so a host
  must set `MarketData.timeframe`. `tf` is a `series string`; a literal, `const`,
  input or computed one all lower onto `time(tf)`. With no argument,
  `timeframe.in_seconds()` measures the chart timeframe.

**Drawing-style constants.** The drawing namespaces contribute a set
of enum-like *style* constants — `xloc.*`, `yloc.*`, `extend.*`,
`line.style_*`, `hline.style_*`, `label.style_*`, `size.*`, `text.align_*`,
and `text.wrap_*`. Each is modelled as an opaque **`const string`** token
whose value is the member name itself (the same mechanism as the `format.*`
display constants), so distinct members hold distinct values and round-trip
through the checker, CSIL, and VM (`line.style_solid ≠ line.style_dashed`).
These constants are consumed by the drawing-object model below.

**Drawing objects.** The stateful drawing namespaces — `chart.point`,
`line`, `label`, `box`, `table`, `polyline`, and `linefill` — are **heap
reference objects** (the same object model as UDTs). The member surfaces are
namespace-specific: line/label/box provide construction, field getters/setters,
copy, and delete; tables provide cell creation/mutation, clear/merge, and
delete; polylines provide construction/delete; and linefills provide
construction, line/color access, color mutation, and delete. `chart.point`
offers `from_index(index, price)`, `from_time(time, price)`, and `now(price)`
coordinate constructors. Exact signatures and fields are tracked by the
coverage matrix. Live drawing objects are tracked in per-kind registries and
serialized into `RunResult.drawings` for hosts
to render (see [browser.md](browser.md)); `chart.point` is a coordinate object
and is not registered/serialized. The indicator `max_lines_count`,
`max_labels_count`, `max_boxes_count`, and `max_polylines_count` parameters set
the corresponding oldest-first live-object caps; table and linefill lifetime
details are recorded in the matrix. Conditional creation simply does not create
on skipped bars, and realtime rollback undoes a rolled-back tick's drawing
creations/mutations/deletions.

- **Errors (`E0238`).** A call to an unknown drawing member, a getter/setter for
  an unknown field, or a wrong-arity or wrong-receiver-type drawing call, is
  `E0238`. Getters, setters, `delete`, `line.get_price`, `table.clear`, and
  `table.merge_cells` bind named arguments by their parameter names
  (`table_id` on table members, `id` elsewhere); an unknown or repeated name is
  `E0228`, and a named `table.clear` call that leaves `start_column` or
  `start_row` unbound is `E0204`. A getter/setter/delete
  on an `na` object is a structured **runtime** error (never a panic).

See `tests/compile-pass/drawings.pine`.

**Compound and point-taking setters.** Beyond the single-field
`set_<field>`, the drawing namespaces offer setters that touch several fields at
once, always evaluating the receiver **exactly once** (a hidden slot captures the
handle, so a `<ns>.new(...)` receiver is never re-registered):

- **Compound coordinate setters** — `line.set_xy1(id, x, y)`/`set_xy2`,
  `label.set_xy(id, x, y)`, `box.set_lefttop(id, left, top)`/`set_rightbottom`
  write two adjacent coordinate fields; `box.set_xloc(id, left, right, xloc)`
  writes the two x-bounds plus the `xloc.*` token.
- **`chart.point`-taking setters** — `line.set_first_point(id, point)`/
  `set_second_point`, `label.set_point(id, point)`, `box.set_top_left_point`/
  `set_bottom_right_point` read the point's `index`→x-field and `price`→y-field.
- **`line.get_price(id, x)`** returns the interpolated price on the line at bar
  `x`: `y1 + (x - x1) * (y2 - y1) / (x2 - x1)` (extrapolating outside the
  segment).
- **Box text/extent & label metadata** — the `box` schema now carries
  `extend`/`xloc`/`text`/`text_color`/`text_size`/`text_halign`/`text_valign`/
  `text_font_family`/`text_wrap`/`text_formatting`, and `label` carries
  `tooltip`/`text_font_family`/`text_formatting`; their single-field setters (`box.set_text`,
  `label.set_tooltip`, …) surface in the corresponding `drawings.*` DTO.

An `na` receiver (or, for the point setters, an `na` point) is a structured
runtime error, never a panic.

**Additional drawing namespaces.** Three further stateful namespaces
reuse the same heap-reference-object model, live registry, GC, and
`RunResult.drawings` serialization as `line`/`label`/`box`:

- **`table`** — `table.new(position, columns, rows, bgcolor?, frame_color?,
  frame_width?, border_color?, border_width?)` constructs a grid anchored to a
  chart `position.*`; `table.set_<field>(...)` mutates a table-level attribute
  (`position`/`bgcolor`/`frame_color`/`frame_width`/`border_color`/
  `border_width`); `table.delete(id)` removes it. Serialized under
  `drawings.tables`.
  - **Replacement receivers.** Creating a new table at an occupied position
    displaces the earlier table and makes its aliases `na`-observable. The VM
    retains a runtime-only distinction: a full `table.cell` call on that
    displaced receiver completes without modifying the replacement table.
    Arguments still evaluate normally. A never-initialized `table t = na`
    remains an error receiver, as do displaced receivers for the existing
    single-property and table-level setters. The literal-`na` diagnostic
    remains `NaFieldAccess`; zero-column tables raise PineWASM's own bounds
    error.
    [Direct-source tests](../crates/pinewasm_compiler/tests/table_displaced_cell.rs)
    cover the narrow full-cell call and marker; the
    [VM lifetime tests](../crates/pinewasm_vm/src/tests/table_displacement.rs)
    cover copied aliases, ordinary unavailable-value cases and rollback.
    Displaced table handles keep their identity inside collections. The
    [collection identity test](../crates/pinewasm_compiler/tests/retired_table_identity.rs)
    checks that two displaced aliases remain separately
    findable by `array.indexof`, that `na()` is true for them, and that
    a literal `na` query does not match them. Local tests extend identity
    propagation through aliases, maps, matrices, fields, copies and helper
    returns, and check `array.includes`/`lastindexof`. This is a bounded
    identity test; other collection and receiver cases are not covered. The
    standalone table-position-replacement refusal is unchanged.
  - **Per-cell grid.** A table also has a 2D cell grid, stored
    out-of-line keyed by the table handle and bounded by its declared
    `columns × rows`. `table.cell(table, column, row, text?, width?, height?,
    text_color?, text_halign?, text_valign?, text_size?, bgcolor?, tooltip?,
    text_font_family?, text_formatting?)` defines a cell, writing supplied
    attributes and resetting omitted attributes to defaults
    ([`cell_replacement_resets_all_omitted_attributes`](../crates/pinewasm_compiler/tests/v6_drawing_default_contracts.rs));
    each `table.cell_set_<attr>(table, column, row,
    value)` mutates one attribute of an existing cell; `table.clear(table,
    start_column, start_row, end_column?, end_row?)` removes an inclusive cell
    range (end defaults to start); and `table.merge_cells(table, start_column,
    start_row, end_column, end_row)` marks a rectangular span merged. An
    out-of-range `(column, row)` or an ordinary `na` table receiver is a
    structured runtime error — never a panic; the displaced-receiver exception
    above applies only to the full `table.cell` call. The populated cells serialize under
    `drawings.tables[i].cells` as `{ column, row, merged?, attrs }` entries in
    `(column, row)` order.
- **`polyline`** — `polyline.new(points, curved?, closed?, xloc?, line_color?,
  fill_color?, line_style?, line_width?)` where `points` is an
  `array<chart.point>` (constructed with `array.new<chart.point>()` and
  `array.push`). Getters/setters cover every field; serialized under
  `drawings.polylines` (the `points` field carries the array handle).
- **`linefill`** — `linefill.new(line1, line2, color)` shades between two
  existing `line` objects; `linefill.get_line1`/`get_line2`/`get_color` and
  `linefill.set_color` derive from the `line1`/`line2`/`color` fields.
  Serialized under `drawings.linefills`.

These namespaces surface through the `RunResult.drawings` DTO for an embedding
host to consume. See `tests/compile-pass/drawings.pine`.

**Drawing-typed variables.** The six registered drawing type names plus
`chart.point` (`line`, `label`, `box`, `table`, `linefill`, `polyline`,
`chart.point`) are usable as explicit variable-declaration **type annotations**,
alongside the scalar `int`/`float`/`bool`/`string` keywords:

```
line l = na              // na-typed declaration: l is a line
l := line.new(0, o, 1, c)  // matching constructor — type-checks
box b = box.new(1, 10.0, 4, 40.0)  // typed decl from the constructor
var label lb = na        // combinable with var / varip
```

A drawing annotation resolves to the object type `Type::Object(<kind>)` (the same
type `<kind>.new(...)` produces), so a bare `na` initializer adopts it (no
`E0220`) and a later `:=` from the matching constructor type-checks; a wrong-type
initializer is `E0226` and a wrong-type reassignment is `E0218`. Only the seven
known drawing type names are recognized in the annotation position.

**Named drawing-array constructors.** `array.new_line()`, `array.new_label()`,
`array.new_box()`, `array.new_table()`, and `array.new_linefill()` are thin
aliases of the generic `array.new<line>()` etc. — each produces a fresh empty
`array<drawing>`, into which the matching `<kind>.new(...)` objects can be pushed
(reference semantics preserved through the array).

Non-finite input values (NaN/±Inf in the market data) read as `na`.

### Functions

The registry contains output/effect functions, stateful builtins (the
slot-based `ta.change`/`ta.sma` pair and the engine-backed batch), collection
and drawing operations, request interfaces, and pure builtins. Calls declared
`void` cannot be used as values (`E0212`); calls such as `plot` return typed
handles specifically so later global-scope calls such as `fill` can refer to
them. Unknown functions (e.g. `sma(...)`)
are `E0202`; calling a non-function value (including a namespaced
constant, `math.pi(2)`) is `E0203`; naming a function
without calling it is `E0216`.

```
indicator(title: const string, ...) → declaration
```

Top-level script metadata declaration. The title must be a string literal
(`E0205`); the supported optional parameters are accepted and type-checked,
with runtime/display effects tracked individually in the coverage matrix. At
most one script-kind declaration is allowed (`E0213`/`E0240`), and it must be
top level (`E0214`). A script without `indicator(...)`, `strategy(...)`, or
`library(...)` compiles with warning `W0001`.

```
plot(series: series numeric, title?: const string, ...) → plot
```

Records one output sample per bar into a plot slot and returns a compile-time
`plot` handle usable by `fill`. `series` accepts numeric values up to the
`series` qualifier; an `int` is widened to float. The full supported style,
color, display, offset, and format parameters are preserved as host metadata;
the matrix is authoritative for each parameter's rendering effect. `title`,
when given, must be a string literal (parentheses allowed); a const-string
variable is rejected. Plot-family calls are global-scope-only and produce
`E0254` in a local block. Plot slots are numbered in declaration order.

### The `ta` namespace — stateful built-in functions

The registry is data-driven: a `ta.*` function is a table entry (name,
signature, per-parameter rules), not new checker or VM code paths beyond
the shared machinery below. Two lowering strategies coexist (see
[`ir.md`](ir.md)): the original **slot-based** pair below, and the
**engine-backed** batch in the
[next subsection](#engine-backed-stateful-built-ins--fixnan-mathsum-and-the-ta-batch).
The slot-based functions are:

```
ta.change(source: series numeric, length = 1) → series float
```

`ta.change(source, length)` returns `source` minus `source[length]`
(`ta.change(close)` equals `close - close[1]`). The result is `na` when
either side is `na` — in particular during **warm-up**: the first `length`
bars are `na`.

```
ta.sma(source: series numeric, length) → series float
```

The arithmetic mean of the last `length` non-`na` source observations at
this call site. Missing sources add no observation; before enough present
observations exist the result is `na`, and a missing source after warm-up
retains the established mean. Dynamic lengths select from the retained
present-observation history, so growth can use observations from before a
long missing run. These finite cases are covered by the
[source-observation regressions](../crates/pinewasm_compiler/tests/sma_missing_observations.rs).
`length` is required. Repeated execution on the same bar replaces that bar's
current observation, but source-level stateful calls inside loops remain
`E0222`; the repeated-call storage test uses validated shared-slot bytecode.

Shared rules:

- **Source.** `source` accepts any numeric series; an `int` source is
  coerced to `float` with an explicit `IntToFloat` node, so ta state always
  holds floats and results are always `series float`.
- **Length.** `length` must resolve to an integer in `1..=100000`.
  Literal and input-backed lengths are selected for each run; host overrides
  resize the next run without recompilation, and aliases/helper parameters
  preserve that dependency. `ta.sma` also accepts runtime integer expressions
  through `WINDOW_MEAN_DYNAMIC`; invalid runtime values produce
  `InvalidTaLength`. Dynamic SMA history retains up to the maximum legal
  window length, while a fixed window retains its selected capacity.
  Native WebAssembly supports the literal/direct-input route; dynamic-length
  routes use the hosted VM. `ta.change` retains its fixed-length restrictions
  and defaults to length 1. Wrong argument counts are `E0204`, and
  non-numeric sources are `E0205`. See compatibility note 14.
- **One hidden slot per call site.** Each syntactic `ta.*` call site owns
  one hidden, non-persistent series slot (`%h0`, `%h1`, ... — the same
  machinery as materialized history): when the site executes, it stores the
  current `source` value into the slot's current-bar cell. `ta.change` reads
  that bar history; `ta.sma` separately tracks the slot's non-`na`
  observations. Two identical call sites keep independent state.
- **Not-every-bar execution warns (`W0004`).** A `ta.*` call inside an `if`
  branch should be executed on every bar for consistent results. A skipped
  call adds no SMA observation; bar-history operations such as `ta.change`
  still encounter a missing history cell. Conditional SMA source semantics
  have local regression coverage; other control paths are not tested; see
  [`ta_sma_in_a_branch_warns_and_gap_bars_produce_na_windows`](../crates/pinewasm_vm/src/tests.rs).
  A `ta.*` call inside a **`var` initializer** also warns `W0004`, with
  run-once wording: the call executes when the variable initializes, and
  the variable retains that computed result unless reassigned. Whether the
  initial result is `na` depends on the call's inputs and warmup
  ([`ta_call_in_var_initializer_warns_w0004`](../crates/pinewasm_semantics/src/tests.rs)).

### Engine-backed stateful built-ins — `fixnan`, `math.sum`, and the `ta.*` batch

Every other stateful builtin is backed by the **VM-native ta engine**: each
syntactic call site owns a `TaStateId` into the program's `ta_states` table
and lowers to one `CALL_TA` instruction; the VM keeps the per-site state
(ring buffers for windows, scalars for recursive functions) and advances it
**only when the site executes** (see [`runtime.md`](runtime.md) and
[`ir.md`](ir.md)). Shared rules:

Signatures and formulas (nuances not pinned by tests are marked *assumed*):

| Function | Result | Semantics |
|---|---|---|
| `fixnan(source)` | float | `source` with `na` replaced by the last non-`na` value seen at this call site; `na` until one exists. (PineWASM's engine sources are float; the coverage matrix records this.) |
| `math.sum(source, length)` | float | Sliding sum of the last `length` values. Window warm-up/poison rules (*assumed* for the na case). |
| `ta.ema(source, length)` | float | Exponential MA, `alpha = 2 / (length + 1)`; seeds with the sma of the first `length` non-na values (*assumed* seeding). |
| `ta.rma(source, length)` | float | Wilder's smoothing, `alpha = 1 / length`; same seeding. |
| `ta.wma(source, length)` | float | Linearly weighted MA: weights 1 (oldest) .. `length` (newest), denominator `length·(length+1)/2`. |
| `ta.vwma(source, length)` | float | `sma(source · volume, length) / sma(volume, length)`, with independent non-`na` observation histories for numerator and denominator. A missing source skips its weighted observation but does not mask an otherwise present volume from the denominator; zero denominator → `na`. Integer lengths may vary by bar or come through user-defined fields; the dynamic form uses the hosted VM. The [independent-history tests](../crates/pinewasm_ta/tests/vwma_independent.rs) cover this rule. |
| `ta.hma(source, length)` | float | Hull MA: `wma(2·wma(source, length/2) − wma(source, length), round(sqrt(length)))` with int-truncating `length/2` and half-away-from-zero rounding (*assumed* composition details). `length` ≥ 2. |
| `ta.swma(source)` | float | Symmetric 4-bar weighted MA: `x[3]/6 + 2·x[2]/6 + 2·x[1]/6 + x[0]/6`. |
| `ta.alma(series, length, offset, sigma, floor = false)` | float | Arnaud Legoux MA: `m = offset·(length−1)` when `floor` is false, or `m = floor(offset·(length−1))` when true; `s = length/sigma`; gaussian weights `exp(−(i−m)²/(2s²))` over the window (i = 0 oldest), normalized. Both forms are value-tested. |
| `ta.rsi(source, length)` | float | `100·up/(up+down)` where up/down are `rma`s of the positive/negative parts of the bar-to-bar change. Bounded [0, 100]; all-flat windows (0/0) are `na` (*assumed*). Warm-up: `length` changes, i.e. `length + 1` source values. |
| `ta.atr(length)` | float | `rma` of the true range (`ta.tr(true)` form). No source argument; reads high/low/close directly. |
| `ta.tr(handle_na)` | float | `max(high − low, abs(high − close[1]), abs(low − close[1]))`. Without a previous close (bar 0 or a data hole): `high − low` when `handle_na` is true, `na` when false. |
| `ta.stdev(source, length, biased = true)` | float | Standard deviation from the window's mean. `biased = true` (the default) divides by `length` (population); `false` divides by `length − 1` (sample; `na` for `length` 1). |
| `ta.dev(source, length)` | float | Mean **absolute** deviation from the window's sma. |
| `ta.variance(source, length, biased = true)` | float | As `ta.stdev` without the square root. |
| `ta.highest(source = high, length)` / `ta.lowest(source = low, length)` | float | Highest/lowest source value in the window. The one-argument and named-`length` forms read the default high/low series. |
| `ta.highestbars(source = high, length)` / `ta.lowestbars(source = low, length)` | **int** | Offset (≤ 0) of the highest/lowest bar in the window: 0 = current bar, −1 = one bar back, ... The one-argument and named-`length` forms read the default high/low series. Ties resolve to the most recent bar (*assumed*). |
| `ta.mom(source, length)` | float | `source − source[length]` (endpoints only; middle `na`s do not poison). |
| `ta.roc(source, length)` | float | `100 · (source − source[length]) / source[length]`; a zero denominator is `na`. |
| `ta.cum(source)` | float | Running total of every non-na `source` value the site has seen; `na` input yields `na` without touching the total (*assumed*). |
| `ta.crossover(a, b)` / `ta.crossunder(a, b)` | **bool** | True when `a > b` on this bar and `a <= b` on the previous one (crossunder: mirrored). Plain false — never `na` — on the first bar or when any of the four values is `na` (*assumed*). |
| `ta.cross(a, b)` | **bool** | `crossover or crossunder`. |
| `ta.barssince(condition)` | **int** | Bars since the condition was last true (0 on a true bar); `na` until it has ever been true. |
| `ta.valuewhen(condition, source, occurrence)` | float, int, bool, or color, matching `source` | The `source` value captured on the `occurrence`-th most recent bar where `condition` was true (0 = most recent); `na` until enough occurrences exist. A captured `na` source stays `na`. The int path uses an f64 state cell, so integer values above 2^53 are not exact. |
| `ta.linreg(source, length, offset)` | float | Least-squares line over the window (x = 0 oldest .. length−1 newest), evaluated at `x = length − 1 − offset`. `length` 1 has an undefined slope → `na` (*assumed*). |
| `ta.rising(source, length)` / `ta.falling(source, length)` | **bool** | True when the current value is greater/less than **every** one of the previous `length` values (compared with every value in the window, not bar-to-bar monotonicity). Plain false — never `na` — during warm-up or when the window holds an `na` (*assumed*). |
| `ta.cci(source, length)` | float | `(source − sma) / (0.015 · dev)` with `dev` the mean absolute deviation; a flat window (`dev` 0) is `na`. |
| `ta.cmo(source, length)` | float | `100 · (Σup − Σdown) / (Σup + Σdown)` over the last `length` bar-to-bar changes (warm-up: `length + 1` values); an all-flat window (0/0) is `na`. |
| `ta.cog(source, length)` | float | Center of gravity: `−Σ source[i]·(i+1) / Σ source[i]` with i = 0 the newest bar; a zero window sum is `na`. |
| `ta.correlation(source1, source2, length)` | float | Pearson correlation coefficient over the window (two runtime sources; an `na` on either side poisons the cell). Zero variance on either side is `na`. |
| `ta.median(source, length)` | float | Window median; even lengths average the two middle values (*assumed*). |
| `ta.mode(source, length)` | float | The most frequent window value; frequency ties — including all-distinct windows — resolve to the smallest value. |
| `ta.percentile_linear_interpolation(source, length, percentage)` | float | Linear interpolation between the two nearest ranks over positions `0 ..= length − 1` (the inclusive method — *assumed*); the result need not be a window member. `percentage` is a number literal or unchanged float input in 0..=100 (`E0219`). |
| `ta.percentile_nearest_rank(source, length, percentage)` | float | Nearest rank: the smallest window value with at least `percentage`% of the data at or below it — always a window member; percentage 0 selects the smallest value (*assumed*). |
| `ta.percentrank(source, length)` | float | `100 · count(previous length values ≤ current) / length` (warm-up: `length + 1` values). |
| `ta.range(source, length)` | float | Window max minus window min. |
| `ta.mfi(source, length)` | float | `100 − 100/(1 + Σup/Σdown)` over per-bar flows `volume · source` gated by the sign of the source's change; an **na** change (the first bar, or right after an `na` source) contributes to **both** flows (a `na` change takes the else branch). Reads volume. A zero down-flow pins to 100; 0/0 is `na`. |
| `ta.wpr(length)` | float | Williams %R: `100 · (close − highest(high)) / (highest(high) − lowest(low))` over the window, in [−100, 0]. Reads high/low/close; no source argument. A flat window is `na`. |
| `ta.stoch(source, high, low, length)` | float | `100 · (source − lowest(low)) / (highest(high) − lowest(low))` — scalar, three runtime sources; a flat window is `na`. |
| `ta.sar(start, inc, max)` | float | Wilder's parabolic SAR: `na` on bar 0, trend seeded from the first close-to-close move, acceleration `start` growing by `inc` (capped at `max`) on new extremes, result clamped to the prior two bars' lows/highs, and reversal to the extreme point when price crosses the SAR. Reads high/low/close. `start`/`inc`/`max` accept number literals or unchanged float inputs and must be **positive** (*assumed*). A data hole is `na` and freezes the recursion (*assumed*). |
| `ta.tsi(source, short_length, long_length)` | float | True strength index in **[−1, 1]** (not scaled by 100): double ema smoothing — long then short — of the bar-to-bar change, divided by the same smoothing of its absolute value; 0/0 (an all-flat history) is `na`. |
| `ta.rci(source, length)` | float | Rank correlation index in [−100, 100]: Spearman's rank correlation between the source and the bar index over the window; tied prices share average ranks (*assumed*). A flat window (zero rank variance) is `na`. |
| `ta.bbw(source, length, mult)` | float | Bollinger Bands width in percent units: `((basis + dev) − (basis − dev)) / basis · 100` with `basis = sma(source, length)`, `dev = mult · stdev(source, length)` (population). A zero basis is `na`. The finite [BBW unit test](../crates/pinewasm_compiler/tests/requested_float_comparison_policy.rs) keeps the factor 100; KCW uses ratio units instead. |
| `ta.kcw(source, length, mult, useTrueRange = true)` | float | Keltner Channels width as a ratio: `2·mult·r / basis`, with `basis = ema(source, length)` and `r = ema(span, length)`, where `span` is the true range (the `handle_na = false` form — `na` on bar 0) or `high − low` when `useTrueRange` is `false`. A zero basis is `na`; the result is not multiplied by 100. Existing smoothing and warm-up policies are unchanged. |
| `ta.pivothigh(source = high, leftbars, rightbars)` / `ta.pivotlow(source = low, leftbars, rightbars)` | float | The pivot's source value, reported on the **confirmation bar** (`rightbars` bars after the candidate); `na` on every other bar. A candidate is a pivot when it is **strictly** higher (pivothigh) / lower (pivotlow) than every value `leftbars` back and `rightbars` forward — ties are not pivots (*assumed*) — and an `na` anywhere in the window is `na` (*assumed*). |

#### Variable-style stateful built-ins — `ta.obv`, `ta.vwap`, and friends

Nine `ta.*` built-in **variables** are implemented as engine
functions with **empty signatures**: they pop no operands (they read
market data directly, like `ta.atr`) and are referenced as **bare names**
— `plot(ta.obv)`, no parentheses. Each mention is its own call site with
its own state (so the `W0004` conditional-execution warning applies as
usual), history works through the same hidden-slot materialization as
compound expressions (`ta.obv[1]`), and passing arguments is `E0204`.
Formulas:

| Name | Semantics |
|---|---|
| `ta.obv` | `cum(sign(change(close)) · volume)`. The bar-0 change is `na`, so bar 0 is `na` (the `ta.cum` rule: `na` terms skip the total). |
| `ta.accdist` | `cum(clv · volume)` with `clv = (2·close − low − high)/(high − low)`; a flat bar (`high == low`) contributes 0 (*assumed*). Starts at the first bar's term, so bar 0 is a value, not `na`. |
| `ta.nvi` / `ta.pvi` | Negative/positive volume index. Starts at 1.0; on a volume **decrease** (nvi) / **increase** (pvi) the value applies the close's rate of change (`value += (close − close[1])/close[1] · value`); zero or `na` closes carry the value unchanged, and an `na` previous volume compares as 0. Never `na`. |
| `ta.pvt` | `cum(change(close)/close[1] · volume)`; the bar-0 term is `na` (skipped). |
| `ta.wad` | Williams accumulation/distribution: `cum(gain)` with `gain = close − min(low, close[1])` on up bars, `close − max(high, close[1])` on down bars, else 0 — and 0 when the change is `na`, so bar 0 is 0.0, not `na`. |
| `ta.wvad` | `(close − open)/(high − low) · volume`, **per bar** (not cumulative); a flat bar is `na`. |
| `ta.iii` | `(2·close − high − low)/((high − low) · volume)`, per bar; a flat bar or zero volume is `na`. |
| `ta.vwap` | Volume-weighted average price of `hlc3`, cumulative **from bar 0** (the variable form is described in the compatibility notes). Data holes are `na` and leave the sums untouched (*assumed*). The function overloads `ta.vwap(source[, anchor[, stdev_mult]])` are separate and support explicit anchor resets and tuple bands. |

The `ta.tr` **variable** form (equivalent to `ta.tr(false)`) is supported: a
bare `ta.tr` reference desugars to a `ta.tr(false)` call site (the `handle_na =
false` argument is synthesized), so it shares the engine state and semantics of
the explicit function form.

#### Tuple-returning `ta.*` functions

Six engine-backed call forms return a **tuple** of `series float` values,
consumed only by a destructuring declaration (`[a, b, ...] = f(...)` — see
[Tuples](#tuples)). `ta.macd`/`ta.bb`/`ta.kc`/`ta.dmi` and the three-argument
`ta.vwap(source, anchor, stdev_mult)` overload are 3-tuples;
`ta.supertrend` is a 2-tuple.
Each is a normal engine call site (one `CALL_TA`, one `ta_states` entry, its
own per-call-site state); the only difference is that its `CALL_TA` pushes
`engine_result_arity()` values instead of one (see [`ir.md`](ir.md)). The
const arguments follow the same int-literal `1..=100000` window-length rule as
every other engine function (`E0219` otherwise); `mult`/`factor` is a finite
number literal. Nuances not pinned by tests are marked *assumed*; each is verified by an identity
or a hand-computed trace against an already-tested primitive.

| Function | Returns | Semantics |
|---|---|---|
| `ta.macd(source, fast_length, slow_length, signal_length)` | `[macdLine, signalLine, histLine]` | `macdLine = ema(source, fast) − ema(source, slow)`; `signalLine = ema(macdLine, signal)`; `histLine = macdLine − signalLine`. Each line is `na` until its constituent emas warm up (the signal is warmer than the macd line, and the histogram warmer still). Verified: `macdLine == ta.ema(fast) − ta.ema(slow)` and `histLine == macdLine − signalLine` elementwise. |
| `ta.bb(source, length, mult)` | `[middle, upper, lower]` | Bollinger Bands: `dev = mult · stdev(source, length)` (population — the `ta.stdev` default); `upper = middle + dev`, `lower = middle − dev`. The existing engine window produces `na` during warm-up or on an `na` in its window. On the dense-source regression, `middle == ta.sma` and `upper == middle + mult·ta.stdev`; that test does not establish sparse-source equivalence with SMA's present-observation history. |
| `ta.kc(source, length, mult, useTrueRange = true)` | `[middle, upper, lower]` | Keltner Channels: `middle = ema(source, length)`; `range = ema(span, length)` where `span` is the true range (the `handle_na = false` form — `na` on bar 0) or `high − low` when `useTrueRange` is `false`; `upper = middle + range·mult`, `lower = middle − range·mult`. Verified: `middle == ta.ema` and the bands are symmetric about the middle. |
| `ta.dmi(di_length, adx_smoothing)` | `[+DI, −DI, ADX]` | Wilder's directional movement index (reads high/low/close; no source operand). `+DM`/`−DM` and the true range are `rma`-smoothed over `di_length`; `+DI = 100·rma(+DM)/rma(TR)`, `−DI` mirrored; `DX = 100·|+DI − −DI|/(+DI + −DI)`; `ADX = rma(DX, adx_smoothing)`. All three are in `[0, 100]`; `na` during warm-up (*assumed* seeding, shared with `ta.rma`/`ta.atr`). |
| `ta.supertrend(factor, atr_period)` | `[supertrend, direction]` | SuperTrend trailing stop (reads high/low/close; no source operand). `atr = ta.atr(atr_period)`, `src = hl2`; `upperBasic = src + factor·atr`, `lowerBasic = src − factor·atr`; the final bands lock to the prior band unless the basic pierces it or `close[1]` crosses it (prev bands seeded to the current basic on the first valid bar). `direction` is **−1 in an uptrend, +1 in a downtrend** (`+1` on the first valid bar — *assumed*); `supertrend = direction == −1 ? lowerBand : upperBand`. `na` until the ATR seeds. Verified by a hand-computed rally-then-crash trace pinning both directions and both reversals. |

The `ta.vwap(...)` function overloads are implemented: `ta.vwap(source)` returns
the run-cumulative volume-weighted average of `source`, `ta.vwap(source, anchor)`
resets on each `series bool` anchor pulse, and the banded
`ta.vwap(source, anchor, stdev_mult)` returns the tuple `[vwap, upper, lower]`
with `upper/lower = vwap ± stdev_mult·dev` (the volume-weighted population
standard deviation accumulated since the reset). Anchored overloads produce `na`
until the first true anchor starts a segment. The bare `ta.vwap` *variable*
keeps its cumulative `hlc3` form (compatibility note 18).

### Pure built-in functions — `na`, `nz`, and the `math.*` namespace

Pure builtins are stateless: same arguments in, same value out, no hidden
slots, no bar-to-bar memory (so they never warn `W0004`). Each one is a
data-driven registry entry plus a single VM dispatch arm behind the one
`CALL_PURE` opcode (see [`ir.md`](ir.md)). Shared rules:

- **Numeric parameters accept `int` or `float`.** Float-only functions
  coerce int arguments with an explicit `IntToFloat`; the int-preserving
  functions are called out below. Non-numeric arguments are `E0205`.
- **na propagation.** Every function yields `na` when any argument is
  `na` — except `na()` and `nz()`, whose purpose is inspecting/replacing
  `na`.
- **Domain edges are `na`, not errors.** `math.sqrt(-1)`, `math.log(0)`,
  `math.asin(2)`, float overflow, and int-range overflow
  (`math.abs` of the most negative int, `math.floor` of a huge float)
  all produce `na` via the usual non-finite → `na` normalization.
- **Arity.** Wrong argument counts are `E0204`; `math.min`/`math.max`/
  `math.avg` are variadic and accept 2 to 10 arguments.
- **Result qualifier** is the join of the argument qualifiers
  (`math.random` is always `series`: it differs per execution).

Signatures (nuances not pinned by tests are marked *assumed*):

| Function | Result | Notes |
|---|---|---|
| `na(value)` | `bool` | True iff `value` is `na`; accepts any value type; never `na` itself. |
| `nz(source)` | source's type | `na` replaced by the typed zero: `0`, `0.0`, or `false`. `source` must be int, float, or bool (`E0205`). |
| `nz(source, replacement)` | source's type | `na` replaced by `replacement`, which must match the source's type — mixed int/float widens both to float; any other mix is `E0221`. |
| `math.abs(x)` | int → int, float → float | Checked: `math.abs` of the most negative int is `na`. |
| `math.sign(x)` | int → int, float → float | −1, 0, or 1 with the argument's numeric type (*assumed*: the type of the int case is not pinned by tests). |
| `math.sqrt(x)` | float | `x < 0` → `na`. |
| `math.pow(base, exponent)` | float | Overflow → `na`. |
| `math.exp(x)` | float | |
| `math.log(x)` | float | **Natural** (base-e) logarithm; `x ≤ 0` → `na`. |
| `math.log10(x)` | float | `x ≤ 0` → `na`. |
| `math.sin(x)` / `math.cos(x)` / `math.tan(x)` | float | Radians. |
| `math.asin(x)` / `math.acos(x)` | float | Outside [−1, 1] → `na`. |
| `math.atan(x)` | float | |
| `math.floor(x)` / `math.ceil(x)` | int | Identity on int input; a float outside the i64 range → `na`. |
| `math.round(x)` | int | Rounds **half away from zero**; identity on int input; outside the i64 range → `na`. |
| `math.round(x, precision)` | float | Rounded to `precision` decimal digits (half away from zero). `precision` is an int (`E0205` if float). A statically known value outside `[0, 100]` is `E0248`; a runtime-computed value is clamped into that range, so a negative runtime value rounds to 0 digits. |
| `math.min(v0, v1, ...)` / `math.max(v0, v1, ...)` | int when **all** args are int, else float | 2–10 arguments; any `na` argument → `na`. |
| `math.avg(v0, v1, ...)` | float | 2–10 arguments. |
| `math.todegrees(x)` / `math.toradians(x)` | float | |
| `math.random(min = 0, max = 1, seed?)` | float | Uniform in `[min, max)` (*assumed*: the bound's openness is not pinned by tests), drawn from the VM's deterministic per-run PRNG — see [`runtime.md`](runtime.md) for the determinism contract. `seed` must be an int. |
| `math.round_to_mintick(x)` | float | Rounds to the nearest multiple of the market data's `mintick` (default 0.01; see [`runtime.md`](runtime.md) and [`browser.md`](browser.md)). |

`math.sum(source, length)` is **windowed state**, not a pure function; it
is implemented by the ta engine (see
[the engine-backed builtins](#engine-backed-stateful-built-ins--fixnan-mathsum-and-the-ta-batch)),
as is the stateful `fixnan`.

#### Type-cast functions

The scalar type casts are pure builtins over the one `CALL_PURE` opcode
(the keyword casts `int`/`float`/`bool`/`string` parse as call expressions
when immediately followed by `(`; `color` is an ordinary identifier). Verified
by tests:

| Cast | Result | Notes |
|---|---|---|
| `int(x)` | int | Truncates a float **toward zero** (`int(3.7)==3`, `int(-3.7)==-3`); an int passes through; `na → na`. |
| `float(x)` | float | Widens an int (`float(5)==5.0`); `na → na`. |
| `bool(x)` | bool | `false` when `x` is `na`, `false`, or a numeric `0`; `true` otherwise. **Not** na-propagating (a bare `na` operand adopts `bool`). |
| `string(x)` | string | Equals `str.tostring(x)` (reuses that `PureFunc`); `string(42)=="42"`. |
| `color(x)` | color | Identity on a color; `na → na`; a non-color is `E0205`. |

The **drawing casts** `line(x)`/`label(x)`/`box(x)`/`table(x)`/`linefill(x)`
cast `na` or a matching drawing object to that object type. They are
compile-time-checked identities (the object is a heap reference already tagged
with its type, so the cast returns `x` unchanged and emits no runtime op); a
wrong operand type is `E0205`. `polyline`/`chart.point` have no cast
function.

#### `weekofyear`

`weekofyear` exists in both forms. The **variable** is the ISO 8601 week-of-year
(1..=53) of the bar's open time in the exchange timezone — a `BuiltinSeries`
like `month`/`dayofweek`. The **function** `weekofyear(time, timezone?)`
decomposes an arbitrary UNIX-ms `time` (default timezone `syminfo.timezone`) to
its ISO week via a `CALL_PURE`. The ISO week is computed by pure integer date
math (the week belongs to the year containing its Thursday). Both forms yield
`na` for the few dates at the start of the i64 millisecond range (on or before
-292275055-05-16) whose ISO Thursday has no i64 epoch-ms representation.

### Namespaced constants

The `math.*` constants resolve like builtin series but produce compile-time
`const float` values (using one where a series is required works
anywhere a float constant would; `math.pi[1]` is `E0209` — constants
have no history):

| Name | Value |
|---|---|
| `math.pi` | π (the exact IEEE-754 double, 3.141592653589793) |
| `math.e` | e (2.718281828459045) |
| `math.phi` | the golden ratio (1 + √5)/2 (≈ 1.618033988749895) |
| `math.rphi` | the golden-ratio conjugate (√5 − 1)/2 (≈ 0.618033988749895) |

The three `text.format_*` constants have the `const text_format` type,
not `string`. Addition unions the bold and italic flags; `format_none` is the
identity, and union is commutative and idempotent. Label, box, and table-cell
constructors/setters consume these values. Ordinary strings, nullable values,
string conversions, and history are rejected for this domain. `text_format` is
not an explicit type-annotation keyword. See the [focused contract tests](../crates/pinewasm_compiler/tests/v6_text_format_contracts.rs)
and local regression tests.

There are seven `display.*` constants. A display value is a set of
destinations: the pane, the price scale, the status line, the Data Window and
the screener. `display.all` holds all five destinations, and
`display.none` holds none. `display.all` and `display.none` have the
`const plot_simple_display` type. The other five constants have the
`const plot_display` type. Neither type is `string`. A `plot_simple_display`
value is accepted wherever a `plot_display` value is. `+` is set union and `-`
is set difference, so `display.all - display.status_line` is the pane, the
price scale, the Data Window and the screener. The result of `+` or `-` is
always a `plot_display` value. Display arithmetic on constants is folded at compile time. An
operand known only at run time, such as `X` in `X = sel ? display.pane :
display.none` then `X + display.price_scale` or `display.all - X`, is combined
per run with the same union and difference. For the plot-family `display`
arguments (`plot`, `plotshape`, `plotchar`, `plotarrow`, `plotbar`,
`plotcandle`), a variable, ternary, `switch` or reassignment that holds the
result carries one display value. A plot-family `display` argument accepts
every destination, including `display.pine_screener`.

An `input.*` `display` argument is a constant. A display constant held in a
variable is accepted wherever the same expression written inline is, so `K =
display.all - display.status_line` then `input.int(1, display = K)` shows the
input in the Data Window only. Input destinations are the Data Window and the
status line. `display.pane`, `display.price_scale` and `display.pine_screener`
on an input are `E0230`, inline or held. A held alias keeps how it was written,
so `K = display.pane + display.price_scale + display.pine_screener` is `E0230`.
`na` is not a display value (`E0220`); `display.none`
hides an output. The plot-family `display` argument is `input plot_display`, so
a display set that changes per bar, such as `close > open ? display.pane :
display.none`, is `E0206` there, with or without arithmetic. Strings are not
display values: a string literal, an `input.string` or a string variable
passed as a plot-family `display` is `E0205`, a string or string variable
passed as an `input.*` `display` is `E0230`, display values cannot be passed to
string functions or combined with strings, and they cannot be array elements
(`E0234`).

The `display` parameter of `hline`, `bgcolor`, `barcolor` and `fill` has the
`plot_simple_display` type. The compiler enforces
it. These four calls accept only `display.all`, `display.none`, or a value that
keeps the `plot_simple_display` type. A variable, a ternary, a `switch` or a
reassignment keeps that type when every value it can hold is `display.all` or
`display.none`. For example, `D = enabled ? display.all : display.none` then
`hline(70, display = D)` compiles. Any other display value is `E0205` on these
four calls. This includes a single destination such as `display.pane`, any
`+` or `-` result such as `display.all - display.none`, and an alias of such a
value. A `display.pine_screener` value is `E0205` on them too, also when a
ternary or input choice can select it. A reassignment changes the type only for
the reads after it, and the four calls above check the type at the point of
use. See the [two-state display
tests](../crates/pinewasm_compiler/tests/v6_display_destinations.rs).

The VM also guards these four outputs. A program compiled before this check
can still give `hline`, `bgcolor`, `barcolor` or `fill` a display set that the
engine resolves after the run. When that set includes the screener but is
not `display.all`, the VM hides the output: it sets the display to
`display.none` and adds an entry for `display` to `DisplayStyle.unsupported`. The [runtime safeguard
test](../crates/pinewasm_compiler/tests/v6_display_arithmetic_variables.rs)
covers this case.

Display values travel on the string lane as canonical tokens: a `display.*` name for
the seven named sets, and otherwise the member names joined with `+` in the
order pane, price scale, status line, Data Window, screener (for example
`display.pane+display.price_scale`). Hosts never see these joined tokens. A
display binding (`DisplayStyle.bound_scalars`, field `display`) names a set as
one `display.*` name or as a `+` union of member names, including when the engine
resolves a reassigned variable after a run. A set chosen by inputs binds as
ternaries over input values whose choices are such names or unions; display
arithmetic on it is applied under each choice, so a host never evaluates a
set difference or a joined token. When those choices would make a binding
larger than the protocol accepts (more than 64 sets, or more than 256 nodes,
for example `X0 + X1 - X2` with three four-way `switch` inputs), the display is
listed in `runtime_scalars` instead and the engine binds the one set it drew
after the run. See the [focused contract
tests](../crates/pinewasm_compiler/tests/v6_display_arithmetic_variables.rs).

### The `input.*` interface — host overrides and controls

Scripts declare their tunable parameters with `input.*` calls.
Each declaration is an **overridable default**: the value comes from the host
at run time, falling back to the compile-time default. An embedding host may
render controls for numeric, bool, string/options, color, enum, time/price, and
builtin-source inputs from the compiled manifest. Changing one updates
`RunOptions.inputs` and permits a rerun without recompiling. A host may also
supply its own cross-script source lanes; the compiler does not discover or
render those lanes.

Supported core forms are below (arguments must be **compile-time constants**;
a violation is `E0230`). Except where noted, every namespaced form also
accepts the optional `tooltip`, `inline`, `group`, `confirm`, `display`, and
`active` metadata tail. `input.text_area` omits `inline`; `input.source`
places `confirm` after `active`; generic `input()` omits `confirm`:

| Call | Result type / qualifier | Parameters |
|---|---|---|
| `input.int(defval, title?, minval?, maxval?, step?)` or `input.int(defval, title?, options=[...])` | `input int` | numeric const args; `defval` must lie within `minval`/`maxval` and be one of `options` |
| `input.float(defval, title?, minval?, maxval?, step?)` / `... options=[...]` | `input float` | as `input.int`; an int-form `defval` coerces |
| `input.bool(defval, title?)` | `input bool` | — |
| `input.string(defval, title?)` | `input string` | — |
| `input.color(defval, title?)` | `input color` | `defval` is a `#RRGGBB` literal or a `color.*` constant |
| `input.source(defval, title?)` | **`series float`** | `defval` is one of the eight supported price series: `open`/`high`/`low`/`close`/`hl2`/`hlc3`/`ohlc4`/`hlcc4`; derived sources retain their exact OHLC formulas |
| `input.timeframe`/`input.session`/`input.symbol`/`input.text_area(defval, title?)` | `input string` | string const default (format validation is a documented gap) |
| `input.price(defval, title?)` / `input.time(defval, title?)` | `input float` / `input int` (epoch ms) | the `defval` or a host override (chart click-to-place is a host concern) |
| `input.enum(defval, title?)` | **`input <Enum>`** | `defval` is a member of a declared enum (`MyEnum.first`); the read types as the declaring enum, and its runtime value is the member's variant index (a host override selects another member) |
| `input(defval, title?)` (generic) | dispatched | dispatches on the compile-time `defval` type to the matching namespaced kind (int→`input.int`, float→`input.float`, bool→`input.bool`, string→`input.string`, color→`input.color`, a supported price series→`input.source`); an unclassifiable `defval` is `E0230` |

Every declaration records an entry in the program's `inputs` metadata table
(id, kind, title, default, constraints, and supported presentation fields such
as tooltip/inline/group/confirm/display), serialized for hosts. At run time the
host may override any input by **id** through `RunOptions.inputs`; the VM reads
the override-or-default value (see docs/browser.md for the override API).

`input.source` is special: its value is a **series sample**, so it is
`series float`, not `input`-qualified. Builtin selections preserve their exact
`BuiltinSeries` identity and derived-price formula. A host may alternatively
supply a validated cross-script source identity and matching per-bar lane;
missing or malformed external lanes fall back to the declared builtin source.
Every other kind yields an `input`-qualified value.

Every scalar input kind (`input.int/float/bool/string/color/time/price/enum`)
is a lattice producer of the `input` qualifier (only `input.source`, being a
series read, is not). Because `input` is *weaker than* `const`, an input value is
rejected wherever a compile-time constant is required — e.g. a `plot`/
`indicator` title (`E0206`). TA integer parameters additionally accept
`input.int` declarations: a host override determines the window for the next
run. The same compiled program can therefore evaluate a parameter sweep.

The generic `input(defval, ...)` overload is a **semantics-level desugar**: it
dispatches on the compile-time `defval` type to the matching namespaced input and
then reuses the same machinery (so overrides and constraints behave identically).
`input.enum(...)` binds an enum member as the default (see
[Enums](#enums-user-defined)) — the read types as the declaring enum and its
runtime value is the member's variant index. `input.enum`'s `input`-qualified
value participates in the lattice like every other scalar input.

### Collections (arrays)

The `array<T>` collection core is supported via the **namespace call
form** — `array.push(id, value)`. Method syntax (`id.push(value)`) and
`<type>` generics (`array.new<float>()`) are also available as front-end
sugar: method syntax desugars to the namespace form, and a generic with a
scalar element type is rewritten to the typed constructor (one that names
an object type stays on the generic form); see the
"Collection method-call syntax" and "`for … in`" grammar notes above.

**Reference semantics.** An array value is a *handle*, not the
elements. This has three observable consequences:

- **Aliasing.** `b = a` copies the handle, so `a` and `b` refer to the same
  array; a mutation through one is visible through the other
  (`array.push(b, 1.0)` then `array.size(a)` is `1`).
- **Persistence.** A `var` array persists across bars — the handle carries
  forward and the underlying array lives the whole run, so pushing one element
  per bar grows it to the bar count.
- **`na` receiver.** An unassigned collection variable is `na`; calling a
  method on it (`array.size(na_array)`) is a structured runtime error, not a
  panic. `na(arr)` correctly reports whether a handle is `na`.

**Typed constructors:** `array.new_float(size = 0,
initial = na)` and its `array.new_int`/`new_bool`/`new_string`/`new_color`
siblings, plus `array.from(v0, v1, …)` (variadic; element type inferred from
the arguments — mixed int/float widens to float, mixed unrelated types are
`E0234`).

**Operations** (receiver-first): `push`, `pop`, `get`, `set`, `size`, `clear`,
`shift`, `unshift`, `insert`, `remove`, `includes`, `indexof`, `lastindexof`,
`reverse`, `first`, `last`, `concat`, `slice`, `copy`, `fill`, `join`.
`slice`/`copy` return an independent
shallow copy, `concat` mutates and returns its receiver, and out-of-bounds
`get`/`set`/`insert`/`remove`/`slice`/`fill` and `pop`/`shift`/`first`/`last`
on an empty array are structured runtime errors. `get`, `set` and `remove` take
a negative index from the end (`-1` is the last element); `insert`, `slice` and
`fill` do not.
Element typing is strict: `array<int>` is not `array<float>`, and the only
implicit element coercion is int → float widening (`E0234` otherwise). A
non-array receiver is `E0235`.

**Aggregates, transforms, and ordering:** the numeric
aggregates `sum`, `avg`, `min`, `max`, `median`, `range`, `mode`, `variance`,
`stdev` reduce a numeric (`int`/`float`) array to a `float`, skipping `na`
elements; `sum` of an empty array is `0`, while `avg`/`min`/`max`/`median`/
`range`/`mode` of an empty (or all-`na`) array are `na`. `variance`/`stdev` use
the population form (denominator `n`); `mode` breaks ties toward the smallest
equally-frequent value (assumed). The transforms `abs` and `standardize` build a
new `array<float>` (`na` elements stay `na`); `standardize` is
`(x - mean) / stdev`. `sort(id, order = order.ascending)` sorts in place (void)
and `sort_indices(id, order)` returns a new `array<int>` sorting permutation;
`order.ascending` (0) / `order.descending` (1) pick the direction and `na`
elements sort last (assumed). A non-numeric receiver for any of these is
`E0235`.

**Statistics, percentiles, and binary search.** These receiver-first operations
complete the numeric `array.*` surface:

| Function | Result | Notes |
|---|---|---|
| `array.covariance(id1, id2, biased = true)` | `float` | Covariance of two equal-length numeric arrays, pairing elements by index (a pair with an `na` on either side is skipped). `biased = true` divides by `n` (population); `false` by `n − 1` (sample). `cov(x, x)` equals `array.variance(x)`. Both arrays must be numeric. |
| `array.percentrank(id, index)` | `float` | Percentile rank of the element at `index`: `100 ·` (fraction of the *other* non-`na` elements that are `≤` it). Out-of-range `index` is a structured error; a lone non-`na` element → `na`. (Denominator assumed.) |
| `array.percentile_nearest_rank(id, percentage)` | `float` | The value at the given `percentage` (0–100) using the nearest-rank method: sort ascending, take ordinal rank `ceil(percentage/100 · n)`. `0` → min, `100` → max. |
| `array.percentile_linear_interpolation(id, percentage)` | `float` | The value at `percentage` (0–100), linearly interpolating between the two nearest ranks at position `percentage/100 · (n − 1)`. (Position assumed.) |
| `array.binary_search(id, val)` | `int` | Index of `val` in the **ascending-sorted** receiver, or `-1` if absent (never a negative index — the not-found convention). |
| `array.binary_search_leftmost(id, val)` | `int` | Leftmost matching index, or (if absent) the index just left of the insertion point, clamped to `0` (see the array search tests). Always valid for a non-empty array. |
| `array.binary_search_rightmost(id, val)` | `int` | Rightmost matching index, or (if absent) the index just right of the insertion point, clamped to `size − 1`. Always valid for a non-empty array. |

The percentile/percentrank family and the binary-search variants require a
numeric receiver (`E0235`); the `percentage` argument coerces int → float. The
binary-search functions assume the receiver is already sorted ascending (a
precondition the caller must satisfy). `array.every(id)` and `array.some(id)`
take no callback: they reduce a bool or numeric array to `bool`. For numeric
arrays, zero and `na` are false and every other value is true; on an empty
array, `every` is true and `some` is false. String/color/object arrays are
rejected with `E0235`. The contract is pinned by
[`array_every_and_some_over_bool_arrays`](../crates/pinewasm_vm/src/tests.rs),
[`array_every_and_some_over_numeric_arrays_treat_zero_and_na_as_false`](../crates/pinewasm_vm/src/tests.rs),
and
[`array_every_takes_no_callback_argument`](../crates/pinewasm_semantics/src/tests.rs).

Reference-typed constructors are implemented for
`array.new_line`/`new_label`/`new_box`/`new_table`/`new_linefill` (and through
the generic `array.new<T>()` form). They produce ordinary typed arrays of
drawing handles; see
[`array_new_line_is_an_array_of_line`](../crates/pinewasm_semantics/src/tests.rs)
and
[`array_new_label_holds_a_retrievable_label`](../crates/pinewasm_vm/src/tests.rs).

Growing an array past
`RunOptions.max_collection_elements` (default 100 000) is a structured
`CollectionLimitExceeded` error — the heap-growth-bomb guard. The runtime detail
(heap, limits, error kinds, realtime rollback) is in
[`runtime.md`](runtime.md#collections-the-reference-value-heap); the
`CALL_COLLECTION` opcode and its metering in [`ir.md`](ir.md).

### Collections (maps)

Maps are **insertion-ordered key→value dictionaries** with reference
semantics — the same handle-copy aliasing, `var`-persistence, and `na`-receiver
runtime error as arrays, backed by the same per-run heap and `CALL_COLLECTION`
opcode. They are used via the **namespace call form**: `map.put(id, key, value)`
(or the equivalent method syntax `id.put(key, value)`).

**Typed constructors.** `map.new<K, V>()` is the preferred form;
`map.new_<key>_<val>()` aliases remain available for scalar pairings, e.g.
`map.new_string_float()` and `map.new_int_int()`. Keys must be scalar value
types (`float`/`int`/`bool`/`string`/`color`) or an enum
(`map.new<MyEnum, V>()`,
[`map_new_accepts_enum_key_type`](../crates/pinewasm_semantics/src/tests.rs)).
Values may be scalar, an enum (`map.new<K, MyEnum>()`,
[`map_new_accepts_enum_value_type`](../crates/pinewasm_semantics/src/tests.rs)),
a user-defined type (`map.new<string, Point>()`), or `chart.point`
(`map.new<string, chart.point>()`). Nested collection values are outside the
current map subset, and the other drawing-object value types have no map test,
so no claim is made for them.
A constructor takes no runtime arguments (`E0204`
otherwise). The resulting type is `map<K, V>` (e.g. `map<string, float>`), and
different key/value types remain distinct. UDT values are pinned by
[`map_string_to_udt_constructs`](../crates/pinewasm_semantics/src/tests.rs),
and `chart.point` values by
[`dotted_map_and_matrix_point_reads_select_point_overloads`](../crates/pinewasm_compiler/tests/v6_chart_point_coordinates.rs).

**Operations** (receiver-first):

| Function | Result | Notes |
|---|---|---|
| `map.put(id, key, value)` | `V` | Insert or overwrite; returns the **previous** value for `key`, or `na` if new. An overwrite keeps the key's insertion position. int→float value/key widening only (`E0234` otherwise). Value-producing, so a bare `map.put(...)` statement warns `W0002`. |
| `map.get(id, key)` | `V` | The value for `key`, or `na` if absent. |
| `map.contains(id, key)` | `bool` | Membership test. |
| `map.remove(id, key)` | `V` | Remove `key`; returns the removed value, or `na` if absent. |
| `map.size(id)` | `int` | Entry count. |
| `map.clear(id)` | void | Remove all entries. |
| `map.keys(id)` | `array<K>` | The keys, **in insertion order**. |
| `map.values(id)` | `array<V>` | The values, **in insertion order**. |
| `map.copy(id)` | `map<K, V>` | A shallow copy; mutating it leaves the source untouched. |
| `map.put_all(dest, src)` | void | Copy every entry of `src` into `dest`, overwriting on key collision. Both maps must share key/value types (`E0234`). |

A key or value argument whose type does not match the map's key/value type is
`E0234`; a non-map receiver is `E0235`. Growing a map past
`RunOptions.max_collection_elements` is the same structured `CollectionLimitExceeded`
guard as arrays. See [`ir.md`](ir.md) for the constructor's `ConstInt` type-tag
lowering and per-function metering, and
[`runtime.md`](runtime.md#collections-the-reference-value-heap) for the heap.

### Collections (matrices)

Matrices are **`rows × columns`, row-major** collections with the same
reference semantics as arrays and maps (handle-copy aliasing, `var`-persistence,
`na`-receiver runtime error), backed by the same per-run heap and
`CALL_COLLECTION` opcode. They are used via the **namespace call form**:
`matrix.get(id, row, column)` (or the equivalent method syntax
`id.get(row, column)`).

**Typed constructors.** `matrix.new<T>([rows[, columns[, initial_value]]])`
supports scalar element types (`float`/`int`/`bool`/`string`/`color`),
user-defined types, and `chart.point` (construction with an initial value and a
`get` read are pinned by
[`dotted_map_and_matrix_point_reads_select_point_overloads`](../crates/pinewasm_compiler/tests/v6_chart_point_coordinates.rs)).
An enum element type is accepted
([`matrix_new_accepts_enum_element_type`](../crates/pinewasm_semantics/src/tests.rs)),
and the other drawing-object element types have no matrix test, so no claim is
made for them. Both dimensions default to zero; `initial_value` defaults
to `na`. For UDT cells an omitted `initial_value` leaves `na` objects
([`matrix_sort_udt_na_field_and_missing_object_errors`](../crates/pinewasm_vm/src/tests.rs)),
so pass a UDT value to start with objects, for example
`matrix.new<Item>(2, 1, Item.new(...))`. Dimensions are ints and the initial
value must match `T` (int→float widening allowed). Arguments bind by position,
then by the names `rows`, `columns`, and `initial_value`. Unknown or
duplicate names are `E0228`; a non-int dimension is `E0234`. For UDT cells, the
value tests that run from source cover construction, `get`/`set`,
`rows`/`columns`, and `matrix.sort(sort_field=...)`; other matrix operations
over UDT cells are not implied by that support.

**Operations** (receiver-first). Bounds violations (cell coordinates, row/column
indices) are the structured `CollectionIndexOutOfBounds` runtime error; shape
disagreements are `MatrixDimensionMismatch`; a singular `matrix.inv` is
`MatrixSingular` — never a panic.

| Function | Result | Notes |
|---|---|---|
| `matrix.new_<T>([rows[, columns[, initial_value]]])` | `matrix<T>` | Fresh matrix; dimensions default to zero and cells default to `na`. A supplied `initial_value` must be compatible with `T`. |
| `matrix.get(id, row, column)` | `T` | Cell accessor (bounds-checked). |
| `matrix.set(id, row, column, value)` | void | Overwrite a cell (bounds-checked). |
| `matrix.rows(id)` / `matrix.columns(id)` / `matrix.elements_count(id)` | `int` | Shape queries (`elements_count = rows * columns`). |
| `matrix.fill(id, value[, from_row[, to_row[, from_column[, to_column]]]])` | void | Fill the whole matrix when bounds are omitted, or the half-open rectangular subrange `[from_row, to_row) × [from_column, to_column)` selected by the supplied prefix of bounds; pinned by [`matrix_fill_honors_its_row_and_column_bounds`](../crates/pinewasm_vm/src/tests.rs). |
| `matrix.copy(id)` | `matrix<T>` | New, independently mutable matrix with shallow-copied cells. The VM copies a reference-valued cell as its handle. User-defined cells are constructible from source (`matrix.new<MyType>()`), but no test runs `matrix.copy` over reference-valued cells. |
| `matrix.add_row(id, row, array<T>)` / `matrix.add_col(id, column, array<T>)` | void | Insert a line from an array (length must match the opposite dimension). An omitted or `na` index appends, and an omitted or `na` array inserts a line of `na`. |
| `matrix.remove_row(id, row)` / `matrix.remove_col(id, column)` | `array<T>` | Mutate `id` and return the removed line. The explicit index is currently required; the optional-index form is deferred. Discarding the returned array is allowed without `W0002`. |
| `matrix.swap_rows(id, a, b)` / `matrix.swap_columns(id, a, b)` | void | Swap two lines. |
| `matrix.reverse(id)` | void | Reverse the element order across both axes. |
| `matrix.transpose(id)` | `matrix<T>` | New transposed matrix. |
| `matrix.reshape(id, rows, columns)` | void | In-place reshape (same element count). |
| `matrix.submatrix(id, from_row, to_row, from_column, to_column)` | `matrix<T>` | New matrix of a rectangular region (`to_*` exclusive). |
| `matrix.concat(id1, id2)` | `matrix<T>` | Append `id2`'s rows to `id1` (same column count); returns `id1`. |
| `matrix.sum(id, matrix\|scalar)` / `matrix.diff(id, matrix\|scalar)` | `matrix<float>` | Elementwise (shape-checked) or scalar-broadcast add/subtract. |
| `matrix.mult(id, matrix\|scalar)` | `matrix<float>` | Matrix product (inner-dimension check) or scalar scaling. |
| `matrix.det(id)` / `matrix.trace(id)` | `float` | Determinant (LU, partial pivoting) / diagonal sum. |
| `matrix.rank(id)` | `int` | Numerical rank via Gaussian elimination with partial pivoting. |
| `matrix.row(id, row)` / `matrix.col(id, column)` | `array<T>` | Extract a row / column as a fresh shallow array; supported scalar cells keep their exact representation. Reference cells keep the same handle. User-defined cells are constructible from source (`matrix.new<MyType>()`), but handle identity through this call is tested only at VM level. Out-of-range index → error. |
| `matrix.inv(id)` | `matrix<float>` | Inverse (Gauss-Jordan, partial pivoting); `inv(A)·A ≈ I` within `1e-3`; singular → error. |
| `matrix.pow(id, power)` | `matrix<float>` | Integer matrix power (square only). |
| `matrix.kron(id1, id2)` | `matrix<float>` | Kronecker product. |
| `matrix.avg/min/max/median/mode(id)` | `float` | Aggregate over the non-`na` cells (skip-`na`; `mode` tie-break = smallest, assumed). |
| `matrix.is_square/is_identity/is_diagonal/is_antidiagonal/is_symmetric/is_antisymmetric/is_triangular/is_stochastic/is_binary/is_zero(id)` | `bool` | Shape/structure predicates (`1e-9` tolerance). |
| `matrix.eigenvalues(id)` | `array<float>` | Eigenvalues of a **symmetric** matrix (cyclic Jacobi, iteration-capped) in descending order (assumed); non-symmetric/non-square → error. |
| `matrix.eigenvectors(id)` | `matrix<float>` | Eigenvectors of a **symmetric** matrix as matrix columns, matched to the eigenvalue order; `A·v ≈ λ·v`. |
| `matrix.pinv(id)` | `matrix<float>` | Moore-Penrose pseudo-inverse of a **full-rank** matrix (normal equations, reusing the LU inverse); rank-deficient → error. |
| `matrix.sort(id, column = 0, order = order.ascending[, sort_field])` | `void` | Reorders rows in place by a primitive int/float/string column or by a UDT int/float/string field selected by const index/name. Missing primitive keys sort last ascending and first descending; UDT `na` objects produce a field-access runtime error. |

The **linear-algebra, aggregate, and numeric predicate** operations require an
`int` or `float` matrix (`E0234` on a string/bool/color matrix). The
second operand of `sum`/`diff`/`mult` must be a same-element-type matrix **or** a
scalar of the element type (`E0234` otherwise); a non-matrix receiver is `E0235`.
`matrix.eigenvalues`/`eigenvectors` are implemented via the cyclic Jacobi
algorithm for **symmetric** matrices and `matrix.pinv` via the normal equations
for **full-rank** matrices; non-symmetric eigen and rank-deficient pinv are
structured errors (general/complex spectra and the SVD-based general pinv are out
of scope). See [`ir.md`](ir.md) for the constructor's `ConstInt`
element-type-tag lowering and per-function metering.

### Qualifiers

Every expression carries a qualifier: `const < input < simple < series`. The
compiler produces `const` (literals and `color.*`/`math.*` constants), `input`
(the `input.*` interface above), `simple` (chart-context values such as
`syminfo.ticker`), and `series` (OHLCV, `ta.*`, user-defined-type field reads,
and anything derived from them). Combining values takes the join — the
weakest guarantee wins. Parameters declare a maximum accepted qualifier;
passing a value above that maximum is `E0206`. The UDT producer is pinned by
[`udt_field_reads_are_series_qualified`](../crates/pinewasm_semantics/src/tests.rs);
the field-history rule by
[`field_history_is_rejected_only_when_written_on_the_field`](../crates/pinewasm_semantics/src/tests.rs).

## Worked example

```pine
//@version=6
indicator("Example")
x = close - close[1]
plot(x)
```

Run against market data with `close = [10, 12, 11]` (all five OHLCV arrays
must have equal length), the script executes once per bar:

| bar | `close` | `close[1]` | `x`  | plotted |
|----:|--------:|-----------:|-----:|--------:|
| 0   | 10      | `na`       | `na` | `na`    |
| 1   | 12      | 10         | 2    | 2.0     |
| 2   | 11      | 12         | −1   | −1.0    |

The single plot series is `[na, 2.0, -1.0]`. On bar 0 the history read
`close[1]` falls before the first bar, so it yields `na`, and `na`
propagates through the subtraction. Across the Wasm/JSON boundary `na`
serializes as `null`, so a JavaScript host receives:

```json
{
  "plots": [
    { "slot": 0, "title": null, "values": [null, 2.0, -1.0] }
  ],
  "barsExecuted": 3,
  "runtimeErrors": [],
  "diagnostics": []
}
```

## Diagnostic codes

Diagnostics are structured objects, never bare strings:
`{ severity, code, message, labels, notes }`, where each label is
`{ span, message }` and spans are UTF-8 byte offsets. Severity is `error`
or `warning`. `code` is absent only for internal errors that predate a code
assignment.

| Code  | Severity | Meaning |
|-------|----------|---------|
| E0001 | error | Source exceeds an input guard: more than 1 MiB of UTF-8, or more than 1,048,576 tokens once lexed (one per byte of the largest source; block structure adds `Newline`/`Indent`/`Dedent` tokens that no byte spells). In a multi-source compile, also a limit on the library table (see [`libraries.md`](libraries.md)) |
| E0002 | error | More errors than a compile reports. The first 1,000 are listed and this one, after them, says that the rest were left out. It has no label. The limits are described after this table |
| E0100 | error | Inconsistent indentation: dedent does not match any enclosing block |
| E0101 | error | Unexpected character (e.g. `!` without `=`, non-ASCII symbol). A lone `:` is a real token (the ternary branch separator); strays are parse errors (`E0110`), not lexer errors |
| E0102 | error | Unterminated string literal (including a `"""`/`'''` multiline string without its closing delimiter) |
| E0110 | error | Parse error: unexpected token, malformed version directive, misplaced version directive, trailing tokens after a statement, expression or declared type nested too deeply, integer literal out of range, assignment to a namespaced name or to `na`, a dot not followed by an identifier, a ternary `?` without its `:` or a stray `:` outside a ternary, a type-annotated declaration using `:=` instead of `=`, a `var`/`varip` declaration using `:=` instead of `=`, etc. |
| E0201 | error | Unknown identifier (with "did you mean" suggestion when close to a built-in) |
| E0202 | error | Unknown function |
| E0203 | error | Called a name that is a value, not a function |
| E0204 | error | Wrong argument count, including a missing required script-declaration title or too many positional arguments; also an `indicator()` `timeframe_gaps` argument without a `timeframe` argument |
| E0205 | error | Argument type mismatch, including a script/output title that is not the required string literal, and a user-function parameter default that does not have the parameter's declared type |
| E0206 | error | Argument qualifier too strong for the parameter (e.g. series value where const required) |
| E0207 | error | Operand type mismatch for a unary or binary operator; also mismatched `if`/`switch`-expression branch types, or a `switch` case label not comparable to the subject |
| E0208 | error | `if` or `while` condition is not `bool`; also a subjectless `switch` case condition that is not `bool` |
| E0209 | error | History is written directly on a literal, a built-in value without direct-history capability, enum member, object field (`obj.field[1]`), or a built-in `text_format` value; computed weak-qualifier values can materialize history |
| E0210 | error | History offset is not numeric (`int` or `float`); fractional offsets truncate toward zero |
| E0211 | error | Duplicate declaration in the same scope, or shadowing a built-in variable the script also uses |
| E0212 | error | Void misuse: assigning a void value, or using an effect call inside an expression |
| E0213 | error | Duplicate declaration of the same script kind (`indicator`, `strategy`, or `library`) |
| E0214 | error | A script-global declaration/configuration call (`indicator(...)` or `max_bars_back(...)`) appears in local scope |
| E0215 | error | **Reserved** — history on compound expressions is now supported via implicit slot materialization (see [History references](#history-references)); the code number stays reserved and is never emitted |
| E0216 | error | Function name used as a value without calling it |
| E0217 | error | `:=` target is not assignable: undeclared variable (declare it first with `=`), built-in series, function name, the counter of an enclosing `for` loop ("cannot reassign the loop counter"), or, inside a function, a global or caller variable or one of the function's own parameters |
| E0218 | error | `:=` type mismatch: the value's type does not equal the variable's type (and is not the permitted int → float widening) |
| E0219 | error | Invalid compile-time-fixed argument to a stateful builtin. Callees requiring fixed window lengths accept an in-range int literal or a propagated compile-time-known int binding (including an `input.int` default and a forwarded simple-int function argument); their reassigned/series values, general expressions, floats, and out-of-range values are rejected. Documented runtime-length forms such as `ta.sma` instead validate the current integer length at runtime. Other structurally fixed parameters retain their callee-specific literal/range rules, including offsets, pivot widths, ALMA parameters, multipliers, percentages, SAR parameters, and boolean flags. |
| E0220 | error | The `na` literal in a boolean context (booleans cannot be missing), or in a position whose expected type cannot be inferred (*unannotated* declarations, unannotated `var` initializers, bare statements, unary `-`, `if` conditions); also an `if`/`switch` expression whose every branch is `na` with no outer expected type; carries a note pointing to type annotations (`float x = na`). See [the na literal](#lexical-structure) |
| E0221 | error | Invalid argument to a pure builtin where E0205/E0219 do not fit: a relational violation such as an `nz` replacement whose type does not match the source's, or a void argument to the any-typed `na(x)` parameter |
| E0222 | error | `for` loop bound/step error, or a stateful construct inside a loop body: a non-numeric `start`/`end`/`step` bound, or a stateful `ta.*` call or compound-history materialization inside a `for`/`while` body (per-call-site state assumes once-per-bar execution — see [PineWASM-specific semantic rules](#pinewasm-specific-semantic-rules)). Numeric zero/negative steps compile and fail at runtime. |
| E0223 | error | `break` or `continue` outside of a loop (including inside an `if` that is not itself inside a loop); it is only valid inside a `for` or `while` body |
| E0224 | error | User-defined function error: a definition nested inside a block (definitions are top-level only), a duplicate parameter name, a second definition with the same name and signature, wrong argument count at a call site (or no overload taking the arguments), a call site that omits a parameter without a default (`f(a = 1.0, b)` called as `f(5.0)`), direct or mutual recursion, the per-program instantiation-size guard tripping (a pathological call graph inlines to more than 10 000 statements), or the nesting guard tripping (expressions and blocks nest more than 200 units deep along one call path) |
| E0225 | error | `switch` structural/context error (more than one `default` arm, a labeled arm after the default, or a `switch` subject that produces no value); **or** a tuple/destructuring error: a destructuring pattern whose target count does not match the tuple arity (including a scalar RHS treated as a 1-tuple), a tuple `[...]` in an illegal position (anywhere but a destructuring RHS or a function body's last statement), or a tuple-returning call (`ta.macd`/`ta.bb`/`ta.kc`/`ta.dmi`/`ta.supertrend`, three-argument `ta.vwap`, or a tuple-bodied user function, method or library function) used as a scalar value or written as a statement instead of being destructured |
| E0226 | error | Type-annotated declaration mismatch: the initializer's type is incompatible with the declared annotation (`int n = 1.5`, `bool b = 1`, `string s = 3`). Only a `float` annotation may take an `int` initializer (coerced); a bare `na` adopts the annotation |
| E0227 | error | A value-producing `if`/`switch` branch is empty or lacks a value-producing tail; this also applies when its trailing counted loop has no final expression or assignment |
| E0228 | error | Named-argument binding error at a registry-bound built-in, declaration, scalar request interface, typed matrix constructor, or user-function call: an argument bound twice or an unknown parameter name (with a did-you-mean over the signature). A positional argument after a named one is a parse error; dedicated positional-only interfaces use their own diagnostic codes. |
| E0229 | error | Color literal/value error. Currently emitted by the lexer for a malformed hex color literal — a `#` followed by anything other than exactly 6 (`#RRGGBB`) or 8 (`#RRGGBBAA`) hex digits. (Reserved for the remaining `color.*` function/value errors as they land.) |
| E0230 | error | `input.*` declaration error: a non-constant argument (`input.int(close)`), a wrong-typed default (`input.int("x")`), a numeric default outside its declared `minval`/`maxval` or absent from its `options` list, `minval`/`maxval`/`step` combined with `options`, a malformed `options` list, a missing `defval`, too many arguments, or an `input.source` default that does not name a builtin series. (Named-argument binding errors for `input.*` are `E0228`, shared with every other call.) |
| E0231 | error | Reserved for deferred time-argument errors; never emitted |
| E0232 | error | Reserved for deferred `request.*` argument errors; never emitted |
| E0233 | error | A literal history offset exceeds 5000 bars, the most a history buffer holds. A literal offset within that limit needs no declaration: it raises the effective `max_bars_back` ceiling (default 500, also raised by the largest `indicator(max_bars_back=N)` argument or top-level `max_bars_back(source, N)` call) to its own depth. An invalid `max_bars_back` argument itself is `E0219`; a too-deep dynamic offset is the structured runtime error `HistoryBufferLimitExceeded`. See [`runtime.md`](runtime.md). |
| E0234 | error | Collection type-argument / element-type error. This covers malformed/unknown generic types, an element/key/value that does not match its array/map/matrix type, incompatible collection operands, invalid dimensions/index argument types, and unsupported nested shapes. The only implicit element coercion is int → float widening. |
| E0235 | error | Collection operation/receiver error. The receiver is not the required array/map/matrix type, an operation is unavailable for that element type (for example, `array.every` on strings), or a collection call uses a disallowed argument form. |
| E0236 | error | User-defined type declaration error, including a field default that is a function call or other expression rather than a literal, a built-in variable or constant, or an enum member |
| E0237 | error | User-defined type field or method resolution error |
| E0238 | error | Drawing-API error. A call to an unknown drawing member (`line.foo`), a getter/setter naming an unknown field, a wrong-arity or wrong-receiver-type drawing call, or an unknown, repeated, or missing `table.cell` parameter. (A getter on an `na` drawing object returns `na`, a setter on one is a structured **runtime** error, `NaFieldAccess`, not this compile code, and a delete on one is a no-op.) |
| E0239 | error | Enum error. A duplicate enum name, a duplicate member within one enum, an enum with the name of a built-in type or namespace (`enum ta`), or a reference to a member that the enum does not declare (`Signal.typo`). (`E0236`/`E0237` are the type-declaration and field/method-resolution errors, described in the [User-defined types](#user-defined-types) section.) |
| E0240 | error | Script-kind conflict: a script declares more than one of `indicator(...)`, `strategy(...)`, `library(...)` (they are mutually exclusive) |
| E0241 | error | Strategy primitive/getter used without a `strategy(...)` declaration (order primitives, getters, per-trade accessors, risk rules, conversions) — see the [broker-emulator contract](runtime.md#strategies-the-broker-emulator) |
| E0242 | error | A request in a local scope or unresolved if/switch control when `indicator`, `strategy` or `library` sets `dynamic_requests=false`, or a `request.security(...)` restriction: a non-constant symbol/timeframe, an out-of-range timeframe, a bad `barmerge.*` flag, or `gaps`/`lookahead` requested `_on` together with an external (host-backed) symbol — see the [`request.security` runtime contract](runtime.md#requestsecurity-and-secondary-contexts) |
| E0243 | error | Library/import/export error: an unresolved `import` (path absent from the provided source map), an `export` in a non-library script, a call to a private (non-exported) function through an alias, a library that exports nothing, or a library source that fails to parse/oversized |
| E0244 | error | A `request.security(...)` compound sub-expression uses a construct the secondary higher-timeframe context cannot evaluate. The supported tree includes constants, resampled OHLCV/derived sources, direct input-selected sources, fixed history reads (including history of supported compounds), unary negation, arithmetic/comparison, and `ta.sma`/`ta.ema`/`ta.hma`/`ta.vwma`/`ta.change` with supported literal or direct input-selected lengths on the same feed, including unchanged numeric expression aliases; see the [`request.security` runtime contract](runtime.md#requestsecurity-and-secondary-contexts). |
| E0245 | error | Order-flow footprint accessor error (`footprint.*` / `volume_row.*`): wrong arity, a receiver that is not the expected footprint/volume-row object, or named arguments where the interface is positional |
| E0246 | error | Restriction on a chart-feed-only `request.*` interface. `request.footprint` is positional; `request.security_lower_tf` accepts its seven canonical positional/named parameters but requires the chart's own symbol, a constant timeframe, and a plain price-source expression. |
| E0247 | error | `ta.pivot_point_levels` pivot-system argument error: the type argument is not a constant string literal, or names an unsupported pivot system |
| E0248 | error | Numeric builtin argument is statically outside its valid range, such as `math.round` precision outside `[0, 100]`; runtime precision values are clamped |
| E0249 | error | `//@version=` below 6. PineWASM implements version 6 semantics only, so the script must be migrated first; `pinewasm migrate` rewrites older source (see [migration.md](migration.md)) |
| E0250 | error | A `for`, `for...in`, or `while` loop used as an expression is preserved by the migrator but is not yet lowered by the compiler |
| E0251 | error | A `strategy.risk.*` argument is too large to evaluate ahead of the script: with the variables it reads replaced by the expressions that define them, it has more than 4096 expression nodes or 256 nested levels, or the risk arguments of the script have more than 262144 nodes together |
| E0252 | error | The compiler cannot prove that a persistent value or request control is fixed before a metadata or other weak-qualified consumer. A possible later write can reach an earlier read across executions. This is a local compiler limitation, not a claim that the script is invalid; data and per-bar paint keep their runtime meaning. |
| E0253 | error | A script limit is exceeded: more than 64 plot counts (reported once, with the script's total), or more than 1,000 variables declared in one scope. |
| E0254 | error | Cannot use a plot-family call or `alertcondition()` in local scope, or a `plot()`/`hline()` call as a ternary operand |
| E9999 | error | Internal compiler error (a compiler bug, not an error in the script) |
| W0001 | warning | Script has no `indicator(...)`, `strategy(...)`, or `library(...)` declaration |
| W0002 | warning | Expression result is discarded. Side-effect-first drawing creation, `array.pop`/`array.shift`/`array.remove`, and `matrix.remove_row`/`matrix.remove_col` calls are exempt because their optional return values may be ignored. |
| W0003 | warning | `//@version=` value above 6 (treated as 6) |
| W0004 | warning | Series-materializing construct (a materialized history site or a stateful builtin call site — slot-based or engine-backed) that does not execute on every bar. Inside a **series-controlled** conditional branch (an `if`/`switch` **statement or expression** branch), a slot-based site records `na` for bars where the branch does not execute (unavailable history yields `false` for Boolean results and `na` otherwise), and an engine-backed site's state freezes on those bars (see [`runtime.md`](runtime.md)). Input/simple-controlled branches are run-stable and do not warn. Inside a `var` initializer the construct executes only once. Series-controlled ternary (`?:`) branches warn too, because only the selected arm runs (see [the ternary conditional operator](#the-ternary-conditional-operator)) |
| W0005 | warning | A repeated `//@version=` directive, which is ignored (the first one applies) |
| W0006 | warning | A variable shadows a built-in variable |
| W0007 | warning | More warnings than a compile reports. The first 1,000 are listed and this one, after them, says that the rest were left out. It has no label |
| W0008 | warning | A stateful call is guarded by the lazily evaluated right-hand side of `and` or `or`, so some bars can skip it and its history can drift. Evaluate the call once before the boolean expression and reference its result. |
| W0009 | warning | A stateful call appears in a ternary branch. Only the selected arm runs, so the call's history can advance unevenly; evaluate it before the conditional and select its saved result. |

Every code is defined once as a constant in `pinewasm_span::codes` (with its
canonical meaning as the doc comment); emission sites reference the constants,
never raw string literals, so this table and the compiler cannot silently
disagree on a code's identity.

A diagnostic for a name that is not known (an identifier, a function, a
method, a named argument, a type) may carry the note ``did you mean `X`?``,
where `X` is a known name within two edits of the one written, comparing the
first 32 characters of each. The nearest name is offered, and among equally
near names the first in name order, so a script always gets the same
suggestion. A compile stops searching for these notes once it has looked at
1,048,576 candidate names; the search under way when it gets there finishes.
That is about 1,400 distinct unknown identifiers or functions, each of which
is searched for once however often it is used. A script with more gets the
later diagnostics without the note, and is otherwise diagnosed the same.

A compile reports at most 1,000 errors and 1,000 warnings: the first of each,
in the order the compiler found them. A source can be wrong in far more places
than that (1 MiB holds half a million unknown names), and holding a diagnostic
for each costs memory in proportion. When there are more, the list
ends with `E0002` (errors were left out), `W0007` (warnings were left out), or
both, `E0002` first. Neither has a label, because it is about the list and
not about a place in the source.

- The two limits are separate. Warnings never take the place of an error, and
  a list that left an error out still holds errors, so what is left out never
  changes whether a compile succeeds or the program it produces.
- The limits cover the compile as a whole: the main source and every library
  together, and the lexer, the parser and the analyzer together. Each of
  those applies the same limits when it is called on its own.
- The limits are on how many diagnostics are reported. One diagnostic can
  still be long when its message quotes a long name from the source.
- Fix what is reported and compile again to see what remains.

## Current coverage boundaries

This language reference explains the implemented core and selected
interfaces; it is not an exhaustive support inventory. The generated
[`coverage matrix`](pine-v6-coverage.md) is authoritative for every
builtin, overload, and compatibility qualification. Its rows link to the tests
that gate each status. If this narrative and the matrix ever disagree, use the
matrix and its cited tests.

The current high-level boundaries are:

- **Language and types.** `var`/`varip`, explicit annotations (including
  drawing, UDT, and enum types), UDT construction/fields/methods, enums,
  counted and collection loops, `while`, `switch`, user functions, and
  library function import/export are implemented. `matrix<T>` and `map<K, V>`
  annotations are implemented with compile-time tests only (see
  [Type annotations](#type-annotations)). Remaining tracked subsets include
  linking exported UDTs/enums/methods; input-of-UDT; per-field UDT history;
  test coverage for `matrix<UDT>` operations beyond construction, `get`/`set`,
  `rows`/`columns`, and field sorting; the `array.new<MyEnum>()`,
  `matrix.new<MyEnum>()`, `map.new<MyEnum, V>()`, and `map.new<K, MyEnum>()`
  constructors and a `map<MyEnum, V>` variable or field annotation (`E0234`);
  and `for … in` over a matrix or a map (`E0235`).
- **Collections and drawings.** Arrays (including `every`/`some` and drawing
  arrays), maps, and the documented matrix surface are implemented, as are
  `chart.point`, line, label, box, table, polyline, and linefill objects. The
  matrix records exact numeric-domain, rendering, object-cap, and garbage-
  collection qualifications rather than treating a namespace as all-or-none.
- **Outputs, strategy, and realtime.** Plot-family outputs, plot handles,
  fills/horizontal lines, color/display metadata, the broker-emulated strategy
  surface, realtime rollback, `varip`, `barstate.*`, and bounded history rings
  are implemented. Headless rendering metadata and overload differences
  remain per-row qualifications in the matrix and [`runtime.md`](runtime.md).
- **Requests and chart context.** `request.security` supports same-feed
  resampling and the documented compound/history tree; external requests,
  `request.security_lower_tf`, and `request.seed` are host-data interfaces with
  explicit `na`/empty defaults. Time/session functions, calendar values,
  `syminfo.*`, and `timeframe.*` are substantially implemented, with exact
  resampling, calendar, metadata-default, and no-intrabar-feed boundaries
  recorded in the matrix.
- **Builtins and overloads.** `math.*`, `str.*`, `color.*`, `input.*`, and the
  implemented `ta.*` catalog include many overloads beyond the examples in this
  file. Treat omission here as omission from the narrative, never proof that a
  symbol is unsupported.

## PineWASM-specific semantic rules

The numbered entries below are stable identifiers, so existing test references
keep their meaning. Each entry states a rule of PineWASM's own that needs
explicit mention. The
[coverage matrix](pine-v6-coverage.md) is authoritative for current
status.

1. **Plot-family calls are global-scope only.** Calls inside an
   `if`, loop, or user function are rejected with `E0254`.
   The diagnostic covers the complete call span and is pinned by
   [`plot_family_calls_in_local_scope_are_rejected_with_e0254_and_full_call_spans`](../crates/pinewasm_semantics/src/tests.rs).
2. **History offsets accept a literal or a numeric expression.** A
   non-negative integer literal takes the compile-time fast path (up to
   5000 bars back, `E0233` beyond, raising the effective `max_bars_back`
   ceiling to its depth); an `int`- or `float`-typed
   expression (`close[n]`) is resolved at run time, truncating fractional
   offsets toward zero, and checked against that effective ceiling.
   A non-negative dynamic offset above the ceiling raises
   `HistoryBufferLimitExceeded`. Unavailable history, including offsets
   before bar 0, yields `false` for Boolean results and `na` otherwise.
   A nonnumeric offset is `E0210`. Negative-offset and `na`-offset behavior
   remains **assumed**, with existing tests pinning the local behavior.
3. **`and`/`or` short-circuit, with Kleene three-valued `na` logic on top.** The rhs is skipped exactly when the lhs alone decides
   (`false and …`, `true or …`), so a guarded rhs never runs — its runtime
   faults (an out-of-bounds `array.get` behind a size guard) never fire and
   its stateful `ta.*` calls do not execute on skipped bars (the behavior
   W0008 warns about). PineWASM booleans can hold `na`: an `na` lhs is legal and
   still evaluates the rhs so the strong-Kleene combine can decide
   (`false and na` is `false`, `true or na` is `true`, `na and false` is
   `false`, `na or true` is `true`, otherwise `na`).
4. **Comparing against `na` yields `na`** (not `false`), keeping unknowns
   explicit instead of silently collapsing them to a boolean.
5. **An `if` condition that evaluates to `na` is treated as false** (the
   branch is not taken), which avoids aborting the bar for undefined
   conditions.
6. **Division by zero, any non-finite float result, and any int overflow
   produce `na`** so NaN/±Inf and wrapped integers never enter the value
   domain and every "no meaningful value" state funnels through one
   representation. (Int overflow → `na` and the sign-of-dividend `%` rule
   are **assumed** behaviors, pinned by tests — see the operator rules
   above; `int / int` keeps its fraction.)
7. **Non-finite input data (NaN/±Inf in OHLCV arrays) reads as `na`** for
   the same reason: data holes should behave like missing data, not like
   propagating NaNs.
8. **`//@version=` values below 6 are rejected (`E0249`)** because PineWASM
   implements only version 6 semantics. `pinewasm migrate` rewrites older
   source. Values above 6 compile with a warning (`W0003`) and version-6
   semantics.
9. **Current compatibility note — `varip`, realtime rollback,
   `barstate.*`, and bounded history storage are implemented.** An optional
   `RunOptions.realtime` segment replays ticks on the last bar(s) with PineWASM's
   rollback semantics: all non-`varip` state is restored to the bar's
   start before each tick, and only bar-close values commit. `varip`
   opts a slot out of that rollback so it persists across ticks; the seven
   `barstate.*` predicates expose the driver's bar/tick state as
   `series bool`. A literal offset past the 5000-bar buffer limit is `E0233`,
   and a dynamic offset past the effective `max_bars_back` ceiling is
   `HistoryBufferLimitExceeded`. User-slot
   history is stored in a bounded ring sized from the compiled history depth,
   not one cell per slot per bar. (`var` and `:=` behave identically to before
   on historical bars.) See [`runtime.md`](runtime.md),
   [`slot_history_is_a_ring_not_one_cell_per_bar`](../crates/pinewasm_vm/src/tests.rs),
   and
   [`varip_counts_ticks_var_counts_bars`](../crates/pinewasm_vm/src/tests.rs).
10. **Strings support only the `\n` `\t` `\"` `\\` escapes**, the minimal
    set needed for titles; other escapes are rejected loudly rather than
    passed through ambiguously.
11. **Tabs count as 4 columns** in indentation measurement — a fixed,
    documented rule; scripts
    that mix tabs and spaces get deterministic block structure.
12. **`not` binds looser than comparisons and equality** (`not a > b` is
    `not (a > b)`). Use explicit parentheses where precedence matters.
13. **Script-declaration and `plot` titles must be string literals.** A
    `const string` variable is rejected with `E0205` rather than
    accepted-and-dropped; constant folding does not currently widen this
    literal-only surface.
14. **TA length rules depend on the builtin and execution route.**
    `input.int` declarations, aliases, and simple-int helper parameters retain
    their input dependency. Host overrides resize slot-based `ta.sma` /
    `ta.change` windows and engine-backed integer parameters, including
    `math.sum`, without recompiling. Fixed-length-only forms reject general
    integer expressions and series lengths with `E0219`; documented dynamic
    forms such as `ta.sma` accept runtime integer lengths with their stated
    native/hosted limits. Direct input-backed lengths inside same-feed
    `request.security` compounds work for `ta.sma`, `ta.ema`, `ta.hma`, `ta.vwma`, and `ta.change`;
    external-symbol input-backed compounds remain `E0244`. Literal and supported
    input-selected request timeframes, plus unchanged numeric source expressions,
    retain their provenance through nested helper parameters; persistent selector
    arguments are not frozen. Fixed history on unchanged numeric aliases and
    helper parameters applies to the reconstructed requested-context expression,
    including input-selected sources. Integer-input history offsets bind to each run's selection in direct reads and forwarded helper parameters; arithmetic and numeric casts over input offsets bind per run, while chart-varying offsets remain unsupported. See
    [`security_input_history_binds_selected_offset_in_direct_and_nested_helpers`](../crates/pinewasm_vm/src/tests.rs),
    [`input_history_round_trips_and_rejects_malformed_bindings`](../crates/pinewasm_csil/src/tests.rs),
    [`security_helper_history_uses_requested_price_and_sma_history`](../crates/pinewasm_vm/src/tests.rs),
    [`security_helper_history_preserves_input_source_overrides_and_constant_offsets`](../crates/pinewasm_vm/src/tests.rs),
    [`security_nested_helper_preserves_selectors_and_requested_source_context`](../crates/pinewasm_vm/src/tests.rs),
    [`security_nested_helper_does_not_freeze_persistent_or_chart_reactive_arguments`](../crates/pinewasm_vm/src/tests.rs),
    [`ta_input_lengths_compile_once_and_match_changed_defaults`](../crates/pinewasm_compiler/tests/inputs.rs),
    [`runtime_ta_input_lengths_match_recompiled_defaults_and_vm`](../crates/pinewasm_aot/tests/differential.rs),
    and [`ta_length_sweeps_reuse_one_standalone_wasm_instance_and_match_changed_defaults`](../crates/pinewasm_aot/tests/standalone_wasm.rs).
15. **`math.random` is deterministic per run** — the VM owns a fixed
    counter-based PRNG (splitmix64) keyed by the optional `seed` argument
    (a fixed default seed when omitted), so identical inputs always
    reproduce identical draws. See
    [`runtime.md`](runtime.md#mathrandom-determinism).
16. **Explicit type annotations provide an expected type for `na`.** Scalar/color, `array<T>`, drawing, UDT, and enum annotations are
    implemented, so declarations such as `float x = na`, `Point p = na`, and
    `Signal s = na` compile. An unannotated `x = na` still has no inferable
    type and is `E0220`, matching the need for an expected type. See
    [Lexical structure](#lexical-structure) and
    [`annotation_with_na_initializer_types_the_variable`](../crates/pinewasm_semantics/src/tests.rs).
17. **`?:` is lazy.** An out-of-bounds read or `ta.*` call in the unselected
    arm never runs. Ternaries with a non-literal arm lower to real branch
    blocks, so only the selected arm runs, as with `if`/`switch` expressions. Pinned by
    [`ternary_does_not_advance_unselected_ta_state`](../crates/pinewasm_vm/src/tests.rs),
    [`ternary_size_guard_skips_out_of_bounds_get`](../crates/pinewasm_vm/src/tests.rs),
    and [`series_ternary_branch_history_sites_warn_w0004`](../crates/pinewasm_semantics/src/tests.rs).
18. **The bare `ta.vwap` variable accumulates from bar 0.** That variable form
    keeps a running VWAP of `hlc3` from bar 0 and does not reset. The function overloads are implemented separately:
    `ta.vwap(source)`, `ta.vwap(source, anchor)`, and the three-result
    `ta.vwap(source, anchor, stddev_mult)` form; the anchor overload resets on
    true pulses. Pinned by
    [`ta_vwap_cumulative_from_bar_zero_and_constant_identity`](../crates/pinewasm_vm/src/tests.rs),
    [`ta_vwap_anchor_overload_resets_on_anchor_pulses`](../crates/pinewasm_vm/src/tests.rs),
    and
    [`ta_vwap_band_overload_returns_weighted_population_sigma_bands`](../crates/pinewasm_vm/src/tests.rs).

19. **Stateful `ta.*` calls and compound-history materialization are
    forbidden inside loop bodies (`E0222`).** A `ta.*` call or a
    `(expr)[n]`/`ta.obv`-family history site owns per-call-site state (hidden
    slots, ring buffers) that assumes it executes once per bar; a `for`/
    `while` body would advance it several times per bar, so PineWASM rejects
    it outright rather than producing an ill-defined result. Pinned by
    [`stateful_ta_in_a_loop_body_is_e0222`](../crates/pinewasm_semantics/src/tests.rs)
    and
    [`compound_history_in_a_loop_body_is_e0222`](../crates/pinewasm_semantics/src/tests.rs).

20. **Loop-local scoping mirrors branch scoping (assumed).** The counter is
    immutable inside the body (`E0217`), and the loop body is a nested scope
    like an `if` branch (declarations do not leak, `:=` joins `series`). The
    precise loop-local history semantics are **assumed**. The loop bound is
    re-evaluated on each iteration and is pinned by
    [`for_end_boundary_is_reevaluated_each_iteration`](../crates/pinewasm_vm/src/tests.rs);
    the local runtime reports a generic `UserError` for a nonpositive step.
21. **Function bodies and defaults resolve names at their definition.** Every
    call is inlined at its call site, but a body's free names, and an omitted
    parameter's default, resolve in the function's own scopes and then the
    globals. A caller's parameter or local cannot stand in for the global the
    definition names, pinned by
    [`a_body_reads_the_global_not_a_caller_parameter_of_the_same_name`](../crates/pinewasm_compiler/tests/function_lexical_scope.rs)
    and
    [`a_default_reads_the_global_not_a_caller_parameter_of_the_same_name`](../crates/pinewasm_compiler/tests/function_lexical_scope.rs).
    A body and its defaults also see only the globals declared above the
    definition: a global declared after the definition but before the call is
    `E0201` (or the built-in of that name, with `E0211` for shadowing and
    using it), pinned by
    [`a_global_declared_below_the_definition_is_unknown_in_the_body`](../crates/pinewasm_compiler/tests/function_lexical_scope.rs),
    [`a_global_declared_below_the_definition_is_unknown_in_a_default`](../crates/pinewasm_compiler/tests/function_lexical_scope.rs)
    and
    [`a_library_function_sees_only_the_library_globals_above_it`](../crates/pinewasm_compiler/tests/function_lexical_scope.rs).
22. **Calls to user functions do not depend on definition order.** PineWASM collects
    every function and method definition before analyzing statements, so a
    call may name one defined below it, from the top level or from another
    body, pinned by
    [`calls_to_user_functions_do_not_depend_on_order`](../crates/pinewasm_compiler/tests/function_lexical_scope.rs).
    This accepts calls to functions defined below the call. A call can
    resolve differently only when a definition below it shares the called
    name: PineWASM chooses among every overload of the name, and a user method
    defined below a call takes precedence over a built-in method of the same
    name (`a.sum()` on an `array<float>`). A plain user function cannot take a
    built-in function's name (`E0211`). Recursion is still `E0224`, and a
    body's globals still follow its own definition (entry 21).

### Numeric types and declarations

The int/float numeric tower covers these cases:
`bar_index` is typed `series int`, and `int` is a
real type — see
[`bar_index_is_int_and_supports_modulo`](../crates/pinewasm_vm/src/tests.rs)
and the [coverage matrix](pine-v6-coverage.md). Reassignment is provided by
the `var`/`:=` feature (see
[`var_counter_increments_across_bars`](../crates/pinewasm_vm/src/tests.rs)),
while same-scope `=` redeclaration remains `E0211`, matching the
declare-versus-reassign distinction. History on compound expressions is
supported through implicit slot materialization: `(a - b)[1]` compiles through
a hidden series slot (see [History references](#history-references) and
[`compound_history_is_na_on_bar_zero`](../crates/pinewasm_vm/src/tests.rs)),
and `E0215` is reserved.

### Integer `ta.median`

`ta.median` of an `int` source returns an `int` and truncates its result
toward zero, in two cases that the result alone cannot tell apart:

- an even window of whole ints, whose median is the midpoint of the two
  middle values (`ta.median` over 1 and 2 is 1, not 1.5);
- an `int` source that holds a fraction (`n / 2 + bar_index` with `n = 7`
  over three bars gives 4, where the same `float` source gives 4.5).

Keeping the fraction in the second case would also turn the first into 1.5.
The truncation is the implemented behaviour and is pinned by tests
(`crates/pinewasm_vm/src/tests/ta_int_forms.rs`); changing it is
a deliberate change of those pins. `ta.mode` and `ta.range` have no such
ambiguity (a mode is one of the window's values, a range a difference of
two) and keep the fraction of a fractional `int` source, as `ta.valuewhen`
and `fixnan` do. Where such a result is written to an integer-only place
(an `int` array, map, matrix or field) the whole part is stored. Cast the source with `float()` to get the float median.

## Fractional `int / int` results

Arithmetic on two `int` operands has a static `int` type, including `/`,
while an uneven quotient keeps its fractional runtime value. PineWASM keeps
the fraction in an `int / int` result. Version-specific behaviour, if ever
added, is a separate design question; the compiler accepts and executes
version 6 source.

| Expression | PineWASM result |
| --- | ---: |
| `int x = 5 / 2; plot(x)` | 2.5 |
| `int x = a; x /= 2; plot(f(5))` inside `f(int a)` | 2.5 |
| `array.get(array.from(10, 20, 30, 40), 5 / 2)` | 30 (index 2) |

Tests in this repository cover integer-division reassignment.

PineWASM gives `int / int` its static `int` type and emits `PineIntDiv`.
The VM stores a whole quotient as an exact `Int` and a fractional quotient as
a `Float`, then carries that fraction through subsequent statically-int
arithmetic and `IntToF64`. The `IntDiv` opcode still truncates, for serialized
programs that use it. Integer-only consumers read the whole part (next
section). The interpreter-hosted WebAssembly tier runs every program
containing `PineIntDiv`. Native typed `i64` lanes cannot represent a
fractional int cell, so the native tier accepts `PineIntDiv` only where the
quotient never has to live in one: a quotient that is widened at once, kept
in a slot that is only widened or plotted, compared directly against an
integer, or read at once by `int()`, `math.floor`, `math.ceil` or
`math.round`. Any other kept quotient is carried as float code, on the chart
tier and in the standalone compile alike; a script on the chart tier that hands such a
value to the interpreter host (text formatting, logging) stays hosted. See
`docs/aot.md`.

In the checked-in PineWASM test programs, the [current census](aot.md#8-native-coverage)
distinguishes native chart modules, host-assisted residuals, and standalone
indicator/strategy exports. Reproduce it with `cargo xtask aot-census`; both
census tests pin the supported fixture boundary.

The semantic and VM tests cover the initializer, reassignment,
chained arithmetic, explicit cast, and array index. The tests also validate
the generated interpreter-hosted WebAssembly module, but do not separately
execute its imported interpreter entry point for these cases. Native and standalone WebAssembly use the guarded float-carry lowering
described above for supported retained quotients. Unsupported consumers still
require a tagged fractional representation or an equivalent lowering with the same guarantees. The exact integer arithmetic
path must stay intact: converting every integer slot to `f64` would lose
precision above 2^53.

### Integer-only consumers

A fractional int that only integer-only operands read is given an explicit
`int(...)` when the program is lowered
(`crates/pinewasm_csil/src/fraction.rs`). `reads_whole_part` in that file is
the positive list of such operands: a collection index, size or count, an
element stored in or looked up in an `int` container, a run-time window or
EMA length, a `math.round` precision, a `math.random` seed, a `str.repeat`
count, a `str.substring` or `str.replace` position, a calendar or
`timestamp` argument, a table cell coordinate, a `ticker.linebreak` line
count, a `ticker.pointfigure` reversal, an `int` field or drawing
coordinate. An operand that is not on the list receives the value as it is:
int arithmetic, `?:`, `nz`, `math.abs`/`min`/`max`/`sign`, comparisons,
`float`, `int`/`math.floor`/`math.ceil`/`math.round`, `na`, `bool`, a
history offset, `str.tostring`, `str.format` and `log.*` use the fraction,
and a consumer nobody has classified is left to do what it did before. A
value that is also read as a number, or stored in a variable, keeps its
fraction there; the cast is placed on the reading that needs a whole number.
The cast is an ordinary charged instruction, and a whole part outside the
int range is `na`.

The `reads_whole_part` list is this compiler's own rule, pinned by the
[whole-part tests](../crates/pinewasm_vm/src/tests/fraction_whole_part.rs).
It describes what PineWASM does. With `n = 7` and `n / 2`:

| Consumer | Without the cast | With the cast |
| --- | --- | --- |
| `array.set`/`push`/`insert`/`unshift`/`fill`/`includes`/`indexof` on `array<int>`, `array.new_int` initial value, `matrix.set` and `matrix.new` initial value on `matrix<int>`, `map.put` int key or value, `matrix.new` dimension, `math.round` precision, `math.random` seed, `table.cell` column or row, `ta.ema` run-time length | runtime error | whole part (3) |
| `array.new_*` size, `timestamp` and calendar arguments | no error, wrong result (size 0, `na`) | whole part |
| `array.get` index, `array.slice` bounds, `str.repeat` count, `str.substring` position, `ta.sma` and `ta.highest` run-time length | whole part, truncated by the consumer | same value |
| an `int` field of a user-defined type (constructor or assignment), an `int` element built by `array.from`, a drawing coordinate (`label.new` x, `label.set_x`, `line.new` x2, `box.new` right) | the fraction was stored and read back (3.5) | whole part (3) |

Rows two and four change the value of a script that would otherwise run
without error: a size of 0 becomes 3, an `na` from `timestamp`, a calendar function
or `time(..., bars_back = n / 2)` becomes the value for the whole part, and
a stored 3.5 becomes 3. The last row is the group `Use::Stored` in
`classify`, switched by `STORED_READS_WHOLE`. A fraction that comes
back out of `ta.valuewhen`, `fixnan`, `ta.mode` or `ta.range` (the int forms
that return a source value) is followed: an integer-only operand reads the
whole part of such a result too. `ta.median` of an `int` source truncates
on its own (see "Integer `ta.median`").

### Parser and function regression checks

Parenthesised expressions may continue on wrapped lines; the test below lists the layouts covered. [`parentheses_wrap_at_any_indent_without_changing_block_scope`](../crates/pinewasm_compiler/tests/block_return_values.rs) checks calls, function parameters, local expressions, and following statements.

Functions called only for effects can end in loops or conditionals containing loops; [`statement_helpers_execute_trailing_loops_and_nested_effects`](../crates/pinewasm_compiler/tests/block_return_values.rs) asserts their state changes and preserves value-producing argument calls. Returning a loop result as a scalar remains unsupported.
