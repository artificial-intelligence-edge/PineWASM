//! The single source of truth for diagnostic codes.
//!
//! Every diagnostic the pipeline emits carries one of these stable codes.
//! Emission sites reference these constants — never raw string literals — so
//! a typo cannot mint a new code, and each code's canonical meaning lives in
//! exactly one place (this file, mirrored by the user-facing table in
//! `docs/language-support.md` § Diagnostics). Messages remain per-site and
//! contextual, like rustc's: the *code* is the stable contract, the wording
//! is not.
//!
//! Tests deliberately assert the raw `"E####"` strings rather than these
//! constants: the string is the wire format hosts see, and asserting it
//! independently means a mistyped constant is caught rather than propagated.
//!
//! Codes are grouped by pipeline stage: `E0001` (compiler input guard),
//! `E0002` (errors left out of a full list), `E01xx` (lexer/parser), `E02xx`
//! (semantics), `E9999` (internal error), and `W0xxx` (warnings).

// -- compiler input guards ---------------------------------------------------

/// Source exceeds an input guard: the size cap (1 MiB), the token budget, or
/// a limit on the library table of a multi-source compile.
pub const E0001: &str = "E0001";

// -- diagnostic list limits --------------------------------------------------

/// More errors than a list of diagnostics holds: the first 1,000 are reported
/// and this one, after them, says that the rest were left out.
pub const E0002: &str = "E0002";

// -- lexer -------------------------------------------------------------------

/// Inconsistent indentation: a dedent that matches no enclosing block.
pub const E0100: &str = "E0100";
/// Unexpected character (e.g. `!` without `=`, a non-ASCII symbol).
pub const E0101: &str = "E0101";
/// Unterminated string literal.
pub const E0102: &str = "E0102";

// -- parser ------------------------------------------------------------------

/// Parse error: unexpected token, malformed/misplaced version directive,
/// unknown escape, trailing tokens, over-deep nesting, out-of-range integer
/// literal, bad assignment target, stray `:`/`?`, and similar.
pub const E0110: &str = "E0110";

// -- semantics: names, calls, types -------------------------------------------

