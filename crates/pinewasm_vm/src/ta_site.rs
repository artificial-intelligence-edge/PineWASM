//! Pure kernels of the interpreter's `CALL_TA` call site: the operand
//! encoding (`num`, `cond`, `pack`), the result decoding (`decode`) and the
//! site-table lifecycle (`fresh_table`, `reset_table`).
//!
//! [`crate::ta`] is thin glue over these kernels: it views each [`Value`]
//! operand as a [`SiteOperand`], looks up the function's operand kinds and
//! result kind, and formats the type-mismatch message when a kernel reports
//! the unit error [`SiteMismatch`]. Nothing here allocates, formats or reads
//! a `Value`, so Charon/Aeneas can extract it (`--features aeneas`, roots in
//! [`aeneas_roots_vm_ta_site`]).
//!
//! Floats follow the `FloatOps` pattern (docs of `pinewasm_ta::state`): every
//! kernel is generic over [`SiteFloat`], and the shipped interpreter
//! instantiates it at `f64`, whose methods are the primitive operations in
//! the module `site_f64_ops` (opaque to the Aeneas extraction).
//!
//! [`Value`]: pinewasm_mir::Value

/// The float operations the call-site kernels need, one method per Rust
/// primitive. The single instance is `f64`.
pub(crate) trait SiteFloat: Copy {
    /// `f64::NAN`.
    fn nan() -> Self;
    /// `0.0`.
    fn zero() -> Self;
    /// `1.0`.
    fn one() -> Self;
    /// `self.is_nan()`.
    fn is_nan(self) -> bool;
    /// `self.is_finite()`.
    fn is_finite(self) -> bool;
    /// `self == 0.0`.
    fn eq_zero(self) -> bool;
    /// `v as f64`.
    fn from_i64(v: i64) -> Self;
    /// `self as i64` (Rust's saturating float-to-int cast).
    fn to_i64(self) -> i64;
}

/// The shipped primitives. Opaque to the extraction
/// (`--opaque=pinewasm_vm::ta_site::site_f64_ops`), so each one is treated as
/// an abstract binary64 operation.
mod site_f64_ops {
    #[inline(always)]
    pub(super) fn nan() -> f64 {
        f64::NAN
    }
    #[inline(always)]
    pub(super) fn zero() -> f64 {
        0.0
    }
    #[inline(always)]
    pub(super) fn one() -> f64 {
        1.0
    }
    #[inline(always)]
    pub(super) fn is_nan(a: f64) -> bool {
        a.is_nan()
    }
    #[inline(always)]
    pub(super) fn is_finite(a: f64) -> bool {
        a.is_finite()
    }
    #[inline(always)]
    #[allow(clippy::float_cmp)]
    pub(super) fn eq_zero(a: f64) -> bool {
        a == 0.0
    }
    #[inline(always)]
    #[allow(clippy::cast_precision_loss)]
    pub(super) fn from_i64(v: i64) -> f64 {
        v as f64
    }
    #[inline(always)]
    #[allow(clippy::cast_possible_truncation)]
    pub(super) fn to_i64(a: f64) -> i64 {
        a as i64
    }
}

impl SiteFloat for f64 {
    #[inline(always)]
    fn nan() -> f64 {
        site_f64_ops::nan()
    }
    #[inline(always)]
    fn zero() -> f64 {
        site_f64_ops::zero()
    }
    #[inline(always)]
    fn one() -> f64 {
        site_f64_ops::one()
    }
    #[inline(always)]
    fn is_nan(self) -> bool {
        site_f64_ops::is_nan(self)
    }
    #[inline(always)]
    fn is_finite(self) -> bool {
        site_f64_ops::is_finite(self)
    }
    #[inline(always)]
    fn eq_zero(self) -> bool {
        site_f64_ops::eq_zero(self)
    }
    #[inline(always)]
    fn from_i64(v: i64) -> f64 {
        site_f64_ops::from_i64(v)
    }
    #[inline(always)]
    fn to_i64(self) -> i64 {
        site_f64_ops::to_i64(self)
    }
}

