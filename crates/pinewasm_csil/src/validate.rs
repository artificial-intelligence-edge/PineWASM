//! CSIL validator.
//!
//! Rejects malformed bytecode before it reaches the VM:
//! - header/version checks and structural bounds (constants, slots, jump
//!   targets, source-map length);
//! - instruction counts fit the wire format's zero-based `u32` offset space;
//! - abstract stack interpretation over all execution paths: no underflow,
//!   consistent stack heights at merge points, type-correct operands where
//!   statically knowable, empty stack at `Return`, no falling off the end;
//! - `max_stack` re-computation (declared value must not understate usage).

#![deny(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

use crate::{CsilInstr, CsilProgram, PureFunc, StackEffectError, StackEffectRule};
use pinewasm_hir::{
    input_active_call_argc_bounds, CollectionAbstract, InputActiveExpr, InputKind, InputValue,
    TaOperand, TaParamRule, TaResult, MAX_INPUT_ACTIVE_CALL_ARGUMENTS,
    MAX_INPUT_ACTIVE_EXPRESSION_DEPTH, MAX_INPUT_ACTIVE_EXPRESSION_NODES,
    MAX_INPUT_ACTIVE_STRING_BYTES,
};
use thiserror::Error;

mod binding_state;
mod budget_observer;
#[cfg(test)]
mod budget_scan;
mod budget_state;
#[cfg(test)]
mod declaration_budget_diff_tests;
#[cfg(test)]
mod declaration_scan;
mod declaration_state;
#[path = "validate/function_entry.rs"]
mod function_adapter;
mod function_diagnostics;
mod function_finish;
mod function_operations;
mod function_sequence;
mod function_state;
mod function_structure;
mod global_scan;
mod header_kernel;
#[cfg(test)]
mod input_diagnostic_sink_tests;
mod input_diagnostics;
mod input_domain_reason;
mod input_numeric_bits;
#[cfg(test)]
mod input_request_tests;
mod input_requests;
use input_numeric_bits::numeric_input_value_bits;
pub mod lanes;
mod program_driver;
mod program_prefix;
#[cfg(feature = "aeneas")]
pub(crate) mod scalar_fields;
mod structure_kernel;
mod ta_user_state;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ValidationError {
    #[error("bad header: {0}")]
    Header(String),
    #[error("program must contain at least a `main` function")]
    NoFunctions,
    #[error("ta_states[{index}]: {message}")]
    TaState { index: usize, message: String },
    #[error("inputs[{index}]: {message}")]
    Input { index: usize, message: String },
    #[error("metadata: {0}")]
    Metadata(String),
    #[error("function `{function}`: {message}")]
    Structure { function: String, message: String },
    #[error("function `{function}` at {offset:04}: {message}")]
    AtInstruction {
        function: String,
        offset: u32,
        message: String,
    },
}

/// Abstract operand type. `Int`/`F64`/`Bool`/`Str` all include `na`; `Any`
/// is the join used when paths disagree or the value is statically unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbstractTy {
    Int,
    F64,
    Bool,
    Str,
    /// An RGBA color (`PushColor`, `color.*`). A distinct value type: it never
    /// unifies with the numeric or string types.
    Color,
    /// A collection handle (`Value::Ref`). A distinct value type that never
    /// unifies with a scalar; produced by the collection constructors and
    /// consumed by the collection operations.
    Ref,
    Any,
}

// The tag rules are free functions over an explicit tag comparison (no
// `PartialEq`, no inherent methods): extraction would place an inherent
// method of `AbstractTy` in a namespace where the result type `Bool` would
// resolve to the `AbstractTy::Bool` constructor.

/// Whether two abstract tags are the same tag (`==` on `AbstractTy`).
fn same_abstract_ty(left: AbstractTy, right: AbstractTy) -> bool {
    use AbstractTy::{Any, Bool, Color, Int, Ref, Str, F64};
    matches!(
        (left, right),
        (Int, Int)
            | (F64, F64)
            | (Bool, Bool)
            | (Str, Str)
            | (Color, Color)
            | (Ref, Ref)
            | (Any, Any)
    )
}

/// `actual` satisfies the exact requirement `required`: the same tag, or the
/// wildcard `Any` as the actual value (a required `Any` is not a wildcard).
pub(crate) fn abstract_ty_accepts(actual: AbstractTy, required: AbstractTy) -> bool {
    same_abstract_ty(actual, required) || same_abstract_ty(actual, AbstractTy::Any)
}

/// The join at control-flow merges, `Select` and `nz`: equal tags are kept,
/// every disagreement becomes `Any`.
pub(crate) fn abstract_ty_merge(left: AbstractTy, right: AbstractTy) -> AbstractTy {
    if same_abstract_ty(left, right) {
        left
    } else {
        AbstractTy::Any
    }
}

/// Operand requirement for the abstract stack checks. `Numeric` is used by
/// the polymorphic comparisons and the numeric-polymorphic pure builtins;
/// `NumericOrBool` by `NZ`; `AnyValue` (no constraint) by `NA`.
///
/// Nominally `pub` (the module is private) so the Aeneas extraction roots in
/// `aeneas_roots_csil_validate` can name it.
#[derive(Debug, Clone, Copy)]
pub enum Req {
    Exact(AbstractTy),
    Numeric,
    NumericOrBool,
    NumericOrColor,
    AnyValue,
}

impl Req {
    pub(crate) fn accepts(self, ty: AbstractTy) -> bool {
        match self {
            Req::Exact(required) => abstract_ty_accepts(ty, required),
            Req::Numeric => matches!(ty, AbstractTy::Int | AbstractTy::F64 | AbstractTy::Any),
            Req::NumericOrBool => matches!(
                ty,
                AbstractTy::Int | AbstractTy::F64 | AbstractTy::Bool | AbstractTy::Any
            ),
            Req::NumericOrColor => matches!(
                ty,
                AbstractTy::Int | AbstractTy::F64 | AbstractTy::Color | AbstractTy::Any
            ),
            Req::AnyValue => true,
        }
    }

    fn describe(self) -> String {
        match self {
            Req::Exact(required) => format!("{required:?}"),
            Req::Numeric => "numeric (Int or F64)".to_string(),
            Req::NumericOrBool => "Int, F64, or Bool".to_string(),
            Req::NumericOrColor => "Int, F64, or Color".to_string(),
            Req::AnyValue => "any value".to_string(),
        }
    }
}

/// Convert an in-memory instruction index to the CSIL wire offset carried by
/// diagnostics. [`validate`] rejects a function whose last index does not fit
/// before any validation pass calls [`instruction_offset`].
fn checked_instruction_offset(offset: usize) -> Option<u32> {
    u32::try_from(offset).ok()
}

fn exceeds_offset_space(len: usize) -> bool {
    !structure_kernel::instruction_count_admitted(len)
}

fn instruction_offset(offset: usize) -> u32 {
    checked_instruction_offset(offset)
        .expect("instruction index was preflighted against the u32 wire offset")
}

/// Operand requirements of `CALL_COLLECTION`, **top of stack first**. The
/// checker owns strict element typing; at the CSIL level a `Ref` operand must be
/// a handle, index/size operands must be `Int`, a separator must be `Str`, and
/// an element operand may be any value (already checked). Void never appears
/// as an operand requirement.
fn collection_operand_reqs(func: pinewasm_hir::CollectionFunc, argc: u8) -> Vec<Req> {
    func.operand_reqs(argc)
        .into_iter()
        .map(|cat| match cat {
            CollectionAbstract::Ref => Req::Exact(AbstractTy::Ref),
            CollectionAbstract::Int => Req::Exact(AbstractTy::Int),
            CollectionAbstract::Str => Req::Exact(AbstractTy::Str),
            CollectionAbstract::Elem => Req::AnyValue,
            CollectionAbstract::Bool | CollectionAbstract::Float | CollectionAbstract::Void => {
                Req::AnyValue
            }
        })
        .collect()
}

/// The abstract type a `CALL_COLLECTION` pushes (one value always; void
/// mutators push `na`, represented as `Any`).
pub fn collection_result_ty(func: pinewasm_hir::CollectionFunc) -> AbstractTy {
    match func.result() {
        CollectionAbstract::Ref => AbstractTy::Ref,
        CollectionAbstract::Int => AbstractTy::Int,
        CollectionAbstract::Bool => AbstractTy::Bool,
        CollectionAbstract::Str => AbstractTy::Str,
        CollectionAbstract::Float => AbstractTy::F64,
        // Element results are dynamically typed at the CSIL level (the exact
        // element type is not tracked through a `Ref`), and void mutators push
        // `na`.
        CollectionAbstract::Elem | CollectionAbstract::Void => AbstractTy::Any,
    }
}

/// Operand requirements of an `ORDER` instruction, top of stack first.
///
/// The front end checks the language's richer qualifiers and nominal enum types. CSIL
/// still records enough scalar information to reject a malformed serialized
/// program before the VM interprets a bool as a price or a number as an id.
fn order_operand_reqs(kind: pinewasm_hir::OrderKind) -> Vec<Req> {
    kind.param_names()
        .iter()
        .rev()
        .map(|name| match *name {
            "immediately" | "disable_alert" => Req::Exact(AbstractTy::Bool),
            "direction" | "id" | "from_entry" | "oca_name" | "oca_type" | "comment"
            | "comment_profit" | "comment_loss" | "comment_trailing" | "alert_message"
            | "alert_profit" | "alert_loss" | "alert_trailing" | "type" => {
                Req::Exact(AbstractTy::Str)
            }
            "value" if kind == pinewasm_hir::OrderKind::RiskAllowEntryIn => {
                Req::Exact(AbstractTy::Str)
            }
            "count"
                if matches!(
                    kind,
                    pinewasm_hir::OrderKind::RiskMaxIntradayFilledOrders
                        | pinewasm_hir::OrderKind::RiskMaxConsLossDays
                ) =>
            {
                Req::Exact(AbstractTy::Int)
            }
            _ => Req::Exact(AbstractTy::F64),
        })
        .collect()
}

/// Operand requirements of `CALL_PURE`, **top of stack first** (the last
/// argument is on top). Mirrors the registry's surface rules after the
/// compiler's coercions: float-only functions take `F64` operands,
/// int-preserving ones take any numeric operand.
pub(crate) fn pure_operand_reqs(func: PureFunc, argc: u8) -> Vec<Req> {
    use AbstractTy::{Color, Int, Str, F64};
    let n = argc as usize;
    match func {
        PureFunc::Na => vec![Req::AnyValue],
        // source and replacement; the matching-type rule is relational and
        // checked after the pops (like equality).
        PureFunc::Nz => vec![Req::NumericOrColor; 2],
        PureFunc::MathAbs | PureFunc::MathSign | PureFunc::MathFloor | PureFunc::MathCeil => {
            vec![Req::Numeric]
        }
        PureFunc::MathRound => {
            if n <= 1 {
                vec![Req::Numeric]
            } else {
                // Top of stack is the precision (Int), under it the value.
                vec![Req::Exact(Int), Req::Exact(F64)]
            }
        }
        PureFunc::MathMin | PureFunc::MathMax => vec![Req::Numeric; n],
        PureFunc::MathAvg => vec![Req::Exact(F64); n],
        PureFunc::MathRandom => {
            if n == 3 {
                // Top of stack is the seed (Int), under it max and min.
                vec![Req::Exact(Int), Req::Exact(F64), Req::Exact(F64)]
            } else {
                vec![Req::Exact(F64); n]
            }
        }
        PureFunc::MathPow => vec![Req::Exact(F64); 2],
        PureFunc::MathSqrt
        | PureFunc::MathExp
        | PureFunc::MathLog
        | PureFunc::MathLog10
        | PureFunc::MathSin
        | PureFunc::MathCos
        | PureFunc::MathTan
        | PureFunc::MathAsin
        | PureFunc::MathAcos
        | PureFunc::MathAtan
        | PureFunc::MathToDegrees
        | PureFunc::MathToRadians
        | PureFunc::MathRoundToMintick => vec![Req::Exact(F64)],
        // ---- str.* namespace ---------------------------------------------
        // Requirements are top-of-stack first. `Str` operands are the string
        // arguments; `AnyValue` covers the scalar values tostring/format
        // stringify.
        PureFunc::StrTonumber
        | PureFunc::StrLength
        | PureFunc::StrUpper
        | PureFunc::SyminfoPrefix
        | PureFunc::SyminfoTicker
        | PureFunc::StrLower
        | PureFunc::StrTrim => vec![Req::Exact(Str)],
        // Operands, deepest first: the value (any scalar), then an optional
        // format string on top.
        PureFunc::StrTostring => {
            if n == 2 {
                vec![Req::Exact(Str), Req::AnyValue]
            } else {
                vec![Req::AnyValue]
            }
        }
        // Operands, deepest first: the source string, the begin position, then
        // an optional end position.
        PureFunc::StrSubstring => {
            if n == 3 {
                vec![Req::Exact(Int), Req::Exact(Int), Req::Exact(Str)]
            } else {
                vec![Req::Exact(Int), Req::Exact(Str)]
            }
        }
        // Two string operands.
        PureFunc::StrContains
        | PureFunc::StrStartswith
        | PureFunc::StrEndswith
        | PureFunc::StrPos
        | PureFunc::StrMatch => vec![Req::Exact(Str); 2],
        // Three strings, with an int occurrence on top (materialized by the
        // compiler).
        PureFunc::StrReplace => vec![
            Req::Exact(Int),
            Req::Exact(Str),
            Req::Exact(Str),
            Req::Exact(Str),
        ],
        PureFunc::StrReplaceAll => vec![Req::Exact(Str); 3],
        // Source string, then an int count, with an optional string separator
        // on top.
        PureFunc::StrRepeat => {
            if n == 3 {
                vec![Req::Exact(Str), Req::Exact(Int), Req::Exact(Str)]
            } else {
                vec![Req::Exact(Int), Req::Exact(Str)]
            }
        }
        // An int time (deepest operand), with an optional string format and an
        // optional string timezone above it.
        PureFunc::StrFormatTime => match n {
            3 => vec![Req::Exact(Str), Req::Exact(Str), Req::Exact(Int)],
            2 => vec![Req::Exact(Str), Req::Exact(Int)],
            _ => vec![Req::Exact(Int)],
        },
        // format(fmt, arg0, ...): a string format (deepest operand) plus any
        // scalar substitution args above it.
        PureFunc::StrFormat => {
            let mut reqs = vec![Req::AnyValue; n.saturating_sub(1)];
            reqs.push(Req::Exact(Str));
            reqs
        }
        // ---- color.* namespace -------------------------------------------
        // Requirements are top-of-stack first.
        // Transparency (numeric; a float is accepted and rounded at runtime) on
        // top, color under it.
        PureFunc::ColorNew => vec![Req::Numeric, Req::Exact(Color)],
        // Four numeric channels (int or float; float values are rounded and
        // clamped at runtime), transparency on top.
        PureFunc::ColorRgb => vec![Req::Numeric; 4],
        // top_color and bottom_color (colors) on top, then three floats under
        // them (top, bottom and value positions, coerced by the checker).
        PureFunc::ColorFromGradient => vec![
            Req::Exact(Color),
            Req::Exact(Color),
            Req::Exact(F64),
            Req::Exact(F64),
            Req::Exact(F64),
        ],
        // A single color argument for the r, g, b and t accessors.
        PureFunc::ColorR
        | PureFunc::ColorG
        | PureFunc::ColorB
        | PureFunc::ColorT
        | PureFunc::ColorRFloat
        | PureFunc::ColorGFloat
        | PureFunc::ColorBFloat
        | PureFunc::ColorTFloat => {
            vec![Req::Exact(Color)]
        }
        // ---- strategy trade accessors / conversions ----------------------
        // Accessors take a single int `trade_num`; conversions a single float
        // `value` (int coerced by the checker).
        PureFunc::StrategyClosedTrade(_) | PureFunc::StrategyOpenTrade(_) => {
            vec![Req::Exact(Int)]
        }
        PureFunc::StrategyConvertToAccount
        | PureFunc::StrategyConvertToSymbol
        | PureFunc::StrategyDefaultEntryQty => {
            vec![Req::Exact(F64)]
        }
        // in_seconds takes one string operand; from_seconds takes one int.
        PureFunc::TimeframeInSeconds => vec![Req::Exact(Str)],
        PureFunc::TimeframeFromSeconds => vec![Req::Exact(Int)],
        // Three string operands (timeframe, session, timezone), with the two
        // integer bars-back offsets materialized above them.
        PureFunc::TimeOpen | PureFunc::TimeClose => {
            vec![
                Req::Exact(Int),
                Req::Exact(Int),
                Req::Exact(Str),
                Req::Exact(Str),
                Req::Exact(Str),
            ]
        }
        // Six int operands (the checker materializes the optional-argument
        // defaults). The overload with a leading timezone has the timezone
        // string under the six ints.
        PureFunc::Timestamp => vec![Req::Exact(Int); 6],
        PureFunc::TimestampTz => {
            let mut reqs = vec![Req::Exact(Int); 6];
            reqs.push(Req::Exact(Str));
            reqs
        }
        // ticker.* interface functions: the identifying arguments are strings
        // or numbers, all stringifiable into the canonical ticker-id; the
        // registry signature enforces the precise per-function types, so the
        // structural CSIL check only needs "some value" per operand.
        PureFunc::Ticker(_) => vec![Req::AnyValue; n],
        // type casts: int/float take a numeric operand; bool
        // accepts numeric-or-bool; color takes a color. `string(x)` reuses
        // `StrTostring`, so it is covered above.
        PureFunc::CastInt | PureFunc::CastFloat => vec![Req::Numeric],
        PureFunc::CastBool => vec![Req::NumericOrBool],
        PureFunc::CastColor => vec![Req::Exact(Color)],
        // The calendar accessors share one operand shape: an int time (deepest
        // operand), with an optional string timezone on top.
        PureFunc::WeekOfYear | PureFunc::Calendar(_) => {
            if n == 2 {
                vec![Req::Exact(Str), Req::Exact(Int)]
            } else {
                vec![Req::Exact(Int)]
            }
        }
    }
}

/// Abstract result type of `CALL_PURE`, from the popped operand types
/// (top of stack first). Mirrors the checker's result rules.
pub fn pure_result_ty(func: PureFunc, argc: u8, popped: &[AbstractTy]) -> AbstractTy {
    use AbstractTy::{Bool, Color, Int, Str, F64};
    match func {
        // Constructors push a color. New source emits documented float
        // accessors; the integer accessor opcodes keep the int type.
        PureFunc::ColorNew | PureFunc::ColorRgb | PureFunc::ColorFromGradient => Color,
        PureFunc::ColorR | PureFunc::ColorG | PureFunc::ColorB | PureFunc::ColorT => Int,
        PureFunc::ColorRFloat
        | PureFunc::ColorGFloat
        | PureFunc::ColorBFloat
        | PureFunc::ColorTFloat => F64,
        // str.* results.
        PureFunc::StrTostring
        | PureFunc::StrFormat
        | PureFunc::StrSubstring
        | PureFunc::StrReplace
        | PureFunc::StrReplaceAll
        | PureFunc::StrUpper
        | PureFunc::StrLower
        | PureFunc::StrTrim
        | PureFunc::StrRepeat
        | PureFunc::StrMatch
        | PureFunc::StrFormatTime
        | PureFunc::SyminfoPrefix
        | PureFunc::SyminfoTicker => Str,
        PureFunc::StrTonumber => F64,
        PureFunc::StrLength | PureFunc::StrPos => Int,
        // Trade accessors: result type follows the field (string ids/comments,
        // int bar-indices/times, else float — the `_ => F64` fallback).
        PureFunc::StrategyClosedTrade(field) | PureFunc::StrategyOpenTrade(field) => {
            match field.result_type() {
                pinewasm_hir::Type::String => Str,
                pinewasm_hir::Type::Int => Int,
                _ => F64,
            }
        }
        PureFunc::StrContains | PureFunc::StrStartswith | PureFunc::StrEndswith => Bool,
        PureFunc::TimeframeInSeconds => Int,
        PureFunc::TimeframeFromSeconds => Str,
        PureFunc::TimeOpen | PureFunc::TimeClose => Int,
        // timestamp: UNIX time in ms (int). ticker.*: a ticker-id string.
        PureFunc::Timestamp | PureFunc::TimestampTz => Int,
        PureFunc::Ticker(_) => Str,
        // type casts: the cast names its own result type.
        PureFunc::CastInt => Int,
        PureFunc::CastFloat => F64,
        PureFunc::CastBool => Bool,
        PureFunc::CastColor => Color,
        PureFunc::WeekOfYear | PureFunc::Calendar(_) => Int,
        PureFunc::Na => Bool,
        // The relational check already rejected mismatched known types.
        PureFunc::Nz => abstract_ty_merge(popped[0], popped[1]),
        PureFunc::MathAbs | PureFunc::MathSign => popped[0],
        PureFunc::MathFloor | PureFunc::MathCeil => Int,
        PureFunc::MathRound => {
            if argc == 1 {
                Int
            } else {
                F64
            }
        }
        PureFunc::MathMin | PureFunc::MathMax => min_max_result_ty(popped),
        _ => F64,
    }
}