/// Unknown identifier (carries a "did you mean" suggestion when close).
pub const E0201: &str = "E0201";
/// Unknown function.
pub const E0202: &str = "E0202";
/// Called a name that is a value, not a function.
pub const E0203: &str = "E0203";
/// Wrong argument count (also: missing `indicator` title, extra arguments,
/// `timeframe_gaps` without `timeframe`).
pub const E0204: &str = "E0204";
/// Argument type mismatch (also: non-string `indicator` title, and a
/// user-function parameter default that is not of the declared type).
pub const E0205: &str = "E0205";
/// Argument qualifier too strong for the parameter (e.g. series where const
/// is required).
pub const E0206: &str = "E0206";
/// Operand type mismatch for an operator; also mismatched `if`/`switch`
/// expression branch types, or a case label not comparable to the subject.
pub const E0207: &str = "E0207";
/// `if`/`while` condition (or subjectless `switch` case condition) is not
/// `bool`.
pub const E0208: &str = "E0208";
/// History reference target is not a series value.
pub const E0209: &str = "E0209";
/// History offset is not an `int`.
pub const E0210: &str = "E0210";
/// Duplicate declaration in the same scope, or shadowing a built-in variable
/// the script also uses.
pub const E0211: &str = "E0211";
/// Void misuse: assigning a void value, or an effect call inside an
/// expression.
pub const E0212: &str = "E0212";
/// Duplicate `indicator(...)` declaration.
pub const E0213: &str = "E0213";
/// `indicator(...)` not at the top level of the script.
pub const E0214: &str = "E0214";
/// **Retired** — history on compound expressions is now supported via
/// implicit slot materialization. Reserved; never emitted.
pub const E0215: &str = "E0215";
/// Function name used as a value without calling it.
pub const E0216: &str = "E0216";
/// `:=` target is not assignable (undeclared variable, built-in series,
/// function name, an enclosing `for` counter, or inside a function a global,
/// caller or parameter binding).
pub const E0217: &str = "E0217";
/// `:=` type mismatch (value's type is not the variable's, and is not the
/// permitted int → float widening).
pub const E0218: &str = "E0218";
/// Invalid const argument to a stateful builtin (window lengths, offsets,
/// `ta.alma` offset/sigma, percentile percentages, boolean flags, ...).
pub const E0219: &str = "E0219";
/// The `na` literal in a position whose expected type cannot be inferred.
pub const E0220: &str = "E0220";
/// Invalid argument to a pure builtin where E0205/E0219 do not fit (e.g. an
/// `nz` replacement whose type does not match the source's).
pub const E0221: &str = "E0221";
/// `for` loop bound/step error, or a stateful construct inside a loop body.
pub const E0222: &str = "E0222";
/// `break`/`continue` outside of a loop.
pub const E0223: &str = "E0223";
/// User-defined function error (nested definition, duplicate parameter or
/// name, wrong call arity or an omitted parameter without a default,
/// recursion, instantiation-size guard, nesting guard).
pub const E0224: &str = "E0224";
/// `switch` structural error, or a tuple/destructuring error (arity
/// mismatch, tuple in an illegal position, tuple call used as a scalar).
pub const E0225: &str = "E0225";
/// Type-annotated declaration mismatch (initializer incompatible with the
/// annotation; only `float x = <int>` coerces).
pub const E0226: &str = "E0226";
/// A value-producing `if`/`switch` branch is empty or does not end in a
/// value expression.
pub const E0227: &str = "E0227";
/// Named-argument binding error at a call site (positional after named,
/// double binding, unknown parameter name).
pub const E0228: &str = "E0228";
/// Color literal/value error (malformed `#RRGGBB[AA]` hex literal).
pub const E0229: &str = "E0229";
/// `input.*` declaration error (non-constant argument, wrong-typed default,
/// default out of range / absent from options, malformed options, ...).
pub const E0230: &str = "E0230";
/// Reserved for deferred time argument errors. Never emitted.
pub const E0231: &str = "E0231";
/// Reserved for deferred `request.*` argument errors. Never emitted.
pub const E0232: &str = "E0232";
/// History offset exceeds the effective `max_bars_back` ceiling.
pub const E0233: &str = "E0233";
/// Collection type-argument / element-type error: element type
/// mismatch, un-inferable `array.from`, cross-type `concat`/`put_all`,
/// unknown generic element type, nested collections.
pub const E0234: &str = "E0234";
/// Collection method-call error: receiver is not an
/// array/map/matrix, or named arguments on a positional-only collection
/// function.
pub const E0235: &str = "E0235";
/// User-defined type declaration error (including a field default that is
/// not a literal, built-in variable or constant, or enum member).
pub const E0236: &str = "E0236";
/// User-defined type field/method resolution error.
pub const E0237: &str = "E0237";
/// Drawing-API error: unknown drawing member, unknown field,
/// wrong-arity or wrong-receiver drawing call.
pub const E0238: &str = "E0238";
/// Enum error: duplicate enum/member, a built-in type or namespace name, or
/// unknown member reference.
pub const E0239: &str = "E0239";
/// Script-kind conflict: more than one of `indicator(...)`,
/// `strategy(...)`, `library(...)`.
pub const E0240: &str = "E0240";
/// Strategy primitive/getter used without a `strategy(...)` declaration.
pub const E0241: &str = "E0241";
/// `request.security(...)` restriction: non-constant symbol/timeframe,
/// out-of-range timeframe, bad `barmerge.*` flag, lookahead-on with an
/// external symbol.
pub const E0242: &str = "E0242";
/// Library/import/export error: unresolved import, `export` outside a
/// library, a library that exports nothing, private call through an alias,
/// unparsable library source.
pub const E0243: &str = "E0243";
/// `request.security(...)` compound sub-expression uses a construct
/// the secondary context cannot evaluate.
pub const E0244: &str = "E0244";
/// Order-flow footprint accessor error (`footprint.*` / `volume_row.*`):
/// wrong arity, wrong receiver object type, or named arguments where the
/// interface is positional.
pub const E0245: &str = "E0245";
/// `request.*` order-flow interface restriction (`request.footprint`):
/// named arguments, or a symbol other than `syminfo.tickerid` (the chart's
/// own feed).
pub const E0246: &str = "E0246";
/// `ta.pivot_point_levels` pivot-system argument error: not a constant
/// string literal, or not a supported system name.
pub const E0247: &str = "E0247";
/// Numeric builtin argument: a statically-known value outside the
/// function's valid range, e.g. `math.round`'s `precision` outside `[0, 100]`.
/// A runtime value is clamped instead (see `pinewasm_num::clamp_precision`).
pub const E0248: &str = "E0248";
/// `//@version=` value below 6. Earlier versions give some identical-looking
/// code a different meaning (integer division, lazy `and`/`or`, boolean
/// `na`), so compiling them as v6 would silently change results; the script
/// must be migrated first (`pinewasm migrate` handles version 5).
pub const E0249: &str = "E0249";
/// Loop-result expressions parse for v5-to-v6 migration, but the compiler
/// does not yet lower a `for`, `for...in`, or `while` value expression.
pub const E0250: &str = "E0250";
/// A `strategy.risk.*` argument whose value, with the variables it reads
/// substituted by their defining expressions, is larger than the compiler
/// hoists (`pinewasm_mir::MAX_RISK_OPERAND_NODES` nodes or
/// `pinewasm_mir::MAX_RISK_OPERAND_DEPTH` nested levels, and
/// `pinewasm_mir::MAX_RISK_HOISTED_NODES` nodes for the whole script).
pub const E0251: &str = "E0251";
/// A persistent value may carry a later write, and the compiler cannot prove
/// that the value is fixed for a metadata or other weak-qualified consumer.
/// This is a local temporal-fixedness limitation, not a claim that the script is invalid.
pub const E0252: &str = "E0252";
/// A script limit is exceeded: more than 64 plot counts,
/// or more than 1,000 variables in one scope.
pub const E0253: &str = "E0253";
/// Cannot use a plot-family call or `alertcondition()` in local scope, or a
/// plot handle as a ternary operand.
pub const E0254: &str = "E0254";