/// A `CALL_TA` operand as the kernels see it: the scalar variants of `Value`
/// that the encoding distinguishes, and `Other` for every other variant
/// (which is always a type mismatch).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SiteOperand<F> {
    Na,
    Int(i64),
    Float(F),
    Bool(bool),
    Other,
}

/// How one engine operand slot is filled: `Skip` leaves it `NaN` (no
/// operand declared at that position, or no argument supplied).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SiteSlot {
    Skip,
    Num,
    Cond,
}

/// The unit error of the encoding kernels: some operand had the wrong type.
/// The glue turns it into the message naming the operand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SiteMismatch;

/// The result type a function's `CALL_TA` pushes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SiteResultKind {
    Float,
    Int,
    Bool,
}

/// A decoded engine result, the scalar `Value` it becomes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SiteDecoded<F> {
    Na,
    Int(i64),
    Float(F),
    Bool(bool),
}

/// A numeric operand in the engine encoding: `na` is `NaN`, ints widen.
pub(crate) fn num<F: SiteFloat>(value: SiteOperand<F>) -> Result<F, SiteMismatch> {
    match value {
        SiteOperand::Na => Ok(F::nan()),
        SiteOperand::Int(v) => Ok(F::from_i64(v)),
        SiteOperand::Float(v) => Ok(v),
        SiteOperand::Bool(_) | SiteOperand::Other => Err(SiteMismatch),
    }
}

/// A bool condition operand as `0.0`/`1.0`; an `na` condition is false.
pub(crate) fn cond<F: SiteFloat>(value: SiteOperand<F>) -> Result<F, SiteMismatch> {
    match value {
        SiteOperand::Na => Ok(F::zero()),
        SiteOperand::Bool(b) => {
            if b {
                Ok(F::one())
            } else {
                Ok(F::zero())
            }
        }
        SiteOperand::Int(_) | SiteOperand::Float(_) | SiteOperand::Other => Err(SiteMismatch),
    }
}

/// One engine operand slot.
pub(crate) fn encode<F: SiteFloat>(
    slot: SiteSlot,
    value: SiteOperand<F>,
) -> Result<F, SiteMismatch> {
    match slot {
        SiteSlot::Skip => Ok(F::nan()),
        SiteSlot::Num => num(value),
        SiteSlot::Cond => cond(value),
    }
}

/// The engine's three operand slots (slot `i` with argument `i`), encoded in
/// argument order; the first
/// mismatch wins. Explicit matches instead of `?` keep the extracted control
/// flow free of the `Try` trait.
#[allow(clippy::question_mark)]
pub(crate) fn pack<F: SiteFloat>(
    slot0: SiteSlot,
    arg0: SiteOperand<F>,
    slot1: SiteSlot,
    arg1: SiteOperand<F>,
    slot2: SiteSlot,
    arg2: SiteOperand<F>,
) -> Result<[F; 3], SiteMismatch> {
    let a = match encode(slot0, arg0) {
        Ok(v) => v,
        Err(e) => return Err(e),
    };
    let b = match encode(slot1, arg1) {
        Ok(v) => v,
        Err(e) => return Err(e),
    };
    let c = match encode(slot2, arg2) {
        Ok(v) => v,
        Err(e) => return Err(e),
    };
    Ok([a, b, c])
}

/// The engine's result as the declared result type: na is `NaN` on the way
/// out (a non-finite float result is na, `Value::from_f64`).
pub(crate) fn decode<F: SiteFloat>(kind: SiteResultKind, value: F) -> SiteDecoded<F> {
    match kind {
        SiteResultKind::Float => {
            if value.is_finite() {
                SiteDecoded::Float(value)
            } else {
                SiteDecoded::Na
            }
        }
        SiteResultKind::Int => {
            if value.is_nan() {
                SiteDecoded::Na
            } else {
                SiteDecoded::Int(value.to_i64())
            }
        }
        SiteResultKind::Bool => {
            if value.is_nan() {
                SiteDecoded::Na
            } else {
                SiteDecoded::Bool(!value.eq_zero())
            }
        }
    }
}