/// `math.min`/`math.max` result: `Int` when every operand is `Int`, else
/// `F64` when every operand is numeric, else `Any`. An index loop (no
/// iterator closures) so Aeneas can extract it.
fn min_max_result_ty(popped: &[AbstractTy]) -> AbstractTy {
    let mut all_int = true;
    let mut all_numeric = true;
    let mut i = 0;
    while i < popped.len() {
        match popped[i] {
            AbstractTy::Int => {}
            AbstractTy::F64 => all_int = false,
            _ => {
                all_int = false;
                all_numeric = false;
            }
        }
        i += 1;
    }
    if all_int {
        AbstractTy::Int
    } else if all_numeric {
        AbstractTy::F64
    } else {
        AbstractTy::Any
    }
}

pub fn validate(program: &CsilProgram) -> Result<(), Vec<ValidationError>> {
    validate_with_checks(program, &budget_observer::ProductionChecks)
}

// The production stage wiring is shared with the generic extraction boundary.
// Production reference validation still re-enters `validate` with ProductionChecks.
fn validate_with_checks<C: budget_observer::Checks>(
    program: &CsilProgram,
    checks: &C,
) -> Result<(), Vec<ValidationError>> {
    program_driver::run(&ProgramStages { program, checks })
}

#[cfg(feature = "aeneas")]
#[allow(dead_code)]
fn aeneas_native_program_stages<C: budget_observer::Checks>(
    program: &CsilProgram,
    checks: &C,
) -> Result<(), Vec<ValidationError>> {
    validate_with_checks(program, checks)
}

/// Full validation: every earlier stage (the module-private `validate`),
/// then the lane stage ([`lanes::check_lanes`]) when the program declares
/// lanes. This is what the crate exports as `pinewasm_csil::validate`.
///
/// A program that declares nothing gets exactly the earlier stages' verdict.
/// A declaration that fails its check is a validation error. The lane stage
/// is kept out of the inner function because the budget stage calls that
/// function on a copy of the program whose `main` is the pre-optimization
/// reference code, and declarations describe the code that runs
/// (`CsilFunction::code`), not the reference.
pub fn validate_program(program: &CsilProgram) -> Result<(), Vec<ValidationError>> {
    validate_with_lane_facts(program).map(|_| ())
}

/// [`validate_program`] for a program [`crate::lower_mir`] returned in this
/// process and that nothing has changed since: the compiler checking its own
/// output.
///
/// It leaves out the two budget-reference checks that restate how lowering
/// built the reference: that its encoding is under the size limit, and that
/// optimizing it reproduces the function's code. Lowering has just counted
/// that encoding and run that optimization on the same input. Every other
/// check runs, the validation of the reference as code among them.
///
/// On such a program the verdict is [`validate_program`]'s; a debug build
/// asserts it. A program from anywhere else (a cache, a host, code that
/// edited it) has to go through [`validate_program`].
pub fn validate_lowered_program(program: &CsilProgram) -> Result<(), Vec<ValidationError>> {
    let verdict = validate_with_checks(program, &budget_observer::LoweredChecks)
        .and_then(|()| lanes::validate_declared(program).map(|_| ()));
    // Expression position: the macro cannot expand to an item.
    let () = debug_assert_eq!(
        verdict,
        validate_program(program),
        "a lowered program's verdict depends on a budget check lowering had done"
    );
    verdict
}

/// [`validate_program`], returning what the lane checker established when
/// the program declares lanes ([`lanes::LaneFacts`]) and `None` when it
/// declares nothing.
pub fn validate_with_lane_facts(
    program: &CsilProgram,
) -> Result<Option<lanes::LaneFacts>, Vec<ValidationError>> {
    validate(program)?;
    lanes::validate_declared(program)
}

struct ProgramStages<'a, C> {
    program: &'a CsilProgram,
    checks: &'a C,
}
impl<C: budget_observer::Checks> program_driver::Stages<ValidationError> for ProgramStages<'_, C> {
    fn prefix(&self, errors: &mut Vec<ValidationError>) -> program_prefix::Flow {
        program_prefix::check_with_state(
            self.program,
            &program_prefix::PrefixContext {
                program: self.program,
            },
            errors,
        )
    }
    fn ta_states(&self, errors: &mut Vec<ValidationError>) {
        validate_ta_states(self.program, errors);
    }
    fn ta_bindings(&self, errors: &mut Vec<ValidationError>) {
        validate_ta_input_bindings(self.program, errors);
    }
    fn inputs(&self, errors: &mut Vec<ValidationError>) {
        validate_inputs(self.program, errors);
        validate_rendering_bindings(self.program, errors);
    }
    fn ta_users(&self, errors: &mut Vec<ValidationError>) {
        ta_user_state::check(self.program, errors);
    }
    fn functions(&self, errors: &mut Vec<ValidationError>) {
        functions_stage(self.program, errors);
    }
    fn budgets(&self, errors: &mut Vec<ValidationError>) {
        budgets_stage(self.program, self.checks, errors);
    }
}

fn validate_rendering_bindings(program: &CsilProgram, errors: &mut Vec<ValidationError>) {
    validate_rendering_ownership(program, errors);
    for plot in &program.plots {
        if plot
            .style
            .ohlc_body_style
            .as_ref()
            .is_some_and(|body| body.runtime_colorer)
            && !plot
                .color_roles
                .contains(&pinewasm_hir::PlotColorRole::Color)
        {
            errors.push(ValidationError::Metadata(
                "runtime OHLC body colorer requires its declared color role".into(),
            ));
        }
        if let Some(colorers) = plot.style.arrow_runtime_colorers {
            for (enabled, role) in [
                (colorers.up, pinewasm_hir::PlotColorRole::Up),
                (colorers.down, pinewasm_hir::PlotColorRole::Down),
            ] {
                if enabled && !plot.color_roles.contains(&role) {
                    errors.push(ValidationError::Metadata(
                        "runtime arrow colorer requires its declared color role".into(),
                    ));
                }
            }
        }
    }
    for (style, fill, plot_lane) in program
        .plots
        .iter()
        .map(|plot| (&plot.style, false, true))
        .chain(
            program
                .visuals
                .iter()
                .map(|visual| (&visual.style, false, false)),
        )
        .chain(program.fills.iter().map(|fill| (&fill.style, true, false)))
    {
        let mut seen = std::collections::HashSet::new();
        let mut color_sources = Vec::new();
        if !fill && style.fill_color_mode.is_some() {
            errors.push(ValidationError::Metadata(
                "fill color mode belongs only to fill outputs".into(),
            ));
        }
        if style.plot_color_mode.is_some()
            && (!plot_lane || fill || style.kind != pinewasm_hir::DisplayKind::Plot)
        {
            errors.push(ValidationError::Metadata(
                "plot color mode belongs only to ordinary plot outputs".into(),
            ));
        }
        if let Some(body) = &style.ohlc_body_style {
            // Actual container ownership is required independently of the
            // style's claimed family, including old global contract versions.
            if !plot_lane
                || !matches!(
                    style.kind,
                    pinewasm_hir::DisplayKind::PlotCandle | pinewasm_hir::DisplayKind::PlotBar
                )
                || style.group.is_none()
                || style.ohlc_field.is_none()
            {
                errors.push(ValidationError::Metadata(
                    "OHLC body style belongs only to grouped candle/bar lanes".into(),
                ));
            }
            if (!body.static_color_present && body.static_color.is_some())
                || body
                    .static_color
                    .as_ref()
                    .is_some_and(|color| !color.t.is_finite() || !(0.0..=100.0).contains(&color.t))
            {
                errors.push(ValidationError::Metadata(
                    "invalid OHLC body property color or transparency".into(),
                ));
            }
        }
        if let Some(colors) = &style.arrow_static_colors {
            use pinewasm_hir::ArrowStaticColorOrigin as Origin;
            if !plot_lane || fill || style.kind != pinewasm_hir::DisplayKind::PlotArrow {
                errors.push(ValidationError::Metadata(
                    "static arrow colors belong only to plotarrow outputs".into(),
                ));
            }
            for (color, up) in [(&colors.up, true), (&colors.down, false)] {
                let expected_default = (if up { 0 } else { 255 }, if up { 255 } else { 0 }, 0, 0.0);
                let valid = color.t.is_finite()
                    && (0.0..=100.0).contains(&color.t)
                    && match color.origin {
                        Origin::Omitted | Origin::Dynamic => {
                            (color.r, color.g, color.b, color.t) == expected_default
                        }
                        Origin::Missing => (color.r, color.g, color.b, color.t) == (0, 0, 0, 100.0),
                        Origin::Constant => true,
                    };
                if !valid {
                    errors.push(ValidationError::Metadata(
                        "invalid static arrow color provenance or transparency".into(),
                    ));
                }
            }
        }
        if style.arrow_runtime_colorers.is_some()
            && (!plot_lane
                || fill
                || style.kind != pinewasm_hir::DisplayKind::PlotArrow
                || style.arrow_static_colors.is_none())
        {
            errors.push(ValidationError::Metadata(
                "runtime arrow colorer bindings require complete plotarrow static colors".into(),
            ));
        }
        for entry in &style.color_palette {
            let allowed = if fill {
                pinewasm_hir::FILL_COLOR_ROLES
            } else {
                style.kind.color_roles()
            };
            if !allowed.contains(&entry.role)
                || matches!(
                    entry.role,
                    pinewasm_hir::PlotColorRole::TopValue
                        | pinewasm_hir::PlotColorRole::BottomValue
                )
                || color_sources.contains(&entry)
                || entry.resolved_color.is_some()
            {
                errors.push(ValidationError::Metadata(
                    "invalid or duplicate automatic color source role".into(),
                ));
            }
            color_sources.push(entry);
            let valid = match entry.source {
                pinewasm_hir::StyleColorSource::Constant { t, .. } => {
                    t.is_finite() && (0.0..=100.0).contains(&t)
                }
                pinewasm_hir::StyleColorSource::Input { input } => program
                    .inputs
                    .get(input as usize)
                    .is_some_and(|decl| decl.kind == InputKind::Color),
            };
            if !valid {
                errors.push(ValidationError::Metadata(
                    "invalid automatic color source or input identity".into(),
                ));
            }
        }
        for binding in &style.runtime_scalars {
            if binding.field == "ohlc_body_color"
                || !pinewasm_hir::DISPLAY_STYLE_BINDING_FIELDS.contains(&binding.field.as_str())
                || !seen.insert(binding.field.as_str())
                || pinewasm_hir::display_style_binding_qualifier(&binding.field)
                    .is_none_or(|maximum| binding.qualifier > maximum)
                || program
                    .series_slots
                    .get(binding.slot.0 as usize)
                    .is_none_or(|slot| {
                        !slot.name.starts_with("%render") || !slot.persistent || slot.varip
                    })
            {
                errors.push(ValidationError::Metadata(format!(
                    "invalid or duplicate runtime rendering binding `{}`",
                    binding.field
                )));
            }
        }
        for binding in &style.bound_scalars {
            if binding.field == "ohlc_body_color" {
                let input = match &binding.expression {
                    pinewasm_hir::InputActiveExpr::Input { input } => {
                        program.inputs.get(*input as usize)
                    }
                    _ => None,
                };
                let valid_default = match (input, &style.ohlc_body_style) {
                    (Some(input), Some(body))
                        if input.kind == InputKind::Color
                            && body.static_color_present
                            && body.runtime_colorer =>
                    {
                        match (&input.defval, &body.static_color) {
                            (pinewasm_hir::InputValue::Color { r, g, b, .. }, Some(color)) => {
                                (*r, *g, *b) == (color.r, color.g, color.b)
                            }
                            _ => false,
                        }
                    }
                    _ => false,
                };
                if !plot_lane
                    || fill
                    || style.kind != pinewasm_hir::DisplayKind::PlotBar
                    || style.group.is_none()
                    || style.ohlc_field.is_none()
                    || !valid_default
                {
                    errors.push(ValidationError::Metadata(
                        "OHLC body input property requires a grouped bar and exact color input identity".into()));
                }
            }
            if !pinewasm_hir::DISPLAY_STYLE_BINDING_FIELDS.contains(&binding.field.as_str())
                || !seen.insert(binding.field.as_str())
            {
                errors.push(ValidationError::Metadata(format!(
                    "invalid or duplicate rendering binding target `{}`",
                    binding.field
                )));
            }
            let mut nodes = 0;
            let mut dependencies = Vec::new();
            let mut expression_errors = Vec::new();
            validate_active_expression_with_renderer(
                &binding.expression,
                program.inputs.len(),
                1,
                &mut nodes,
                &mut dependencies,
                &mut expression_errors,
                &input_requests::ProductionRenderer,
            );
            for error in expression_errors {
                errors.push(ValidationError::Metadata(format!(
                    "invalid rendering binding `{}`: {error:?}",
                    binding.field
                )));
            }
            if !dependencies.is_empty()
                && pinewasm_hir::display_style_binding_qualifier(&binding.field)
                    == Some(pinewasm_hir::Qualifier::Const)
            {
                errors.push(ValidationError::Metadata(format!(
                    "constant rendering binding `{}` references adjustable inputs",
                    binding.field
                )));
            }
            for input in dependencies {
                if program
                    .inputs
                    .get(input as usize)
                    .is_some_and(|input| matches!(input.kind, InputKind::Source))
                {
                    errors.push(ValidationError::Metadata(format!(
                        "rendering binding `{}` references series-qualified input {input}",
                        binding.field
                    )));
                }
            }
        }
    }
}

/// Whether a program's output ownership invariants (family and slot, unique
/// source order, fill anchors) are enforced. Only unversioned metadata
/// (version 0) is exempt. Every versioned contract keeps them, so
/// raising [`crate::RENDERING_CONTRACT_VERSION`] cannot switch them off; the
/// program prefix separately rejects a version newer than the compiler's.
fn rendering_ownership_is_strict(version: u32) -> bool {
    !crate::rendering_contract_is_unversioned(&version)
}

fn validate_rendering_ownership(program: &CsilProgram, errors: &mut Vec<ValidationError>) {
    use pinewasm_hir::{DisplayKind, OhlcField, PlotColorRole};
    let strict = rendering_ownership_is_strict(program.metadata.rendering_contract_version);
    // A new atomic body record carries its own group invariants even when an
    // older cache has no global rendering-contract version marker. Untouched
    // groups without the extension keep their existing compatibility.
    let body_groups = program
        .plots
        .iter()
        .filter_map(|plot| plot.style.ohlc_body_style.as_ref().and(plot.style.group))
        .collect::<std::collections::HashSet<_>>();
    let roles_valid = |roles: &[PlotColorRole], domain: &[PlotColorRole]| {
        let mut previous = None;
        for role in roles {
            let Some(index) = domain.iter().position(|candidate| candidate == role) else {
                return false;
            };
            if previous.is_some_and(|last| index <= last) {
                return false;
            }
            previous = Some(index);
        }
        true
    };
    let mut groups: std::collections::HashMap<u32, Vec<&crate::CsilPlot>> =
        std::collections::HashMap::new();
    let mut orders = std::collections::HashMap::new();
    for (index, plot) in program.plots.iter().enumerate() {
        if !roles_valid(&plot.color_roles, plot.style.kind.color_roles()) {
            errors.push(ValidationError::Metadata(format!(
                "plot {index} has invalid color role ownership or ordering"
            )));
        }
        if !strict {
            if let Some(group) = plot.style.group.filter(|group| body_groups.contains(group)) {
                groups.entry(group).or_default().push(plot);
            }
            continue;
        }
        if matches!(
            plot.style.kind,
            DisplayKind::Bgcolor | DisplayKind::Barcolor
        ) {
            errors.push(ValidationError::Metadata(format!(
                "plot {index} has invalid slot or output family"
            )));
        }
        if matches!(
            plot.style.kind,
            DisplayKind::PlotCandle | DisplayKind::PlotBar
        ) {
            if let Some(group) = plot.style.group {
                groups.entry(group).or_default().push(plot);
            } else {
                errors.push(ValidationError::Metadata(format!(
                    "plot {index} lacks its OHLC group"
                )));
            }
        } else if plot.style.group.is_some() || plot.style.ohlc_field.is_some() {
            errors.push(ValidationError::Metadata(format!(
                "plot {index} carries unexpected OHLC metadata"
            )));
        }
        if let Some(order) = plot.style.source_order {
            let identity = plot.style.group.filter(|_| {
                matches!(
                    plot.style.kind,
                    DisplayKind::PlotCandle | DisplayKind::PlotBar
                )
            });
            if let Some(previous) = orders.insert(order, (identity, 'p')) {
                if identity.is_none() || previous != (identity, 'p') {
                    errors.push(ValidationError::Metadata(
                        "output source order is reused by different calls".into(),
                    ));
                }
            }
        } else {
            errors.push(ValidationError::Metadata(format!(
                "plot {index} lacks source order"
            )));
        }
    }
    for (group, plots) in groups {
        let expected = [
            Some(OhlcField::Open),
            Some(OhlcField::High),
            Some(OhlcField::Low),
            Some(OhlcField::Close),
        ];
        if plots.len() != 4 || plots.iter().map(|p| p.style.ohlc_field).ne(expected) {
            errors.push(ValidationError::Metadata(format!(
                "OHLC group {group} is incomplete or out of order"
            )));
            continue;
        }
        let mut first = plots[0].style.clone();
        first.ohlc_field = None;
        for plot in &plots[1..] {
            let mut style = plot.style.clone();
            style.ohlc_field = None;
            if style != first {
                errors.push(ValidationError::Metadata(format!(
                    "OHLC group {group} has inconsistent routing or styles"
                )));
                break;
            }
        }
    }
    for (index, visual) in program.visuals.iter().enumerate() {
        if !roles_valid(&visual.color_roles, visual.style.kind.color_roles()) {
            errors.push(ValidationError::Metadata(format!(
                "visual {index} has invalid color roles"
            )));
        }
        if strict
            && (!matches!(
                visual.style.kind,
                DisplayKind::Bgcolor | DisplayKind::Barcolor
            ))
        {
            errors.push(ValidationError::Metadata(format!(
                "visual {index} has invalid family or slot"
            )));
        }
        if strict
            && visual
                .style
                .source_order
                .is_none_or(|order| orders.insert(order, (None, 'v')).is_some())
        {
            errors.push(ValidationError::Metadata(format!(
                "visual {index} has missing or reused source order"
            )));
        }
    }
    for (index, fill) in program.fills.iter().enumerate() {
        let gradient = fill
            .color_roles
            .iter()
            .any(|role| *role != PlotColorRole::Color);
        if !roles_valid(&fill.color_roles, pinewasm_hir::FILL_COLOR_ROLES)
            || (gradient && fill.color_roles != pinewasm_hir::FILL_COLOR_ROLES[1..])
        {
            errors.push(ValidationError::Metadata(format!(
                "fill {index} has invalid solid/gradient color roles"
            )));
        }
        if strict && (fill.style.kind != DisplayKind::Plot) {
            errors.push(ValidationError::Metadata(format!(
                "fill {index} has invalid family or slot"
            )));
        }
        if strict
            && fill
                .style
                .source_order
                .is_none_or(|order| orders.insert(order, (None, 'f')).is_some())
        {
            errors.push(ValidationError::Metadata(format!(
                "fill {index} has missing or reused source order"
            )));
        }
        if strict {
            match (
                program.plots.get(fill.plot1 as usize),
                program.plots.get(fill.plot2 as usize),
            ) {
                (Some(a), Some(b))
                    if a.style.kind == b.style.kind
                        && matches!(a.style.kind, DisplayKind::Plot | DisplayKind::Hline) => {}
                _ => errors.push(ValidationError::Metadata(format!(
                    "fill {index} references incompatible or missing anchors"
                ))),
            }
        }
    }
}

