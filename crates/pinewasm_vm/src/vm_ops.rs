//! Pure kernels of the interpreter's scalar opcode arms: constants, integer
//! and float arithmetic, comparisons, Kleene logic, `select`, the two
//! conditional-jump tests, the operand decoders behind `pop_float`/`pop_bool`,
//! the dynamic user-slot read, and the operand check of string `+`; and the
//! pure parts of the source-read arms: the market-feed builtin series
//! ([`feed_series_read`]), the `max_bars_back` check of `ReadBuiltinDynamic`
//! ([`dynamic_offset_limit`]), the scalar input values
//! ([`input_scalar_value`]) and `Pop` ([`discard`]).
//!
//! `Vm::exec_instr` keeps the stack pops and pushes (in the same order, with
//! the same errors), the charging, and the error formatting; each arm hands
//! its popped operands to one kernel here. A kernel reports a type mismatch
//! with the unit error [`OpMismatch`], and the arm formats the message from
//! the operands it still holds. Nothing here formats, allocates or touches the
//! VM state, so Charon/Aeneas can extract it (`--features aeneas`, roots in
//! `aeneas_roots_vm_ops`).
//!
//! Every `f64` operation goes through the private module [`vm_f64_ops`]
//! (kept opaque in the extraction, `--opaque=pinewasm_vm::vm_ops::vm_f64_ops`;
//! the extraction model defines its functions on the binary64 bit pattern),
//! and `String` equality through [`vm_str_ops`] (also opaque). Non-finite float
//! results become `na` through `Value::from_f64`.

#[cfg(test)]
use pinewasm_hir::BuiltinSeries;
use pinewasm_mir::Value;

/// A type mismatch found by a kernel; the calling arm formats the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OpMismatch;

/// The shipped float primitives, one function per Rust operation. Opaque in
/// the extraction; the extraction model defines them on the bit pattern.
pub(crate) mod vm_f64_ops {
    #[inline(always)]
    pub(crate) fn add(a: f64, b: f64) -> f64 {
        a + b
    }
    #[inline(always)]
    pub(crate) fn sub(a: f64, b: f64) -> f64 {
        a - b
    }
    #[inline(always)]
    pub(crate) fn mul(a: f64, b: f64) -> f64 {
        a * b
    }
    #[inline(always)]
    pub(crate) fn div(a: f64, b: f64) -> f64 {
        a / b
    }
    #[inline(always)]
    pub(crate) fn rem(a: f64, b: f64) -> f64 {
        a % b
    }
    #[inline(always)]
    pub(crate) fn neg(a: f64) -> f64 {
        -a
    }
    /// `v as f64` (round to nearest).
    #[inline(always)]
    #[allow(clippy::cast_precision_loss)]
    pub(crate) fn from_i64(v: i64) -> f64 {
        v as f64
    }
    /// `a == b` (IEEE: false on NaN, `0.0 == -0.0`).
    #[inline(always)]
    #[allow(clippy::float_cmp)]
    pub(crate) fn eq(a: f64, b: f64) -> bool {
        a == b
    }
    /// `a > b` (false on NaN).
    #[inline(always)]
    pub(crate) fn gt(a: f64, b: f64) -> bool {
        a > b
    }
    /// `a >= b` (false on NaN).
    #[inline(always)]
    pub(crate) fn ge(a: f64, b: f64) -> bool {
        a >= b
    }
    /// `a < b` (false on NaN).
    #[inline(always)]
    pub(crate) fn lt(a: f64, b: f64) -> bool {
        a < b
    }
    /// `a <= b` (false on NaN).
    #[inline(always)]
    pub(crate) fn le(a: f64, b: f64) -> bool {
        a <= b
    }
    /// `f64::midpoint(a, b)` (the `hl2` average).
    #[inline(always)]
    pub(crate) fn midpoint(a: f64, b: f64) -> f64 {
        f64::midpoint(a, b)
    }
    /// The literal `3.0` (the `hlc3` divisor).
    #[inline(always)]
    pub(crate) fn three() -> f64 {
        3.0
    }
    /// The literal `4.0` (the `ohlc4`/`hlcc4` divisor).
    #[inline(always)]
    pub(crate) fn four() -> f64 {
        4.0
    }
    /// A fractional history offset (integer division can produce one):
    /// truncate toward zero (`-0.5` → `0`); `None` for a non-finite value,
    /// a negative truncation, or one not below `u64::MAX as f64`.
    #[inline(always)]
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    pub(crate) fn history_offset(v: f64) -> Option<u64> {
        if v.is_finite() {
            let truncated = v.trunc();
            if truncated >= 0.0 && truncated < u64::MAX as f64 {
                Some(truncated as u64)
            } else {
                None
            }
        } else {
            None
        }
    }
}

/// The shipped string primitive. Opaque in the extraction.
pub(crate) mod vm_str_ops {
    /// `a == b` on the UTF-8 contents. Takes `&String` (not `&str`) so the
    /// extracted kernel has no `Deref` call before this opaque function.
    #[inline(always)]
    #[allow(clippy::ptr_arg)]
    pub(crate) fn eq(a: &String, b: &String) -> bool {
        a == b
    }
}

/// Extraction-only marker, kept opaque, that makes Aeneas emit the
/// hand-written `TypesExternal` module defining `F64`.
#[cfg(feature = "aeneas")]
#[allow(dead_code)]
pub struct F64Model;

/// The operand of a constant push (`PushNa`/`PushBool`/`PushI64`/`PushF64`/
/// `PushColor`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum VmConst {
    Na,
    Bool(bool),
    Int(i64),
    Float(f64),
    Color { r: u8, g: u8, b: u8, t: f64 },
}

/// The pushed value of a constant; a non-finite float literal pushes `na`.
#[inline(always)]
pub(crate) fn const_value(c: VmConst) -> Value {
    match c {
        VmConst::Na => Value::Na,
        VmConst::Bool(b) => Value::Bool(b),
        VmConst::Int(v) => Value::Int(v),
        VmConst::Float(v) => Value::from_f64(v),
        VmConst::Color { r, g, b, t } => Value::Color {
            r,
            g,
            b,
            t,
            input_origin: None,
        },
    }
}

/// `pop_float`'s decode: `na` is `Ok(None)`, a float `Ok(Some(v))`, any
/// other variant a mismatch (handed back for the message). By value, so the
/// interpreter's hot path moves the operand instead of dropping it.
#[inline(always)]
pub(crate) fn float_operand(v: Value) -> Result<Option<f64>, Value> {
    match v {
        Value::Na => Ok(None),
        Value::Float(x) => Ok(Some(x)),
        other => Err(other),
    }
}

/// `pop_bool`'s decode: `na` is `Ok(None)`, a bool `Ok(Some(b))`, any
/// other variant a mismatch (handed back for the message).
#[inline(always)]
pub(crate) fn bool_operand(v: Value) -> Result<Option<bool>, Value> {
    match v {
        Value::Na => Ok(None),
        Value::Bool(x) => Ok(Some(x)),
        other => Err(other),
    }
}

/// A runtime history offset (`Vm::runtime_history_offset`): a
/// non-negative int, or a finite float truncated toward zero; `None` (the
/// read is `na`) for anything else.
#[inline(always)]
pub(crate) fn history_offset(v: &Value) -> Option<u64> {
    match v {
        Value::Int(x) => crate::vm_offset::decode_offset(Some(*x)),
        Value::Float(x) => vm_f64_ops::history_offset(*x),
        _ => None,
    }
}

/// The Pine integer opcodes `IntAdd`/`IntSub`/`IntMul`/`IntRem`/`PineIntDiv`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IntArithOp {
    Add,
    Sub,
    Mul,
    Rem,
    PineDiv,
}

/// The float fallback of a Pine integer opcode once an operand is
/// fractional (or an exact division leaves a remainder).
#[inline(always)]
fn fractional_int_op(op: IntArithOp, lhs: f64, rhs: f64) -> Value {
    let raw = match op {
        IntArithOp::Add => vm_f64_ops::add(lhs, rhs),
        IntArithOp::Sub => vm_f64_ops::sub(lhs, rhs),
        IntArithOp::Mul => vm_f64_ops::mul(lhs, rhs),
        IntArithOp::Rem => vm_f64_ops::rem(lhs, rhs),
        IntArithOp::PineDiv => vm_f64_ops::div(lhs, rhs),
    };
    Value::from_f64(raw)
}

/// Pine `int / int`: the exact quotient when the division is exact, the
/// float quotient otherwise, `na` on division by zero or overflow.
#[inline(always)]
fn pine_int_div(l: i64, r: i64) -> Value {
    // `checked_div` and `checked_rem` fail on exactly the same operands
    // (`r == 0`, and `i64::MIN / -1`); the explicit guard keeps the extracted
    // code independent of the Aeneas model
    // of signed `checked_rem`.
    if r == 0 || (l == i64::MIN && r == -1) {
        return Value::Na;
    }
    match l.checked_div(r) {
        Some(q) => match l.checked_rem(r) {
            Some(0) => Value::Int(q),
            Some(_) => Value::from_f64(vm_f64_ops::div(
                vm_f64_ops::from_i64(l),
                vm_f64_ops::from_i64(r),
            )),
            None => Value::Na,
        },
        None => Value::Na,
    }
}

/// The value of a Pine integer opcode on its popped operands: exact checked
/// `i64` arithmetic on two ints, the float operation once either operand is
/// a float, `na` if either is `na`; any other operand is a mismatch.
#[inline(always)]
pub(crate) fn int_arith(op: IntArithOp, lhs: &Value, rhs: &Value) -> Result<Value, OpMismatch> {
    match (lhs, rhs) {
        (Value::Na, _) | (_, Value::Na) => Ok(Value::Na),
        (Value::Int(l), Value::Int(r)) => Ok(match op {
            IntArithOp::PineDiv => pine_int_div(*l, *r),
            IntArithOp::Add => int_value(crate::vm_int::int_binop(
                crate::vm_int::IntBinOp::Add,
                Some(*l),
                Some(*r),
            )),
            IntArithOp::Sub => int_value(crate::vm_int::int_binop(
                crate::vm_int::IntBinOp::Sub,
                Some(*l),
                Some(*r),
            )),
            IntArithOp::Mul => int_value(crate::vm_int::int_binop(
                crate::vm_int::IntBinOp::Mul,
                Some(*l),
                Some(*r),
            )),
            IntArithOp::Rem => int_value(crate::vm_int::int_binop(
                crate::vm_int::IntBinOp::Rem,
                Some(*l),
                Some(*r),
            )),
        }),
        (Value::Int(l), Value::Float(r)) => Ok(fractional_int_op(op, vm_f64_ops::from_i64(*l), *r)),
        (Value::Float(l), Value::Int(r)) => Ok(fractional_int_op(op, *l, vm_f64_ops::from_i64(*r))),
        (Value::Float(l), Value::Float(r)) => Ok(fractional_int_op(op, *l, *r)),
        _ => Err(OpMismatch),
    }
}