/// The site table's lifecycle, generic over the engine's initializer:
/// `blank` allocates a table for a declaration list, `init` is
/// `Engine::init` over its regions.
pub(crate) trait SiteInit<D, T> {
    /// Allocate the regions for `decls` (contents unspecified).
    fn blank(decls: D) -> T;
    /// Initialize every site of `table` (`Engine::init`).
    fn init(table: &mut T);
}

/// `TaStates::new`: allocate, then initialize.
pub(crate) fn fresh_table<D, T, I: SiteInit<D, T>>(decls: D) -> T {
    let mut table = I::blank(decls);
    I::init(&mut table);
    table
}

/// `TaStates::reset`: initialize the existing regions again.
pub(crate) fn reset_table<D, T, I: SiteInit<D, T>>(table: &mut T) {
    I::init(table);
}

/// Extraction-only marker that makes Aeneas emit the definition of `F64`.
#[cfg(feature = "aeneas")]
#[allow(dead_code)]
pub struct F64Model;

/// Extraction roots (`--features aeneas` only): the generic kernels, the
/// `f64` instance, and the lifecycle.
#[cfg(feature = "aeneas")]
#[allow(dead_code, private_interfaces)]
pub mod aeneas_roots_vm_ta_site {
    use super::{
        SiteDecoded, SiteFloat, SiteInit, SiteMismatch, SiteOperand, SiteResultKind, SiteSlot,
    };

    /// Root that makes [`super::F64Model`] part of the extraction.
    pub fn site_f64_model(_model: &super::F64Model) {}

    /// Root for `num`.
    pub fn site_num<F: SiteFloat>(value: SiteOperand<F>) -> Result<F, SiteMismatch> {
        super::num(value)
    }

    /// Root for `cond`.
    pub fn site_cond<F: SiteFloat>(value: SiteOperand<F>) -> Result<F, SiteMismatch> {
        super::cond(value)
    }

    /// Root for `pack`.
    pub fn site_pack<F: SiteFloat>(
        slot0: SiteSlot,
        arg0: SiteOperand<F>,
        slot1: SiteSlot,
        arg1: SiteOperand<F>,
        slot2: SiteSlot,
        arg2: SiteOperand<F>,
    ) -> Result<[F; 3], SiteMismatch> {
        super::pack(slot0, arg0, slot1, arg1, slot2, arg2)
    }

    /// Root for `decode`.
    pub fn site_decode<F: SiteFloat>(kind: SiteResultKind, value: F) -> SiteDecoded<F> {
        super::decode(kind, value)
    }

    /// `pack` at the shipped `f64` instance (makes Aeneas emit the instance).
    pub fn site_pack_f64(
        slot0: SiteSlot,
        arg0: SiteOperand<f64>,
        slot1: SiteSlot,
        arg1: SiteOperand<f64>,
        slot2: SiteSlot,
        arg2: SiteOperand<f64>,
    ) -> Result<[f64; 3], SiteMismatch> {
        super::pack(slot0, arg0, slot1, arg1, slot2, arg2)
    }

    /// `decode` at the shipped `f64` instance.
    pub fn site_decode_f64(kind: SiteResultKind, value: f64) -> SiteDecoded<f64> {
        super::decode(kind, value)
    }

    /// Root for `fresh_table`.
    pub fn site_fresh_table<D, T, I: SiteInit<D, T>>(decls: D) -> T {
        super::fresh_table::<D, T, I>(decls)
    }

    /// Root for `reset_table`.
    pub fn site_reset_table<D, T, I: SiteInit<D, T>>(table: &mut T) {
        super::reset_table::<D, T, I>(table);
    }
}