// Production-called free stages keep bounded captures independent of the
// all-stage trait implementation. Their concrete program wiring is unchanged.
fn functions_stage(program: &CsilProgram, errors: &mut Vec<ValidationError>) {
    let function_tables = ProgramTables { program };
    let function_htf = function_operations::ProgramHtfPolicy {
        inputs: &program.inputs,
    };
    function_sequence::all_functions(
        program,
        errors,
        &function_tables,
        &program.ta_states,
        &function_htf,
        &function_operations::ProductionStackNarrowFailure,
        &function_adapter::ProductionRenderer,
    );
}
fn budgets_stage<C: budget_observer::Checks>(
    program: &CsilProgram,
    checks: &C,
    errors: &mut Vec<ValidationError>,
) {
    budget_state::references(program, &BudgetContext { program, checks }, errors);
}

/// Test-only driver that runs the validation stages one after another,
/// stopping early when a stage reports errors.
#[cfg(test)]
fn baseline_validate_driver(program: &CsilProgram) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();

    if program_prefix::check(
        program,
        &mut program_prefix::VecSink {
            program,
            errors: &mut errors,
        },
    ) == program_prefix::Flow::Stop
    {
        return Err(errors);
    }

    let before_ta_states = errors.len();
    validate_ta_states(program, &mut errors);
    validate_ta_input_bindings(program, &mut errors);
    if errors.len() > before_ta_states {
        // The per-function checks look ta_states entries up; do not run
        // them over a malformed table.
        return Err(errors);
    }

    let before_inputs = errors.len();
    validate_inputs(program, &mut errors);
    validate_rendering_bindings(program, &mut errors);
    if errors.len() > before_inputs {
        // The per-function checks look inputs entries up for the READ_INPUT
        // push type; do not run them over a malformed table.
        return Err(errors);
    }

    // Each ta state id belongs to at most one CALL_TA site. The temporary
    // users vector is released inside this helper, before the function stage.
    ta_user_state::check(program, &mut errors);

    let function_tables = ProgramTables { program };
    let function_htf = function_operations::ProgramHtfPolicy {
        inputs: &program.inputs,
    };
    function_sequence::all_functions(
        program,
        &mut errors,
        &function_tables,
        &program.ta_states,
        &function_htf,
        &function_operations::ProductionStackNarrowFailure,
        &function_adapter::ProductionRenderer,
    );

    // A reference is untrusted wire data. Admit it only when it is bounded,
    // independently executable, and reproduces the optimized function under
    // the compiler's deterministic pass sequence.
    budget_state::references(
        program,
        &BudgetContext {
            program,
            checks: &budget_observer::ProductionChecks,
        },
        &mut errors,
    );

    function_finish::finish_errors(errors)
}

struct BudgetContext<'a, C> {
    program: &'a CsilProgram,
    checks: &'a C,
}
impl<C: budget_observer::Checks> budget_state::Reference<Vec<ValidationError>>
    for BudgetContext<'_, C>
{
    fn observe(&self, errors: &mut Vec<ValidationError>, index: usize) {
        budget_observer::observe(
            self.program,
            errors,
            index,
            self.checks,
            &budget_observer::ProductionErrorFactory,
        );
    }
}

#[cfg(test)]
struct BudgetErrors<'a> {
    program: &'a CsilProgram,
    errors: &'a mut Vec<ValidationError>,
}
#[cfg(test)]
impl budget_scan::Reference for BudgetErrors<'_> {
    fn observe(self, index: usize) -> Self {
        let program = self.program;
        let errors = &mut *self.errors;
        let func = &program.functions[index];
        let Some(reference) = &func.budget_reference else {
            return self;
        };
        if index != 0
            || !crate::budget_reference_version_known(reference.version)
            || !crate::budget_reference_within_limit(reference)
            || reference.code.len() != reference.source_map.len()
        {
            errors.push(ValidationError::Structure {
                function: func.name.clone(),
                message: "invalid or oversized budget reference".to_string(),
            });
            return self;
        }
        let mut reference_program = program.clone();
        for function in &mut reference_program.functions {
            function.budget_reference = None;
        }
        let reference_func = &mut reference_program.functions[index];
        reference_func.code.clone_from(&reference.code);
        reference_func.source_map.clone_from(&reference.source_map);
        reference_func.max_stack = reference.max_stack;
        if validate(&reference_program).is_err()
            || !crate::lower::matches_budget_reference(reference, func, &program.series_slots)
        {
            errors.push(ValidationError::Structure {
                function: func.name.clone(),
                message: "budget reference does not validate or reproduce optimized code"
                    .to_string(),
            });
        }

        self
    }
}

struct DeclarationContext<'a> {
    program: &'a CsilProgram,
}
impl declaration_state::TaDeclaration<Vec<ValidationError>> for DeclarationContext<'_> {
    fn observe(&self, errors: &mut Vec<ValidationError>, index: usize) {
        validate_ta_declaration(self.program, index, errors);
    }
}
impl declaration_state::InputDeclaration<Vec<ValidationError>> for DeclarationContext<'_> {
    fn observe(&self, errors: &mut Vec<ValidationError>, index: usize) {
        validate_input_declaration(self.program, index, errors);
    }
}

/// Validate the `ta_states` table itself: engine functions only, and each
/// declaration's const parameters must satisfy the function's data-table
/// rules (window lengths in range, `f64` bit patterns finite, flags 0/1).
fn validate_ta_states(program: &CsilProgram, errors: &mut Vec<ValidationError>) {
    declaration_state::ta_states(program, errors, &DeclarationContext { program });
}
fn validate_ta_declaration(program: &CsilProgram, index: usize, errors: &mut Vec<ValidationError>) {
    let decl = &program.ta_states[index];
    let mut bad = |message: String| errors.push(ValidationError::TaState { index, message });
    if !decl.func.is_engine() {
        bad(format!(
            "`{}` is slot-based, not an engine function",
            decl.func.name()
        ));
        return;
    }
    let rules = decl.func.engine_param_rules();
    if decl.params.len() != rules.len() {
        bad(format!(
            "`{}` takes {} const parameter(s), found {}",
            decl.func.name(),
            rules.len(),
            decl.params.len()
        ));
        return;
    }
    for (i, (rule, value)) in rules.iter().zip(&decl.params).enumerate() {
        match rule {
            TaParamRule::Length { min } => {
                if !(*min..=MAX_WINDOW_LENGTH as i64).contains(value) {
                    bad(format!(
                        "`{}` param {i}: window length {value} out of range \
                         ({min}..={MAX_WINDOW_LENGTH})",
                        decl.func.name()
                    ));
                }
            }
            TaParamRule::IntRange { min, max } => {
                if !(*min..=*max).contains(value) {
                    bad(format!(
                        "`{}` param {i}: {value} out of range ({min}..={max})",
                        decl.func.name()
                    ));
                }
            }
            TaParamRule::FloatBits { positive } => {
                let f = f64::from_bits(u64::from_ne_bytes(value.to_ne_bytes()));
                if !f.is_finite() {
                    bad(format!(
                        "`{}` param {i}: bit pattern does not decode to a finite float",
                        decl.func.name()
                    ));
                } else if *positive && f <= 0.0 {
                    bad(format!(
                        "`{}` param {i}: must be greater than zero, found {f}",
                        decl.func.name()
                    ));
                }
            }
            TaParamRule::Percent => {
                let f = f64::from_bits(u64::from_ne_bytes(value.to_ne_bytes()));
                if !f.is_finite() || !(0.0..=100.0).contains(&f) {
                    bad(format!(
                        "`{}` param {i}: percentage bit pattern must decode to a \
                         finite float in 0..=100",
                        decl.func.name()
                    ));
                }
            }
            TaParamRule::Bool01 => {
                if !(0..=1).contains(value) {
                    bad(format!(
                        "`{}` param {i}: flag must be 0 or 1, found {value}",
                        decl.func.name()
                    ));
                }
            }
        }
    }
}

/// Validate the `inputs` table: ids, value domains, and relationships between
/// defaults and constraints must reconstruct the invariants established by the
/// source compiler. The VM, AOT metadata writer, and abstract interpreter all
/// trust these checks for deserialized programs.
struct InputRequestContext<'a, R> {
    program: &'a CsilProgram,
    renderer: &'a R,
}
impl<R: input_requests::Renderer> declaration_state::InputDeclaration<Vec<ValidationError>>
    for InputRequestContext<'_, R>
{
    fn observe(&self, errors: &mut Vec<ValidationError>, index: usize) {
        validate_input_declaration_with_renderer(self.program, index, errors, self.renderer);
    }
}
fn validate_inputs(program: &CsilProgram, errors: &mut Vec<ValidationError>) {
    validate_inputs_with_renderer(program, errors, &input_requests::ProductionRenderer);
}
fn validate_inputs_with_renderer<R: input_requests::Renderer>(
    program: &CsilProgram,
    errors: &mut Vec<ValidationError>,
    renderer: &R,
) {
    declaration_state::inputs(program, errors, &InputRequestContext { program, renderer });
}
fn validate_input_declaration(
    program: &CsilProgram,
    index: usize,
    errors: &mut Vec<ValidationError>,
) {
    validate_input_declaration_with_renderer(
        program,
        index,
        errors,
        &input_requests::ProductionRenderer,
    );
}

fn validate_input_declaration_with_renderer<R: input_requests::Renderer>(
    program: &CsilProgram,
    index: usize,
    errors: &mut Vec<ValidationError>,
    renderer: &R,
) {
    let input_index = index;
    let decl = &program.inputs[index];
    if decl.id as usize != index {
        let message = input_requests::render(
            renderer,
            input_index,
            input_requests::Request::IdMismatch { declared: decl.id },
        );
        input_diagnostics::emit(errors, input_index, message);
    }
    if !value_matches_kind(&decl.defval, decl.kind) {
        let message = input_requests::render(
            renderer,
            input_index,
            input_requests::Request::DefaultKindMismatch { kind: decl.kind },
        );
        input_diagnostics::emit(errors, input_index, message);
    }
    if let Some(reason) = input_domain_reason::invalid_reason(&decl.defval) {
        let message = input_requests::render(
            renderer,
            input_index,
            input_requests::Request::Domain {
                location: input_requests::ValueLocation::Default,
                reason,
            },
        );
        input_diagnostics::emit(errors, input_index, message);
    }
    for (location, value) in [
        (
            input_requests::ValueLocation::Minimum,
            decl.constraints.min.as_ref(),
        ),
        (
            input_requests::ValueLocation::Maximum,
            decl.constraints.max.as_ref(),
        ),
        (
            input_requests::ValueLocation::Step,
            decl.constraints.step.as_ref(),
        ),
    ] {
        if let Some(v) = value {
            if !value_matches_kind(v, decl.kind) {
                let message = input_requests::render(
                    renderer,
                    input_index,
                    input_requests::Request::ConstraintKindMismatch {
                        location: &location,
                    },
                );
                input_diagnostics::emit(errors, input_index, message);
            }
            if let Some(reason) = input_domain_reason::invalid_reason(v) {
                let message = input_requests::render(
                    renderer,
                    input_index,
                    input_requests::Request::Domain { location, reason },
                );
                input_diagnostics::emit(errors, input_index, message);
            }
        }
    }
    for opt in &decl.constraints.options {
        if !value_matches_kind(opt, decl.kind) {
            let message = input_requests::render(
                renderer,
                input_index,
                input_requests::Request::OptionKindMismatch,
            );
            input_diagnostics::emit(errors, input_index, message);
        }
        if let Some(reason) = input_domain_reason::invalid_reason(opt) {
            let message = input_requests::render(
                renderer,
                input_index,
                input_requests::Request::Domain {
                    location: input_requests::ValueLocation::Option,
                    reason,
                },
            );
            input_diagnostics::emit(errors, input_index, message);
        }
    }
    // min/max/step are numeric-only. Options additionally belong to the
    // string and enum dropdown forms.
    let has_numeric_bounds = decl.constraints.min.is_some()
        || decl.constraints.max.is_some()
        || decl.constraints.step.is_some();
    if !matches!(decl.kind, InputKind::Int | InputKind::Float) && has_numeric_bounds {
        let message = input_requests::render(
            renderer,
            input_index,
            input_requests::Request::NumericConstraintsNotAllowed { kind: decl.kind },
        );
        input_diagnostics::emit(errors, input_index, message);
    }
    if !matches!(
        decl.kind,
        InputKind::Int | InputKind::Float | InputKind::String | InputKind::Enum
    ) && !decl.constraints.options.is_empty()
    {
        let message = input_requests::render(
            renderer,
            input_index,
            input_requests::Request::OptionsNotAllowed { kind: decl.kind },
        );
        input_diagnostics::emit(errors, input_index, message);
    }
    if has_numeric_bounds && !decl.constraints.options.is_empty() {
        let message = input_requests::render(
            renderer,
            input_index,
            input_requests::Request::BoundsOptionsExclusive,
        );
        input_diagnostics::emit(errors, input_index, message);
    }

    if let (Some(default), Some(minimum)) = (
        numeric_input_value_bits(&decl.defval),
        match decl.constraints.min.as_ref() {
            Some(value) => numeric_input_value_bits(value),
            None => None,
        },
    ) {
        let default = f64::from_bits(default);
        let minimum = f64::from_bits(minimum);
        if default < minimum {
            let message = input_requests::render(
                renderer,
                input_index,
                input_requests::Request::DefaultBelowMinimum,
            );
            input_diagnostics::emit(errors, input_index, message);
        }
    }
    if let (Some(default), Some(maximum)) = (
        numeric_input_value_bits(&decl.defval),
        match decl.constraints.max.as_ref() {
            Some(value) => numeric_input_value_bits(value),
            None => None,
        },
    ) {
        let default = f64::from_bits(default);
        let maximum = f64::from_bits(maximum);
        if default > maximum {
            let message = input_requests::render(
                renderer,
                input_index,
                input_requests::Request::DefaultAboveMaximum,
            );
            input_diagnostics::emit(errors, input_index, message);
        }
    }
    if !decl.constraints.options.is_empty()
        && !decl
            .constraints
            .options
            .iter()
            .any(|option| input_values_share_domain_value(&decl.defval, option, decl.kind))
    {
        let message = input_requests::render(
            renderer,
            input_index,
            input_requests::Request::DefaultAbsentFromOptions,
        );
        input_diagnostics::emit(errors, input_index, message);
    }
    if let InputValue::Enum {
        name: enum_name,
        index: default_index,
        ..
    } = &decl.defval
    {
        let mut seen_indexes = Vec::new();
        let mut seen_members: Vec<&str> = Vec::new();
        let mut contains_default = decl.constraints.options.is_empty();
        for option in &decl.constraints.options {
            let InputValue::Enum {
                name,
                index,
                member,
                ..
            } = option
            else {
                continue;
            };
            if name != enum_name {
                let message = input_requests::render(
                    renderer,
                    input_index,
                    input_requests::Request::EnumNameMismatch { name, enum_name },
                );
                input_diagnostics::emit(errors, input_index, message);
            }
            if seen_indexes.contains(index) {
                let message = input_requests::render(
                    renderer,
                    input_index,
                    input_requests::Request::DuplicateEnumIndex { index: *index },
                );
                input_diagnostics::emit(errors, input_index, message);
            }
            seen_indexes.push(*index);
            if !member.is_empty() {
                if seen_members.contains(&member.as_str()) {
                    let message = input_requests::render(
                        renderer,
                        input_index,
                        input_requests::Request::DuplicateEnumMember { member },
                    );
                    input_diagnostics::emit(errors, input_index, message);
                }
                seen_members.push(member);
            }
            contains_default |= name == enum_name && index == default_index;
        }
        if !contains_default {
            let message = input_requests::render(
                renderer,
                input_index,
                input_requests::Request::EnumDefaultAbsent,
            );
            input_diagnostics::emit(errors, input_index, message);
        }
    }
    let mut previous_dependency = None;
    for dependency in &decl.active.dependencies {
        if *dependency as usize >= index {
            let message = input_requests::render(
                renderer,
                input_index,
                input_requests::Request::DependencyNotPrior {
                    dependency: *dependency,
                },
            );
            input_diagnostics::emit(errors, input_index, message);
        }
        if previous_dependency.is_some_and(|previous| previous >= *dependency) {
            let message = input_requests::render(
                renderer,
                input_index,
                input_requests::Request::DependenciesNotSorted,
            );
            input_diagnostics::emit(errors, input_index, message);
        }
        previous_dependency = Some(*dependency);
    }
    if let Some(direct) = decl.active.direct {
        if direct.input as usize >= index {
            let message = input_requests::render(
                renderer,
                input_index,
                input_requests::Request::DirectNotPrior {
                    input: direct.input,
                },
            );
            input_diagnostics::emit(errors, input_index, message);
        } else if program.inputs[direct.input as usize].kind != InputKind::Bool {
            let message = input_requests::render(
                renderer,
                input_index,
                input_requests::Request::DirectNotBool {
                    input: direct.input,
                },
            );
            input_diagnostics::emit(errors, input_index, message);
        }
        if !decl.active.dependencies.contains(&direct.input) {
            let message = input_requests::render(
                renderer,
                input_index,
                input_requests::Request::DirectMissing {
                    input: direct.input,
                },
            );
            input_diagnostics::emit(errors, input_index, message);
        }
    }
    if let Some(expression) = &decl.active.expression {
        let mut expression_dependencies = Vec::new();
        let mut expression_nodes = 0;
        validate_active_expression_with_renderer(
            expression,
            input_index,
            1,
            &mut expression_nodes,
            &mut expression_dependencies,
            errors,
            renderer,
        );
        expression_dependencies.sort_unstable();
        expression_dependencies.dedup();
        if expression_dependencies != decl.active.dependencies {
            let message = input_requests::render(
                renderer,
                input_index,
                input_requests::Request::ExpressionDependenciesMismatch {
                    actual: &expression_dependencies,
                    declared: &decl.active.dependencies,
                },
            );
            input_diagnostics::emit(errors, input_index, message);
        }
    }
}

#[cfg(test)]
fn invalid_input_value_domain(value: &InputValue) -> Option<&'static str> {
    match value {
        InputValue::Float(value) if !value.is_finite() => Some("float must be finite"),
        InputValue::Color { t, .. } if !t.is_finite() || !(0.0..=100.0).contains(t) => {
            Some("color transparency is outside 0..=100")
        }
        _ => None,
    }
}

#[cfg(test)]
fn numeric_input_value(value: &InputValue) -> Option<f64> {
    match value {
        InputValue::Int(value) => Some(*value as f64),
        InputValue::Float(value) if value.is_finite() => Some(*value),
        _ => None,
    }
}

fn input_values_share_domain_value(left: &InputValue, right: &InputValue, kind: InputKind) -> bool {
    match kind {
        InputKind::Float => match (
            numeric_input_value_bits(left),
            numeric_input_value_bits(right),
        ) {
            (Some(left), Some(right)) => f64::from_bits(left) == f64::from_bits(right),
            (None, None) => true,
            _ => false,
        },
        InputKind::Enum => matches!(
            (left, right),
            (
                InputValue::Enum {
                    name: left_name,
                    index: left_index,
                    ..
                },
                InputValue::Enum {
                    name: right_name,
                    index: right_index,
                    ..
                }
            ) if left_name == right_name && left_index == right_index
        ),
        _ => left == right,
    }
}

#[cfg(test)]
fn validate_active_expression(
    expression: &InputActiveExpr,
    input_index: usize,
    depth: usize,
    nodes: &mut usize,
    dependencies: &mut Vec<u32>,
    bad: &mut impl FnMut(String),
) {
    input_diagnostic_sink_tests::copied_validate_active_expression(
        expression,
        input_index,
        depth,
        nodes,
        dependencies,
        bad,
    );
}

#[cfg(test)]
fn validate_active_expression_with_errors(
    expression: &InputActiveExpr,
    input_index: usize,
    depth: usize,
    nodes: &mut usize,
    dependencies: &mut Vec<u32>,
    errors: &mut Vec<ValidationError>,
) {
    validate_active_expression_with_renderer(
        expression,
        input_index,
        depth,
        nodes,
        dependencies,
        errors,
        &input_requests::ProductionRenderer,
    );
}