/// `Some(v)` → `Int(v)`, `None` → `na`.
#[inline(always)]
fn int_value(v: Option<i64>) -> Value {
    match v {
        Some(x) => Value::Int(x),
        None => Value::Na,
    }
}

/// `IntNeg`: checked negation of an int, float negation of a float, `na`
/// for `na` (and for `-i64::MIN`); any other operand is a mismatch.
#[inline(always)]
pub(crate) fn int_neg(v: &Value) -> Result<Value, OpMismatch> {
    match v {
        Value::Na => Ok(Value::Na),
        Value::Int(x) => Ok(int_value(crate::vm_int::int_neg(Some(*x)))),
        Value::Float(x) => Ok(Value::from_f64(vm_f64_ops::neg(*x))),
        _ => Err(OpMismatch),
    }
}

/// `IntToF64`: widen an int, keep a float (re-normalized), `na` for `na`;
/// any other operand is a mismatch.
#[inline(always)]
pub(crate) fn int_to_f64(v: &Value) -> Result<Value, OpMismatch> {
    match v {
        Value::Na => Ok(Value::Na),
        Value::Int(x) => Ok(Value::from_f64(vm_f64_ops::from_i64(*x))),
        Value::Float(x) => Ok(Value::from_f64(*x)),
        _ => Err(OpMismatch),
    }
}

/// The float opcodes `F64Add`/`F64Sub`/`F64Mul`/`F64Div`/`F64Rem`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FloatArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

/// The value of a float opcode on `pop_float`'s operands: `na` if either is
/// `na`, otherwise the IEEE result, with a non-finite result (division by
/// zero, overflow, `x % 0`) mapped to `na`.
#[inline(always)]
pub(crate) fn float_arith(op: FloatArithOp, lhs: Option<f64>, rhs: Option<f64>) -> Value {
    match (lhs, rhs) {
        (Some(l), Some(r)) => {
            let raw = match op {
                FloatArithOp::Add => vm_f64_ops::add(l, r),
                FloatArithOp::Sub => vm_f64_ops::sub(l, r),
                FloatArithOp::Mul => vm_f64_ops::mul(l, r),
                FloatArithOp::Div => vm_f64_ops::div(l, r),
                FloatArithOp::Rem => vm_f64_ops::rem(l, r),
            };
            Value::from_f64(raw)
        }
        _ => Value::Na,
    }
}

/// `F64Neg` on `pop_float`'s operand.
#[inline(always)]
pub(crate) fn float_neg(v: Option<f64>) -> Value {
    match v {
        Some(x) => Value::from_f64(vm_f64_ops::neg(x)),
        None => Value::Na,
    }
}

/// The ordering opcodes `CmpGt`/`CmpGe`/`CmpLt`/`CmpLe`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CmpOp {
    Gt,
    Ge,
    Lt,
    Le,
}

#[inline(always)]
fn cmp_int(op: CmpOp, l: i64, r: i64) -> bool {
    match op {
        CmpOp::Gt => l > r,
        CmpOp::Ge => l >= r,
        CmpOp::Lt => l < r,
        CmpOp::Le => l <= r,
    }
}

#[inline(always)]
fn cmp_float_eq(l: f64, r: f64) -> bool {
    vm_f64_ops::eq(l, r) || pinewasm_num::comparison::within_tolerance(l, r)
}

#[inline(always)]
fn cmp_float(op: CmpOp, l: f64, r: f64) -> bool {
    match op {
        CmpOp::Gt => vm_f64_ops::gt(l, r) && !cmp_float_eq(l, r),
        CmpOp::Ge => vm_f64_ops::ge(l, r) || cmp_float_eq(l, r),
        CmpOp::Lt => vm_f64_ops::lt(l, r) && !cmp_float_eq(l, r),
        CmpOp::Le => vm_f64_ops::le(l, r) || cmp_float_eq(l, r),
    }
}

/// The value of an ordering opcode: exact `i64` order on two ints, script
/// tolerance on floats, the int side promoted on a mixed pair; `false` when
/// either operand is `na` or the float order is undefined (NaN).
/// Any other operand is a mismatch.
#[inline(always)]
pub(crate) fn cmp_values(op: CmpOp, lhs: &Value, rhs: &Value) -> Result<Value, OpMismatch> {
    match (lhs, rhs) {
        (Value::Na, _) | (_, Value::Na) => Ok(Value::Bool(false)),
        (Value::Int(l), Value::Int(r)) => Ok(Value::Bool(cmp_int(op, *l, *r))),
        (Value::Float(l), Value::Float(r)) => Ok(Value::Bool(cmp_float(op, *l, *r))),
        (Value::Int(l), Value::Float(r)) => {
            Ok(Value::Bool(cmp_float(op, vm_f64_ops::from_i64(*l), *r)))
        }
        (Value::Float(l), Value::Int(r)) => {
            Ok(Value::Bool(cmp_float(op, *l, vm_f64_ops::from_i64(*r))))
        }
        _ => Err(OpMismatch),
    }
}

/// `CmpEq`/`CmpNe` (`negate`): `false` when either operand is `na` (for
/// both opcodes); an int compared with a float is promoted; two values of
/// the same non-float variant compare structurally; floats use script tolerance;
/// any other pair is a mismatch.
#[inline(always)]
pub(crate) fn cmp_eq(negate: bool, lhs: &Value, rhs: &Value) -> Result<Value, OpMismatch> {
    let eq = match (lhs, rhs) {
        (Value::Na, _) | (_, Value::Na) => return Ok(Value::Bool(false)),
        (Value::Int(l), Value::Float(r)) => cmp_float_eq(vm_f64_ops::from_i64(*l), *r),
        (Value::Float(l), Value::Int(r)) => cmp_float_eq(*l, vm_f64_ops::from_i64(*r)),
        (Value::Bool(l), Value::Bool(r)) => *l == *r,
        (Value::Int(l), Value::Int(r)) => *l == *r,
        (Value::Float(l), Value::Float(r)) => cmp_float_eq(*l, *r),
        (Value::String(l), Value::String(r)) => vm_str_ops::eq(l, r),
        (
            Value::Color {
                r: lr,
                g: lg,
                b: lb,
                t: lt,
                ..
            },
            Value::Color {
                r: rr,
                g: rg,
                b: rb,
                t: rt,
                ..
            },
        ) => *lr == *rr && *lg == *rg && *lb == *rb && *lt == *rt,
        (Value::Ref(l), Value::Ref(r)) => l.0 == r.0,
        _ => return Err(OpMismatch),
    };
    Ok(Value::Bool(if negate { !eq } else { eq }))
}

/// `BoolAnd` on `pop_bool`'s operands: Kleene conjunction over `na`.
#[inline(always)]
pub(crate) fn bool_and(lhs: Option<bool>, rhs: Option<bool>) -> Value {
    match (lhs, rhs) {
        (Some(false), _) | (_, Some(false)) => Value::Bool(false),
        (Some(true), Some(true)) => Value::Bool(true),
        _ => Value::Na,
    }
}

/// `BoolOr` on `pop_bool`'s operands: Kleene disjunction over `na`.
#[inline(always)]
pub(crate) fn bool_or(lhs: Option<bool>, rhs: Option<bool>) -> Value {
    match (lhs, rhs) {
        (Some(true), _) | (_, Some(true)) => Value::Bool(true),
        (Some(false), Some(false)) => Value::Bool(false),
        _ => Value::Na,
    }
}

/// `BoolNot` on `pop_bool`'s operand (`na` stays `na`).
#[inline(always)]
pub(crate) fn bool_not(v: Option<bool>) -> Value {
    match v {
        Some(b) => Value::Bool(!b),
        None => Value::Na,
    }
}

/// `Select`: the then-value when the condition is `true`; an `na` condition
/// selects the else-value, like `JumpIfFalse`.
#[inline(always)]
pub(crate) fn select_value<T>(cond: Option<bool>, then_value: T, else_value: T) -> T {
    match cond {
        Some(true) => then_value,
        _ => else_value,
    }
}

/// `JumpIfFalse`'s test: the branch is taken unless the condition is `true`
/// (an `na` condition counts as false).
#[inline(always)]
pub(crate) fn jump_if_false_taken(cond: Option<bool>) -> bool {
    match cond {
        Some(b) => !b,
        None => true,
    }
}

/// `JumpIfExact`'s test: taken only when the condition is exactly `on`
/// (`na` never jumps).
#[inline(always)]
pub(crate) fn jump_if_exact_taken(cond: Option<bool>, on: bool) -> bool {
    match cond {
        Some(b) => b == on,
        None => false,
    }
}

/// `ReadUserSlotDynamic` after the offset pop: decode the offset
/// ([`history_offset`]), enforce
/// `limit` (`max_bars_back`; `Err(offset)` is the structured error), and
/// read the ring cell (`None`, i.e. `na`, before bar 0, for a non-integer or
/// negative offset, and for a slot or cell out of range).
#[inline(always)]
pub(crate) fn user_slot_dynamic<T: Clone>(
    slots: &[Vec<T>],
    offset: &Value,
    limit: u32,
    bar: usize,
    mask: usize,
    slot: usize,
) -> Result<Option<T>, u64> {
    let read = match offset {
        Value::Int(raw) => crate::vm_offset::dynamic_read(Some(*raw), limit, bar, mask),
        other => crate::vm_offset::dynamic_read_decoded(history_offset(other), limit, bar, mask),
    };
    match read {
        crate::vm_offset::DynamicRead::Na => Ok(None),
        crate::vm_offset::DynamicRead::TooDeep(o) => Err(o),
        crate::vm_offset::DynamicRead::Cell(cell) => {
            if slot < slots.len() {
                let history = &slots[slot];
                if cell < history.len() {
                    Ok(Some(history[cell].clone()))
                } else {
                    Ok(None)
                }
            } else {
                Ok(None)
            }
        }
    }
}