// -- internal ------------------------------------------------------------------

/// Internal compiler error (a compiler bug, not an error in the script).
pub const E9999: &str = "E9999";

// -- warnings -------------------------------------------------------------------

/// Script has no `indicator(...)` declaration.
pub const W0001: &str = "W0001";
/// Expression result is discarded.
pub const W0002: &str = "W0002";
/// `//@version=` value above 6 (treated as 6).
pub const W0003: &str = "W0003";
/// Series-materializing construct that does not execute on every bar
/// (conditional branch or `var` initializer).
pub const W0004: &str = "W0004";
/// A second `//@version=` directive (ignored; the first one applies).
pub const W0005: &str = "W0005";
/// A variable shadows a built-in variable.
pub const W0006: &str = "W0006";
/// More warnings than a list of diagnostics holds: the first 1,000 are
/// reported and this one, after them, says that the rest were left out.
pub const W0007: &str = "W0007";
/// A stateful call is guarded by a lazily evaluated boolean right-hand side,
/// so some bars can skip the call and its history can drift.
pub const W0008: &str = "W0008";
/// A stateful call appears in a ternary branch; evaluate it before the
/// conditional so that its history is the same on every bar.
pub const W0009: &str = "W0009";
/// A compile-time argument of a stateful call (a window length) is calculated
/// from an input, so the call keeps the value the input defaults give and a
/// selected input does not change it.
pub const W0010: &str = "W0010";