fn validate_active_expression_with_renderer<R: input_requests::Renderer>(
    expression: &InputActiveExpr,
    input_index: usize,
    depth: usize,
    nodes: &mut usize,
    dependencies: &mut Vec<u32>,
    errors: &mut Vec<ValidationError>,
    renderer: &R,
) {
    *nodes = nodes.saturating_add(1);
    if *nodes > MAX_INPUT_ACTIVE_EXPRESSION_NODES {
        if *nodes == MAX_INPUT_ACTIVE_EXPRESSION_NODES + 1 {
            let message = input_requests::render(
                renderer,
                input_index,
                input_requests::Request::ExpressionNodeLimit,
            );
            input_diagnostics::emit(errors, input_index, message);
        }
        return;
    }
    if depth > MAX_INPUT_ACTIVE_EXPRESSION_DEPTH {
        let message = input_requests::render(
            renderer,
            input_index,
            input_requests::Request::ExpressionDepthLimit,
        );
        input_diagnostics::emit(errors, input_index, message);
        return;
    }
    match expression {
        InputActiveExpr::Value { value } => match value {
            InputValue::Source(_) | InputValue::ExternalSource(_) => {
                let message = input_requests::render(
                    renderer,
                    input_index,
                    input_requests::Request::ExpressionSeriesSource,
                );
                input_diagnostics::emit(errors, input_index, message);
            }
            InputValue::Float(value) if !value.is_finite() => {
                let message = input_requests::render(
                    renderer,
                    input_index,
                    input_requests::Request::ExpressionNonfiniteFloat,
                );
                input_diagnostics::emit(errors, input_index, message);
            }
            InputValue::Color { t, .. } if !t.is_finite() || !(0.0..=100.0).contains(t) => {
                let message = input_requests::render(
                    renderer,
                    input_index,
                    input_requests::Request::ExpressionColorTransparency { t: *t },
                );
                input_diagnostics::emit(errors, input_index, message);
            }
            InputValue::String(value) if value.len() > MAX_INPUT_ACTIVE_STRING_BYTES => {
                let message = input_requests::render(
                    renderer,
                    input_index,
                    input_requests::Request::ExpressionStringLimit,
                );
                input_diagnostics::emit(errors, input_index, message);
            }
            _ => {}
        },
        InputActiveExpr::Na => {}
        InputActiveExpr::Input { input } => {
            if *input as usize >= input_index {
                let message = input_requests::render(
                    renderer,
                    input_index,
                    input_requests::Request::ExpressionInputNotPrior { input: *input },
                );
                input_diagnostics::emit(errors, input_index, message);
            }
            dependencies.push(*input);
        }
        InputActiveExpr::Unary { operand, .. } => {
            validate_active_expression_with_renderer(
                operand,
                input_index,
                depth + 1,
                nodes,
                dependencies,
                errors,
                renderer,
            );
        }
        InputActiveExpr::Binary { lhs, rhs, .. } => {
            validate_active_expression_with_renderer(
                lhs,
                input_index,
                depth + 1,
                nodes,
                dependencies,
                errors,
                renderer,
            );
            validate_active_expression_with_renderer(
                rhs,
                input_index,
                depth + 1,
                nodes,
                dependencies,
                errors,
                renderer,
            );
        }
        InputActiveExpr::Ternary {
            condition,
            when_true,
            when_false,
        } => {
            validate_active_expression_with_renderer(
                condition,
                input_index,
                depth + 1,
                nodes,
                dependencies,
                errors,
                renderer,
            );
            validate_active_expression_with_renderer(
                when_true,
                input_index,
                depth + 1,
                nodes,
                dependencies,
                errors,
                renderer,
            );
            validate_active_expression_with_renderer(
                when_false,
                input_index,
                depth + 1,
                nodes,
                dependencies,
                errors,
                renderer,
            );
        }
        InputActiveExpr::Call { function, args } => {
            if function.is_empty() || function.len() > 64 {
                let message = input_requests::render(
                    renderer,
                    input_index,
                    input_requests::Request::InvalidCallName,
                );
                input_diagnostics::emit(errors, input_index, message);
            } else if let Some((min, max)) = input_active_call_argc_bounds(function) {
                if !(min..=max).contains(&args.len()) {
                    let message = input_requests::render(
                        renderer,
                        input_index,
                        input_requests::Request::CallArity {
                            function,
                            min,
                            max,
                            actual: args.len(),
                        },
                    );
                    input_diagnostics::emit(errors, input_index, message);
                }
            } else {
                let message = input_requests::render(
                    renderer,
                    input_index,
                    input_requests::Request::UnsupportedCall { function },
                );
                input_diagnostics::emit(errors, input_index, message);
            }
            if args.len() > MAX_INPUT_ACTIVE_CALL_ARGUMENTS {
                let message = input_requests::render(
                    renderer,
                    input_index,
                    input_requests::Request::CallArgumentLimit { function },
                );
                input_diagnostics::emit(errors, input_index, message);
            }
            for arg in args {
                validate_active_expression_with_renderer(
                    arg,
                    input_index,
                    depth + 1,
                    nodes,
                    dependencies,
                    errors,
                    renderer,
                );
            }
        }
    }
}

/// Whether an [`InputValue`] is a valid value for an [`InputKind`]. `Int` is
/// accepted for a `Float` kind (an int default coerces to float).
fn value_matches_kind(value: &InputValue, kind: InputKind) -> bool {
    match (value, kind) {
        // `input.price` is a float and `input.time` an int; an int default
        // coerces to either kind.
        (
            InputValue::Int(_),
            InputKind::Int | InputKind::Float | InputKind::Price | InputKind::Time,
        )
        | (InputValue::Float(_), InputKind::Float | InputKind::Price)
        | (InputValue::Bool(_), InputKind::Bool)
        | (InputValue::String(_), InputKind::String)
        | (InputValue::Color { .. }, InputKind::Color)
        | (InputValue::Enum { .. }, InputKind::Enum) => true,
        (InputValue::Source(series), InputKind::Source) => series.is_input_source(),
        _ => false,
    }
}

#[cfg(test)]
fn validate_function(
    program: &CsilProgram,
    func: &crate::CsilFunction,
    errors: &mut Vec<ValidationError>,
) {
    let tables = ProgramTables { program };
    let htf = function_operations::ProgramHtfPolicy {
        inputs: &program.inputs,
    };
    function_adapter::run(
        program,
        func,
        errors,
        &tables,
        &program.ta_states,
        &htf,
        &function_operations::ProductionStackNarrowFailure,
        &function_adapter::ProductionRenderer,
    );
}

/// The production wrapper's branches, with diagnostic rendering delegated to
/// a sink. Kernels and `ProgramTables` remain the actual production operations.
#[cfg(test)]
fn validate_function_with_sink<S: function_diagnostics::Sink>(
    program: &CsilProgram,
    func: &crate::CsilFunction,
    sink: &mut S,
) {
    let tables = ProgramTables { program };
    let htf = function_operations::ProgramHtfPolicy {
        inputs: &program.inputs,
    };
    validate_function_with_operations(
        program,
        func,
        sink,
        &tables,
        &program.ta_states,
        &htf,
        &function_operations::ProductionStackNarrowFailure,
    );
}

// The explicit seen <= u32::MAX guard prevents truncation. Standard try_from
// matching is unsupported by extraction; preserve overflow via the actual adapter.
#[allow(clippy::checked_conversions, clippy::cast_possible_truncation)]
#[track_caller]
fn narrow_preflighted_stack<N: function_operations::StackNarrowFailure>(
    seen: usize,
    failure: &N,
) -> u32 {
    if seen <= u32::MAX as usize {
        seen as u32
    } else {
        failure.on_overflow(seen)
    }
}

/// Production orchestration with explicit operation interfaces. Actual adapters
/// supply this same program's tables/states/inputs; no scalar bypass is used.
#[cfg(test)]
fn validate_function_with_operations<
    S: function_diagnostics::Sink,
    T: TypedTables,
    F: TaStateFacts,
    H: function_operations::HtfPolicy,
    N: function_operations::StackNarrowFailure,
>(
    program: &CsilProgram,
    func: &crate::CsilFunction,
    sink: &mut S,
    tables: &T,
    states: &[F],
    htf: &H,
    narrowing: &N,
) {
    validate_function_with_emitter(
        program,
        func,
        &function_diagnostics::SinkEmitter,
        sink,
        tables,
        states,
        htf,
        narrowing,
    );
}
#[allow(
    clippy::too_many_arguments,
    reason = "Keep explicit state and production operation dictionaries separate for extraction"
)]
fn validate_function_with_emitter<
    S,
    E: function_diagnostics::Emitter<S>,
    T: TypedTables,
    F: TaStateFacts,
    H: function_operations::HtfPolicy,
    N: function_operations::StackNarrowFailure,
>(
    program: &CsilProgram,
    func: &crate::CsilFunction,
    emitter: &E,
    state: &mut S,
    tables: &T,
    states: &[F],
    htf: &H,
    narrowing: &N,
) {
    use function_diagnostics::Issue;
    let code_len = func.code.len();
    if code_len == 0 {
        emitter.emit(state, Issue::EmptyBody);
        return;
    }
    if func.source_map.len() != code_len {
        emitter.emit(
            state,
            Issue::SourceMapLength {
                actual: func.source_map.len(),
                expected: code_len,
            },
        );
    }
    // Structural bounds cover every instruction, reachable or not.
    let structural_ok = function_structure::structural_issues_with_emitter(
        &function_state::StructureContext { emitter },
        &function_structure::RustStructurePrimitives,
        state,
        program,
        func,
    );
    // Keep the cheap ceiling before typed snapshots can allocate quadratically.
    if func.max_stack > MAX_DECLARED_STACK {
        emitter.emit(
            state,
            Issue::DeclaredStackLimit {
                declared: func.max_stack,
            },
        );
        return;
    }
    if !structural_ok {
        return;
    }

    // Preflight heights/cell budget before allocating typed snapshots.
    let preflight = preflight_heights(states, &func.code);
    #[cfg(test)]
    {
        let rendered = match preflight {
            Ok(()) => Ok(()),
            Err(error) => Err(height_error(&func.name, error, program.ta_states.len())),
        };
        height_diff_tests::assert_preflight_matches_baseline(program, func, &func.name, &rendered);
    }
    if let Err(error) = preflight {
        emitter.emit(state, Issue::Preflight { error });
        return;
    }

    let ta_arity = ta_arity_table(states);
    let outcome = typed_stacks(tables, &ta_arity, &func.code);
    let mut typed_issues = Vec::new();
    for &pc in &outcome.htf_visits {
        if let CsilInstr::ReadSecurityExpr { expr, .. } = &func.code[pc] {
            if let Err(message) = htf.validate(expr) {
                typed_issues.push(Issue::Htf {
                    offset: pc,
                    message,
                });
            }
        }
    }
    let typed_result = match outcome.result {
        Ok(seen) => Ok(narrow_preflighted_stack(seen, narrowing)),
        Err(error) => {
            typed_issues.push(Issue::Typed { error });
            Err(())
        }
    };
    #[cfg(test)]
    {
        let typed_errors = typed_issues
            .iter()
            .cloned()
            .map(|issue| function_diagnostics::render(&func.name, program.ta_states.len(), issue))
            .collect::<Vec<_>>();
        typed_diff_tests::assert_typed_matches_baseline(
            program,
            func,
            &func.name,
            &typed_errors,
            typed_result,
        );
    }
    for issue in typed_issues {
        emitter.emit(state, issue);
    }
    let Ok(max_stack_seen) = typed_result else {
        return;
    };
    if max_stack_seen > func.max_stack {
        emitter.emit(
            state,
            Issue::Underdeclared {
                declared: func.max_stack,
                seen: max_stack_seen,
            },
        );
    }
}

// ---------------------------------------------------------------------------
// Typed abstract interpretation (Aeneas extraction kernel `typed_stacks`).
//
// The kernel is written in the Aeneas-extractable subset: index loops instead
// of iterators and closures, explicit tag comparisons instead of derived `==`,
// no string formatting (rejections are the `TypedErrKind` enum; the glue in
// `validate_function` renders them), and the program tables it needs behind
// the `TypedTables` trait. The `HtfExpr` bounds check stays in the glue: the
// kernel only records which `ReadSecurityExpr` instructions it visited.
// ---------------------------------------------------------------------------

/// Why the typed worklist rejected a function (rendered by [`typed_error`]).
#[derive(Debug, Clone, Copy)]
pub enum TypedErrKind {
    /// The instruction's stack effect does not resolve (only
    /// [`HeightErrKind::InvalidTaState`] or [`HeightErrKind::CountExceedsU32`]).
    StackEffect(HeightErrKind),
    /// The instruction pops more operands than the typed stack holds.
    Underflow { pops: u32, height: usize },
    /// Operand `slot` (0 = top of stack) fails its requirement.
    Operand {
        slot: usize,
        req: Req,
        found: AbstractTy,
    },
    /// `CMP_EQ`/`CMP_NE` operands of two different known types.
    EqualityMismatch { left: AbstractTy, right: AbstractTy },
    /// `nz(source, replacement)` operands of two different known types.
    NzMismatch { left: AbstractTy, right: AbstractTy },
    /// `RETURN` leaves operands on the stack.
    NonEmptyReturn { height: usize },
    /// A successor lies past the last instruction.
    FallsOffEnd,
    /// Two paths reach `pc` with different stack heights.
    MergeMismatch { existing: usize, incoming: usize },
}

/// A typed-worklist rejection at instruction `pc` (the merge point for
/// [`TypedErrKind::MergeMismatch`], else the offending instruction).
#[derive(Debug, Clone, Copy)]
pub struct TypedErr {
    pub pc: usize,
    pub kind: TypedErrKind,
}

/// The result of [`typed_stacks`]: the `ReadSecurityExpr` visits (in visit
/// order, one entry per visit) and the verdict, `Ok(h)` with `h` the largest
/// typed stack height after any visited instruction.
pub struct TypedOutcome {
    pub htf_visits: Vec<usize>,
    pub result: Result<usize, TypedErr>,
}

/// What the typed worklist reads from the program tables. The only
/// implementation is [`ProgramTables`]; each method is one table lookup that
/// the typed pass performs.
pub(crate) trait TypedTables {
    fn fill_reqs(&self, slot: u32) -> Vec<Req>;
    /// `CALL_COLLECTION` operand requirements, top of stack first.
    fn collection_reqs(&self, func: pinewasm_hir::CollectionFunc, argc: u8) -> Vec<Req>;
    /// `CALL_COLLECTION` result type.
    fn collection_result(&self, func: pinewasm_hir::CollectionFunc) -> AbstractTy;
    /// `ORDER` operand requirements, top of stack first.
    fn order_reqs(&self, kind: pinewasm_hir::OrderKind) -> Vec<Req>;
    /// `CALL_TA state` operand requirements, top of stack first.
    fn ta_reqs(&self, state: u32) -> Vec<Req>;
    /// `CALL_TA state` result element type (pushed once per result value).
    fn ta_result_ty(&self, state: u32) -> AbstractTy;
    /// `READ_INPUT input` result type.
    fn input_ty(&self, input: u32) -> AbstractTy;
}

/// [`TypedTables`] over a program. Requires the structural pass (in-range
/// `CALL_TA` states and inputs).
pub(crate) struct ProgramTables<'a> {
    pub(crate) program: &'a CsilProgram,
}

impl TypedTables for ProgramTables<'_> {
    fn fill_reqs(&self, slot: u32) -> Vec<Req> {
        self.program.fills[slot as usize]
            .color_roles
            .iter()
            .rev()
            .map(|role| {
                Req::Exact(
                    if matches!(
                        role,
                        pinewasm_hir::PlotColorRole::TopValue
                            | pinewasm_hir::PlotColorRole::BottomValue
                    ) {
                        AbstractTy::F64
                    } else {
                        AbstractTy::Color
                    },
                )
            })
            .collect()
    }
    fn collection_reqs(&self, func: pinewasm_hir::CollectionFunc, argc: u8) -> Vec<Req> {
        collection_operand_reqs(func, argc)
    }
    fn collection_result(&self, func: pinewasm_hir::CollectionFunc) -> AbstractTy {
        collection_result_ty(func)
    }
    fn order_reqs(&self, kind: pinewasm_hir::OrderKind) -> Vec<Req> {
        order_operand_reqs(kind)
    }
    fn ta_reqs(&self, state: u32) -> Vec<Req> {
        // Requirements are listed top-of-stack first: reverse the
        // argument-order operand table (last operand on top).
        self.program.ta_states[state as usize]
            .func
            .engine_operands()
            .iter()
            .rev()
            .map(|op| match op {
                TaOperand::Num => Req::Exact(AbstractTy::F64),
                TaOperand::Cond => Req::Exact(AbstractTy::Bool),
                TaOperand::Bool => Req::Exact(AbstractTy::Bool),
                TaOperand::Color => Req::Exact(AbstractTy::Color),
            })
            .collect()
    }
    fn ta_result_ty(&self, state: u32) -> AbstractTy {
        match self.program.ta_states[state as usize].func.engine_result() {
            TaResult::Float => AbstractTy::F64,
            TaResult::Int => AbstractTy::Int,
            TaResult::Bool => AbstractTy::Bool,
            TaResult::Color => AbstractTy::Color,
        }
    }
    fn input_ty(&self, input: u32) -> AbstractTy {
        match self.program.inputs[input as usize].kind {
            // An enum input reads its member's variant index (an int).
            InputKind::Int | InputKind::Time | InputKind::Enum => AbstractTy::Int,
            // input.source reads a series float sample.
            InputKind::Float | InputKind::Source | InputKind::Price => AbstractTy::F64,
            InputKind::Bool => AbstractTy::Bool,
            InputKind::String => AbstractTy::Str,
            InputKind::Color => AbstractTy::Color,
        }
    }
}

/// The validator's message for a typed-worklist rejection; `state_count` is
/// the length of the TA state table.
fn typed_error(name: &str, error: TypedErr, state_count: usize) -> ValidationError {
    let message = match error.kind {
        TypedErrKind::StackEffect(kind) => {
            let effect_error = match kind {
                HeightErrKind::InvalidTaState { state } => {
                    StackEffectError::InvalidTaState { state, state_count }
                }
                _ => StackEffectError::CountExceedsU32,
            };
            format!("cannot resolve stack effect: {effect_error}")
        }
        TypedErrKind::Underflow { pops, height } => {
            format!("stack underflow: needs {pops} operand(s), stack has {height}")
        }
        TypedErrKind::Operand { slot, req, found } => format!(
            "operand {} must be {}, found {found:?}",
            slot + 1,
            req.describe()
        ),
        TypedErrKind::EqualityMismatch { left, right } => {
            format!("equality operands must have matching types, found {left:?} and {right:?}")
        }
        TypedErrKind::NzMismatch { left, right } => {
            format!("NZ operands must have matching types, found {left:?} and {right:?}")
        }
        TypedErrKind::NonEmptyReturn { height } => {
            format!("operand stack must be empty at RETURN (has {height} values)")
        }
        TypedErrKind::FallsOffEnd => "execution falls off the end of the function".to_string(),
        TypedErrKind::MergeMismatch { existing, incoming } => {
            format!("inconsistent stack heights at merge point ({existing} vs {incoming})")
        }
    };
    ValidationError::AtInstruction {
        function: name.to_string(),
        offset: instruction_offset(error.pc),
        message,
    }
}

/// `n` copies of `req`.
fn repeat_req(req: Req, n: usize) -> Vec<Req> {
    let mut reqs = Vec::new();
    let mut i = 0;
    while i < n {
        reqs.push(req);
        i += 1;
    }
    reqs
}