/// String `+`'s operand check: `Ok(None)` (the result is `na`) when either
/// operand is `na`, `Ok(Some((l, r)))` for two strings (the arm then applies
/// the length cap, charges and builds), a mismatch otherwise.
#[inline(always)]
#[allow(clippy::type_complexity)]
pub(crate) fn str_concat_operands<'a>(
    lhs: &'a Value,
    rhs: &'a Value,
) -> Result<Option<(&'a String, &'a String)>, OpMismatch> {
    match (lhs, rhs) {
        (Value::Na, _) | (_, Value::Na) => Ok(None),
        (Value::String(l), Value::String(r)) => Ok(Some((l, r))),
        _ => Err(OpMismatch),
    }
}

/// The builtin series whose value is a pure function of the market-data
/// columns, the referenced bar index and the bar count (`Vm::read_builtin`
/// hands them to [`feed_series_read`]; `close` keeps its own kernel,
/// `vm_close_step::close_value`). Every other series (strategy snapshot,
/// calendar, `barstate.*` beyond `isfirst`, `timeframe.*`, `syminfo.*`,
/// `time_close`/`timenow`, host fundamentals and events) stays in the arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FeedSeries {
    Open,
    High,
    Low,
    Volume,
    Hl2,
    Hlc3,
    Ohlc4,
    Hlcc4,
    BarIndex,
    Time,
    LastBarTime,
    LastBarIndex,
    BarstateIsfirst,
    Bid,
    Ask,
}

/// The feed series of a builtin series: `Some` exactly for the series of
/// [`FeedSeries`]. `Vm::read_builtin` dispatches with the same table (one
/// arm per series, a constant argument each); the tests check the two
/// against each other on every `BuiltinSeries`. Not an extraction root (it
/// reads the HIR enum).
#[cfg(test)]
pub(crate) fn feed_series_of(source: BuiltinSeries) -> Option<FeedSeries> {
    match source {
        BuiltinSeries::Open => Some(FeedSeries::Open),
        BuiltinSeries::High => Some(FeedSeries::High),
        BuiltinSeries::Low => Some(FeedSeries::Low),
        BuiltinSeries::Volume => Some(FeedSeries::Volume),
        BuiltinSeries::Hl2 => Some(FeedSeries::Hl2),
        BuiltinSeries::Hlc3 => Some(FeedSeries::Hlc3),
        BuiltinSeries::Ohlc4 => Some(FeedSeries::Ohlc4),
        BuiltinSeries::Hlcc4 => Some(FeedSeries::Hlcc4),
        BuiltinSeries::BarIndex => Some(FeedSeries::BarIndex),
        BuiltinSeries::Time => Some(FeedSeries::Time),
        BuiltinSeries::LastBarTime => Some(FeedSeries::LastBarTime),
        BuiltinSeries::LastBarIndex => Some(FeedSeries::LastBarIndex),
        BuiltinSeries::BarstateIsfirst => Some(FeedSeries::BarstateIsfirst),
        BuiltinSeries::Bid => Some(FeedSeries::Bid),
        BuiltinSeries::Ask => Some(FeedSeries::Ask),
        _ => None,
    }
}

/// The market-data columns a feed series reads (`MarketData`'s lanes).
pub(crate) struct FeedCols<'a> {
    pub(crate) open: &'a [f64],
    pub(crate) high: &'a [f64],
    pub(crate) low: &'a [f64],
    pub(crate) close: &'a [f64],
    pub(crate) volume: &'a [f64],
    pub(crate) time: &'a [i64],
    pub(crate) bid: &'a [f64],
    pub(crate) ask: &'a [f64],
}

/// `MarketData::time_at`: the open time (epoch ms) of bar `index` from the
/// `time` lane, or the synthesized daily sequence when the lane is empty.
#[inline(always)]
#[allow(clippy::len_zero, clippy::cast_possible_wrap)]
pub(crate) fn time_at(time: &[i64], index: usize) -> i64 {
    // `len() == 0` rather than `is_empty()`: the extraction has `len`.
    if time.len() == 0 {
        crate::SYNTHETIC_EPOCH_MS + (index as i64) * crate::MS_PER_DAY
    } else {
        time[index]
    }
}

/// `Vm::read_builtin` for a feed series at `bar - offset`: `na` before bar
/// 0; otherwise the four OHLC loads (in that order, so a short lane panics
/// at the same index) and the series value at that index.
#[inline(always)]
pub(crate) fn feed_series_read(
    series: FeedSeries,
    bar: usize,
    offset: u32,
    bars: usize,
    cols: &FeedCols<'_>,
) -> Value {
    let back = offset as usize;
    if back <= bar {
        feed_series_value(series, bar - back, bars, cols)
    } else {
        Value::Na
    }
}

/// The value of a feed series at bar `index`. Non-finite data is `na`
/// (`Value::from_f64`); `bar_index`, `time`, `last_bar_*` are series ints;
/// `bid`/`ask` are `na` past the end of their (optional) lane.
#[inline(always)]
#[allow(clippy::cast_possible_wrap)]
pub(crate) fn feed_series_value(
    series: FeedSeries,
    index: usize,
    bars: usize,
    cols: &FeedCols<'_>,
) -> Value {
    let o = cols.open[index];
    let h = cols.high[index];
    let l = cols.low[index];
    let c = cols.close[index];
    match series {
        FeedSeries::Open => Value::from_f64(o),
        FeedSeries::High => Value::from_f64(h),
        FeedSeries::Low => Value::from_f64(l),
        FeedSeries::Volume => Value::from_f64(cols.volume[index]),
        FeedSeries::Hl2 => Value::from_f64(vm_f64_ops::midpoint(h, l)),
        FeedSeries::Hlc3 => Value::from_f64(vm_f64_ops::div(
            vm_f64_ops::add(vm_f64_ops::add(h, l), c),
            vm_f64_ops::three(),
        )),
        FeedSeries::Ohlc4 => Value::from_f64(vm_f64_ops::div(
            vm_f64_ops::add(vm_f64_ops::add(vm_f64_ops::add(o, h), l), c),
            vm_f64_ops::four(),
        )),
        FeedSeries::Hlcc4 => Value::from_f64(vm_f64_ops::div(
            vm_f64_ops::add(vm_f64_ops::add(vm_f64_ops::add(h, l), c), c),
            vm_f64_ops::four(),
        )),
        FeedSeries::BarIndex => Value::Int(index as i64),
        FeedSeries::Time => Value::Int(time_at(cols.time, index)),
        FeedSeries::LastBarTime => Value::Int(time_at(cols.time, bars - 1)),
        FeedSeries::LastBarIndex => Value::Int(bars as i64 - 1),
        FeedSeries::BarstateIsfirst => Value::Bool(index == 0),
        FeedSeries::Bid => {
            if index < cols.bid.len() {
                Value::from_f64(cols.bid[index])
            } else {
                Value::Na
            }
        }
        FeedSeries::Ask => {
            if index < cols.ask.len() {
                Value::from_f64(cols.ask[index])
            } else {
                Value::Na
            }
        }
    }
}

/// `ReadBuiltinDynamic`'s `max_bars_back` check on a decoded offset
/// ([`history_offset`]): `Some(offset)` (the arm reads the series that many
/// bars back) when it is at most `limit`, `None` (the structured error)
/// otherwise.
#[inline(always)]
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn dynamic_offset_limit(offset: u64, limit: u32) -> Option<u32> {
    if offset > limit as u64 {
        None
    } else {
        Some(offset as u32)
    }
}

/// A resolved input value of a scalar kind (`InputValue` without its
/// string and source cases, which stay in `Vm::resolve_input`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum InputScalar {
    Int(i64),
    Float(f64),
    Bool(bool),
    Color { r: u8, g: u8, b: u8, t: f64 },
    Enum(u32),
}

/// `Vm::resolve_input`'s value of a scalar input: an int on a `float`/
/// `price` input (`float_kind`) widens; a float is normalized (non-finite
/// is `na`); an enum is its member's variant index.
#[inline(always)]
pub(crate) fn input_scalar_value(x: InputScalar, float_kind: bool) -> Value {
    match x {
        InputScalar::Int(i) => {
            if float_kind {
                Value::from_f64(vm_f64_ops::from_i64(i))
            } else {
                Value::Int(i)
            }
        }
        InputScalar::Float(f) => Value::from_f64(f),
        InputScalar::Bool(b) => Value::Bool(b),
        InputScalar::Color { r, g, b, t } => Value::Color {
            r,
            g,
            b,
            t,
            input_origin: None,
        },
        InputScalar::Enum(index) => Value::Int(index as i64),
    }
}

/// `Pop`: the popped value is dropped (by value).
#[inline(always)]
pub(crate) fn discard(_v: Value) {}

// ---- Residual builtin series, scalar input override and external-source sample
//
// What stays in the arms: the text series (`syminfo.*` strings,
// `timeframe.period`, `strategy.account_currency`,
// `strategy.position_entry_name`), the two string parses (the exchange
// timezone and the chart timeframe, whose results these kernels take as
// arguments), and the broker's snapshot lookup.

// Imports for the items below.
use pinewasm_broker::StrategySnapshot;
use pinewasm_hir::InputKind;

/// `Vm::read_builtin`'s referenced bar, `bar - offset`: `None` (the read is
/// `na`) before bar 0.
#[inline(always)]
pub(crate) fn history_index(bar: usize, offset: u32) -> Option<usize> {
    bar.checked_sub(offset as usize)
}

/// An optional host integer: `na` when absent.
#[inline(always)]
pub(crate) fn opt_int_value(x: Option<i64>) -> Value {
    match x {
        Some(i) => Value::Int(i),
        None => Value::Na,
    }
}

/// An optional host float: `na` when absent or non-finite.
#[inline(always)]
pub(crate) fn opt_float_value(x: Option<f64>) -> Value {
    match x {
        Some(f) => Value::from_f64(f),
        None => Value::Na,
    }
}