/// Operand requirements of `instr`, top of stack first. `CALL_PURE` /
/// `CALL_COLLECTION` / `ORDER` / `CALL_TA` requirements depend on the
/// function; the others are fixed or payload-sized. Equality and `nz` are
/// relational and checked on the popped operands ([`typed_pushes`]).
fn typed_reqs<T: TypedTables>(tables: &T, instr: &CsilInstr) -> Vec<Req> {
    use AbstractTy::{Bool, Color, Int, Ref, Str, F64};
    match instr {
        CsilInstr::CallPure { func, argc } => pure_operand_reqs(*func, *argc),
        CsilInstr::CallCollection { func, argc } => tables.collection_reqs(*func, *argc),
        // Arguments are emitted in signature order; the stack is top-first.
        CsilInstr::CallPivotPointLevels { .. } => {
            vec![Req::Exact(Bool), Req::Exact(Bool), Req::Exact(Str)]
        }
        CsilInstr::Order { kind } => tables.order_reqs(*kind),
        // NEW_OBJECT pops `field_count` field values (any type each).
        CsilInstr::NewObject { field_count, .. } => {
            repeat_req(Req::AnyValue, *field_count as usize)
        }
        // COPY_OBJECT / GET_FIELD pop one object handle (a Ref).
        CsilInstr::CopyObject { .. } | CsilInstr::GetField { .. } => vec![Req::Exact(Ref)],
        // FOOTPRINT_ROW_BY_PRICE pops the price (top, numeric) then the
        // footprint handle. The receiver may be `na` at runtime (structured
        // error), so accept any value there and let the VM raise it.
        CsilInstr::FootprintRowByPrice => vec![Req::Numeric, Req::AnyValue],
        // SET_FIELD pops the value (top, any type) then the object handle.
        CsilInstr::SetField { .. } => vec![Req::AnyValue, Req::Exact(Ref)],
        // REGISTER_DRAWING pops one drawing-object handle (a Ref):
        // construction always yields a handle, so `na` here is invalid.
        CsilInstr::RegisterDrawing { .. } => vec![Req::Exact(Ref)],
        // DELETE_DRAWING pops one handle; the receiver may be `na` at
        // runtime (a no-op in the VM), so accept any value here.
        CsilInstr::DeleteDrawing { .. } => vec![Req::AnyValue],
        // SET_CELL pops (top-first): each attribute value (any type), then
        // row (int), column (int), then the table handle. The receiver may
        // be `na` at runtime (structured error), so accept any value there.
        CsilInstr::SetCell { attrs } => {
            let mut reqs = repeat_req(Req::AnyValue, attrs.len());
            reqs.push(Req::Numeric);
            reqs.push(Req::Numeric);
            reqs.push(Req::AnyValue);
            reqs
        }
        // CLEAR_CELLS / MERGE_CELLS pop (top-first) end_row, end_column,
        // start_row, start_column (all int), then the table handle.
        CsilInstr::ClearCells | CsilInstr::MergeCells => vec![
            Req::Numeric,
            Req::Numeric,
            Req::Numeric,
            Req::Numeric,
            Req::AnyValue,
        ],
        // LOG pops the format string plus its substitution args (any scalar
        // each; the format string, deepest, is guaranteed Str by the
        // checker). Requirements are top-first — all `AnyValue`.
        CsilInstr::Log { argc, .. } => repeat_req(Req::AnyValue, *argc as usize),
        // ALERT pops freq (top) then message (both strings; freq may be na).
        CsilInstr::Alert => vec![Req::Exact(Str), Req::Exact(Str)],
        // RUNTIME_ERROR pops one string message (may be na).
        CsilInstr::RuntimeError => vec![Req::Exact(Str)],
        // ALERT_CONDITION pops message (top), title, then a bool condition.
        CsilInstr::AlertCondition => vec![Req::Exact(Str), Req::Exact(Str), Req::Exact(Bool)],
        CsilInstr::CallTa { state } => tables.ta_reqs(*state),
        CsilInstr::PlotStyledF64 { color_count, .. } => {
            let mut reqs = repeat_req(Req::Exact(Color), *color_count as usize);
            reqs.push(Req::Exact(F64));
            reqs
        }
        CsilInstr::FillColor { fill_slot, .. } => tables.fill_reqs(*fill_slot),
        CsilInstr::VisualColor { color_count, .. } => {
            repeat_req(Req::Exact(Color), *color_count as usize)
        }
        CsilInstr::F64Add
        | CsilInstr::F64Sub
        | CsilInstr::F64Mul
        | CsilInstr::F64Div
        | CsilInstr::F64Rem => vec![Req::Exact(F64), Req::Exact(F64)],
        CsilInstr::IntAdd
        | CsilInstr::IntSub
        | CsilInstr::IntMul
        | CsilInstr::IntDiv
        | CsilInstr::PineIntDiv
        | CsilInstr::IntRem => vec![Req::Exact(Int), Req::Exact(Int)],
        // Comparisons are numeric-polymorphic (the VM promotes a mixed
        // int/float pair, though the type checker never emits one).
        CsilInstr::CmpGt | CsilInstr::CmpGe | CsilInstr::CmpLt | CsilInstr::CmpLe => {
            vec![Req::Numeric, Req::Numeric]
        }
        CsilInstr::F64Neg | CsilInstr::PlotF64 { .. } => vec![Req::Exact(F64)],
        // Numeric offsets are accepted and fractional values are truncated at
        // runtime; dynamic numeric windows likewise accept numeric lengths.
        CsilInstr::ReadBuiltinDynamic { .. }
        | CsilInstr::ReadUserSlotDynamic { .. }
        | CsilInstr::WindowMeanDynamic { .. }
        | CsilInstr::WindowExtremumDynamic { .. } => vec![Req::Numeric],
        CsilInstr::WindowStatisticDynamic { .. } => vec![Req::Exact(Int)],
        CsilInstr::EmaSimpleDynamic { .. } => vec![Req::Exact(Int), Req::Exact(F64)],
        CsilInstr::IntNeg | CsilInstr::IntToF64 => vec![Req::Exact(Int)],
        CsilInstr::BoolAnd | CsilInstr::BoolOr => vec![Req::Exact(Bool), Req::Exact(Bool)],
        CsilInstr::StrConcat => vec![Req::Exact(Str), Req::Exact(Str)],
        CsilInstr::BoolNot | CsilInstr::JumpIfFalse { .. } | CsilInstr::JumpIfExact { .. } => {
            vec![Req::Exact(Bool)]
        }
        // SELECT pops rhs, lhs (any value types), then a bool condition.
        CsilInstr::Select => vec![Req::AnyValue, Req::AnyValue, Req::Exact(Bool)],
        // READ_DRAWINGS_ALL takes no operand; equality accepts any matching
        // operand pair (checked after the pops; the VM re-checks
        // dynamically).
        _ => Vec::new(),
    }
}

/// Whether `left` and `right` may be compared for equality: the same tag, or
/// either one statically unknown.
fn equality_compatible(left: AbstractTy, right: AbstractTy) -> bool {
    same_abstract_ty(left, right)
        || same_abstract_ty(left, AbstractTy::Any)
        || same_abstract_ty(right, AbstractTy::Any)
}

/// The static type of a builtin series read.
fn builtin_series_ty(source: pinewasm_hir::BuiltinSeries) -> AbstractTy {
    if source.is_bool() {
        AbstractTy::Bool
    } else if source.is_int() {
        AbstractTy::Int
    } else if source.is_string() {
        AbstractTy::Str
    } else {
        AbstractTy::F64
    }
}

/// The relational operand checks and the pushed types of `instr`, in push
/// order, from the popped operands (`popped[0]` is the old top of stack).
/// `pushes` is the resolved push count (`CALL_TA` pushes that many copies of
/// its result type).
fn typed_pushes<T: TypedTables>(
    tables: &T,
    instr: &CsilInstr,
    popped: &[AbstractTy],
    pushes: u32,
) -> Result<Vec<AbstractTy>, TypedErrKind> {
    use AbstractTy::{Any, Bool, Color, Int, Ref, Str, F64};
    match instr {
        CsilInstr::CmpEq | CsilInstr::CmpNe => {
            let (left, right) = (popped[0], popped[1]);
            if !equality_compatible(left, right) {
                return Err(TypedErrKind::EqualityMismatch { left, right });
            }
        }
        // NZ's source/replacement pair is relational, like equality: the
        // replacement must have the source's type where statically known.
        CsilInstr::CallPure {
            func: PureFunc::Nz, ..
        } => {
            let (left, right) = (popped[0], popped[1]);
            if !equality_compatible(left, right) {
                return Err(TypedErrKind::NzMismatch { left, right });
            }
        }
        _ => {}
    }
    let pushed = match instr {
        CsilInstr::PushNa => vec![Any],
        CsilInstr::PushBool(_) => vec![Bool],
        CsilInstr::PushI64(_) => vec![Int],
        CsilInstr::PushF64(_) => vec![F64],
        CsilInstr::PushStr(_) => vec![Str],
        CsilInstr::PushColor { .. } => vec![Color],
        CsilInstr::Dup => vec![popped[0], popped[0]],
        // `bar_index` and the epoch/bar-index time builtins are int series;
        // the `barstate.*` predicates are bool series; the OHLCV series and
        // derived averages are floats. Dynamic-offset reads mirror the
        // literal forms' result type (an out-of-range or `na` offset yields
        // `na` at run time, still that type slot).
        CsilInstr::ReadBuiltin { source, .. } | CsilInstr::ReadBuiltinDynamic { source } => {
            vec![builtin_series_ty(*source)]
        }
        // Direct `request.security` sources and host-backed `request.*` reads
        // are floats. Compound security expressions preserve a boolean root
        // result when their HIR tree carries the explicit result marker.
        CsilInstr::ReadSecurity { .. } | CsilInstr::ReadRequest { .. } => vec![F64],
        CsilInstr::ReadSecurityExpr { expr, .. } if expr.result_is_bool() => vec![Bool],
        CsilInstr::ReadSecurityExpr { .. } => vec![F64],
        // `request.footprint(...)`, `request.security_lower_tf(...)` and
        // `ta.pivot_point_levels(...)` build and push a handle (a Ref).
        CsilInstr::BuildFootprint { .. }
        | CsilInstr::ReadSecurityLowerTf { .. }
        | CsilInstr::ReadPivotLevels { .. }
        | CsilInstr::CallPivotPointLevels { .. } => vec![Ref],
        // `time(timeframe)` / `time_close(timeframe)` push the enclosing
        // bucket's open/close time — a `series int` (epoch ms).
        CsilInstr::ReadTimeTf { .. } => vec![Int],
        // The matching row handle, or `na` when none matches — so `Any`.
        CsilInstr::FootprintRowByPrice => vec![Any],
        // User slots are dynamically typed storage.
        CsilInstr::ReadUserSlot { .. } | CsilInstr::ReadUserSlotDynamic { .. } => vec![Any],
        // An input read pushes the declaration's kind type.
        CsilInstr::ReadInput { input } => vec![tables.input_ty(*input)],
        CsilInstr::IntAdd
        | CsilInstr::IntSub
        | CsilInstr::IntMul
        | CsilInstr::IntDiv
        | CsilInstr::PineIntDiv
        | CsilInstr::IntRem
        | CsilInstr::IntNeg => vec![Int],
        CsilInstr::F64Add
        | CsilInstr::F64Sub
        | CsilInstr::F64Mul
        | CsilInstr::F64Div
        | CsilInstr::F64Rem
        | CsilInstr::F64Neg
        | CsilInstr::IntToF64
        // A window mean is a float or na.
        | CsilInstr::WindowMean { .. }
        | CsilInstr::WindowMeanDynamic { .. }
        | CsilInstr::WindowExtremumDynamic { .. }
        | CsilInstr::EmaSimpleDynamic { .. } => vec![F64],
        CsilInstr::WindowStatisticDynamic { func, .. } => vec![match func.engine_result() {
            pinewasm_hir::TaResult::Int => Int,
            _ => F64,
        }],
        CsilInstr::CallPure { func, argc } => vec![pure_result_ty(*func, *argc, popped)],
        CsilInstr::CallCollection { func, .. } => vec![tables.collection_result(*func)],
        // NEW_OBJECT / COPY_OBJECT push an object handle (a Ref); GET_FIELD
        // pushes the field value (any type — the checker owns field typing);
        // SET_FIELD pushes nothing. REGISTER_DRAWING pushes the same object
        // handle back; DELETE pushes nothing.
        CsilInstr::NewObject { .. }
        | CsilInstr::CopyObject { .. }
        | CsilInstr::RegisterDrawing { .. }
        | CsilInstr::ReadDrawingsAll { .. } => vec![Ref],
        CsilInstr::GetField { .. } => vec![Any],
        // One value per tuple element (scalar functions push one; tuple
        // functions push their whole tuple, all of the same element type).
        CsilInstr::CallTa { state } => {
            let ty = tables.ta_result_ty(*state);
            let mut values = Vec::new();
            let mut i = 0;
            while i < pushes {
                values.push(ty);
                i += 1;
            }
            values
        }
        // The result is one of the two branch values: merge their abstract
        // types (popped[0] is rhs, popped[1] is lhs).
        CsilInstr::Select => vec![abstract_ty_merge(popped[0], popped[1])],
        CsilInstr::CmpGt
        | CsilInstr::CmpGe
        | CsilInstr::CmpLt
        | CsilInstr::CmpLe
        | CsilInstr::CmpEq
        | CsilInstr::CmpNe
        | CsilInstr::BoolAnd
        | CsilInstr::BoolOr
        | CsilInstr::BoolNot => vec![Bool],
        CsilInstr::StrConcat => vec![Str],
        _ => Vec::new(),
    };
    Ok(pushed)
}

/// One typed transfer: resolve the stack effect, check the operand
/// requirements against the top of `stack` (top first), pop the operands,
/// check the relational rules and push the results. Every rejection is in the
/// order the checks are listed.
pub(crate) fn typed_step<T: TypedTables>(
    tables: &T,
    ta_arity: &[(u32, u32)],
    instr: &CsilInstr,
    mut stack: Vec<AbstractTy>,
) -> Result<Vec<AbstractTy>, TypedErrKind> {
    let (pops, pushes) = match height_effect(instr, ta_arity) {
        Ok(effect) => effect,
        Err(kind) => return Err(TypedErrKind::StackEffect(kind)),
    };
    let pops_usize = pops as usize;
    if stack.len() < pops_usize {
        return Err(TypedErrKind::Underflow {
            pops,
            height: stack.len(),
        });
    }
    let reqs = typed_reqs(tables, instr);
    // The first operand (top first) that fails its requirement, if any. The
    // loop has no early exit (Aeneas); the rejection is reported after it.
    let mut slot = 0;
    while slot < reqs.len() && reqs[slot].accepts(stack[stack.len() - 1 - slot]) {
        slot += 1;
    }
    if slot < reqs.len() {
        return Err(TypedErrKind::Operand {
            slot,
            req: reqs[slot],
            found: stack[stack.len() - 1 - slot],
        });
    }
    // Pop, remembering the operand types (top of stack first).
    let mut popped = Vec::new();
    let mut k = 0;
    while k < pops_usize {
        match stack.pop() {
            Some(ty) => popped.push(ty),
            None => unreachable!(),
        }
        k += 1;
    }
    let pushed = typed_pushes(tables, instr, &popped, pushes)?;
    let mut i = 0;
    while i < pushed.len() {
        stack.push(pushed[i]);
        i += 1;
    }
    Ok(stack)
}

fn is_read_security_expr(instr: &CsilInstr) -> bool {
    matches!(instr, CsilInstr::ReadSecurityExpr { .. })
}

/// The pointwise join of two equal-height typed stacks.
fn merge_stacks(existing: &[AbstractTy], incoming: &[AbstractTy]) -> Vec<AbstractTy> {
    let mut merged = Vec::new();
    let mut i = 0;
    while i < existing.len() {
        merged.push(abstract_ty_merge(existing[i], incoming[i]));
        i += 1;
    }
    merged
}

/// Whether two equal-height typed stacks carry the same tags.
fn same_stack(left: &[AbstractTy], right: &[AbstractTy]) -> bool {
    let mut i = 0;
    while i < left.len() && same_abstract_ty(left[i], right[i]) {
        i += 1;
    }
    i == left.len()
}

/// Records `stack` at `successor` (queuing it on first visit), or joins it
/// into the recorded stack and re-queues `successor` when the join changes it.
fn visit_typed_successor(
    stacks: &mut [Option<Vec<AbstractTy>>],
    worklist: &mut Vec<usize>,
    pc: usize,
    successor: usize,
    stack: &[AbstractTy],
) -> Result<(), TypedErr> {
    if successor >= stacks.len() {
        return Err(TypedErr {
            pc,
            kind: TypedErrKind::FallsOffEnd,
        });
    }
    // Copied out before matching (Aeneas cannot expand an enum matched in
    // place behind a slice index).
    let recorded: Option<Vec<AbstractTy>> = stacks[successor].clone();
    match recorded {
        None => {
            stacks[successor] = Some(stack.to_vec());
            worklist.push(successor);
            Ok(())
        }
        Some(existing) => {
            if existing.len() != stack.len() {
                return Err(TypedErr {
                    pc: successor,
                    kind: TypedErrKind::MergeMismatch {
                        existing: existing.len(),
                        incoming: stack.len(),
                    },
                });
            }
            let merged = merge_stacks(&existing, stack);
            if !same_stack(&merged, &existing) {
                stacks[successor] = Some(merged);
                worklist.push(successor);
            }
            Ok(())
        }
    }
}

/// One worklist visit of [`typed_stacks`]: the typed transfer of `code[pc]`
/// from its recorded stack, then its successors (target first, as
/// [`height_successors`] lists them). Returns the stack height after the
/// transfer.
fn typed_visit<T: TypedTables>(
    tables: &T,
    ta_arity: &[(u32, u32)],
    code: &[CsilInstr],
    stacks: &mut [Option<Vec<AbstractTy>>],
    worklist: &mut Vec<usize>,
    htf_visits: &mut Vec<usize>,
    pc: usize,
) -> Result<usize, TypedErr> {
    // Worklist entries always have a recorded stack.
    let recorded: Option<Vec<AbstractTy>> = stacks[pc].clone();
    let Some(stack) = recorded else {
        unreachable!()
    };
    let instr = &code[pc];
    let stack = match typed_step(tables, ta_arity, instr, stack) {
        Ok(stack) => stack,
        Err(kind) => return Err(TypedErr { pc, kind }),
    };
    if is_read_security_expr(instr) {
        htf_visits.push(pc);
    }
    let height = stack.len();
    if is_return(instr) {
        if height != 0 {
            return Err(TypedErr {
                pc,
                kind: TypedErrKind::NonEmptyReturn { height },
            });
        }
        return Ok(height);
    }
    // The conditional jumps merge their two successor paths (target first);
    // JumpIfSlotWritten branches on VM slot state, not the stack. The same
    // successor function as the height preflight.
    let (first, second) = height_successors(instr, pc);
    visit_typed_successor(stacks, worklist, pc, first, &stack)?;
    if let Some(fallthrough) = second {
        visit_typed_successor(stacks, worklist, pc, fallthrough, &stack)?;
    }
    Ok(height)
}

/// Typed abstract interpretation of one function (Aeneas extraction root): a
/// depth-first worklist from `pc = 0` with the empty stack. Each visit
/// re-runs the typed transfer of `code[pc]` on the stack recorded for `pc`
/// **as it is at that visit** (the join of the predecessor stacks that have
/// arrived so far) and stops at the first rejection. A later predecessor that
/// widens the join re-queues `pc`, but an earlier rejection is final: the
/// verdict depends on the visit order (`validate_schedule_sensitive_extracted`
/// in the Aeneas package).
///
/// Precondition (panics otherwise): `code` is non-empty. `ta_arity` is
/// [`ta_arity_table`] of the state table `tables` reads.
pub(crate) fn typed_stacks<T: TypedTables>(
    tables: &T,
    ta_arity: &[(u32, u32)],
    code: &[CsilInstr],
) -> TypedOutcome {
    let mut htf_visits = Vec::new();
    let result = typed_worklist(tables, ta_arity, code, &mut htf_visits);
    TypedOutcome { htf_visits, result }
}

/// The worklist of [`typed_stacks`]: `Ok(h)` with `h` the largest typed stack
/// height after any visit, or the first rejection in visit order.
fn typed_worklist<T: TypedTables>(
    tables: &T,
    ta_arity: &[(u32, u32)],
    code: &[CsilInstr],
    htf_visits: &mut Vec<usize>,
) -> Result<usize, TypedErr> {
    let mut stacks: Vec<Option<Vec<AbstractTy>>> = Vec::new();
    let mut i = 0;
    while i < code.len() {
        stacks.push(None);
        i += 1;
    }
    stacks[0] = Some(Vec::new());
    let mut worklist = Vec::new();
    worklist.push(0usize);
    let mut max_seen = 0usize;
    while let Some(pc) = worklist.pop() {
        let height = typed_visit(
            tables,
            ta_arity,
            code,
            &mut stacks,
            &mut worklist,
            htf_visits,
            pc,
        )?;
        if height > max_seen {
            max_seen = height;
        }
    }
    Ok(max_seen)
}

/// Why the stack-height preflight rejected a function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeightErrKind {
    /// The instruction pops more operands than the stack holds.
    Underflow { pops: u32, height: u32 },
    /// The height after pushing does not fit `u32`.
    Overflow,
    /// The height after the instruction exceeds [`MAX_DECLARED_STACK`].
    ExceedsLimit,
    /// `RETURN` leaves operands on the stack.
    NonEmptyReturn { height: u32 },
    /// A successor lies past the last instruction.
    FallsOffEnd,
    /// The summed first-visit heights exceed
    /// [`MAX_VALIDATION_ABSTRACT_CELLS`]. Reported without an offset.
    ComplexityLimit,
    /// Two paths reach `pc` with different heights.
    MergeMismatch { existing: u32, incoming: u32 },
    /// `CALL_TA state` names no entry of the TA state table
    /// ([`StackEffectError::InvalidTaState`]).
    InvalidTaState { state: u32 },
    /// A payload operand or result count does not fit `u32`
    /// ([`StackEffectError::CountExceedsU32`]).
    CountExceedsU32,
}

/// A stack-height preflight rejection at instruction `pc` (the merge point
/// for [`HeightErrKind::MergeMismatch`], else the offending instruction).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeightErr {
    pub pc: usize,
    pub kind: HeightErrKind,
}

/// Runs the pure preflight kernel on one function and renders a rejection
/// exactly as the validator reports it. Requires the structural pass to have
/// succeeded (non-empty code, in-range jump targets).
#[cfg(test)]
fn preflight_stack_heights(
    program: &CsilProgram,
    func: &crate::CsilFunction,
    name: &str,
) -> Result<(), ValidationError> {
    let result = match preflight_heights(&program.ta_states, &func.code) {
        Ok(()) => Ok(()),
        Err(error) => Err(height_error(name, error, program.ta_states.len())),
    };
    #[cfg(test)]
    height_diff_tests::assert_preflight_matches_baseline(program, func, name, &result);
    result
}

/// Tests that check the preflight and TA-input kernels against test-only
/// Rust implementations of the same rules.
#[cfg(test)]
mod height_diff_tests;
#[cfg(test)]
mod typed_diff_tests;

/// What the validator glue reads from one `ta_states` entry. The glue below
/// is generic over it so that it extracts (Aeneas) without the program
/// tables; the only implementation is [`pinewasm_hir::TaStateDecl`], whose
/// methods read the engine's operand and result metadata and owned rules.
pub(crate) trait TaStateFacts {
    /// `func.engine_operands().len()`: the operands `CALL_TA` pops.
    fn operand_count(&self) -> usize;
    /// `func.engine_result_arity()`: the values `CALL_TA` pushes.
    fn result_arity(&self) -> u32;
    /// `func.engine_param_rule(param)`: one owned engine parameter rule.
    fn param_rule(&self, param: u32) -> Option<TaParamRule>;
}

impl TaStateFacts for pinewasm_hir::TaStateDecl {
    fn operand_count(&self) -> usize {
        self.func.engine_operands().len()
    }
    fn result_arity(&self) -> u32 {
        self.func.engine_result_arity()
    }
    fn param_rule(&self, param: u32) -> Option<TaParamRule> {
        self.func.engine_param_rule(param)
    }
}

/// What the validator glue reads from one `inputs` entry: its default when
/// that default is an integer.
pub(crate) trait InputDefaultFacts {
    /// `Some(v)` iff `defval` is `InputValue::Int(v)`.
    fn default_int(&self) -> Option<i64>;
}

impl InputDefaultFacts for pinewasm_hir::InputDecl {
    fn default_int(&self) -> Option<i64> {
        match self.defval {
            InputValue::Int(value) => Some(value),
            _ => None,
        }
    }
}

/// The `CALL_TA` arity table the preflight consults: entry `k` is
/// `(operand_count, result_arity)` of `ta_states[k]` (Aeneas extraction
/// root, through [`preflight_heights`]). The operand tables are static and
/// short, so the conversion to `u32` cannot fail.
pub(crate) fn ta_arity_table<S: TaStateFacts>(states: &[S]) -> Vec<(u32, u32)> {
    let mut table = Vec::new();
    let mut i = 0;
    while i < states.len() {
        let Ok(operands) = u32::try_from(states[i].operand_count()) else {
            unreachable!()
        };
        table.push((operands, states[i].result_arity()));
        i += 1;
    }
    table
}

/// The validator's stack-height preflight decision for one function (Aeneas
/// extraction root): [`stack_heights`] over [`ta_arity_table`], with the
/// heights discarded. [`preflight_stack_heights`] only renders the error.
pub(crate) fn preflight_heights<S: TaStateFacts>(
    states: &[S],
    code: &[CsilInstr],
) -> Result<(), HeightErr> {
    let ta_arity = ta_arity_table(states);
    match stack_heights(code, &ta_arity) {
        Ok(_) => Ok(()),
        Err(error) => Err(error),
    }
}

/// `CsilProgram::resolve_ta_input(binding, None)` as the validator uses it
/// (Aeneas extraction root): the binding's integer default, negated if asked,
/// accepted only inside the binding's `ta_input_bounds`. Same lookups as the
/// two `CsilProgram` methods (a missing input or state, or a non-integer
/// default, rejects), calling the same integer kernels.
pub(crate) fn ta_binding_default<S: TaStateFacts, I: InputDefaultFacts>(
    states: &[S],
    inputs: &[I],
    binding: &pinewasm_hir::TaInputBinding,
) -> Option<i64> {
    let input = binding.input as usize;
    if input >= inputs.len() {
        return None;
    }
    // An explicit match stands in for `?`, which on `Option` has no Aeneas
    // library model.
    match inputs[input].default_int() {
        Some(value) => {
            let bounds = match binding.target {
                pinewasm_hir::TaInputTarget::State { state, param } => {
                    let state = state as usize;
                    if state < states.len() {
                        match states[state].param_rule(param) {
                            Some(rule) => crate::ta_inputs::ta_param_rule_bounds(&rule),
                            None => None,
                        }
                    } else {
                        None
                    }
                }
                pinewasm_hir::TaInputTarget::Sma { .. }
                | pinewasm_hir::TaInputTarget::Change { .. } => {
                    crate::ta_inputs::ta_target_bounds(binding.target, None)
                }
            };
            crate::ta_inputs::resolve_ta_int(value, binding.negated, bounds)
        }
        None => None,
    }
}

/// The validator's message for a preflight rejection; `state_count` is the
/// length of the TA state table.
fn height_error(name: &str, error: HeightErr, state_count: usize) -> ValidationError {
    let message = match error.kind {
        HeightErrKind::ComplexityLimit => {
            return ValidationError::Structure {
                function: name.to_string(),
                message: format!(
                    "abstract stack analysis exceeds the {MAX_VALIDATION_ABSTRACT_CELLS}-cell complexity limit"
                ),
            };
        }
        HeightErrKind::Underflow { pops, height } => {
            format!("stack underflow: needs {pops} operand(s), stack has {height}")
        }
        HeightErrKind::Overflow => "operand stack height overflows u32".to_string(),
        HeightErrKind::ExceedsLimit => {
            format!("actual operand stack exceeds the limit of {MAX_DECLARED_STACK}")
        }
        HeightErrKind::NonEmptyReturn { height } => {
            format!("operand stack must be empty at RETURN (has {height} values)")
        }
        HeightErrKind::FallsOffEnd => "execution falls off the end of the function".to_string(),
        HeightErrKind::MergeMismatch { existing, incoming } => {
            format!("inconsistent stack heights at merge point ({existing} vs {incoming})")
        }
        HeightErrKind::InvalidTaState { state } => format!(
            "cannot resolve stack effect: {}",
            StackEffectError::InvalidTaState { state, state_count }
        ),
        HeightErrKind::CountExceedsU32 => format!(
            "cannot resolve stack effect: {}",
            StackEffectError::CountExceedsU32
        ),
    };
    ValidationError::AtInstruction {
        function: name.to_string(),
        offset: instruction_offset(error.pc),
        message,
    }
}

/// Stack-height preflight (Aeneas extraction kernel): a depth-first worklist
/// walk from `pc = 0` assigning each reachable instruction its entry height.
/// Each instruction's `(pops, pushes)` is its [`CsilInstr::stack_effect_rule`]
/// resolved as [`CsilInstr::resolve_stack_effect`] does, with
/// `ta_arity[state]` as `CALL_TA state`'s arity. Returns the entry heights
/// (`None` for unreachable code) or the first rejection in visit order.
///
/// Precondition (panics otherwise): `code` is non-empty. Jump targets and
/// `CALL_TA` states need not be in range.
pub(crate) fn stack_heights(
    code: &[CsilInstr],
    ta_arity: &[(u32, u32)],
) -> Result<Vec<Option<u32>>, HeightErr> {
    let mut heights = vec![None; code.len()];
    let mut worklist = Vec::new();
    worklist.push(0usize);
    heights[0] = Some(0u32);
    let mut abstract_cells = 0usize;
    while let Some(pc) = worklist.pop() {
        abstract_cells = stack_height_step(
            code,
            ta_arity,
            &mut heights,
            &mut worklist,
            abstract_cells,
            pc,
        )?;
    }
    Ok(heights)
}

/// One worklist visit of [`stack_heights`]; returns the new cell total.
fn stack_height_step(
    code: &[CsilInstr],
    ta_arity: &[(u32, u32)],
    heights: &mut [Option<u32>],
    worklist: &mut Vec<usize>,
    abstract_cells: usize,
    pc: usize,
) -> Result<usize, HeightErr> {
    // Worklist entries always have a recorded height. No message: a `&str`
    // literal is not supported in extracted code.
    // Copied out first, as in `visit_height_successor`.
    let recorded: Option<u32> = heights[pc];
    let Some(height) = recorded else {
        unreachable!()
    };
    let instr = &code[pc];
    let (pops, pushes) = match height_effect(instr, ta_arity) {
        Ok(effect) => effect,
        Err(kind) => return Err(HeightErr { pc, kind }),
    };
    let Some(after_pops) = height.checked_sub(pops) else {
        return Err(HeightErr {
            pc,
            kind: HeightErrKind::Underflow { pops, height },
        });
    };
    let Some(next_height) = after_pops.checked_add(pushes) else {
        return Err(HeightErr {
            pc,
            kind: HeightErrKind::Overflow,
        });
    };
    if next_height > MAX_DECLARED_STACK {
        return Err(HeightErr {
            pc,
            kind: HeightErrKind::ExceedsLimit,
        });
    }
    if is_return(instr) {
        if next_height != 0 {
            return Err(HeightErr {
                pc,
                kind: HeightErrKind::NonEmptyReturn {
                    height: next_height,
                },
            });
        }
        return Ok(abstract_cells);
    }
    let (first, second) = height_successors(instr, pc);
    let abstract_cells =
        visit_height_successor(heights, worklist, abstract_cells, pc, first, next_height)?;
    match second {
        Some(successor) => visit_height_successor(
            heights,
            worklist,
            abstract_cells,
            pc,
            successor,
            next_height,
        ),
        None => Ok(abstract_cells),
    }
}

/// `(pops, pushes)` for the preflight: [`CsilInstr::resolve_stack_effect`]
/// with the TA state table given as its arities (`CALL_TA` cannot know its
/// arity alone). The payload casts are checked against `u32::MAX` first
/// (plain casts keep the kernel in the Aeneas-extractable subset).
#[allow(clippy::cast_possible_truncation)]
fn height_effect(instr: &CsilInstr, ta_arity: &[(u32, u32)]) -> Result<(u32, u32), HeightErrKind> {
    match instr.stack_effect_rule() {
        StackEffectRule::Fixed { pops, pushes } => Ok((pops, pushes)),
        StackEffectRule::Payload { pops, pushes } => {
            if pops > u32::MAX as usize || pushes > u32::MAX as usize {
                Err(HeightErrKind::CountExceedsU32)
            } else {
                Ok((pops as u32, pushes as u32))
            }
        }
        StackEffectRule::TaState { state } => {
            if (state as usize) < ta_arity.len() {
                Ok(ta_arity[state as usize])
            } else {
                Err(HeightErrKind::InvalidTaState { state })
            }
        }
    }
}

fn is_return(instr: &CsilInstr) -> bool {
    matches!(instr, CsilInstr::Return)
}

/// Successors in visit order: a conditional jump yields its target, then
/// the fall-through.
fn height_successors(instr: &CsilInstr, pc: usize) -> (usize, Option<usize>) {
    match instr {
        CsilInstr::Jump { target } => (*target as usize, None),
        CsilInstr::JumpIfFalse { target }
        | CsilInstr::JumpIfExact { target, .. }
        | CsilInstr::JumpIfSlotWritten { target, .. } => (*target as usize, Some(pc + 1)),
        _ => (pc + 1, None),
    }
}

/// Records `next_height` at `successor` (queuing it on first visit) or
/// checks it against the recorded height; returns the new cell total.
fn visit_height_successor(
    heights: &mut [Option<u32>],
    worklist: &mut Vec<usize>,
    abstract_cells: usize,
    pc: usize,
    successor: usize,
    next_height: u32,
) -> Result<usize, HeightErr> {
    if successor >= heights.len() {
        return Err(HeightErr {
            pc,
            kind: HeightErrKind::FallsOffEnd,
        });
    }
    // Copy the entry out before matching: Aeneas cannot expand an enum
    // matched in place behind a slice index.
    let recorded: Option<u32> = heights[successor];
    match recorded {
        None => {
            let Some(total) = abstract_cells.checked_add(next_height as usize) else {
                return Err(HeightErr {
                    pc,
                    kind: HeightErrKind::ComplexityLimit,
                });
            };
            if total > MAX_VALIDATION_ABSTRACT_CELLS {
                return Err(HeightErr {
                    pc,
                    kind: HeightErrKind::ComplexityLimit,
                });
            }
            heights[successor] = Some(next_height);
            worklist.push(successor);
            Ok(total)
        }
        Some(existing) => {
            if existing == next_height {
                Ok(abstract_cells)
            } else {
                Err(HeightErr {
                    pc: successor,
                    kind: HeightErrKind::MergeMismatch {
                        existing,
                        incoming: next_height,
                    },
                })
            }
        }
    }
}

/// Ceiling on the declarable operand-stack size. Far above anything the
/// current lowering can produce, far below an allocation-of-interest.
pub const MAX_DECLARED_STACK: u32 = 65_536;

/// Maximum total typed stack cells retained by CSIL abstract interpretation.
///
/// Compiler output is orders of magnitude below this ceiling. The bound keeps
/// validation of an untrusted push/pop triangle within predictable memory and
/// work even when its serialized artifact is small.
pub const MAX_VALIDATION_ABSTRACT_CELLS: usize = 8_000_000;

/// Inclusive upper bound on `WINDOW_MEAN` lengths. The compiler rejects
/// `ta.*` length literals outside 1..=100000 (E0219).
pub const MAX_WINDOW_LENGTH: u32 = 100_000;

/// Count the `ta.*` nodes in an `HtfExpr` tree (saturating, so a crafted deep
/// tree cannot overflow). Valid programs assign `ta_slot`s densely as
/// `0..count`, so this is the exclusive upper bound on every `ta_slot`.
fn count_htf_ta_nodes(expr: &pinewasm_hir::HtfExpr) -> u32 {
    use pinewasm_hir::HtfExpr;
    match expr {
        HtfExpr::BoolResult(inner) => count_htf_ta_nodes(inner),
        HtfExpr::EnumSelect {
            when_equal,
            otherwise,
            ..
        } => count_htf_ta_nodes(when_equal).saturating_add(
            otherwise
                .as_ref()
                .map_or(0, |expr| count_htf_ta_nodes(expr)),
        ),
        HtfExpr::Const(_)
        | HtfExpr::Source(_)
        | HtfExpr::InputSource { .. }
        | HtfExpr::BarstateIslast
        | HtfExpr::BarstateIsRealtime => 0,
        HtfExpr::History { source, .. } | HtfExpr::HistoryInput { source, .. } => {
            count_htf_ta_nodes(source)
        }
        HtfExpr::HistoryRealtime { source, .. } => count_htf_ta_nodes(source),
        HtfExpr::Neg(inner) => count_htf_ta_nodes(inner),
        HtfExpr::Binary { lhs, rhs, .. } => {
            count_htf_ta_nodes(lhs).saturating_add(count_htf_ta_nodes(rhs))
        }
        HtfExpr::Ta { source, .. } | HtfExpr::TaInput { source, .. } => {
            count_htf_ta_nodes(source).saturating_add(1)
        }
    }
}

/// Count the history nodes in an `HtfExpr` tree with the same saturating
/// semantics. The result is the exclusive upper bound for densely assigned
/// `history_slot` values.
fn count_htf_history_nodes(expr: &pinewasm_hir::HtfExpr) -> u32 {
    use pinewasm_hir::HtfExpr;
    match expr {
        HtfExpr::BoolResult(inner) => count_htf_history_nodes(inner),
        HtfExpr::EnumSelect {
            when_equal,
            otherwise,
            ..
        } => count_htf_history_nodes(when_equal).saturating_add(
            otherwise
                .as_ref()
                .map_or(0, |expr| count_htf_history_nodes(expr)),
        ),
        HtfExpr::Const(_)
        | HtfExpr::Source(_)
        | HtfExpr::InputSource { .. }
        | HtfExpr::BarstateIslast
        | HtfExpr::BarstateIsRealtime => 0,
        HtfExpr::History { source, .. } | HtfExpr::HistoryInput { source, .. } => {
            count_htf_history_nodes(source).saturating_add(1)
        }
        HtfExpr::HistoryRealtime { source, .. } => {
            count_htf_history_nodes(source).saturating_add(1)
        }
        HtfExpr::Neg(inner) => count_htf_history_nodes(inner),
        HtfExpr::Binary { lhs, rhs, .. } => {
            count_htf_history_nodes(lhs).saturating_add(count_htf_history_nodes(rhs))
        }
        HtfExpr::Ta { source, .. } | HtfExpr::TaInput { source, .. } => {
            count_htf_history_nodes(source)
        }
    }
}

fn validate_htf_offset(
    expr: &pinewasm_hir::HtfHistoryOffset,
    inputs: &[pinewasm_hir::InputDecl],
) -> Result<(), String> {
    use pinewasm_hir::{BinOp, HtfHistoryOffset as O};
    match expr {
        O::Int(_) => Ok(()),
        O::Float(x) if x.is_finite() => Ok(()),
        O::InputInt { input, default } => {
            if inputs
                .get(*input as usize)
                .is_some_and(|x| x.kind == InputKind::Int && x.defval == InputValue::Int(*default))
            {
                Ok(())
            } else {
                Err(format!("request.security history input {input} is invalid"))
            }
        }
        O::InputFloat { input, default } => {
            if default.is_finite() && inputs.get(*input as usize).is_some_and(|x| {
                x.kind == InputKind::Float
                    && matches!(x.defval, InputValue::Float(v) if v.to_bits()==default.to_bits())
            }) {
                Ok(())
            } else {
                Err(format!("request.security history input {input} is invalid"))
            }
        }
        O::Neg(x) | O::CastInt(x) | O::CastFloat(x) => validate_htf_offset(x, inputs),
        O::Binary {
            op: BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem,
            lhs,
            rhs,
        } => {
            validate_htf_offset(lhs, inputs)?;
            validate_htf_offset(rhs, inputs)
        }
        _ => Err("request.security history offset expression is invalid".into()),
    }
}