/// The numeric `strategy.*` getters (every strategy series but the two text
/// ones, `strategy.account_currency` and `strategy.position_entry_name`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StrategySeries {
    PositionSize,
    PositionAvgPrice,
    OpenProfit,
    NetProfit,
    GrossProfit,
    GrossLoss,
    Equity,
    InitialCapital,
    WinTrades,
    LossTrades,
    EvenTrades,
    ClosedTrades,
    OpenTrades,
    MaxDrawdown,
    MaxRunup,
    AvgTrade,
    AvgWinningTrade,
    AvgLosingTrade,
    MaxContractsHeldAll,
    MaxContractsHeldLong,
    MaxContractsHeldShort,
    AvgLosingTradePercent,
    AvgTradePercent,
    AvgWinningTradePercent,
    ClosedTradesFirstIndex,
    GrossLossPercent,
    GrossProfitPercent,
    MarginLiquidationPrice,
    MaxDrawdownPercent,
    MaxRunupPercent,
    NetProfitPercent,
    OpenProfitPercent,
    OpenTradesCapitalHeld,
}

/// `Vm::read_strategy` on a strategy snapshot: the getter's field; a float
/// metric that is `NaN` (flat position, no trades) or infinite is `na`.
#[inline(always)]
pub(crate) fn strategy_series_value(series: StrategySeries, snap: &StrategySnapshot) -> Value {
    match series {
        StrategySeries::PositionSize => Value::from_f64(snap.position_size),
        StrategySeries::PositionAvgPrice => Value::from_f64(snap.position_avg_price),
        StrategySeries::OpenProfit => Value::from_f64(snap.open_profit),
        StrategySeries::NetProfit => Value::from_f64(snap.net_profit),
        StrategySeries::GrossProfit => Value::from_f64(snap.gross_profit),
        StrategySeries::GrossLoss => Value::from_f64(snap.gross_loss),
        StrategySeries::Equity => Value::from_f64(snap.equity),
        StrategySeries::InitialCapital => Value::from_f64(snap.initial_capital),
        StrategySeries::WinTrades => Value::Int(snap.win_trades),
        StrategySeries::LossTrades => Value::Int(snap.loss_trades),
        StrategySeries::EvenTrades => Value::Int(snap.even_trades),
        StrategySeries::ClosedTrades => Value::Int(snap.closed_trades),
        StrategySeries::OpenTrades => Value::Int(snap.open_trades),
        StrategySeries::MaxDrawdown => Value::from_f64(snap.max_drawdown),
        StrategySeries::MaxRunup => Value::from_f64(snap.max_runup),
        StrategySeries::AvgTrade => Value::from_f64(snap.avg_trade),
        StrategySeries::AvgWinningTrade => Value::from_f64(snap.avg_winning_trade),
        StrategySeries::AvgLosingTrade => Value::from_f64(snap.avg_losing_trade),
        StrategySeries::MaxContractsHeldAll => Value::from_f64(snap.max_contracts_held),
        StrategySeries::MaxContractsHeldLong => Value::from_f64(snap.max_contracts_held_long),
        StrategySeries::MaxContractsHeldShort => Value::from_f64(snap.max_contracts_held_short),
        StrategySeries::AvgLosingTradePercent => Value::from_f64(snap.avg_losing_trade_percent),
        StrategySeries::AvgTradePercent => Value::from_f64(snap.avg_trade_percent),
        StrategySeries::AvgWinningTradePercent => Value::from_f64(snap.avg_winning_trade_percent),
        StrategySeries::ClosedTradesFirstIndex => Value::Int(snap.closed_trades_first_index),
        StrategySeries::GrossLossPercent => Value::from_f64(snap.gross_loss_percent),
        StrategySeries::GrossProfitPercent => Value::from_f64(snap.gross_profit_percent),
        StrategySeries::MarginLiquidationPrice => Value::from_f64(snap.margin_liquidation_price),
        StrategySeries::MaxDrawdownPercent => Value::from_f64(snap.max_drawdown_percent),
        StrategySeries::MaxRunupPercent => Value::from_f64(snap.max_runup_percent),
        StrategySeries::NetProfitPercent => Value::from_f64(snap.net_profit_percent),
        StrategySeries::OpenProfitPercent => Value::from_f64(snap.open_profit_percent),
        StrategySeries::OpenTradesCapitalHeld => Value::from_f64(snap.open_trades_capital_held),
    }
}

/// The calendar series: a part of the bar's open time in the exchange's
/// fixed-offset timezone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CalendarPart {
    DayOfWeek,
    DayOfMonth,
    Month,
    Year,
    Hour,
    Minute,
    Second,
    WeekOfYear,
}

/// A calendar series at a bar whose open time is `time_ms` (epoch ms), in
/// the timezone `off` seconds east of UTC (the arm parses the exchange
/// timezone string and passes the result). `weekofyear` is `na` when the
/// date's ISO Thursday is outside the `i64` epoch-ms range.
#[inline(always)]
pub(crate) fn calendar_series_value(part: CalendarPart, time_ms: i64, off: i64) -> Value {
    let (year, month, day, hour, minute, second, dow) =
        crate::session::civil_from_epoch_ms(time_ms, off);
    match part {
        CalendarPart::DayOfWeek => Value::Int(dow),
        CalendarPart::DayOfMonth => Value::Int(day),
        CalendarPart::Month => Value::Int(month),
        CalendarPart::Year => Value::Int(year),
        CalendarPart::Hour => Value::Int(hour),
        CalendarPart::Minute => Value::Int(minute),
        CalendarPart::Second => Value::Int(second),
        CalendarPart::WeekOfYear => opt_int_value(crate::session::week_of_year_at(time_ms, off)),
    }
}

/// The realtime state the `barstate.*` predicates read (`Vm`'s fields of
/// the same names, and the bar count).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BarState {
    /// The open realtime bar, if a realtime bar is open.
    pub(crate) realtime_bar: Option<usize>,
    /// Whether realtime ticks are replaying.
    pub(crate) realtime_active: bool,
    /// Whether the run has a realtime segment.
    pub(crate) has_realtime: bool,
    /// The current tick is the realtime bar's opening tick.
    pub(crate) bar_is_new: bool,
    /// The current tick is the realtime bar's closing tick.
    pub(crate) bar_is_confirmed: bool,
    /// The bar count of the run.
    pub(crate) bars: usize,
}

/// The `barstate.*` series other than `barstate.isfirst` (a feed series).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BarstateSeries {
    Islast,
    Ishistory,
    Isrealtime,
    Isconfirmed,
    Isnew,
    Islastconfirmedhistory,
}

/// Whether `index` is the open realtime bar.
#[inline(always)]
fn is_realtime_bar(index: usize, realtime_bar: Option<usize>) -> bool {
    match realtime_bar {
        Some(rt) => index == rt,
        None => false,
    }
}

/// `barstate.islast`: while a realtime bar is open it is the only last bar;
/// otherwise the last historical bar, unless a realtime segment follows.
#[inline(always)]
pub(crate) fn is_last_bar(index: usize, st: &BarState) -> bool {
    match st.realtime_bar {
        Some(rt) => index == rt,
        None => !st.has_realtime && index + 1 == st.bars,
    }
}

/// `barstate.ishistory`: every bar but the open realtime bar.
#[inline(always)]
pub(crate) fn is_history(index: usize, st: &BarState) -> bool {
    match st.realtime_bar {
        Some(rt) => index != rt,
        None => true,
    }
}

/// `barstate.isnew`: a committed bar is new; the realtime bar is new only on
/// its opening tick.
#[inline(always)]
pub(crate) fn is_new(index: usize, st: &BarState) -> bool {
    if st.realtime_active && is_realtime_bar(index, st.realtime_bar) {
        st.bar_is_new
    } else {
        true
    }
}

/// `barstate.isconfirmed`: a committed bar is confirmed; the realtime bar
/// only on its closing tick.
#[inline(always)]
pub(crate) fn is_confirmed(index: usize, st: &BarState) -> bool {
    if st.realtime_active && is_realtime_bar(index, st.realtime_bar) {
        st.bar_is_confirmed
    } else {
        true
    }
}

/// `barstate.islastconfirmedhistory`: the final historical bar, including
/// the dataset's last bar when there is no realtime segment.
#[inline(always)]
pub(crate) fn is_last_confirmed_history(index: usize, st: &BarState) -> bool {
    is_history(index, st) && index + 1 == st.bars
}

/// A `barstate.*` series at the referenced bar `index`.
#[inline(always)]
pub(crate) fn barstate_value(series: BarstateSeries, index: usize, st: &BarState) -> Value {
    match series {
        BarstateSeries::Islast => Value::Bool(is_last_bar(index, st)),
        BarstateSeries::Ishistory => Value::Bool(is_history(index, st)),
        BarstateSeries::Isrealtime => Value::Bool(!is_history(index, st)),
        BarstateSeries::Isconfirmed => Value::Bool(is_confirmed(index, st)),
        BarstateSeries::Isnew => Value::Bool(is_new(index, st)),
        BarstateSeries::Islastconfirmedhistory => Value::Bool(is_last_confirmed_history(index, st)),
    }
}

/// The `timeframe.*` series computed from the parsed chart timeframe
/// (`timeframe.period`/`main_period` are text and stay in the arm;
/// `timeframe.isticks` is [`BarFlag::TimeframeIsticks`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TimeframeSeries {
    Multiplier,
    Isseconds,
    Isminutes,
    Isdaily,
    Isweekly,
    Ismonthly,
    Isdwm,
    Isintraday,
}