/// Validate an `HtfExpr` secondary-context tree. Defense in depth — the
/// analyzer already builds only well-formed trees — so a malformed *serialized*
/// program cannot drive the secondary context out of range:
/// - every `ta.*` window length must fall in `1..=MAX_WINDOW_LENGTH`;
/// - every `ta_slot` must be `< ta_node_count` (the analyzer assigns slots
///   densely as `0..N`);
/// - every `history_slot` must be `< history_node_count` under the same dense
///   assignment invariant.
///
/// These bounds size the VM's per-context state from the tree itself, so a
/// crafted huge slot cannot overflow, index out of bounds, or trigger a giant
/// allocation.
fn validate_htf_expr(
    expr: &pinewasm_hir::HtfExpr,
    ta_node_count: u32,
    history_node_count: u32,
    inputs: &[pinewasm_hir::InputDecl],
) -> Result<(), String> {
    use pinewasm_hir::HtfExpr;
    match expr {
        HtfExpr::BoolResult(inner) => {
            validate_htf_expr(inner, ta_node_count, history_node_count, inputs)
        }
        HtfExpr::Const(_)
        | HtfExpr::Source(_)
        | HtfExpr::BarstateIslast
        | HtfExpr::BarstateIsRealtime => Ok(()),
        HtfExpr::InputSource { input } => {
            if inputs
                .get(*input as usize)
                .is_some_and(|decl| decl.kind == InputKind::Source)
            {
                Ok(())
            } else {
                Err(format!(
                    "request.security secondary-context source input {input} is invalid"
                ))
            }
        }
        HtfExpr::HistoryInput {
            history_slot,
            offset,
            source,
        } => {
            validate_htf_offset(offset, inputs)?;
            if *history_slot >= history_node_count {
                return Err(format!(
                    "request.security history_slot {history_slot} out of range"
                ));
            }
            validate_htf_expr(source, ta_node_count, history_node_count, inputs)
        }
        HtfExpr::History {
            history_slot,
            source,
            ..
        } => {
            if *history_slot >= history_node_count {
                return Err(format!(
                    "request.security secondary-context history_slot {history_slot} out of range \
                     (must be < {history_node_count})"
                ));
            }
            validate_htf_expr(source, ta_node_count, history_node_count, inputs)
        }
        HtfExpr::HistoryRealtime {
            history_slot,
            source,
            ..
        } => {
            if *history_slot >= history_node_count {
                return Err(format!(
                    "request.security secondary-context history_slot {history_slot} out of range \
                     (must be < {history_node_count})"
                ));
            }
            validate_htf_expr(source, ta_node_count, history_node_count, inputs)
        }
        HtfExpr::EnumSelect {
            input,
            when_equal,
            otherwise,
            ..
        } => {
            if !inputs.get(*input as usize).is_some_and(|decl| {
                decl.kind == InputKind::Enum && matches!(decl.defval, InputValue::Enum { .. })
            }) {
                return Err(format!(
                    "request.security secondary-context enum input {input} is invalid"
                ));
            }
            validate_htf_expr(when_equal, ta_node_count, history_node_count, inputs)?;
            if let Some(otherwise) = otherwise {
                validate_htf_expr(otherwise, ta_node_count, history_node_count, inputs)?;
            }
            Ok(())
        }
        HtfExpr::Neg(inner) => validate_htf_expr(inner, ta_node_count, history_node_count, inputs),
        HtfExpr::Binary { lhs, rhs, .. } => {
            validate_htf_expr(lhs, ta_node_count, history_node_count, inputs)?;
            validate_htf_expr(rhs, ta_node_count, history_node_count, inputs)
        }
        HtfExpr::Ta {
            length,
            ta_slot,
            source,
            ..
        } => {
            if !(1..=MAX_WINDOW_LENGTH).contains(length) {
                return Err(format!(
                    "request.security secondary-context ta length {length} out of range \
                     (1..={MAX_WINDOW_LENGTH})"
                ));
            }
            if *ta_slot >= ta_node_count {
                return Err(format!(
                    "request.security secondary-context ta_slot {ta_slot} out of range \
                     (must be < {ta_node_count})"
                ));
            }
            validate_htf_expr(source, ta_node_count, history_node_count, inputs)
        }
        HtfExpr::TaInput {
            func,
            ta_slot,
            input,
            binding_target,
            default_length,
            source,
        } => {
            if !matches!(
                func,
                pinewasm_hir::TaFunc::Sma
                    | pinewasm_hir::TaFunc::Change
                    | pinewasm_hir::TaFunc::Ema
                    | pinewasm_hir::TaFunc::Hma
                    | pinewasm_hir::TaFunc::Vwma
                    | pinewasm_hir::TaFunc::Rsi
                    | pinewasm_hir::TaFunc::Wma
            ) {
                return Err(
                    "request.security secondary-context input length has unsupported TA function"
                        .into(),
                );
            }
            if !(1..=MAX_WINDOW_LENGTH).contains(default_length)
                || (*func == pinewasm_hir::TaFunc::Hma && *default_length < 2)
                || *ta_slot >= ta_node_count
            {
                return Err(
                    "request.security secondary-context input TA slot or length is invalid".into(),
                );
            }
            let decl = inputs.get(*input as usize).ok_or_else(|| {
                format!("request.security secondary-context length input {input} is missing")
            })?;
            if decl.kind != InputKind::Int
                || decl.defval != InputValue::Int(i64::from(*default_length))
            {
                return Err(format!(
                    "request.security secondary-context length input {input} is invalid"
                ));
            }
            if !matches!(
                (func, binding_target),
                (
                    pinewasm_hir::TaFunc::Sma,
                    pinewasm_hir::TaInputTarget::Sma { .. }
                ) | (
                    pinewasm_hir::TaFunc::Change,
                    pinewasm_hir::TaInputTarget::Change { .. }
                ) | (
                    pinewasm_hir::TaFunc::Ema
                        | pinewasm_hir::TaFunc::Hma
                        | pinewasm_hir::TaFunc::Vwma
                        | pinewasm_hir::TaFunc::Rsi
                        | pinewasm_hir::TaFunc::Wma,
                    pinewasm_hir::TaInputTarget::State { param: 0, .. }
                )
            ) {
                return Err(
                    "request.security secondary-context TA binding target is invalid".into(),
                );
            }
            validate_htf_expr(source, ta_node_count, history_node_count, inputs)
        }
    }
}

#[cfg(all(test, target_pointer_width = "64"))]
mod tests {
    use super::{checked_instruction_offset, exceeds_offset_space};

    #[test]
    fn instruction_count_preflight_accepts_exactly_the_representable_indices() {
        let max = usize::try_from(u32::MAX).expect("u32 fits a 64-bit usize");
        assert!(!exceeds_offset_space(0));
        assert!(!exceeds_offset_space(1));
        assert!(!exceeds_offset_space(max + 1));
        assert!(exceeds_offset_space(max + 2));
    }

    #[test]
    fn instruction_offset_conversion_rejects_the_first_unrepresentable_index() {
        let max = usize::try_from(u32::MAX).expect("u32 fits a 64-bit usize");
        assert_eq!(checked_instruction_offset(max), Some(u32::MAX));
        assert_eq!(checked_instruction_offset(max + 1), None);
    }
}

/// Bindings change execution, so old ABI readers must reject them instead of
/// silently executing the defaults. Validate every reference before allocation.
type SecondaryMap = std::collections::HashMap<pinewasm_hir::TaInputTarget, (u32, u32, usize)>;
type SlotsMap = std::collections::HashMap<pinewasm_hir::TaInputTarget, (usize, u32)>;
type BindingTargets = std::collections::HashSet<pinewasm_hir::TaInputTarget>;

struct BindingContext<'a> {
    program: &'a CsilProgram,
}
impl binding_state::Stages<SecondaryMap, SlotsMap, BindingTargets, Vec<ValidationError>>
    for BindingContext<'_>
{
    fn new_secondary(&self) -> SecondaryMap {
        std::collections::HashMap::new()
    }
    fn scan_secondary(&self, map: SecondaryMap) -> SecondaryMap {
        use pinewasm_hir::{HtfExpr, TaInputTarget};
        fn collect_secondary(
            expr: &HtfExpr,
            out: &mut std::collections::HashMap<TaInputTarget, (u32, u32, usize)>,
        ) {
            match expr {
                HtfExpr::TaInput {
                    binding_target,
                    input,
                    default_length,
                    source,
                    ..
                } => {
                    let entry = out
                        .entry(*binding_target)
                        .or_insert((*input, *default_length, 0));
                    if entry.0 != *input || entry.1 != *default_length {
                        entry.2 = usize::MAX;
                    } else {
                        entry.2 = entry.2.saturating_add(1);
                    }
                    collect_secondary(source, out);
                }
                HtfExpr::Ta { source, .. }
                | HtfExpr::History { source, .. }
                | HtfExpr::HistoryInput { source, .. }
                | HtfExpr::HistoryRealtime { source, .. }
                | HtfExpr::Neg(source) => {
                    collect_secondary(source, out);
                }
                HtfExpr::Binary { lhs, rhs, .. } => {
                    collect_secondary(lhs, out);
                    collect_secondary(rhs, out);
                }
                HtfExpr::EnumSelect {
                    when_equal,
                    otherwise,
                    ..
                } => {
                    collect_secondary(when_equal, out);
                    if let Some(otherwise) = otherwise {
                        collect_secondary(otherwise, out);
                    }
                }
                HtfExpr::BoolResult(inner) => collect_secondary(inner, out),
                HtfExpr::Const(_)
                | HtfExpr::Source(_)
                | HtfExpr::InputSource { .. }
                | HtfExpr::BarstateIslast
                | HtfExpr::BarstateIsRealtime => {}
            }
        }
        struct Secondary(std::collections::HashMap<TaInputTarget, (u32, u32, usize)>);
        impl global_scan::Secondary for Secondary {
            fn observe(mut self, expr: &HtfExpr) -> Self {
                collect_secondary(expr, &mut self.0);
                self
            }
        }
        global_scan::secondary(self.program, Secondary(map)).0
    }
    fn finish_secondary(&self, secondary: &SecondaryMap, errors: &mut Vec<ValidationError>) {
        let program = self.program;
        for (target, (input, _, count)) in secondary {
            if *count == usize::MAX
                || !program.ta_input_bindings.iter().any(|binding| {
                    binding.target == *target && binding.input == *input && !binding.negated
                })
            {
                errors.push(ValidationError::Header(
                    "secondary-context TA input has no matching binding".into(),
                ));
            }
        }
    }
    fn collect_slots(&self) -> SlotsMap {
        use pinewasm_hir::TaInputTarget;
        let program = self.program;
        let slots: std::collections::HashMap<TaInputTarget, (usize, u32)> = program
            .ta_input_bindings
            .iter()
            .filter(|b| !matches!(b.target, TaInputTarget::State { .. }))
            .map(|b| (b.target, (0, 0)))
            .collect();
        slots
    }
    fn scan_slots(&self, slots: SlotsMap) -> SlotsMap {
        use pinewasm_hir::TaInputTarget;
        struct Slots(std::collections::HashMap<TaInputTarget, (usize, u32)>);
        impl global_scan::Binding for Slots {
            fn observe(mut self, target: TaInputTarget, value: u32) -> Self {
                if let Some((count, default)) = self.0.get_mut(&target) {
                    *count += 1;
                    *default = value;
                }
                self
            }
        }
        global_scan::bindings(self.program, Slots(slots)).0
    }
    fn new_targets(&self) -> BindingTargets {
        std::collections::HashSet::new()
    }
    fn binding_count(&self) -> usize {
        binding_state::count(self.program)
    }
    fn binding(
        &self,
        secondary: &SecondaryMap,
        slots: &SlotsMap,
        targets: &mut BindingTargets,
        errors: &mut Vec<ValidationError>,
        index: usize,
    ) {
        use pinewasm_hir::TaInputTarget;
        let program = self.program;
        let binding = &program.ta_input_bindings[index];
        let bad = |errors: &mut Vec<ValidationError>, detail: &str| {
            errors.push(ValidationError::Header(format!(
                "invalid TA input binding: {detail}"
            )));
        };
        if ![crate::TA_INPUT_ABI_VERSION, crate::INDEXED_SCAN_ABI_VERSION]
            .contains(&program.header.runtime_abi_version)
        {
            bad(errors, "requires runtime ABI 1");
        }
        if !targets.insert(binding.target) {
            bad(errors, "duplicate target");
        }
        let float_rule = matches!(
            program.ta_input_rule(binding),
            Some(TaParamRule::FloatBits { .. } | TaParamRule::Percent)
        );
        if !program
            .inputs
            .get(binding.input as usize)
            .is_some_and(|input| {
                if float_rule {
                    matches!(
                        input.kind,
                        InputKind::Int | InputKind::Float | InputKind::Price
                    )
                } else {
                    input.kind == InputKind::Int
                }
            })
        {
            bad(errors, "input kind does not match the TA parameter");
            return;
        }
        let default = if float_rule {
            program.resolve_ta_input(binding, None)
        } else {
            ta_binding_default(&program.ta_states, &program.inputs, binding)
        };
        let Some(default) = default else {
            bad(errors, "invalid TA parameter or default");
            return;
        };
        let valid_target = match binding.target {
            TaInputTarget::State { state, param } => program
                .ta_states
                .get(state as usize)
                .and_then(|decl| decl.params.get(param as usize))
                .is_some_and(|value| *value == default),
            TaInputTarget::Sma { slot } | TaInputTarget::Change { slot } => {
                (slot as usize) < program.series_slots.len()
                    && slots.get(&binding.target).is_some_and(|(count, value)| {
                        (*count == 1 && i64::from(*value) == default)
                            || (*count == 0
                                && secondary.get(&binding.target).is_some_and(
                                    |(input, length, count)| {
                                        *input == binding.input
                                            && i64::from(*length) == default
                                            && *count > 0
                                            && *count != usize::MAX
                                    },
                                ))
                    })
            }
        };
        if !valid_target {
            bad(
                errors,
                "target must carry this input's default exactly once",
            );
        }
    }
}
fn validate_ta_input_bindings(program: &CsilProgram, errors: &mut Vec<ValidationError>) {
    binding_state::run(&BindingContext { program }, errors);
}

#[cfg(test)]
fn ta_input_bindings_slice_check(program: &CsilProgram, errors: &mut Vec<ValidationError>) {
    use pinewasm_hir::{HtfExpr, TaInputTarget};
    fn collect_secondary(
        expr: &HtfExpr,
        out: &mut std::collections::HashMap<TaInputTarget, (u32, u32, usize)>,
    ) {
        match expr {
            HtfExpr::TaInput {
                binding_target,
                input,
                default_length,
                source,
                ..
            } => {
                let entry = out
                    .entry(*binding_target)
                    .or_insert((*input, *default_length, 0));
                if entry.0 != *input || entry.1 != *default_length {
                    entry.2 = usize::MAX;
                } else {
                    entry.2 = entry.2.saturating_add(1);
                }
                collect_secondary(source, out);
            }
            HtfExpr::Ta { source, .. }
            | HtfExpr::History { source, .. }
            | HtfExpr::HistoryInput { source, .. }
            | HtfExpr::HistoryRealtime { source, .. }
            | HtfExpr::Neg(source) => {
                collect_secondary(source, out);
            }
            HtfExpr::Binary { lhs, rhs, .. } => {
                collect_secondary(lhs, out);
                collect_secondary(rhs, out);
            }
            HtfExpr::EnumSelect {
                when_equal,
                otherwise,
                ..
            } => {
                collect_secondary(when_equal, out);
                if let Some(otherwise) = otherwise {
                    collect_secondary(otherwise, out);
                }
            }
            HtfExpr::BoolResult(inner) => collect_secondary(inner, out),
            HtfExpr::Const(_)
            | HtfExpr::Source(_)
            | HtfExpr::InputSource { .. }
            | HtfExpr::BarstateIslast
            | HtfExpr::BarstateIsRealtime => {}
        }
    }
    struct Secondary(std::collections::HashMap<TaInputTarget, (u32, u32, usize)>);
    impl global_scan::Secondary for Secondary {
        fn observe(mut self, expr: &HtfExpr) -> Self {
            collect_secondary(expr, &mut self.0);
            self
        }
    }
    let secondary = global_scan::secondary(program, Secondary(std::collections::HashMap::new())).0;
    for (target, (input, _, count)) in &secondary {
        if *count == usize::MAX
            || !program.ta_input_bindings.iter().any(|binding| {
                binding.target == *target && binding.input == *input && !binding.negated
            })
        {
            errors.push(ValidationError::Header(
                "secondary-context TA input has no matching binding".into(),
            ));
        }
    }
    let slots: std::collections::HashMap<TaInputTarget, (usize, u32)> = program
        .ta_input_bindings
        .iter()
        .filter(|b| !matches!(b.target, TaInputTarget::State { .. }))
        .map(|b| (b.target, (0, 0)))
        .collect();
    struct Slots(std::collections::HashMap<TaInputTarget, (usize, u32)>);
    impl global_scan::Binding for Slots {
        fn observe(mut self, target: TaInputTarget, value: u32) -> Self {
            if let Some((count, default)) = self.0.get_mut(&target) {
                *count += 1;
                *default = value;
            }
            self
        }
    }
    // Scan bytecode once, rather than once per binding on untrusted programs.
    let slots = global_scan::bindings(program, Slots(slots)).0;
    let mut targets = std::collections::HashSet::new();
    for binding in &program.ta_input_bindings {
        let bad = |errors: &mut Vec<ValidationError>, detail: &str| {
            errors.push(ValidationError::Header(format!(
                "invalid TA input binding: {detail}"
            )));
        };
        if ![crate::TA_INPUT_ABI_VERSION, crate::INDEXED_SCAN_ABI_VERSION]
            .contains(&program.header.runtime_abi_version)
        {
            bad(errors, "requires runtime ABI 1");
        }
        if !targets.insert(binding.target) {
            bad(errors, "duplicate target");
        }
        let float_rule = matches!(
            program.ta_input_rule(binding),
            Some(TaParamRule::FloatBits { .. } | TaParamRule::Percent)
        );
        if !program
            .inputs
            .get(binding.input as usize)
            .is_some_and(|input| {
                if float_rule {
                    matches!(
                        input.kind,
                        InputKind::Int | InputKind::Float | InputKind::Price
                    )
                } else {
                    input.kind == InputKind::Int
                }
            })
        {
            bad(errors, "input kind does not match the TA parameter");
            continue;
        }
        let default = if float_rule {
            program.resolve_ta_input(binding, None)
        } else {
            ta_binding_default(&program.ta_states, &program.inputs, binding)
        };
        let Some(default) = default else {
            bad(errors, "invalid TA parameter or default");
            continue;
        };
        let valid_target = match binding.target {
            TaInputTarget::State { state, param } => program
                .ta_states
                .get(state as usize)
                .and_then(|decl| decl.params.get(param as usize))
                .is_some_and(|value| *value == default),
            TaInputTarget::Sma { slot } | TaInputTarget::Change { slot } => {
                (slot as usize) < program.series_slots.len()
                    && slots.get(&binding.target).is_some_and(|(count, value)| {
                        (*count == 1 && i64::from(*value) == default)
                            || (*count == 0
                                && secondary.get(&binding.target).is_some_and(
                                    |(input, length, count)| {
                                        *input == binding.input
                                            && i64::from(*length) == default
                                            && *count > 0
                                            && *count != usize::MAX
                                    },
                                ))
                    })
            }
        };
        if !valid_target {
            bad(
                errors,
                "target must carry this input's default exactly once",
            );
        }
    }
}

/// Tests of the validator glue made generic for Aeneas
/// (`ta_arity_table`, `preflight_heights`, `ta_binding_default`), each
/// comparing the result with the function it delegates to.
#[cfg(test)]
mod glue_diff_tests {
    use super::{preflight_heights, stack_heights, ta_arity_table, ta_binding_default};
    use crate::{CsilInstr, CsilProgram, InputValue, TaFunc, MAX_WINDOW_LENGTH};
    use pinewasm_hir::{TaInputBinding, TaInputTarget, TaStateDecl};

    fn compile(source: &str) -> CsilProgram {
        let parsed = pinewasm_parse::parse(source);
        assert!(!pinewasm_span::has_errors(&parsed.diagnostics));
        let analysis = pinewasm_semantics::analyze(&parsed.program);
        assert!(!pinewasm_span::has_errors(&analysis.diagnostics));
        crate::lower_mir(&pinewasm_mir::lower(&analysis.hir)).expect("lowers")
    }

    fn state(func: TaFunc) -> TaStateDecl {
        TaStateDecl {
            func,
            params: Vec::new(),
        }
    }

    /// The table the preflight consults is, entry by entry, the resolved
    /// `CALL_TA` effect (an iterator `.map(..).collect()` built the same
    /// pairs), for every TA function and for the empty table.
    #[test]
    fn ta_arity_table_is_the_resolved_call_ta_effect() {
        let states: Vec<TaStateDecl> = TaFunc::ALL.into_iter().map(state).collect();
        let table = ta_arity_table(&states);
        assert_eq!(table.len(), states.len());
        for (k, entry) in table.iter().enumerate() {
            let effect = CsilInstr::CallTa {
                state: u32::try_from(k).unwrap(),
            }
            .resolve_stack_effect(&states)
            .expect("declared state resolves");
            assert_eq!(*entry, (effect.pops, effect.pushes), "{:?}", states[k].func);
            assert_eq!(
                *entry,
                (
                    u32::try_from(states[k].func.engine_operands().len()).unwrap(),
                    states[k].func.engine_result_arity()
                )
            );
        }
        assert!(ta_arity_table::<TaStateDecl>(&[]).is_empty());
    }

    /// `preflight_heights` is `stack_heights` over that table with the
    /// heights dropped, on accepted and rejected code, including a
    /// `CALL_TA` whose arity decides the outcome.
    #[test]
    fn preflight_heights_is_stack_heights_over_the_table() {
        for func in TaFunc::ALL {
            let states = vec![state(func)];
            let operands = func.engine_operands().len();
            let mut code = vec![CsilInstr::PushNa; operands];
            code.push(CsilInstr::CallTa { state: 0 });
            for _ in 0..func.engine_result_arity() {
                code.push(CsilInstr::Pop);
            }
            code.push(CsilInstr::Return);
            let variants = [
                code.clone(),
                code[1..].to_vec(),
                vec![CsilInstr::Pop, CsilInstr::Return],
                vec![CsilInstr::CallTa { state: 1 }, CsilInstr::Return],
            ];
            for code in variants {
                let table = ta_arity_table(&states);
                assert_eq!(
                    preflight_heights(&states, &code),
                    stack_heights(&code, &table).map(|_| ()),
                    "{func:?} {code:?}"
                );
            }
            assert_eq!(preflight_heights(&states, &code), Ok(()), "{func:?}");
        }
    }

    /// The validator's binding check agrees with
    /// `CsilProgram::resolve_ta_input(binding, None)` for every TA function,
    /// target shape (in and out of range), negation, input index (in and out
    /// of range) and default (integer at and past the bounds, `i64::MIN`,
    /// non-integer).
    #[test]
    fn ta_binding_default_is_resolve_ta_input_without_override() {
        let mut program = compile(
            "//@version=6\nindicator(\"g16\")\nlen = input.int(14, \"len\")\nplot(ta.rsi(close, 14))\n",
        );
        assert!(!program.ta_states.is_empty() && !program.inputs.is_empty());
        let n_states = u32::try_from(program.ta_states.len()).unwrap();
        let n_inputs = u32::try_from(program.inputs.len()).unwrap();
        let mut targets = vec![
            TaInputTarget::Sma { slot: 0 },
            TaInputTarget::Change { slot: 7 },
            TaInputTarget::State {
                state: n_states,
                param: 0,
            },
            TaInputTarget::State {
                state: u32::MAX,
                param: 0,
            },
            TaInputTarget::State {
                state: 0,
                param: u32::MAX,
            },
        ];
        targets.extend((0..8).map(|param| TaInputTarget::State { state: 0, param }));
        let max = i64::from(MAX_WINDOW_LENGTH);
        let defaults: Vec<InputValue> = [0, 1, 2, 14, -14, max, max + 1, -max, i64::MIN, i64::MAX]
            .into_iter()
            .map(InputValue::Int)
            .chain([InputValue::Float(14.0), InputValue::Bool(true)])
            .collect();
        let (mut accepted, mut rejected) = (0usize, 0usize);
        for func in TaFunc::ALL {
            program.ta_states[0].func = func;
            for target in &targets {
                for negated in [false, true] {
                    for input in [0, n_inputs, u32::MAX] {
                        let binding = TaInputBinding {
                            input,
                            negated,
                            target: *target,
                        };
                        // This extracted kernel covers integer TA defaults;
                        // float and percent defaults use the runtime resolver.
                        if matches!(
                            program.ta_input_rule(&binding),
                            Some(
                                pinewasm_hir::TaParamRule::FloatBits { .. }
                                    | pinewasm_hir::TaParamRule::Percent
                            )
                        ) {
                            continue;
                        }
                        for default in &defaults {
                            program.inputs[0].defval = default.clone();
                            let new =
                                ta_binding_default(&program.ta_states, &program.inputs, &binding);
                            let old = program.resolve_ta_input(&binding, None);
                            assert_eq!(new, old, "{func:?} {binding:?} {default:?}");
                            if new.is_some() {
                                accepted += 1;
                            } else {
                                rejected += 1;
                            }
                        }
                    }
                }
            }
        }
        assert!(accepted > 500 && rejected > 500, "{accepted} {rejected}");
    }
}

#[cfg(test)]
mod table_diff_tests {
    use super::*;

    const TAGS: [AbstractTy; 7] = [
        AbstractTy::Int,
        AbstractTy::F64,
        AbstractTy::Bool,
        AbstractTy::Str,
        AbstractTy::Color,
        AbstractTy::Ref,
        AbstractTy::Any,
    ];

    /// Test-only copy of the min/max result rule written with iterator
    /// closures, used to cross-check the production rule.
    fn min_max_iterator_rule(popped: &[AbstractTy]) -> AbstractTy {
        if popped.iter().all(|t| *t == AbstractTy::Int) {
            AbstractTy::Int
        } else if popped
            .iter()
            .all(|t| matches!(t, AbstractTy::Int | AbstractTy::F64))
        {
            AbstractTy::F64
        } else {
            AbstractTy::Any
        }
    }

    fn all_stacks(max_len: usize) -> Vec<Vec<AbstractTy>> {
        let mut out = vec![Vec::new()];
        let mut layer = vec![Vec::new()];
        for _ in 0..max_len {
            let mut next = Vec::new();
            for stack in &layer {
                for tag in TAGS {
                    let mut grown: Vec<AbstractTy> = stack.clone();
                    grown.push(tag);
                    next.push(grown);
                }
            }
            out.extend(next.iter().cloned());
            layer = next;
        }
        out
    }

    #[test]
    fn tag_rules_match_the_derived_equality() {
        for a in TAGS {
            for b in TAGS {
                assert_eq!(same_abstract_ty(a, b), a == b);
                assert_eq!(abstract_ty_accepts(a, b), a == b || a == AbstractTy::Any);
                assert_eq!(
                    abstract_ty_merge(a, b),
                    if a == b { a } else { AbstractTy::Any }
                );
            }
        }
    }

    #[test]
    fn min_max_kernel_matches_iterator_form() {
        let stacks = all_stacks(4);
        assert_eq!(stacks.len(), 1 + 7 + 49 + 343 + 2401);
        for popped in &stacks {
            assert_eq!(
                min_max_result_ty(popped),
                min_max_iterator_rule(popped),
                "{popped:?}"
            );
            for func in [PureFunc::MathMin, PureFunc::MathMax] {
                for argc in [0u8, 1, 2, 3, 4, 255] {
                    assert_eq!(
                        pure_result_ty(func, argc, popped),
                        min_max_iterator_rule(popped)
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod binding_state_diff_tests {
    use super::*;
    fn compile(source: &str) -> CsilProgram {
        let parsed = pinewasm_parse::parse(source);
        assert!(!pinewasm_span::has_errors(&parsed.diagnostics));
        let analysis = pinewasm_semantics::analyze(&parsed.program);
        assert!(!pinewasm_span::has_errors(&analysis.diagnostics));
        crate::lower_mir(&pinewasm_mir::lower(&analysis.hir)).expect("lowers")
    }
    #[test]
    fn count_reads_complete_native_inventory() {
        let mut p =
            compile("//@version=6\nindicator(\"count\")\nn = input.int(3)\nplot(ta.sma(close, n))");
        assert_eq!(binding_state::count(&p), 1);
        let mut malformed = p.ta_input_bindings[0];
        malformed.input = u32::MAX;
        p.ta_input_bindings.push(malformed);
        assert_eq!(binding_state::count(&p), 2);
        assert_eq!(
            binding_state::Stages::binding_count(&BindingContext { program: &p }),
            2
        );
        p.ta_input_bindings.clear();
        assert_eq!(binding_state::count(&p), 0);
        assert_eq!(
            binding_state::Stages::binding_count(&BindingContext { program: &p }),
            0
        );
    }
    #[test]
    fn actual_binding_differential() {
        for source in [
            "//@version=6\nindicator(\"bindings\")\nn = input.int(3)\nplot(ta.sma(close, n))",
            "//@version=6\nindicator(\"bindings\")\nn = input.int(3)\nplot(ta.change(close, n))",
            "//@version=6\nindicator(\"bindings\")\nn = input.int(3)\nplot(ta.ema(close, n))",
            "//@version=6\nindicator(\"bindings\")\nn = input.float(0.02)\nplot(ta.sar(n, 0.02, 0.2))",
            "//@version=6\nindicator(\"bindings\")\nn = input.int(3)\nplot(request.security(syminfo.tickerid, \"D\", ta.sma(close, n)))",
        ] {
        let base = compile(source);
        assert!(!base.ta_input_bindings.is_empty());
        for mode in 0..12 {
            let mut p = base.clone();
            match mode {
                1 => p.header.runtime_abi_version = 0,
                2 => {
                    p.ta_input_bindings.push(p.ta_input_bindings[0]);
                }
                3 => p.ta_input_bindings[0].input = u32::MAX,
                4 => p.inputs.clear(),
                5 => p.series_slots.clear(),
                6 => p.ta_input_bindings.clear(),
                7 => p.ta_input_bindings[0].negated = true,
                8 => {
                    for f in &mut p.functions { f.code.retain(|row| !matches!(row, CsilInstr::WindowMean {..} | CsilInstr::ReadUserSlot {..} | CsilInstr::ReadSecurityExpr {..})); }
                }
                9 => {
                    for f in &mut p.functions {
                        if let Some(row) = f.code.iter().find(|row| matches!(row, CsilInstr::WindowMean {..} | CsilInstr::ReadUserSlot {..} | CsilInstr::ReadSecurityExpr {..})).cloned() {
                            f.code.push(CsilInstr::Return); f.code.push(row);
                        }
                    }
                }
                10 => {
                    for f in &mut p.functions { for row in &mut f.code {
                        match row {
                            CsilInstr::WindowMean {length,..} => *length = 4,
                            CsilInstr::ReadUserSlot {offset,..} => *offset = 4,
                            CsilInstr::ReadSecurityExpr {expr: pinewasm_hir::HtfExpr::TaInput {default_length,..},..} => *default_length = 4,
                            _ => {}
                        }
                    }}
                }
                11 => p.inputs[0].defval = InputValue::Bool(true),
                _ => {}
            }
            let prefix = ValidationError::Header("existing prefix".into());
            let mut actual = vec![ValidationError::Header("existing prefix".into())];
            let mut old = vec![prefix];
            validate_ta_input_bindings(&p, &mut actual);
            ta_input_bindings_slice_check(&p, &mut old);
            assert_eq!(format!("{actual:?}"), format!("{old:?}"), "mode {mode}");
        }
        }
    }
}

#[cfg(test)]
mod binding_native_empty_tests {
    use super::*;
    use binding_state::Stages;

    fn compile(source: &str) -> CsilProgram {
        let parsed = pinewasm_parse::parse(source);
        assert!(!pinewasm_span::has_errors(&parsed.diagnostics));
        let analysis = pinewasm_semantics::analyze(&parsed.program);
        assert!(!pinewasm_span::has_errors(&analysis.diagnostics));
        crate::lower_mir(&pinewasm_mir::lower(&analysis.hir)).expect("lowers")
    }
    fn bound_program() -> CsilProgram {
        compile("//@version=6\nindicator(\"native bindings\")\nn = input.int(3)\nplot(ta.sma(close, n))")
    }
    #[test]
    fn actual_empty_stages_preserve_existing_diagnostics() {
        let p = compile("//@version=6\nindicator(\"empty\")\nplot(close)");
        assert!(p.ta_input_bindings.is_empty());
        let context = BindingContext { program: &p };
        let secondary = context.new_secondary();
        assert!(secondary.is_empty());
        assert!(context.scan_secondary(secondary).is_empty());
        assert!(context.collect_slots().is_empty());
        assert!(context.new_targets().is_empty());
        let mut errors = Vec::with_capacity(8);
        errors.push(ValidationError::Header("first payload".into()));
        errors.push(ValidationError::Header("second payload".into()));
        let original = vec![
            ValidationError::Header("first payload".into()),
            ValidationError::Header("second payload".into()),
        ];
        let pointer = errors.as_ptr();
        let capacity = errors.capacity();
        context.finish_secondary(&context.new_secondary(), &mut errors);
        assert_eq!(errors, original);
        assert_eq!(errors.as_ptr(), pointer);
        assert_eq!(errors.capacity(), capacity);
    }
    #[test]
    fn complete_unobserved_inventory_preserves_nonempty_maps() {
        let mut p = bound_program();
        let target = p.ta_input_bindings[0].target;
        p.ta_input_bindings.clear();
        for function in &mut p.functions {
            function.code = vec![CsilInstr::Return, CsilInstr::PushI64(7)];
        }
        let mut additional = p.functions[0].clone();
        additional.name = "unreachable-extra".into();
        p.functions.push(additional);
        let context = BindingContext { program: &p };
        let secondary = SecondaryMap::from([(target, (19, 23, 2))]);
        let slots = SlotsMap::from([(target, (3, 29))]);
        assert_eq!(context.scan_secondary(secondary.clone()), secondary);
        assert_eq!(context.scan_slots(slots.clone()), slots);
    }
    #[test]
    fn empty_bindings_do_not_make_nonempty_secondary_finish_inert() {
        let mut p = bound_program();
        let target = p.ta_input_bindings[0].target;
        p.ta_input_bindings.clear();
        let context = BindingContext { program: &p };
        let secondary = SecondaryMap::from([(target, (0, 3, 1))]);
        let mut errors = vec![ValidationError::Header("prefix".into())];
        context.finish_secondary(&secondary, &mut errors);
        assert_eq!(
            errors,
            vec![
                ValidationError::Header("prefix".into()),
                ValidationError::Header(
                    "secondary-context TA input has no matching binding".into()
                )
            ]
        );
    }
    #[test]
    fn observed_rows_after_return_are_still_scanned() {
        let mut p = bound_program();
        let window = p
            .functions
            .iter()
            .flat_map(|f| &f.code)
            .find(|row| matches!(row, CsilInstr::WindowMean { .. }))
            .expect("window row")
            .clone();
        let target = p.ta_input_bindings[0].target;
        for function in &mut p.functions {
            function.code = vec![CsilInstr::Return];
        }
        p.functions[0].code.push(window);
        let context = BindingContext { program: &p };
        let slots = context.scan_slots(context.collect_slots());
        assert_eq!(slots[&target], (1, 3));

        let mut security = compile("//@version=6\nindicator(\"security\")\nn = input.int(3)\nplot(request.security(syminfo.tickerid, \"D\", ta.sma(close, n)))");
        let row = security
            .functions
            .iter()
            .flat_map(|f| &f.code)
            .find(|row| matches!(row, CsilInstr::ReadSecurityExpr { .. }))
            .expect("security row")
            .clone();
        for function in &mut security.functions {
            function.code = vec![CsilInstr::Return];
        }
        security.functions[0].code.push(row);
        let context = BindingContext { program: &security };
        assert!(!context.scan_secondary(context.new_secondary()).is_empty());
    }
}

#[cfg(test)]
mod owned_rule_diff_tests {
    use super::*;
    use crate::TaFunc;
    use pinewasm_hir::{TaInputBinding, TaInputTarget, TaStateDecl};
    use std::cell::Cell;

    // Test-only copy of the slice-based lookup, used to cross-check the new lookup.
    fn previous(
        states: &[TaStateDecl],
        inputs: &[Option<i64>],
        binding: &TaInputBinding,
    ) -> Option<i64> {
        let input = binding.input as usize;
        if input >= inputs.len() {
            return None;
        }
        match inputs[input] {
            Some(value) => {
                let rules = match binding.target {
                    TaInputTarget::State { state, .. } => {
                        let state = state as usize;
                        if state < states.len() {
                            Some(states[state].func.engine_param_rules())
                        } else {
                            None
                        }
                    }
                    TaInputTarget::Sma { .. } | TaInputTarget::Change { .. } => None,
                };
                crate::ta_inputs::resolve_ta_int(
                    value,
                    binding.negated,
                    crate::ta_inputs::ta_target_bounds(binding.target, rules),
                )
            }
            None => None,
        }
    }
    impl InputDefaultFacts for Option<i64> {
        fn default_int(&self) -> Option<i64> {
            *self
        }
    }

    #[test]
    fn owned_rule_matches_historical_slice_for_all_functions_and_bounds() {
        let max = i64::from(MAX_WINDOW_LENGTH);
        let inputs = [
            None,
            Some(i64::MIN),
            Some(i64::MAX),
            Some(-100001),
            Some(-max),
            Some(-14),
            Some(-2),
            Some(-1),
            Some(0),
            Some(1),
            Some(2),
            Some(14),
            Some(100000),
            Some(100001),
            Some(max),
            Some(max + 1),
        ];
        for func in TaFunc::ALL {
            let states = [TaStateDecl {
                func,
                params: Vec::new(),
            }];
            for param in [0, 1, 2, 3, 4, 7, u32::MAX] {
                assert_eq!(
                    states[0].param_rule(param),
                    func.engine_param_rules().get(param as usize).copied(),
                    "{func:?}/{param}"
                );
                for target in [
                    TaInputTarget::State { state: 0, param },
                    TaInputTarget::State { state: 1, param },
                    TaInputTarget::State {
                        state: u32::MAX,
                        param,
                    },
                    TaInputTarget::Sma { slot: u32::MAX },
                    TaInputTarget::Change { slot: u32::MAX },
                ] {
                    for negated in [false, true] {
                        for input in (0..=u32::try_from(inputs.len())
                            .expect("fixed fixture fits u32"))
                            .chain([u32::MAX])
                        {
                            let binding = TaInputBinding {
                                input,
                                negated,
                                target,
                            };
                            assert_eq!(
                                ta_binding_default(&states, &inputs, &binding),
                                previous(&states, &inputs, &binding),
                                "{func:?}/{binding:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    struct ObservedState<'a>(&'a Cell<u32>);
    impl TaStateFacts for ObservedState<'_> {
        fn operand_count(&self) -> usize {
            panic!("not consulted")
        }
        fn result_arity(&self) -> u32 {
            panic!("not consulted")
        }
        fn param_rule(&self, param: u32) -> Option<TaParamRule> {
            self.0.set(self.0.get() + 1);
            if param == 0 {
                Some(TaParamRule::Length { min: 2 })
            } else {
                None
            }
        }
    }
    #[test]
    fn owned_rule_default_first_and_target_bypass() {
        let calls = Cell::new(0);
        let states = [ObservedState(&calls)];
        let mut b = TaInputBinding {
            input: 0,
            negated: false,
            target: TaInputTarget::State { state: 0, param: 0 },
        };
        assert_eq!(ta_binding_default(&states, &[None], &b), None);
        b.input = u32::MAX;
        assert_eq!(ta_binding_default(&states, &[Some(2)], &b), None);
        b.input = 0;
        b.target = TaInputTarget::State {
            state: u32::MAX,
            param: 0,
        };
        assert_eq!(ta_binding_default(&states, &[Some(2)], &b), None);
        for target in [
            TaInputTarget::Sma { slot: u32::MAX },
            TaInputTarget::Change { slot: u32::MAX },
        ] {
            b.target = target;
            assert_eq!(ta_binding_default(&states, &[Some(1)], &b), Some(1));
        }
        assert_eq!(calls.get(), 0);
        b.target = TaInputTarget::State { state: 0, param: 0 };
        assert_eq!(ta_binding_default(&states, &[Some(1)], &b), None);
        assert_eq!(ta_binding_default(&states, &[Some(2)], &b), Some(2));
        b.target = TaInputTarget::State {
            state: 0,
            param: u32::MAX,
        };
        assert_eq!(ta_binding_default(&states, &[Some(2)], &b), None);
        assert_eq!(calls.get(), 3);
    }
}

#[cfg(test)]
mod rendering_ownership_version_tests {
    use super::*;

    fn visual_without_source_order(version: u32) -> Vec<ValidationError> {
        let parsed =
            pinewasm_parse::parse("//@version=6\nindicator(\"ownership\")\nbgcolor(color.red)");
        assert!(!pinewasm_span::has_errors(&parsed.diagnostics));
        let analysis = pinewasm_semantics::analyze(&parsed.program);
        assert!(!pinewasm_span::has_errors(&analysis.diagnostics));
        let mut program = crate::lower_mir(&pinewasm_mir::lower(&analysis.hir)).expect("lowers");
        program.metadata.rendering_contract_version = version;
        program.visuals[0].style.source_order = None;
        let mut errors = Vec::new();
        validate_rendering_ownership(&program, &mut errors);
        errors
    }

    #[test]
    fn ownership_strictness_follows_the_rendering_contract_version() {
        assert!(!rendering_ownership_is_strict(0));
        assert!(rendering_ownership_is_strict(
            crate::RENDERING_CONTRACT_VERSION
        ));
        // A future contract version must not fail open.
        assert!(rendering_ownership_is_strict(
            crate::RENDERING_CONTRACT_VERSION + 1
        ));
        for version in 1..=crate::RENDERING_CONTRACT_VERSION + 1 {
            let errors = format!("{:?}", visual_without_source_order(version));
            assert!(
                errors.contains("visual 0 has missing or reused source order"),
                "version {version}: {errors}"
            );
        }
        assert!(visual_without_source_order(0).is_empty());
    }
}