/// A `timeframe.*` series from the parsed chart timeframe: its multiplier
/// `mult` and trailing unit letter `unit` (`session::timeframe_parts`; the
/// arm parses the string).
#[inline(always)]
pub(crate) fn timeframe_series_value(
    series: TimeframeSeries,
    mult: i64,
    unit: Option<char>,
) -> Value {
    use crate::session::TfClass;
    let class = crate::session::tf_class_of_unit(unit);
    match series {
        TimeframeSeries::Multiplier => Value::Int(mult),
        TimeframeSeries::Isseconds => Value::Bool(matches!(class, TfClass::Seconds)),
        TimeframeSeries::Isminutes => Value::Bool(matches!(class, TfClass::Minutes)),
        TimeframeSeries::Isdaily => Value::Bool(matches!(class, TfClass::Daily)),
        TimeframeSeries::Isweekly => Value::Bool(matches!(class, TfClass::Weekly)),
        TimeframeSeries::Ismonthly => Value::Bool(matches!(class, TfClass::Monthly)),
        TimeframeSeries::Isdwm => Value::Bool(matches!(
            class,
            TfClass::Daily | TfClass::Weekly | TfClass::Monthly
        )),
        TimeframeSeries::Isintraday => {
            Value::Bool(matches!(class, TfClass::Seconds | TfClass::Minutes))
        }
    }
}

/// The predicates that depend on the referenced bar's position only: the
/// `session.*` series (this VM models one continuous regular session) and
/// `timeframe.isticks` (no tick timeframes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BarFlag {
    /// `session.isfirstbar` and `session.isfirstbar_regular`.
    SessionIsfirstbar,
    /// `session.islastbar` and `session.islastbar_regular`.
    SessionIslastbar,
    SessionIsmarket,
    SessionIspremarket,
    SessionIspostmarket,
    TimeframeIsticks,
}

/// A [`BarFlag`] series at the referenced bar `index` of a run of `bars`.
#[inline(always)]
pub(crate) fn bar_flag_value(flag: BarFlag, index: usize, bars: usize) -> Value {
    match flag {
        BarFlag::SessionIsfirstbar => Value::Bool(index == 0),
        BarFlag::SessionIslastbar => Value::Bool(index + 1 == bars),
        BarFlag::SessionIsmarket => Value::Bool(true),
        BarFlag::SessionIspremarket | BarFlag::SessionIspostmarket | BarFlag::TimeframeIsticks => {
            Value::Bool(false)
        }
    }
}

/// `MarketData::time_close_at`, the supplied lane: `Some(time_close[index])`
/// when the host supplied `timeClose`, `None` (the caller derives the close
/// from the open time) when the lane is empty. Panics on an index past a
/// nonempty lane.
#[inline(always)]
#[allow(clippy::len_zero)]
pub(crate) fn time_close_lane(time_close: &[i64], index: usize) -> Option<i64> {
    // `len() == 0` rather than `is_empty()`: the extraction has `len`.
    if time_close.len() == 0 {
        None
    } else {
        Some(time_close[index])
    }
}

/// `MarketData::time_close_at`, the fallback: the bar's open time plus the
/// nominal timeframe duration (`tf_seconds`, parsed by the caller),
/// saturating.
#[inline(always)]
pub(crate) fn time_close_fallback(open_ms: i64, tf_seconds: i64) -> i64 {
    open_ms.saturating_add(tf_seconds.saturating_mul(1000))
}

/// `MarketData::trading_day_at`, the supplied lane: the host's trading day
/// of bar `index`, `None` past the end of the (optional) lane.
#[inline(always)]
pub(crate) fn trading_day_lane(trading_day: &[i64], index: usize) -> Option<i64> {
    if index < trading_day.len() {
        Some(trading_day[index])
    } else {
        None
    }
}

/// `MarketData::fallback_trading_day_for_time`: the exchange-local calendar
/// date of `time` (epoch ms) at 00:00 UTC, in the timezone `off` seconds
/// east of UTC (parsed by the caller).
#[inline(always)]
pub(crate) fn fallback_trading_day(time: i64, off: i64) -> i64 {
    let local_seconds = time.div_euclid(1000).saturating_add(off);
    let day_start_local = local_seconds.div_euclid(86_400) * 86_400;
    day_start_local.saturating_mul(1000)
}

/// The numeric host-metadata series: `syminfo.*` (numbers and fundamentals),
/// the chart viewport times and the upcoming dividend/earnings events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostSeries {
    SyminfoMintick,
    SyminfoMinmove,
    SyminfoPricescale,
    SyminfoPointvalue,
    SyminfoMincontract,
    SyminfoExpirationDate,
    SyminfoEmployees,
    SyminfoShareholders,
    SyminfoSharesOutstandingFloat,
    SyminfoSharesOutstandingTotal,
    SyminfoRecommendationsBuy,
    SyminfoRecommendationsBuyStrong,
    SyminfoRecommendationsDate,
    SyminfoRecommendationsHold,
    SyminfoRecommendationsSell,
    SyminfoRecommendationsSellStrong,
    SyminfoRecommendationsTotal,
    SyminfoTargetPriceAverage,
    SyminfoTargetPriceDate,
    SyminfoTargetPriceEstimates,
    SyminfoTargetPriceHigh,
    SyminfoTargetPriceLow,
    SyminfoTargetPriceMedian,
    ChartLeftVisibleBarTime,
    ChartRightVisibleBarTime,
    DividendsFutureAmount,
    DividendsFutureExDate,
    DividendsFuturePayDate,
    EarningsFutureEps,
    EarningsFuturePeriodEndTime,
    EarningsFutureRevenue,
    EarningsFutureTime,
}

/// The host metadata a [`HostSeries`] reads (`MarketData`'s fields).
pub(crate) struct HostCols<'a> {
    pub(crate) syminfo: &'a crate::SymInfo,
    pub(crate) events: &'a crate::UpcomingEvents,
    pub(crate) mintick: f64,
    pub(crate) chart_left_visible_bar_time: Option<i64>,
    pub(crate) chart_right_visible_bar_time: Option<i64>,
}

/// A numeric host-metadata series: the same value on every bar; an absent
/// optional field and a non-finite float are `na`.
#[inline(always)]
pub(crate) fn host_series_value(series: HostSeries, cols: &HostCols<'_>) -> Value {
    let s = cols.syminfo;
    let e = cols.events;
    match series {
        HostSeries::SyminfoMintick => Value::from_f64(cols.mintick),
        HostSeries::SyminfoMinmove => Value::Int(s.minmove),
        HostSeries::SyminfoPricescale => Value::Int(s.pricescale),
        HostSeries::SyminfoPointvalue => Value::from_f64(s.pointvalue),
        HostSeries::SyminfoMincontract => Value::from_f64(s.mincontract),
        HostSeries::SyminfoExpirationDate => opt_int_value(s.expiration_date),
        HostSeries::SyminfoEmployees => opt_int_value(s.employees),
        HostSeries::SyminfoShareholders => opt_int_value(s.shareholders),
        HostSeries::SyminfoSharesOutstandingFloat => opt_float_value(s.shares_outstanding_float),
        HostSeries::SyminfoSharesOutstandingTotal => opt_int_value(s.shares_outstanding_total),
        HostSeries::SyminfoRecommendationsBuy => opt_int_value(s.recommendations_buy),
        HostSeries::SyminfoRecommendationsBuyStrong => opt_int_value(s.recommendations_buy_strong),
        HostSeries::SyminfoRecommendationsDate => opt_int_value(s.recommendations_date),
        HostSeries::SyminfoRecommendationsHold => opt_int_value(s.recommendations_hold),
        HostSeries::SyminfoRecommendationsSell => opt_int_value(s.recommendations_sell),
        HostSeries::SyminfoRecommendationsSellStrong => {
            opt_int_value(s.recommendations_sell_strong)
        }
        HostSeries::SyminfoRecommendationsTotal => opt_int_value(s.recommendations_total),
        HostSeries::SyminfoTargetPriceAverage => opt_float_value(s.target_price_average),
        HostSeries::SyminfoTargetPriceDate => opt_int_value(s.target_price_date),
        HostSeries::SyminfoTargetPriceEstimates => opt_float_value(s.target_price_estimates),
        HostSeries::SyminfoTargetPriceHigh => opt_float_value(s.target_price_high),
        HostSeries::SyminfoTargetPriceLow => opt_float_value(s.target_price_low),
        HostSeries::SyminfoTargetPriceMedian => opt_float_value(s.target_price_median),
        HostSeries::ChartLeftVisibleBarTime => opt_int_value(cols.chart_left_visible_bar_time),
        HostSeries::ChartRightVisibleBarTime => opt_int_value(cols.chart_right_visible_bar_time),
        HostSeries::DividendsFutureAmount => opt_float_value(e.dividends_future_amount),
        HostSeries::DividendsFutureExDate => opt_int_value(e.dividends_future_ex_date),
        HostSeries::DividendsFuturePayDate => opt_int_value(e.dividends_future_pay_date),
        HostSeries::EarningsFutureEps => opt_float_value(e.earnings_future_eps),
        HostSeries::EarningsFuturePeriodEndTime => opt_int_value(e.earnings_future_period_end_time),
        HostSeries::EarningsFutureRevenue => opt_float_value(e.earnings_future_revenue),
        HostSeries::EarningsFutureTime => opt_int_value(e.earnings_future_time),
    }
}

/// `input_value_matches_decl` for a scalar override: whether the value may
/// replace the default of an input declared with `kind`. An int fits `int`,
/// `float`, `price` and `time`; a float must be finite; a color's
/// transparency is at most 100. (An enum override is checked by the arm
/// against the declared names and options, never here.)
#[inline(always)]
pub(crate) fn input_scalar_matches(x: InputScalar, kind: InputKind) -> bool {
    match x {
        InputScalar::Int(_) => matches!(
            kind,
            InputKind::Int | InputKind::Float | InputKind::Price | InputKind::Time
        ),
        InputScalar::Float(f) => match kind {
            // Finite exactly when `from_f64` keeps it (non-finite is `na`).
            InputKind::Float | InputKind::Price => matches!(Value::from_f64(f), Value::Float(_)),
            _ => false,
        },
        InputScalar::Bool(_) => matches!(kind, InputKind::Bool),
        InputScalar::Color { t, .. } => match kind {
            InputKind::Color => t.is_finite() && (0.0..=100.0).contains(&t),
            _ => false,
        },
        InputScalar::Enum(_) => false,
    }
}

/// The current-bar sample of a host-supplied external source lane
/// (`input.source` bound to a series the host supplies): `na` for a missing
/// sample or a bar past the lane.
#[inline(always)]
pub(crate) fn external_sample_value(values: &[Option<f64>], bar: usize) -> Value {
    if bar < values.len() {
        opt_float_value(values[bar])
    } else {
        Value::Na
    }
}

/// Differential tests: every kernel against a verbatim copy of the
/// `exec_instr`/`host_exec` arm body it was extracted from, over special and
/// random operands (`na`, NaN, ±inf, ±0, subnormals, `i64::MIN`/`MAX`, mixed
/// int/float, and every other variant).
#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm_offset;
    use pinewasm_csil::CsilInstr;
    use pinewasm_mir::HandleId;

    // ---- Test-only baseline copies of the arm bodies -----------------------

    /// Baseline copy of the float fallback of the integer opcodes.
    fn old_pine_fractional_int_op(op: &CsilInstr, lhs: f64, rhs: f64) -> Value {
        let raw = match op {
            CsilInstr::IntAdd => lhs + rhs,
            CsilInstr::IntSub => lhs - rhs,
            CsilInstr::IntMul => lhs * rhs,
            CsilInstr::IntRem => lhs % rhs,
            CsilInstr::PineIntDiv => lhs / rhs,
            _ => unreachable!("fractional int arithmetic opcode"),
        };
        Value::from_f64(raw)
    }

    /// Baseline copy of the `IntAdd | IntSub | IntMul | IntRem | PineIntDiv`
    /// arm after its two pops (`Err(())` is the type-mismatch error).
    fn old_int_arith(instr: &CsilInstr, lhs: &Value, rhs: &Value) -> Result<Value, ()> {
        let value = match (lhs, rhs) {
            (Value::Na, _) | (_, Value::Na) => Value::Na,
            (Value::Int(l), Value::Int(r)) => match instr {
                CsilInstr::PineIntDiv => match (l.checked_div(*r), l.checked_rem(*r)) {
                    (Some(q), Some(0)) => Value::Int(q),
                    (Some(_), Some(_)) => Value::from_f64(*l as f64 / *r as f64),
                    _ => Value::Na,
                },
                _ => {
                    let op = match instr {
                        CsilInstr::IntAdd => crate::vm_int::IntBinOp::Add,
                        CsilInstr::IntSub => crate::vm_int::IntBinOp::Sub,
                        CsilInstr::IntMul => crate::vm_int::IntBinOp::Mul,
                        CsilInstr::IntRem => crate::vm_int::IntBinOp::Rem,
                        _ => unreachable!(),
                    };
                    crate::vm_int::int_binop(op, Some(*l), Some(*r)).map_or(Value::Na, Value::Int)
                }
            },
            (Value::Int(l), Value::Float(r)) => old_pine_fractional_int_op(instr, *l as f64, *r),
            (Value::Float(l), Value::Int(r)) => old_pine_fractional_int_op(instr, *l, *r as f64),
            (Value::Float(l), Value::Float(r)) => old_pine_fractional_int_op(instr, *l, *r),
            _ => return Err(()),
        };
        Ok(value)
    }

    fn old_int_neg(v: Value) -> Result<Value, ()> {
        Ok(match v {
            Value::Na => Value::Na,
            Value::Int(v) => crate::vm_int::int_neg(Some(v)).map_or(Value::Na, Value::Int),
            Value::Float(v) => Value::from_f64(-v),
            _ => return Err(()),
        })
    }

    fn old_int_to_f64(v: Value) -> Result<Value, ()> {
        Ok(match v {
            Value::Na => Value::Na,
            Value::Int(v) => Value::from_f64(v as f64),
            Value::Float(v) => Value::from_f64(v),
            _ => return Err(()),
        })
    }

    /// Baseline copy of the `pop_float` decode after the pop.
    fn old_pop_float(v: Value) -> Result<Option<f64>, ()> {
        match v {
            Value::Na => Ok(None),
            Value::Float(v) => Ok(Some(v)),
            _ => Err(()),
        }
    }

    /// Baseline copy of the `pop_bool` decode after the pop.
    fn old_pop_bool(v: Value) -> Result<Option<bool>, ()> {
        match v {
            Value::Na => Ok(None),
            Value::Bool(b) => Ok(Some(b)),
            _ => Err(()),
        }
    }

    fn old_float_arith(instr: &CsilInstr, lhs: Option<f64>, rhs: Option<f64>) -> Value {
        match (lhs, rhs) {
            (Some(l), Some(r)) => {
                let raw = match instr {
                    CsilInstr::F64Add => l + r,
                    CsilInstr::F64Sub => l - r,
                    CsilInstr::F64Mul => l * r,
                    CsilInstr::F64Div => l / r,
                    CsilInstr::F64Rem => l % r,
                    _ => unreachable!(),
                };
                Value::from_f64(raw)
            }
            _ => Value::Na,
        }
    }

    fn old_float_neg(operand: Option<f64>) -> Value {
        operand.map_or(Value::Na, |v| Value::from_f64(-v))
    }

    fn old_cmp(instr: &CsilInstr, lhs: &Value, rhs: &Value) -> Result<Value, ()> {
        let ord = match (lhs, rhs) {
            (Value::Na, _) | (_, Value::Na) => None,
            (Value::Int(l), Value::Int(r)) => Some(l.cmp(r)),
            (Value::Float(l), Value::Float(r)) => l.partial_cmp(r),
            (Value::Int(l), Value::Float(r)) => (*l as f64).partial_cmp(r),
            (Value::Float(l), Value::Int(r)) => l.partial_cmp(&(*r as f64)),
            _ => return Err(()),
        };
        Ok(ord.map_or(Value::Bool(false), |o| {
            Value::Bool(match instr {
                CsilInstr::CmpGt => o.is_gt(),
                CsilInstr::CmpGe => o.is_ge(),
                CsilInstr::CmpLt => o.is_lt(),
                CsilInstr::CmpLe => o.is_le(),
                _ => unreachable!(),
            })
        }))
    }

    fn old_cmp_eq(negate: bool, lhs: &Value, rhs: &Value) -> Result<Value, ()> {
        Ok(match (lhs, rhs) {
            (Value::Na, _) | (_, Value::Na) => Value::Bool(false),
            (Value::Int(l), Value::Float(r)) => {
                let eq = *l as f64 == *r;
                Value::Bool(if negate { !eq } else { eq })
            }
            (Value::Float(l), Value::Int(r)) => {
                let eq = *l == *r as f64;
                Value::Bool(if negate { !eq } else { eq })
            }
            (l, r) if std::mem::discriminant(l) == std::mem::discriminant(r) => {
                let eq = l == r;
                Value::Bool(if negate { !eq } else { eq })
            }
            _ => return Err(()),
        })
    }

    fn old_bool(instr: &CsilInstr, lhs: Option<bool>, rhs: Option<bool>) -> Value {
        match instr {
            CsilInstr::BoolAnd => match (lhs, rhs) {
                (Some(false), _) | (_, Some(false)) => Value::Bool(false),
                (Some(true), Some(true)) => Value::Bool(true),
                _ => Value::Na,
            },
            CsilInstr::BoolOr => match (lhs, rhs) {
                (Some(true), _) | (_, Some(true)) => Value::Bool(true),
                (Some(false), Some(false)) => Value::Bool(false),
                _ => Value::Na,
            },
            _ => unreachable!(),
        }
    }

    fn old_bool_not(operand: Option<bool>) -> Value {
        operand.map_or(Value::Na, |b| Value::Bool(!b))
    }

    fn old_runtime_history_offset(value: Value) -> Option<u64> {
        match value {
            Value::Int(v) => vm_offset::decode_offset(Some(v)),
            // Integer division can produce a fractional runtime value.
            // History offsets truncate toward zero.
            Value::Float(v) if v.is_finite() => {
                let truncated = v.trunc();
                if truncated >= 0.0 && truncated < u64::MAX as f64 {
                    Some(truncated as u64)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Baseline copy of the `ReadUserSlotDynamic` arm after the pop
    /// (`Err(offset)` is the too-deep error).
    fn old_user_slot_dynamic(
        user_slots: &[Vec<Value>],
        offset: Value,
        limit: u32,
        bar: usize,
        mask: usize,
        slot: u32,
    ) -> Result<Value, u64> {
        let read = match offset {
            Value::Int(raw) => vm_offset::dynamic_read(Some(raw), limit, bar, mask),
            other => {
                vm_offset::dynamic_read_decoded(old_runtime_history_offset(other), limit, bar, mask)
            }
        };
        Ok(match read {
            vm_offset::DynamicRead::Na => Value::Na,
            vm_offset::DynamicRead::TooDeep(offset) => return Err(offset),
            vm_offset::DynamicRead::Cell(cell) => user_slots
                .get(slot as usize)
                .and_then(|history| history.get(cell))
                .cloned()
                .unwrap_or(Value::Na),
        })
    }

    /// Baseline copy of the string-`+` operand classification: `Ok(None)` for
    /// the `na` result, `Ok(Some(..))` for the string path, `Err` otherwise.
    fn old_str_concat(lhs: &Value, rhs: &Value) -> Result<Option<String>, ()> {
        match (lhs, rhs) {
            (Value::Na, _) | (_, Value::Na) => Ok(None),
            (Value::String(l), Value::String(r)) => Ok(Some(format!("{l}{r}"))),
            _ => Err(()),
        }
    }

    // ---- operand generation ------------------------------------------------

    /// Bit-exact rendering (floats by bit pattern, so `-0.0` and NaN
    /// payloads are distinguished).
    fn key(v: &Value) -> String {
        match v {
            Value::Float(x) => format!("Float:{:016x}", x.to_bits()),
            other => format!("{other:?}"),
        }
    }

    fn key_opt_f(v: Option<f64>) -> String {
        v.map_or("na".into(), |x| format!("{:016x}", x.to_bits()))
    }

    const F_SPECIAL: [u64; 18] = [
        0,
        0x8000_0000_0000_0000,
        1,
        0x8000_0000_0000_0001,
        0x0010_0000_0000_0000,
        0x3ff0_0000_0000_0000,
        0xbff0_0000_0000_0000,
        0x3fe0_0000_0000_0000,
        0x4008_0000_0000_0000,
        0x7fef_ffff_ffff_ffff,
        0xffef_ffff_ffff_ffff,
        0x7ff0_0000_0000_0000,
        0xfff0_0000_0000_0000,
        0x7ff8_0000_0000_0000,
        0xfff8_0000_0000_4321,
        0x43e0_0000_0000_0000,
        0xc3e0_0000_0000_0000,
        0x4340_0000_0000_0001,
    ];

    const I_SPECIAL: [i64; 11] = [
        0,
        1,
        -1,
        2,
        -2,
        3,
        i64::MIN,
        i64::MIN + 1,
        i64::MAX,
        i64::MAX - 1,
        1 << 53,
    ];

    fn specials() -> Vec<Value> {
        let mut v = vec![Value::Na, Value::Bool(false), Value::Bool(true)];
        v.extend(I_SPECIAL.iter().map(|&i| Value::Int(i)));
        v.extend(F_SPECIAL.iter().map(|&b| Value::Float(f64::from_bits(b))));
        v.push(Value::String(String::new()));
        v.push(Value::String("a".into()));
        v.push(Value::String("ab".into()));
        v.push(Value::Color {
            r: 1,
            g: 2,
            b: 3,
            t: 0.0,
            input_origin: None,
        });
        v.push(Value::Color {
            r: 1,
            g: 2,
            b: 3,
            t: 50.0,
            input_origin: None,
        });
        v.push(Value::Ref(HandleId(0)));
        v.push(Value::Ref(HandleId(7)));
        v
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            // xorshift64*
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }
        fn value(&mut self) -> Value {
            let r = self.next();
            match r % 10 {
                0 => Value::Na,
                1 => Value::Bool(r & 0x100 != 0),
                2 | 3 => Value::Int((self.next() as i64) >> (self.next() % 64)),
                4 => Value::Int((self.next() % 41) as i64 - 20),
                5 | 6 => Value::Float(f64::from_bits(self.next())),
                7 => Value::Float(((self.next() % 4001) as f64 - 2000.0) / 8.0),
                8 => Value::String(["", "x", "xy"][(self.next() % 3) as usize].into()),
                _ => match self.next() % 2 {
                    0 => Value::Color {
                        r: (self.next() % 3) as u8,
                        g: 0,
                        b: 0,
                        t: (self.next() % 2) as f64,
                        input_origin: None,
                    },
                    _ => Value::Ref(HandleId((self.next() % 3) as u32)),
                },
            }
        }
    }

    /// All special pairs, then `n` random pairs.
    fn pairs(n: usize) -> Vec<(Value, Value)> {
        let s = specials();
        let mut out = Vec::new();
        for a in &s {
            for b in &s {
                out.push((a.clone(), b.clone()));
            }
        }
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for _ in 0..n {
            out.push((rng.value(), rng.value()));
        }
        out
    }

    fn res_key(r: &Result<Value, ()>) -> String {
        match r {
            Ok(v) => key(v),
            Err(()) => "mismatch".into(),
        }
    }

    fn new_key(r: Result<Value, OpMismatch>) -> String {
        match r {
            Ok(v) => key(&v),
            Err(OpMismatch) => "mismatch".into(),
        }
    }

    // ---- the differential tests --------------------------------------------

    #[test]
    fn const_value_matches_the_pushes() {
        assert_eq!(key(&const_value(VmConst::Na)), key(&Value::Na));
        for b in [false, true] {
            assert_eq!(key(&const_value(VmConst::Bool(b))), key(&Value::Bool(b)));
        }
        for i in I_SPECIAL {
            assert_eq!(key(&const_value(VmConst::Int(i))), key(&Value::Int(i)));
        }
        let mut rng = Rng(7);
        let floats = F_SPECIAL
            .iter()
            .copied()
            .chain((0..10_000).map(|_| rng.next()));
        for bits in floats {
            let v = f64::from_bits(bits);
            assert_eq!(
                key(&const_value(VmConst::Float(v))),
                key(&Value::from_f64(v))
            );
        }
        let c = const_value(VmConst::Color {
            r: 9,
            g: 8,
            b: 7,
            t: 6.0,
        });
        assert_eq!(
            key(&c),
            key(&Value::Color {
                r: 9,
                g: 8,
                b: 7,
                t: 6.0,
                input_origin: None
            })
        );
    }

    #[test]
    fn operand_decoders_match_pop_float_and_pop_bool() {
        for (v, _) in pairs(20_000) {
            let new_f = float_operand(v.clone()).map_err(|_| ());
            let old_f = old_pop_float(v.clone());
            assert_eq!(
                new_f.map(key_opt_f),
                old_f.map(key_opt_f),
                "float_operand {v:?}"
            );
            let new_b = bool_operand(v.clone()).map_err(|_| ());
            assert_eq!(new_b, old_pop_bool(v.clone()), "bool_operand {v:?}");
        }
    }

    #[test]
    fn int_arith_matches_the_old_arm() {
        let ops = [
            (IntArithOp::Add, CsilInstr::IntAdd),
            (IntArithOp::Sub, CsilInstr::IntSub),
            (IntArithOp::Mul, CsilInstr::IntMul),
            (IntArithOp::Rem, CsilInstr::IntRem),
            (IntArithOp::PineDiv, CsilInstr::PineIntDiv),
        ];
        for (l, r) in pairs(100_000) {
            for (op, instr) in &ops {
                assert_eq!(
                    new_key(int_arith(*op, &l, &r)),
                    res_key(&old_int_arith(instr, &l, &r)),
                    "{instr:?} {l:?} {r:?}"
                );
            }
        }
    }

    #[test]
    fn unary_numeric_kernels_match_the_old_arms() {
        for (v, _) in pairs(50_000) {
            assert_eq!(
                new_key(int_neg(&v)),
                res_key(&old_int_neg(v.clone())),
                "{v:?}"
            );
            assert_eq!(
                new_key(int_to_f64(&v)),
                res_key(&old_int_to_f64(v.clone())),
                "{v:?}"
            );
            if let Ok(f) = old_pop_float(v.clone()) {
                assert_eq!(key(&float_neg(f)), key(&old_float_neg(f)), "{v:?}");
            }
        }
    }

    #[test]
    fn float_arith_matches_the_old_arm() {
        let ops = [
            (FloatArithOp::Add, CsilInstr::F64Add),
            (FloatArithOp::Sub, CsilInstr::F64Sub),
            (FloatArithOp::Mul, CsilInstr::F64Mul),
            (FloatArithOp::Div, CsilInstr::F64Div),
            (FloatArithOp::Rem, CsilInstr::F64Rem),
        ];
        let mut rng = Rng(3);
        let mut operands: Vec<Option<f64>> = vec![None];
        operands.extend(F_SPECIAL.iter().map(|&b| Some(f64::from_bits(b))));
        let mut cases: Vec<(Option<f64>, Option<f64>)> = Vec::new();
        for a in &operands {
            for b in &operands {
                cases.push((*a, *b));
            }
        }
        for _ in 0..100_000 {
            let a = Some(f64::from_bits(rng.next()));
            let b = if rng.next() % 2 == 0 {
                Some(f64::from_bits(rng.next()))
            } else {
                Some(((rng.next() % 2001) as f64 - 1000.0) / 4.0)
            };
            cases.push((a, b));
        }
        for (l, r) in cases {
            for (op, instr) in &ops {
                assert_eq!(
                    key(&float_arith(*op, l, r)),
                    key(&old_float_arith(instr, l, r)),
                    "{instr:?} {l:?} {r:?}"
                );
            }
        }
    }

    #[test]
    fn comparisons_match_the_old_arms_outside_the_intentional_tolerance_change() {
        let ops = [
            (CmpOp::Gt, CsilInstr::CmpGt),
            (CmpOp::Ge, CsilInstr::CmpGe),
            (CmpOp::Lt, CsilInstr::CmpLt),
            (CmpOp::Le, CsilInstr::CmpLe),
        ];
        for (l, r) in pairs(100_000) {
            let promoted = match (&l, &r) {
                (Value::Float(l), Value::Float(r)) => Some((*l, *r)),
                (Value::Int(l), Value::Float(r)) => Some((*l as f64, *r)),
                (Value::Float(l), Value::Int(r)) => Some((*l, *r as f64)),
                _ => None,
            };
            // The baseline copy below models raw IEEE comparison.
            // The cases below cover the script-level tolerance rule.
            if promoted.is_some_and(|(l, r)| l != r && (l - r).abs() <= 1e-10) {
                continue;
            }
            for (op, instr) in &ops {
                assert_eq!(
                    new_key(cmp_values(*op, &l, &r)),
                    res_key(&old_cmp(instr, &l, &r)),
                    "{instr:?} {l:?} {r:?}"
                );
            }
            for negate in [false, true] {
                assert_eq!(
                    new_key(cmp_eq(negate, &l, &r)),
                    res_key(&old_cmp_eq(negate, &l, &r)),
                    "eq negate={negate} {l:?} {r:?}"
                );
            }
        }
        // Self-comparison of the same value (structural equality path).
        for v in specials() {
            for negate in [false, true] {
                assert_eq!(
                    new_key(cmp_eq(negate, &v, &v.clone())),
                    res_key(&old_cmp_eq(negate, &v, &v.clone())),
                    "self eq {v:?}"
                );
            }
        }
    }

    #[test]
    fn script_comparisons_preserve_the_tolerance_and_exact_integers() {
        for (lhs, rhs, equal) in [
            (0.0, 1e-10, true),
            (1.0, 1.0 + 1e-10, false),
            (1.0, 1.0 + 5e-11, true),
            (-1.0, -1.0 - 5e-11, true),
            (0.0, 1.5e-10, false),
        ] {
            for negate in [false, true] {
                assert_eq!(
                    cmp_eq(negate, &Value::Float(lhs), &Value::Float(rhs)),
                    Ok(Value::Bool(equal != negate))
                );
            }
            if equal {
                for (op, wanted) in [
                    (CmpOp::Lt, false),
                    (CmpOp::Gt, false),
                    (CmpOp::Le, true),
                    (CmpOp::Ge, true),
                ] {
                    assert_eq!(
                        cmp_values(op, &Value::Float(lhs), &Value::Float(rhs)),
                        Ok(Value::Bool(wanted))
                    );
                }
            }
        }
        let lhs = Value::Int(9_007_199_254_740_993);
        let rhs = Value::Int(9_007_199_254_740_992);
        assert_eq!(cmp_eq(false, &lhs, &rhs), Ok(Value::Bool(false)));
        assert_eq!(cmp_values(CmpOp::Gt, &lhs, &rhs), Ok(Value::Bool(true)));
        for negate in [false, true] {
            assert_eq!(cmp_eq(negate, &Value::Na, &lhs), Ok(Value::Bool(false)));
        }
    }

    #[test]
    fn logic_select_and_jump_tests_match_the_old_arms() {
        let tri = [None, Some(false), Some(true)];
        for l in tri {
            for r in tri {
                assert_eq!(
                    key(&bool_and(l, r)),
                    key(&old_bool(&CsilInstr::BoolAnd, l, r))
                );
                assert_eq!(
                    key(&bool_or(l, r)),
                    key(&old_bool(&CsilInstr::BoolOr, l, r))
                );
            }
            assert_eq!(key(&bool_not(l)), key(&old_bool_not(l)));
            let (a, b) = (Value::Int(1), Value::Int(2));
            let old_sel = if l.unwrap_or(false) {
                a.clone()
            } else {
                b.clone()
            };
            assert_eq!(key(&select_value(l, a, b)), key(&old_sel));
            assert_eq!(jump_if_false_taken(l), !l.unwrap_or(false));
            for on in [false, true] {
                assert_eq!(jump_if_exact_taken(l, on), l == Some(on));
            }
        }
    }

    #[test]
    fn user_slot_dynamic_matches_the_old_arm() {
        // Two slots of 8 cells (mask 7), one empty slot.
        let slots: Vec<Vec<Value>> = vec![
            (0..8).map(|i| Value::Int(100 + i)).collect(),
            (0..8)
                .map(|i| Value::Float(f64::from(i as u32) + 0.5))
                .collect(),
            Vec::new(),
        ];
        let mut offsets = specials();
        offsets.extend((-3..12).map(Value::Int));
        offsets.extend(
            [-1.5, -0.5, -0.0, 0.5, 0.9, 1.9, 2.5, 4.99, 7.0, 99.5, 1e300]
                .iter()
                .map(|&f| Value::Float(f)),
        );
        let mut rng = Rng(11);
        offsets.extend((0..2_000).map(|_| rng.value()));
        for offset in &offsets {
            for limit in [0u32, 1, 5, 100] {
                for bar in [0usize, 1, 3, 7, 8, 20] {
                    for slot in 0u32..4 {
                        let new = user_slot_dynamic(&slots, offset, limit, bar, 7, slot as usize)
                            .map(|cell| cell.unwrap_or(Value::Na));
                        let old =
                            old_user_slot_dynamic(&slots, offset.clone(), limit, bar, 7, slot);
                        assert_eq!(
                            new.as_ref().map(key),
                            old.as_ref().map(key),
                            "{offset:?} limit={limit} bar={bar} slot={slot}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn history_offset_matches_runtime_history_offset() {
        let mut rng = Rng(5);
        let mut values = specials();
        values.extend((0..50_000).map(|_| rng.value()));
        for _ in 0..20_000 {
            values.push(Value::Float(((rng.next() % 4001) as f64 - 400.0) / 7.0));
        }
        for v in values {
            assert_eq!(
                history_offset(&v),
                old_runtime_history_offset(v.clone()),
                "{v:?}"
            );
        }
    }

    #[test]
    fn str_concat_operands_match_the_old_arm() {
        for (l, r) in pairs(20_000) {
            let new = str_concat_operands(&l, &r)
                .map(|o| o.map(|(a, b)| format!("{a}{b}")))
                .map_err(|OpMismatch| ());
            assert_eq!(new, old_str_concat(&l, &r), "{l:?} {r:?}");
        }
    }

    /// Baseline copy of the `MarketData::time_at` body.
    fn old_time_at(time: &[i64], index: usize) -> i64 {
        if time.is_empty() {
            crate::SYNTHETIC_EPOCH_MS + (index as i64) * crate::MS_PER_DAY
        } else {
            time[index]
        }
    }

    #[test]
    fn time_at_matches_the_old_body() {
        let lanes: [&[i64]; 3] = [&[], &[i64::MIN, -1, 0, 7, i64::MAX], &[5]];
        for lane in lanes {
            for index in [0usize, 1, 2, 4, 5, 1000, 106_751_991] {
                let old = std::panic::catch_unwind(|| old_time_at(lane, index)).ok();
                let new = std::panic::catch_unwind(|| time_at(lane, index)).ok();
                assert_eq!(new, old, "{lane:?} {index}");
            }
        }
    }

    /// Baseline copy of the scalar arms of `resolve_input` (`float_kind` is
    /// `matches!(decl.kind, Float | Price)`).
    fn old_input_scalar(x: InputScalar, float_kind: bool) -> Value {
        match x {
            InputScalar::Int(i) if float_kind => Value::from_f64(i as f64),
            InputScalar::Int(i) => Value::Int(i),
            InputScalar::Float(f) => Value::from_f64(f),
            InputScalar::Bool(b) => Value::Bool(b),
            InputScalar::Color { r, g, b, t } => Value::Color {
                r,
                g,
                b,
                t,
                input_origin: None,
            },
            InputScalar::Enum(index) => Value::Int(index as i64),
        }
    }

    #[test]
    fn input_scalar_value_matches_the_old_arms() {
        let mut xs = vec![
            InputScalar::Bool(false),
            InputScalar::Bool(true),
            InputScalar::Color {
                r: 1,
                g: 2,
                b: 3,
                t: 101.0,
            },
            InputScalar::Enum(0),
            InputScalar::Enum(u32::MAX),
        ];
        xs.extend(I_SPECIAL.iter().map(|&i| InputScalar::Int(i)));
        xs.extend(
            F_SPECIAL
                .iter()
                .map(|&b| InputScalar::Float(f64::from_bits(b))),
        );
        let mut rng = Rng(13);
        for _ in 0..20_000 {
            xs.push(InputScalar::Int(rng.next() as i64 >> (rng.next() % 64)));
            xs.push(InputScalar::Float(f64::from_bits(rng.next())));
        }
        for x in xs {
            for float_kind in [false, true] {
                assert_eq!(
                    key(&input_scalar_value(x, float_kind)),
                    key(&old_input_scalar(x, float_kind)),
                    "{x:?} {float_kind}"
                );
            }
        }
    }

    /// Baseline copy of the offset logic of `exec_read_builtin_dynamic`
    /// (`runtime_history_offset`, then `check_dynamic_history_offset`;
    /// `Err(offset)` is the too-deep error).
    fn old_builtin_dynamic_offset(offset: Value, limit: u32) -> Result<Option<u32>, u64> {
        match old_runtime_history_offset(offset) {
            Some(offset) => {
                if offset > u64::from(limit) {
                    return Err(offset);
                }
                Ok(Some(offset as u32))
            }
            _ => Ok(None),
        }
    }

    /// The production path: `history_offset`, then `dynamic_offset_limit`.
    fn new_builtin_dynamic_offset(offset: &Value, limit: u32) -> Result<Option<u32>, u64> {
        match history_offset(offset) {
            Some(o) => dynamic_offset_limit(o, limit).map(Some).ok_or(o),
            None => Ok(None),
        }
    }

    #[test]
    fn dynamic_offset_limit_matches_the_old_arm() {
        let mut rng = Rng(17);
        let mut offsets = specials();
        offsets.extend((-3..12).map(Value::Int));
        offsets.extend(
            [
                i64::from(u32::MAX) - 1,
                i64::from(u32::MAX),
                i64::from(u32::MAX) + 1,
            ]
            .map(Value::Int),
        );
        offsets.extend(
            [
                -0.5,
                0.5,
                2.5,
                99.9,
                100.0,
                100.5,
                4_294_967_295.5,
                4_294_967_296.0,
                1e19,
                2e19,
            ]
            .map(Value::Float),
        );
        offsets.extend((0..20_000).map(|_| rng.value()));
        for offset in &offsets {
            for limit in [0u32, 1, 2, 99, 100, u32::MAX - 1, u32::MAX] {
                assert_eq!(
                    new_builtin_dynamic_offset(offset, limit),
                    old_builtin_dynamic_offset(offset.clone(), limit),
                    "{offset:?} {limit}"
                );
            }
        }
        for o in [
            0u64,
            1,
            99,
            100,
            101,
            u64::from(u32::MAX),
            u64::from(u32::MAX) + 1,
            u64::MAX,
        ] {
            for limit in [0u32, 100, u32::MAX] {
                let old = if o > u64::from(limit) {
                    None
                } else {
                    Some(o as u32)
                };
                assert_eq!(dynamic_offset_limit(o, limit), old, "{o} {limit}");
            }
        }
    }

    #[test]
    fn discard_drops_every_variant() {
        for v in specials() {
            discard(v);
        }
    }

    /// The opaque helpers are exactly the Rust primitives (spot checks on
    /// the special patterns; the kernels above are compared end to end).
    #[test]
    #[allow(clippy::float_cmp)]
    fn f64_helpers_are_the_primitives() {
        for a in F_SPECIAL {
            let a = f64::from_bits(a);
            assert_eq!(vm_f64_ops::neg(a).to_bits(), (-a).to_bits());
            for b in F_SPECIAL {
                let b = f64::from_bits(b);
                assert_eq!(vm_f64_ops::eq(a, b), a == b);
                assert_eq!(vm_f64_ops::gt(a, b), a > b);
                assert_eq!(vm_f64_ops::ge(a, b), a >= b);
                assert_eq!(vm_f64_ops::lt(a, b), a < b);
                assert_eq!(vm_f64_ops::le(a, b), a <= b);
            }
        }
        for i in I_SPECIAL {
            assert_eq!(vm_f64_ops::from_i64(i).to_bits(), (i as f64).to_bits());
        }
        assert_eq!(vm_f64_ops::three().to_bits(), 3.0f64.to_bits());
        assert_eq!(vm_f64_ops::four().to_bits(), 4.0f64.to_bits());
        for a in F_SPECIAL {
            for b in F_SPECIAL {
                let (a, b) = (f64::from_bits(a), f64::from_bits(b));
                assert_eq!(
                    vm_f64_ops::midpoint(a, b).to_bits(),
                    f64::midpoint(a, b).to_bits()
                );
            }
        }
    }
}
