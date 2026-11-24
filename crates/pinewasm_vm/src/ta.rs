//! The interpreter's binding to the stateful builtin engine backing `CALL_TA`.
//!
//! The engine itself lives in [`pinewasm_ta`], which is `no_std` and
//! allocation-free so the AOT backend can compile it *into* each artifact
//! rather than importing it. This module is the interpreter's half:
//! it owns the state region, converts [`Value`] operands to the engine's plain
//! `f64` encoding (raising type-mismatch errors), and
//! converts results back per the function's declared result type.
//!
//! Exact color capture payloads live beside the numeric engine's bounded
//! capture ring. Each call site still owns one state
//! that advances **only** when its `CALL_TA` instruction executes, so
//! conditional-execution gaps freeze it (see docs/runtime.md); the warm-up and
//! na rules are documented on [`pinewasm_ta::Engine`].

use pinewasm_csil::TaStateDecl;
use pinewasm_hir::{TaFunc, TaOperand, TaResult};
use pinewasm_mir::Value;
use pinewasm_ta::{cells_for, scratch_for, Decl, Engine, Lanes, Layout, Out, State};

use crate::ta_site::{self, SiteDecoded, SiteOperand, SiteResultKind, SiteSlot};
use crate::MarketData;

/// Every engine state for one run: one [`State`] per declaration plus the
/// shared cell region the ring buffers index into.
///
/// Sized once from the declarations (every ring capacity is a baked const
/// param), so a run never reallocates and a realtime snapshot is a plain
/// clone of its fixed-capacity vectors, including exact color capture values.
#[derive(Debug, Clone, Default)]
pub(crate) struct TaStates {
    decls: Vec<Decl>,
    states: Vec<State>,
    cells: Vec<f64>,
    scratch: Vec<f64>,
    color_captures: Vec<Option<ColorCaptures>>,
}

/// Slots mirror the numeric engine's bounded capture ring. Engine cells carry
/// slot IDs; this sidecar keeps exact RGB and fractional transparency values.
#[derive(Debug, Clone)]
struct ColorCaptures {
    values: Vec<Value>,
    next: usize,
}

impl ColorCaptures {
    fn capture(&mut self, value: Value) -> usize {
        let id = self.next;
        self.values[id] = value;
        self.next = (id + 1) % self.values.len();
        id
    }
}

impl TaStates {
    /// Predict the heap payload [`Self::new`] reserves, using the engine's
    /// published cross-target state stride. `None` means the declaration
    /// table's saturating layout arithmetic reached its representable ceiling
    /// and must be rejected before allocation.
    pub(crate) fn allocation_bytes(decls: &[TaStateDecl]) -> Option<usize> {
        Self::allocation_bytes_with_params(decls, |_, _, value| value)
    }

    /// Resolve run-constant params into a bounded stack array before admission;
    /// this estimates the actual rings without cloning the declaration table.
    pub(crate) fn allocation_bytes_with_params(
        decls: &[TaStateDecl],
        value_at: impl Fn(usize, usize, i64) -> i64,
    ) -> Option<usize> {
        let footprints = ParamFootprints {
            decls,
            value_at: &value_at,
        };
        ta_alloc::allocation_bytes_with_color(
            &footprints,
            decls,
            std::mem::size_of::<Decl>(),
            pinewasm_ta::STATE_BYTES as usize,
            std::mem::size_of::<Option<ColorCaptures>>(),
            std::mem::size_of::<Value>(),
        )
    }

    /// Build (and initialize) the states for a program's declaration table.
    ///
    /// A declaration too wide for [`pinewasm_ta::MAX_PARAMS`] cannot be
    /// represented; it becomes a param-less entry, which every advance rule
    /// treats as its defaults. No current engine signature is that wide, and
    /// `every_engine_signature_fits_the_decl_table` keeps it that way.
    pub(crate) fn new(decls: &[TaStateDecl]) -> TaStates {
        ta_site::fresh_table::<_, _, EngineInit>(decls)
    }

    /// Re-initialize every site (a fresh run over the same program).
    ///
    /// No interpreter path calls it today (`new` builds a fresh table per
    /// run); it provides the reset half of the lifecycle that
    /// `tests::new_equals_reset_after_advance` checks against `new`.
    #[allow(dead_code)]
    pub(crate) fn reset(&mut self) {
        ta_site::reset_table::<&[TaStateDecl], _, EngineInit>(self);
    }

    /// Advance site `index` with operands already in the engine's `f64`
    /// encoding (na = NaN, conditions 0.0/1.0). This is the AOT host's entry
    /// point, where the operands never went through [`Value`] at all.
    pub(crate) fn advance_raw(
        &mut self,
        index: u32,
        args: [f64; 3],
        data: &MarketData,
        bar: usize,
    ) -> Option<Out> {
        let lanes = MarketLanes(data);
        Engine::bind(
            &self.decls,
            &mut self.states,
            &mut self.cells,
            &mut self.scratch,
        )
        .advance(index, args, &lanes, bar as u32)
    }
}

/// The declaration table seen through run-constant params: footprint `i` is
/// `(cells_for, scratch_for)` of declaration `i` after resolving its params
/// into a bounded stack array and normalizing like `Decl::new`.
struct ParamFootprints<'a, V> {
    decls: &'a [TaStateDecl],
    value_at: V,
}

/// Resolve and construct exactly the declaration used by one footprint read.
/// `Decl` copies parameter bits; function-specific defaults and clamps remain
/// in `cells_for` and `scratch_for`, after the ordered parameter reads.
pub(crate) fn resolved_state_footprint<V: crate::ta_params::ParamValues>(
    values: &V,
    state: usize,
    func: TaFunc,
    source: &[i64],
) -> (u32, u32) {
    let (params, len) = crate::ta_params::resolve_params(values, state, source);
    match Decl::new(func, &params[..len]) {
        Some(decl) => (cells_for(&decl), scratch_for(&decl)),
        None => match Decl::new(func, &[]) {
            Some(decl) => (cells_for(&decl), scratch_for(&decl)),
            None => {
                let decl = Decl::default();
                (cells_for(&decl), scratch_for(&decl))
            }
        },
    }
}

impl<V: Fn(usize, usize, i64) -> i64> ta_alloc::Footprints for ParamFootprints<'_, V> {
    fn count(&self) -> usize {
        self.decls.len()
    }

    fn footprint(&self, state: usize) -> (u32, u32) {
        let decl = &self.decls[state];
        resolved_state_footprint(&self.value_at, state, decl.func, &decl.params)
    }
}

/// The byte arithmetic of [`TaStates::allocation_bytes`], over the per-
/// declaration footprints. Written in a style that Aeneas can extract.
pub(crate) mod ta_alloc {
    use super::{TaFunc, TaStateDecl};
    /// A declaration table as the kernel sees it: `count` entries, entry `i`
    /// needing `(cells, scratch)` f64 cells.
    pub(crate) trait Footprints {
        fn count(&self) -> usize;
        fn footprint(&self, index: usize) -> (u32, u32);
    }

    /// `count * decl_bytes + count * state_bytes + (cells + scratch) * 8`,
    /// with `cells` the sum and `scratch` the maximum of the footprints, or
    /// `None` as soon as any step overflows `usize`.
    // `?` on `Option` would need trusted `Try`/`FromResidual` models in the
    // extraction (Aeneas has none), so the early returns stay explicit.
    #[allow(clippy::question_mark)]
    pub(crate) fn allocation_bytes<P: Footprints>(
        table: &P,
        decl_bytes: usize,
        state_bytes: usize,
    ) -> Option<usize> {
        let count = table.count();
        let mut cells = 0usize;
        let mut scratch = 0usize;
        let mut i = 0usize;
        while i < count {
            let (c, s) = table.footprint(i);
            cells = match cells.checked_add(c as usize) {
                Some(total) => total,
                None => return None,
            };
            if scratch < s as usize {
                scratch = s as usize;
            }
            i += 1;
        }
        let decl_total = match count.checked_mul(decl_bytes) {
            Some(v) => v,
            None => return None,
        };
        let state_total = match count.checked_mul(state_bytes) {
            Some(v) => v,
            None => return None,
        };
        let header = match decl_total.checked_add(state_total) {
            Some(v) => v,
            None => return None,
        };
        let all_cells = match cells.checked_add(scratch) {
            Some(v) => v,
            None => return None,
        };
        let cell_bytes = match all_cells.checked_mul(8) {
            Some(v) => v,
            None => return None,
        };
        header.checked_add(cell_bytes)
    }

    /// Check one Color capture payload without borrowing or querying its source.
    #[allow(clippy::question_mark)]
    fn color_capture_step(previous: usize, cells: u32, value_bytes: usize) -> Option<usize> {
        let bytes = match (cells as usize).checked_mul(value_bytes) {
            Some(bytes) => bytes,
            None => return None,
        };
        previous.checked_add(bytes)
    }

    fn is_color(func: TaFunc) -> bool {
        func == TaFunc::ValueWhenColor
    }

    /// Add exact Color-sidecar headers and capture payloads after the base plan.
    /// A Color site reads its footprint again, preserving the native callback
    /// order even when the reader uses interior mutability.
    #[allow(clippy::question_mark)]
    pub(crate) fn allocation_bytes_with_color<P: Footprints>(
        table: &P,
        decls: &[TaStateDecl],
        decl_bytes: usize,
        state_bytes: usize,
        color_header_bytes: usize,
        value_bytes: usize,
    ) -> Option<usize> {
        let base = match allocation_bytes(table, decl_bytes, state_bytes) {
            Some(bytes) => bytes,
            None => return None,
        };
        let mut any_color = false;
        let mut index = 0usize;
        while index < decls.len() {
            if decls[index].func == TaFunc::ValueWhenColor {
                any_color = true;
                break;
            }
            index += 1;
        }
        let headers = if any_color {
            match decls.len().checked_mul(color_header_bytes) {
                Some(bytes) => bytes,
                None => return None,
            }
        } else {
            0
        };
        let mut captures = 0usize;
        let mut captures_ok = true;
        let mut index = 0usize;
        while index < decls.len() && captures_ok {
            if is_color(decls[index].func) {
                let (cells, _) = table.footprint(index);
                match color_capture_step(captures, cells, value_bytes) {
                    Some(total) => captures = total,
                    None => captures_ok = false,
                }
            }
            if captures_ok {
                index += 1;
            }
        }
        if !captures_ok {
            return None;
        }
        let with_headers = match base.checked_add(headers) {
            Some(bytes) => bytes,
            None => return None,
        };
        with_headers.checked_add(captures)
    }

    #[cfg(test)]
    mod color_step_tests {
        use super::color_capture_step;

        #[test]
        fn checked_color_step_matches_wide_integer_reference() {
            let previous_values = [0, 1, 8, usize::MAX / 2, usize::MAX - 1, usize::MAX];
            let cell_values = [0, 1, 2, 100_000, u32::MAX];
            let widths = [0, 1, 8, 32, usize::MAX / 2, usize::MAX];
            let mut checked = 0;
            for previous in previous_values {
                for cells in cell_values {
                    for value_bytes in widths {
                        let exact = previous as u128 + u128::from(cells) * value_bytes as u128;
                        let expected = usize::try_from(exact).ok();
                        assert_eq!(color_capture_step(previous, cells, value_bytes), expected);
                        checked += 1;
                    }
                }
            }
            assert_eq!(checked, 180);
        }

        #[test]
        fn checked_color_step_keeps_exact_boundary_and_failure_distinction() {
            assert_eq!(color_capture_step(0, 1, usize::MAX), Some(usize::MAX));
            assert_eq!(
                color_capture_step(usize::MAX, 0, usize::MAX),
                Some(usize::MAX)
            );
            assert_eq!(color_capture_step(1, 1, usize::MAX), None); // Sum overflow.
            assert_eq!(color_capture_step(0, 2, usize::MAX), None); // Product overflow.
            assert_eq!(color_capture_step(usize::MAX - 8, 1, 8), Some(usize::MAX));
        }
    }
}

/// The site-table lifecycle's instance: `blank` allocates the regions for a
/// declaration table (states `Invalid`, cells and scratch `NaN`), `init` is
/// [`Engine::init`] over them. `new` and `reset` are the shared kernels
/// `ta_site::fresh_table` / `ta_site::reset_table` at this instance.
struct EngineInit;

impl<'a> ta_site::SiteInit<&'a [TaStateDecl], TaStates> for EngineInit {
    fn blank(decls: &'a [TaStateDecl]) -> TaStates {
        let decls: Vec<Decl> = decls
            .iter()
            .map(|decl| {
                Decl::new(decl.func, &decl.params)
                    .or_else(|| Decl::new(decl.func, &[]))
                    .unwrap_or_default()
            })
            .collect();
        let layout = Layout::plan(&decls);
        let color_captures = if decls
            .iter()
            .any(|decl| decl.func() == Some(TaFunc::ValueWhenColor))
        {
            decls
                .iter()
                .map(|decl| {
                    (decl.func() == Some(TaFunc::ValueWhenColor)).then(|| ColorCaptures {
                        values: vec![Value::Na; cells_for(decl) as usize],
                        next: 0,
                    })
                })
                .collect()
        } else {
            Vec::new()
        };
        TaStates {
            color_captures,
            states: vec![State::Invalid; decls.len()],
            cells: vec![f64::NAN; layout.cells as usize],
            scratch: vec![f64::NAN; layout.scratch as usize],
            decls,
        }
    }

    fn init(table: &mut TaStates) {
        for captures in table.color_captures.iter_mut().flatten() {
            captures.values.fill(Value::Na);
            captures.next = 0;
        }
        Engine::init(
            &table.decls,
            &mut table.states,
            &mut table.cells,
            &mut table.scratch,
        );
    }
}

/// [`MarketData`] as the engine's lane reader. Out-of-range bars and
/// non-finite holes both read as na, which is what the engine's own
/// finiteness checks expect.
struct MarketLanes<'a>(&'a MarketData);

impl Lanes for MarketLanes<'_> {
    fn open(&self, bar: u32) -> f64 {
        self.0.open.get(bar as usize).copied().unwrap_or(f64::NAN)
    }
    fn high(&self, bar: u32) -> f64 {
        self.0.high.get(bar as usize).copied().unwrap_or(f64::NAN)
    }
    fn low(&self, bar: u32) -> f64 {
        self.0.low.get(bar as usize).copied().unwrap_or(f64::NAN)
    }
    fn close(&self, bar: u32) -> f64 {
        self.0.close.get(bar as usize).copied().unwrap_or(f64::NAN)
    }
    fn volume(&self, bar: u32) -> f64 {
        self.0.volume.get(bar as usize).copied().unwrap_or(f64::NAN)
    }
    fn time_ms(&self, bar: u32) -> i64 {
        let index = bar as usize;
        if self.0.time.is_empty() || index < self.0.time.len() {
            self.0.time_at(index)
        } else {
            0
        }
    }
}

/// A [`Value`] operand as the kernels' [`SiteOperand`].
fn site_operand(value: &Value) -> SiteOperand<f64> {
    match value {
        Value::Na => SiteOperand::Na,
        Value::Int(v) => SiteOperand::Int(*v),
        Value::Float(v) => SiteOperand::Float(*v),
        Value::Bool(b) => SiteOperand::Bool(*b),
        _ => SiteOperand::Other,
    }
}

/// The type-mismatch message for the first operand the encoding rejects
/// (the kernels report only the unit [`ta_site::SiteMismatch`]).
fn mismatch_message(func: TaFunc, args: &[Value]) -> String {
    for (i, operand) in func.engine_operands().iter().enumerate() {
        let Some(value) = args.get(i) else { continue };
        match operand {
            TaOperand::Num => {
                if ta_site::num(site_operand(value)).is_err() {
                    return format!(
                        "{} requires numeric source operands, found {}",
                        func.name(),
                        value.type_name()
                    );
                }
            }
            TaOperand::Cond => {
                if ta_site::cond(site_operand(value)).is_err() {
                    return format!(
                        "{} requires a bool condition operand, found {}",
                        func.name(),
                        value.type_name()
                    );
                }
            }
            TaOperand::Bool => {
                if !matches!(value, Value::Bool(_) | Value::Na) {
                    return format!(
                        "{} requires a bool source, found {}",
                        func.name(),
                        value.type_name()
                    );
                }
            }
            TaOperand::Color => {
                if !matches!(value, Value::Color { .. } | Value::Na) {
                    return format!(
                        "{} requires a color source, found {}",
                        func.name(),
                        value.type_name()
                    );
                }
            }
        }
    }
    format!("{} operand type mismatch", func.name())
}

/// Pack `args` (in argument order) into the engine's fixed operand slots:
/// the shared kernel `ta_site::pack` over the declared operand kinds.
/// Every engine signature has at most three operands
/// (`every_engine_signature_packs_into_three_slots`).
fn pack(func: TaFunc, args: &[Value]) -> Result<[f64; 3], String> {
    let kinds = func.engine_operands();
    let mut slots = [SiteSlot::Skip; 3];
    let mut operands = [SiteOperand::Na; 3];
    for i in 0..3 {
        if let (Some(kind), Some(value)) = (kinds.get(i), args.get(i)) {
            slots[i] = match kind {
                TaOperand::Num => SiteSlot::Num,
                TaOperand::Cond => SiteSlot::Cond,
                TaOperand::Bool | TaOperand::Color => SiteSlot::Num,
            };
            operands[i] = match (kind, value) {
                (TaOperand::Bool, Value::Bool(value)) => {
                    SiteOperand::Float(if *value { 1.0 } else { 0.0 })
                }
                (TaOperand::Color, Value::Color { r, g, b, t, .. })
                    if t.is_finite() && (0.0..=100.0).contains(t) && t.fract() == 0.0 =>
                {
                    let packed = (u32::from(*r) << 24)
                        | (u32::from(*g) << 16)
                        | (u32::from(*b) << 8)
                        | *t as u32;
                    SiteOperand::Float(f64::from(packed))
                }
                (_, value) => site_operand(value),
            };
        }
    }
    let [s0, s1, s2] = slots;
    let [op0, op1, op2] = operands;
    ta_site::pack(s0, op0, s1, op1, s2, op2).map_err(|_| mismatch_message(func, args))
}

/// The engine's `f64` result as the [`Value`] the function's result type
/// declares: the shared kernel `ta_site::decode`, except that
/// `ValueWhenInt`, `FixnanInt`, `ModeInt` and `RangeInt` keep a fractional
/// value that came from a statically `int` expression instead of truncating
/// it. The run-time-length statistics (`WINDOW_STATISTIC_DYNAMIC`) decode
/// through here as well.
pub(super) fn decode(func: TaFunc, value: f64) -> Value {
    if func == TaFunc::ValueWhenColor {
        if !value.is_finite() {
            return Value::Na;
        }
        let packed = value as u32;
        return Value::Color {
            r: (packed >> 24) as u8,
            g: (packed >> 16) as u8,
            b: (packed >> 8) as u8,
            t: f64::from(packed as u8),
            input_origin: None,
        };
    }
    if matches!(func, TaFunc::ValueWhenInt | TaFunc::FixnanInt) {
        if value.is_nan() {
            return Value::Na;
        }
        // A statically `int`-typed expression may carry a fractional quotient.
        // Keep that value fractional, while ordinary captured ints remain integer cells.
        if value.is_finite()
            && value.fract() == 0.0
            && value >= i64::MIN as f64
            && value < 9_223_372_036_854_775_808.0
        {
            return Value::Int(value as i64);
        }
        return Value::from_f64(value);
    }
    // `ta.mode` and `ta.range` of a statically `int` source that holds a
    // fraction: the mode is one of the window's values and the range a
    // difference of two, so this runtime keeps a fractional result only when
    // the source is fractional. A whole (or non-finite) result takes
    // the ordinary int path below. `MedianInt` is deliberately
    // not here, because an even window of whole ints can give a fractional
    // median that the int form truncates (docs/language-support.md, "Integer `ta.median`").
    if matches!(func, TaFunc::ModeInt | TaFunc::RangeInt)
        && value.is_finite()
        && value.fract() != 0.0
    {
        return Value::Float(value);
    }
    let kind = match func.engine_result() {
        TaResult::Float => SiteResultKind::Float,
        TaResult::Int => SiteResultKind::Int,
        TaResult::Bool => SiteResultKind::Bool,
        TaResult::Color => unreachable!("color results have a packed RGBT decoder"),
    };
    match ta_site::decode(kind, value) {
        SiteDecoded::Na => Value::Na,
        SiteDecoded::Int(v) => Value::Int(v),
        SiteDecoded::Float(v) => Value::Float(v),
        SiteDecoded::Bool(b) => Value::Bool(b),
    }
}

/// Advance one **scalar** call site and return the value it pushes.
///
/// `Ok(None)` means the declaration is not a scalar engine function (an
/// invalid declaration, or a tuple one reached through the wrong path).
/// Errors are type-mismatch messages, unreachable for validated programs
/// except through `Any`-typed operands.
pub(crate) fn advance(
    states: &mut TaStates,
    index: u32,
    decl: &TaStateDecl,
    args: &[Value],
    data: &MarketData,
    bar: usize,
) -> Result<Option<Value>, String> {
    if decl.func.is_tuple() {
        return Ok(None);
    }
    if decl.func == TaFunc::ValueWhenColor {
        if !matches!(args.first(), Some(Value::Bool(_) | Value::Na))
            || !matches!(args.get(1), Some(Value::Color { .. } | Value::Na))
        {
            return Err(mismatch_message(decl.func, args));
        }
        let captures = states
            .color_captures
            .get_mut(index as usize)
            .and_then(Option::as_mut)
            .ok_or_else(|| "missing color capture state".to_string())?;
        let id = if matches!(args.first(), Some(Value::Bool(true))) {
            captures.capture(args[1].clone()) as f64
        } else {
            f64::NAN
        };
        let numeric_args = [args[0].clone(), Value::from_f64(id)];
        let packed = pack(TaFunc::ValueWhen, &numeric_args)?;
        let output = states.advance_raw(index, packed, data, bar);
        return Ok(output.map(|out| {
            if out.value.is_finite() {
                states.color_captures[index as usize]
                    .as_ref()
                    .unwrap()
                    .values
                    .get(out.value as usize)
                    .cloned()
                    .unwrap_or(Value::Na)
            } else {
                Value::Na
            }
        }));
    }
    let packed = pack(decl.func, args)?;
    Ok(states
        .advance_raw(index, packed, data, bar)
        .map(|out| decode(decl.func, out.value)))
}

/// Advance one **tuple-returning** call site (`ta.macd`/`ta.bb`/`ta.kc`/
/// `ta.dmi`/`ta.supertrend`/banded `ta.vwap`) and return its values in tuple
/// order, each `na` until warm. The array is fixed at length 3 (the widest
/// tuple); a shorter tuple fills the trailing slots with `na`, and the VM
/// pushes only the function's `engine_result_arity()` leading values.
pub(crate) fn advance_tuple(
    states: &mut TaStates,
    index: u32,
    decl: &TaStateDecl,
    args: &[Value],
    data: &MarketData,
    bar: usize,
) -> Result<Option<[Value; 3]>, String> {
    if !decl.func.is_tuple() {
        return Ok(None);
    }
    let packed = pack(decl.func, args)?;
    Ok(states.advance_raw(index, packed, data, bar).map(|out| {
        [
            Value::from_f64(out.value),
            Value::from_f64(out.extra[0]),
            Value::from_f64(out.extra[1]),
        ]
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    fn reference_resolved_footprint<V: crate::ta_params::ParamValues>(
        values: &V,
        state: usize,
        func: TaFunc,
        source: &[i64],
    ) -> (u32, u32) {
        let (params, len) = crate::ta_params::resolve_params(values, state, source);
        let normalized = Decl::new(func, &params[..len])
            .or_else(|| Decl::new(func, &[]))
            .unwrap_or_default();
        (cells_for(&normalized), scratch_for(&normalized))
    }

    #[test]
    fn scalar_matches_keep_frozen_fallback_and_callback_trace() {
        let inputs = [
            i64::MIN,
            -1,
            0,
            1,
            100_000,
            100_001,
            i64::MAX,
            2.5f64.to_bits() as i64,
            (-0.0f64).to_bits() as i64,
        ];
        let mut cases = 0;
        for func in pinewasm_ta::TaFunc::ALL {
            for width in 0..=7 {
                for rotation in 0..inputs.len() {
                    let source: Vec<_> = (0..width)
                        .map(|i| inputs[(i + rotation) % inputs.len()])
                        .collect();
                    let trace = RefCell::new(Vec::new());
                    let reader = |s, p, default| {
                        trace.borrow_mut().push((s, p, default));
                        default
                    };
                    let actual = resolved_state_footprint(&reader, usize::MAX, func, &source);
                    let actual_trace = trace.replace(Vec::new());
                    let previous = reference_resolved_footprint(&reader, usize::MAX, func, &source);
                    assert_eq!(actual, previous, "{func:?} {source:?}");
                    assert_eq!(actual_trace, *trace.borrow());
                    assert_eq!(actual_trace.len(), if width <= 4 { width } else { 0 });
                    cases += 1;
                }
            }
        }
        assert_eq!(cases, 79 * 8 * 9);
    }

    /// Test-only complete table reader, including its indexed read, separate from production.
    struct PreviousParamFootprints<'a, V> {
        decls: &'a [TaStateDecl],
        value_at: V,
    }

    impl<V: Fn(usize, usize, i64) -> i64> ta_alloc::Footprints for PreviousParamFootprints<'_, V> {
        fn count(&self) -> usize {
            self.decls.len()
        }

        fn footprint(&self, state: usize) -> (u32, u32) {
            let decl = &self.decls[state];
            let (params, len) =
                crate::ta_params::resolve_params(&self.value_at, state, &decl.params);
            let normalized = Decl::new(decl.func, &params[..len])
                .or_else(|| Decl::new(decl.func, &[]))
                .unwrap_or_default();
            (cells_for(&normalized), scratch_for(&normalized))
        }
    }

    #[test]
    fn resolved_footprint_matches_frozen_body() {
        let inputs = [
            i64::MIN,
            -1,
            0,
            1,
            2,
            99_999,
            100_000,
            100_001,
            i64::MAX,
            2.5f64.to_bits() as i64,
            (-0.0f64).to_bits() as i64,
            f64::NAN.to_bits() as i64,
        ];
        let mut cases = 0;
        for func in pinewasm_ta::TaFunc::ALL {
            for width in 0..=7 {
                for rotation in 0..inputs.len() {
                    let source: Vec<i64> = (0..width)
                        .map(|i| inputs[(i + rotation) % inputs.len()])
                        .collect();
                    let declarations = [decl(func, &source)];
                    for state in [0, 17, usize::MAX] {
                        for mode in 0..3 {
                            let reader = |s: usize, p: usize, default: i64| match mode {
                                0 => default,
                                1 => default
                                    .wrapping_add((s as i64).wrapping_mul(17))
                                    .wrapping_sub(p as i64),
                                _ => inputs[p % inputs.len()],
                            };
                            let previous = PreviousParamFootprints {
                                decls: &declarations,
                                value_at: |_: usize, p, v| reader(state, p, v),
                            };
                            assert_eq!(
                                resolved_state_footprint(&reader, state, func, &source),
                                ta_alloc::Footprints::footprint(&previous, 0),
                                "{func:?} {source:?} state={state} mode={mode}"
                            );
                            cases += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(cases, 79 * 8 * 12 * 3 * 3);
    }

    #[test]
    fn resolved_footprint_preserves_order_defaults_and_oversize_suppression() {
        let source = [i64::MIN, (-0.0f64).to_bits() as i64, i64::MAX, -1];
        for width in 0..=source.len() {
            let trace = RefCell::new(Vec::new());
            let reader = |s, p, value| {
                trace.borrow_mut().push((s, p, value));
                value
            };
            let _ = resolved_state_footprint(&reader, usize::MAX, TaFunc::Alma, &source[..width]);
            assert_eq!(
                *trace.borrow(),
                source[..width]
                    .iter()
                    .enumerate()
                    .map(|(p, value)| (usize::MAX, p, *value))
                    .collect::<Vec<_>>()
            );
        }
        let never = |_: usize, _: usize, _: i64| -> i64 {
            panic!("an oversized source must not call the reader")
        };
        for func in pinewasm_ta::TaFunc::ALL {
            let empty = Decl::new(func, &[]).unwrap();
            assert_eq!(
                resolved_state_footprint(&never, usize::MAX, func, &[1, 2, 3, 4, 5]),
                (cells_for(&empty), scratch_for(&empty))
            );
        }
    }

    #[test]
    fn resolved_footprint_keeps_function_defaults_and_length_clamps() {
        let identity = |_: usize, _: usize, value| value;
        for value in [i64::MIN, -1, 0, 1] {
            assert_eq!(
                resolved_state_footprint(&identity, 0, TaFunc::Wma, &[value]),
                (1, 0)
            );
        }
        for value in [100_000, 100_001, i64::MAX] {
            assert_eq!(
                resolved_state_footprint(&identity, 0, TaFunc::Median, &[value]),
                (100_000, 100_000)
            );
        }
        assert_eq!(
            resolved_state_footprint(&identity, 0, TaFunc::Wma, &[]),
            (1, 0)
        );
        assert_eq!(
            resolved_state_footprint(&identity, 0, TaFunc::PivotHigh, &[]),
            (3, 0)
        );
        assert_eq!(
            resolved_state_footprint(&identity, 0, TaFunc::PivotLow, &[i64::MIN, i64::MAX]),
            (100_001, 0)
        );
        assert_eq!(
            resolved_state_footprint(&identity, 0, TaFunc::ValueWhenColor, &[i64::MAX]),
            (100_001, 0)
        );
        assert_eq!(
            resolved_state_footprint(&identity, 0, TaFunc::ValueWhenColor, &[1, 2, 3, 4, 5]),
            (1, 0)
        );
        assert_eq!(
            resolved_state_footprint(&identity, 0, TaFunc::Hma, &[i64::MAX]),
            (100_316, 0)
        );
        assert_eq!(
            resolved_state_footprint(&identity, 0, TaFunc::Swma, &[]),
            (4, 0)
        );
    }

    #[test]
    fn resolved_footprint_stops_at_native_reader_panic() {
        for stopped in 0..4 {
            let trace = RefCell::new(Vec::new());
            let reader = |s, p, value| {
                trace.borrow_mut().push((s, p, value));
                assert_ne!(p, stopped, "reader failure");
                value
            };
            let failed = catch_unwind(AssertUnwindSafe(|| {
                resolved_state_footprint(&reader, 29, TaFunc::Alma, &[11, 22, 33, 44])
            }));
            assert!(failed.is_err());
            assert_eq!(
                *trace.borrow(),
                [11, 22, 33, 44][..=stopped]
                    .iter()
                    .enumerate()
                    .map(|(p, value)| (29, p, *value))
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn resolved_footprint_table_read_precedes_callback() {
        let declarations = [decl(TaFunc::Median, &[7])];
        let calls = RefCell::new(Vec::new());
        let reader = |s, p, value| {
            calls.borrow_mut().push((s, p, value));
            value
        };
        let table = ParamFootprints {
            decls: &declarations,
            value_at: &reader,
        };
        let previous = PreviousParamFootprints {
            decls: &declarations,
            value_at: &reader,
        };
        assert_eq!(
            ta_alloc::Footprints::count(&table),
            ta_alloc::Footprints::count(&previous)
        );
        for index in [1, usize::MAX] {
            assert!(
                catch_unwind(AssertUnwindSafe(|| ta_alloc::Footprints::footprint(
                    &table, index
                )))
                .is_err()
            );
            assert!(
                catch_unwind(AssertUnwindSafe(|| ta_alloc::Footprints::footprint(
                    &previous, index
                )))
                .is_err()
            );
            assert!(calls.borrow().is_empty());
        }
        assert_eq!(ta_alloc::Footprints::footprint(&table, 0), (7, 7));
        assert_eq!(*calls.borrow(), [(0, 0, 7)]);
    }

    #[test]
    fn resolved_footprint_real_allocator_matches_reference() {
        let shapes: [&[i64]; 6] = [
            &[],
            &[i64::MIN],
            &[i64::MAX, -1],
            &[9, 8, 7, 6],
            &[1, 2, 3, 4, 5],
            &[6; 8],
        ];
        for func in pinewasm_ta::TaFunc::ALL {
            for shape in shapes {
                let declarations = [
                    decl(func, shape),
                    decl(TaFunc::Median, &[7]),
                    decl(TaFunc::ValueWhenColor, &[3]),
                ];
                let reader =
                    |s: usize, p: usize, value: i64| value.wrapping_add((s as i64) - (p as i64));
                assert_eq!(
                    TaStates::allocation_bytes_with_params(&declarations, reader),
                    allocation_reference(&declarations, reader)
                );
            }
        }
        assert_eq!(
            TaStates::allocation_bytes_with_params(&[], |_, _, v| v),
            allocation_reference(&[], |_, _, v| v)
        );
    }

    #[test]
    fn resolved_footprint_real_allocator_retains_color_replay_order() {
        let declarations = [
            decl(TaFunc::Median, &[7]),
            decl(TaFunc::ValueWhenColor, &[2, 19]),
            decl(TaFunc::Alma, &[5, -1, 9]),
            decl(TaFunc::ValueWhenColor, &[1, 2, 3, 4, 5]),
        ];
        let trace = RefCell::new(Vec::new());
        let bytes = TaStates::allocation_bytes_with_params(&declarations, |s, p, value| {
            trace.borrow_mut().push((s, p, value));
            value
        });
        assert_eq!(bytes, allocation_reference(&declarations, |_, _, v| v));
        assert_eq!(
            *trace.borrow(),
            [
                (0, 0, 7),
                (1, 0, 2),
                (1, 1, 19),
                (2, 0, 5),
                (2, 1, -1),
                (2, 2, 9),
                (1, 0, 2),
                (1, 1, 19)
            ]
        );
    }

    /// Test-only separate estimate of the engine allocation plus exact color-sidecar payload.
    fn allocation_reference(
        decls: &[TaStateDecl],
        value_at: impl Fn(usize, usize, i64) -> i64,
    ) -> Option<usize> {
        let count = decls.len();
        let mut cells = 0usize;
        let mut scratch = 0usize;
        let mut color_values = 0usize;
        for (state, decl) in decls.iter().enumerate() {
            let mut params = [0; pinewasm_ta::MAX_PARAMS];
            let len = if decl.params.len() <= params.len() {
                decl.params.len()
            } else {
                0
            };
            for (param, value) in decl.params.iter().take(len).enumerate() {
                params[param] = value_at(state, param, *value);
            }
            let normalized = Decl::new(decl.func, &params[..len])
                .or_else(|| Decl::new(decl.func, &[]))
                .unwrap_or_default();
            cells = cells.checked_add(cells_for(&normalized) as usize)?;
            if decl.func == TaFunc::ValueWhenColor {
                color_values = color_values.checked_add(cells_for(&normalized) as usize)?;
            }
            scratch = scratch.max(scratch_for(&normalized) as usize);
        }
        let sidecar_headers = if decls.iter().any(|decl| decl.func == TaFunc::ValueWhenColor) {
            count.checked_mul(std::mem::size_of::<Option<ColorCaptures>>())?
        } else {
            0
        };
        count
            .checked_mul(std::mem::size_of::<Decl>())?
            .checked_add(count.checked_mul(pinewasm_ta::STATE_BYTES as usize)?)?
            .checked_add(
                cells
                    .checked_add(scratch)?
                    .checked_mul(std::mem::size_of::<f64>())?,
            )?
            .checked_add(sidecar_headers)?
            .checked_add(color_values.checked_mul(std::mem::size_of::<Value>())?)
    }

    #[test]
    fn allocation_kernel_matches_reference() {
        let mut seed = 0x0DD_BA11u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let all = pinewasm_ta::TaFunc::ALL;
        for round in 0..3_000 {
            let n = (next() % 9) as usize;
            let decls: Vec<TaStateDecl> = (0..n)
                .map(|_| {
                    let func = all[(next() % all.len() as u64) as usize];
                    let arity = (next() % 5) as usize;
                    let params: Vec<i64> = (0..arity)
                        .map(|_| match next() % 5 {
                            0 => i64::MAX,
                            1 => -1,
                            2 => 100_000,
                            _ => (next() % 400) as i64,
                        })
                        .collect();
                    decl(func, &params)
                })
                .collect();
            let bump = (round % 7) as i64;
            let value_at = |state: usize, param: usize, v: i64| {
                v.saturating_add(bump * (state + param) as i64)
            };
            assert_eq!(
                TaStates::allocation_bytes_with_params(&decls, value_at),
                allocation_reference(&decls, value_at),
                "{decls:?}"
            );
            assert_eq!(
                TaStates::allocation_bytes(&decls),
                allocation_reference(&decls, |_, _, v| v)
            );
        }
    }

    fn decl(func: TaFunc, params: &[i64]) -> TaStateDecl {
        TaStateDecl {
            func,
            params: params.to_vec(),
        }
    }

    fn market(close: &[f64]) -> MarketData {
        MarketData {
            open: close.to_vec(),
            high: close.to_vec(),
            low: close.to_vec(),
            close: close.to_vec(),
            volume: vec![1.0; close.len()],
            ..MarketData::default()
        }
    }

    #[test]
    fn color_capture_sidecars_are_bounded_budgeted_and_absent_for_other_sites() {
        let numeric = [decl(TaFunc::Rma, &[14]), decl(TaFunc::ValueWhen, &[4])];
        assert_eq!(TaStates::new(&numeric).color_captures.capacity(), 0);
        let decls = [
            decl(TaFunc::Rma, &[14]),
            decl(TaFunc::ValueWhenColor, &[4]),
            decl(TaFunc::ValueWhenColor, &[0]),
        ];
        let mut states = TaStates::new(&decls);
        assert_eq!(states.color_captures.len(), 3);
        assert!(states.color_captures[0].is_none());
        assert_eq!(states.color_captures[1].as_ref().unwrap().values.len(), 5);
        assert_eq!(states.color_captures[2].as_ref().unwrap().values.len(), 1);
        let allocated = states.decls.capacity() * std::mem::size_of::<Decl>()
            + states.states.capacity() * std::mem::size_of::<State>()
            + (states.cells.capacity() + states.scratch.capacity()) * std::mem::size_of::<f64>()
            + states.color_captures.capacity() * std::mem::size_of::<Option<ColorCaptures>>()
            + states
                .color_captures
                .iter()
                .flatten()
                .map(|capture| capture.values.capacity() * std::mem::size_of::<Value>())
                .sum::<usize>();
        // The published state stride is deliberately conservative across
        // targets. All other vector payloads, including the color sidecar,
        // are accounted exactly rather than excluded from admission.
        let state_padding =
            decls.len() * (pinewasm_ta::STATE_BYTES as usize - std::mem::size_of::<State>());
        assert_eq!(
            TaStates::allocation_bytes(&decls),
            Some(allocated + state_padding)
        );
        let address = states.color_captures[1].as_ref().unwrap().values.as_ptr();
        let data = market(&[1.0]);
        for bar in 0..1500 {
            let color = Value::Color {
                r: 18,
                g: 52,
                b: 86,
                t: (bar % 256) as f64 * 100.0 / 255.0,
                input_origin: None,
            };
            advance(
                &mut states,
                1,
                &decls[1],
                &[Value::Bool(true), color],
                &data,
                0,
            )
            .unwrap();
        }
        let captures = states.color_captures[1].as_ref().unwrap();
        assert_eq!(captures.values.as_ptr(), address);
        assert_eq!(captures.values.len(), 5);
        assert_eq!(captures.values.capacity(), 5);
    }

    #[test]
    fn color_capture_snapshot_restore_and_reset_keep_exact_values_and_na() {
        let decl = decl(TaFunc::ValueWhenColor, &[2]);
        let mut states = TaStates::new(std::slice::from_ref(&decl));
        let data = market(&[1.0]);
        let mut captures = Vec::new();
        for bar in 0..13 {
            let condition = bar % 3 != 1;
            let color = if bar % 5 == 0 {
                Value::Na
            } else {
                Value::Color {
                    r: 18,
                    g: 52,
                    b: 86,
                    t: bar as f64 * 0.1,
                    input_origin: None,
                }
            };
            if condition {
                captures.push(color.clone());
            }
            let expected = captures
                .len()
                .checked_sub(3)
                .map_or(Value::Na, |i| captures[i].clone());
            assert_eq!(
                advance(
                    &mut states,
                    0,
                    &decl,
                    &[Value::Bool(condition), color],
                    &data,
                    0
                )
                .unwrap(),
                Some(expected)
            );
        }
        let snapshot = states.clone();
        let captured_before_frontier = captures.clone();
        for branch in [47.6, 63.75] {
            states = snapshot.clone();
            captures = captured_before_frontier.clone();
            for bar in 13..40 {
                let condition = bar % 3 != 1;
                let color = if bar % 5 == 0 {
                    Value::Na
                } else {
                    Value::Color {
                        r: 18,
                        g: 52,
                        b: 86,
                        t: branch,
                        input_origin: None,
                    }
                };
                if condition {
                    captures.push(color.clone());
                }
                let expected = captures[captures.len() - 3].clone();
                assert_eq!(
                    advance(
                        &mut states,
                        0,
                        &decl,
                        &[Value::Bool(condition), color],
                        &data,
                        0
                    )
                    .unwrap(),
                    Some(expected)
                );
            }
        }
        states.reset();
        let reset = states.color_captures[0].as_ref().unwrap();
        assert_eq!(reset.next, 0);
        assert!(reset.values.iter().all(|value| *value == Value::Na));
        let mut fresh = TaStates::new(std::slice::from_ref(&decl));
        for condition in [false, true, true, false, true] {
            let args = [
                Value::Bool(condition),
                Value::Color {
                    r: 18,
                    g: 52,
                    b: 86,
                    t: 47.6,
                    input_origin: None,
                },
            ];
            let actual = advance(&mut states, 0, &decl, &args, &data, 0).unwrap();
            let expected = advance(&mut fresh, 0, &decl, &args, &data, 0).unwrap();
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn every_engine_signature_packs_into_three_slots() {
        for func in TaFunc::ALL {
            assert!(func.engine_operands().len() <= 3, "{func:?}");
        }
    }

    #[test]
    fn na_numeric_operand_packs_as_nan() {
        let packed = pack(TaFunc::Rma, &[Value::Na]).unwrap();
        assert!(packed.iter().all(|v| v.is_nan()));
        let packed = pack(
            TaFunc::Stoch,
            &[Value::Na, Value::Int(3), Value::Float(2.5)],
        )
        .unwrap();
        assert!(packed[0].is_nan());
        assert_eq!(packed[1].to_bits(), 3.0f64.to_bits());
        assert_eq!(packed[2].to_bits(), 2.5f64.to_bits());
    }

    #[test]
    fn na_condition_packs_as_zero() {
        let packed = pack(TaFunc::ValueWhen, &[Value::Na, Value::Na]).unwrap();
        assert_eq!(packed[0].to_bits(), 0.0f64.to_bits());
        assert!(packed[1].is_nan());
        assert!(packed[2].is_nan());
        let packed = pack(TaFunc::BarsSince, &[Value::Bool(true)]).unwrap();
        assert_eq!(packed[0].to_bits(), 1.0f64.to_bits());
    }

    #[test]
    fn mismatch_messages_are_unchanged() {
        assert_eq!(
            pack(TaFunc::Rma, &[Value::Bool(true)]).unwrap_err(),
            format!(
                "{} requires numeric source operands, found {}",
                TaFunc::Rma.name(),
                Value::Bool(true).type_name()
            )
        );
        assert_eq!(
            pack(TaFunc::ValueWhen, &[Value::Float(1.0), Value::Float(2.0)]).unwrap_err(),
            format!(
                "{} requires a bool condition operand, found {}",
                TaFunc::ValueWhen.name(),
                Value::Float(1.0).type_name()
            )
        );
        // The first mismatch wins, in argument order.
        assert_eq!(
            pack(
                TaFunc::VwapBands,
                &[Value::Bool(false), Value::Int(1), Value::Na]
            )
            .unwrap_err(),
            format!(
                "{} requires numeric source operands, found {}",
                TaFunc::VwapBands.name(),
                Value::Bool(false).type_name()
            )
        );
    }

    #[test]
    fn nan_result_decodes_as_na_for_every_kind() {
        // Int result (ta.barssince), Bool result (ta.crossover), Float (ta.rma).
        assert_eq!(decode(TaFunc::BarsSince, f64::NAN), Value::Na);
        assert_eq!(decode(TaFunc::Crossover, f64::NAN), Value::Na);
        assert_eq!(decode(TaFunc::Rma, f64::NAN), Value::Na);
        assert_eq!(decode(TaFunc::Rma, f64::INFINITY), Value::Na);
        assert_eq!(decode(TaFunc::BarsSince, 4.0), Value::Int(4));
        assert_eq!(decode(TaFunc::Crossover, 0.0), Value::Bool(false));
        assert_eq!(decode(TaFunc::Crossover, -0.0), Value::Bool(false));
        assert_eq!(decode(TaFunc::Crossover, 1.0), Value::Bool(true));
        assert_eq!(decode(TaFunc::Rma, 1.5), Value::Float(1.5));
    }

    #[test]
    fn int_valuewhen_and_fixnan_keep_fractional_results() {
        for func in [TaFunc::ValueWhenInt, TaFunc::FixnanInt] {
            assert_eq!(decode(func, 2.0), Value::Int(2), "{func:?}");
            assert_eq!(decode(func, 2.5), Value::Float(2.5), "{func:?}");
            assert_eq!(decode(func, f64::NAN), Value::Na, "{func:?}");
        }
    }

    #[test]
    fn integer_statistic_overloads_decode_as_int() {
        for func in [TaFunc::MedianInt, TaFunc::ModeInt, TaFunc::RangeInt] {
            assert_eq!(func.engine_result(), TaResult::Int, "{func:?}");
            assert_eq!(decode(func, 7.0), Value::Int(7), "{func:?}");
            assert_eq!(decode(func, f64::NAN), Value::Na, "{func:?}");
        }
    }

    #[test]
    fn na_round_trips_through_the_engine_encoding() {
        // A numeric na operand is NaN on the way in, and that NaN decodes as na
        // for every result kind (Float, Int, Bool).
        let raw = pack(TaFunc::Rma, &[Value::Na]).unwrap()[0];
        for func in [TaFunc::Rma, TaFunc::HighestBars, TaFunc::Rising] {
            assert_eq!(decode(func, raw), Value::Na, "{func:?}");
        }
    }

    fn regions(t: &TaStates) -> (Vec<Decl>, Vec<State>, Vec<u64>) {
        (
            t.decls.clone(),
            t.states.clone(),
            t.cells.iter().map(|c| c.to_bits()).collect(),
        )
    }

    #[test]
    fn new_equals_reset_after_advance() {
        let decls = [
            decl(TaFunc::Rma, &[3]),
            decl(TaFunc::Highest, &[4]),
            decl(TaFunc::Median, &[5]),
            decl(TaFunc::BarsSince, &[]),
        ];
        let fresh = TaStates::new(&decls);
        let mut table = TaStates::new(&decls);
        let data = market(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]);
        for bar in 0..data.bars() {
            for (i, d) in decls.iter().enumerate() {
                let args: Vec<Value> = match d.func {
                    TaFunc::BarsSince => vec![Value::Bool(bar % 2 == 0)],
                    _ => vec![Value::Float(bar as f64)],
                };
                advance(&mut table, i as u32, d, &args, &data, bar).unwrap();
            }
        }
        assert_ne!(regions(&table), regions(&fresh));
        table.reset();
        assert_eq!(regions(&table), regions(&fresh));
        // Scratch is write-before-read working memory: only its size is fixed.
        assert_eq!(table.scratch.len(), fresh.scratch.len());
    }
}

#[cfg(test)]
mod color_allocation_tests {
    use super::*;
    use std::cell::RefCell;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Read {
        Count,
        Footprint(usize, usize),
    }

    struct TraceTable {
        rows: Vec<(u32, u32)>,
        visits: RefCell<Vec<usize>>,
        trace: RefCell<Vec<Read>>,
        second_extra: u32,
        panic_at: Option<usize>,
    }

    impl TraceTable {
        fn new(rows: Vec<(u32, u32)>, second_extra: u32, panic_at: Option<usize>) -> Self {
            Self {
                visits: RefCell::new(vec![0; rows.len()]),
                rows,
                trace: RefCell::new(Vec::new()),
                second_extra,
                panic_at,
            }
        }
    }

    impl ta_alloc::Footprints for TraceTable {
        fn count(&self) -> usize {
            self.trace.borrow_mut().push(Read::Count);
            self.rows.len()
        }

        fn footprint(&self, index: usize) -> (u32, u32) {
            let mut visits = self.visits.borrow_mut();
            let visit = visits[index];
            visits[index] += 1;
            let mut trace = self.trace.borrow_mut();
            let call = trace.len() - 1;
            trace.push(Read::Footprint(index, visit));
            assert_ne!(self.panic_at, Some(call), "footprint callback panic");
            let (cells, scratch) = self.rows[index];
            (
                if visit == 0 {
                    cells
                } else {
                    cells.saturating_add(self.second_extra)
                },
                scratch,
            )
        }
    }

    fn decl(func: TaFunc, params: Vec<i64>) -> TaStateDecl {
        TaStateDecl { func, params }
    }

    // Test-only copy of the Color wrapper's expressions, with numeric sizes as test inputs.
    fn reference_color_bytes<P: ta_alloc::Footprints>(
        table: &P,
        decls: &[TaStateDecl],
        decl_bytes: usize,
        state_bytes: usize,
        color_header_bytes: usize,
        value_bytes: usize,
    ) -> Option<usize> {
        let base = ta_alloc::allocation_bytes(table, decl_bytes, state_bytes)?;
        let headers = if decls.iter().any(|decl| decl.func == TaFunc::ValueWhenColor) {
            decls.len().checked_mul(color_header_bytes)?
        } else {
            0
        };
        let mut captures = 0usize;
        for (index, decl) in decls.iter().enumerate() {
            if decl.func == TaFunc::ValueWhenColor {
                let (cells, _) = table.footprint(index);
                captures = captures.checked_add((cells as usize).checked_mul(value_bytes)?)?;
            }
        }
        base.checked_add(headers)?.checked_add(captures)
    }

    fn panic_text(value: Box<dyn std::any::Any + Send>) -> String {
        if let Some(text) = value.downcast_ref::<String>() {
            text.clone()
        } else if let Some(text) = value.downcast_ref::<&str>() {
            (*text).to_owned()
        } else {
            "non-string panic".to_owned()
        }
    }

    fn observed<T: std::fmt::Debug>(call: impl FnOnce() -> T) -> Result<T, String> {
        catch_unwind(AssertUnwindSafe(call)).map_err(panic_text)
    }

    #[test]
    fn frozen_color_grid_matches_result_and_callback_order() {
        let max = usize::MAX;
        let sizes = [
            [0, 0, 0, 0],
            [
                40,
                128,
                std::mem::size_of::<Option<ColorCaptures>>(),
                std::mem::size_of::<Value>(),
            ],
            [max, 0, 0, 0],
            [0, max, 0, 0],
            [0, 0, max, 0],
            [0, 0, 0, max],
            [max / 2 + 1, 0, 0, 0],
            [1, max, 0, 0],
            [0, 0, max / 2 + 1, 0],
            [0, 0, 1, max / 2 + 1],
            [1, 1, 1, 1],
            [3, 5, 7, 11],
        ];
        let cells = [0, 1, 2, 100_000, 200_001, u32::MAX];
        let scratch = [0, 1, 100_000, u32::MAX];
        let mut checked = 0;
        for len in 0..=4 {
            for mask in 0..(1usize << len) {
                let decls: Vec<_> = (0..len)
                    .map(|i| {
                        decl(
                            if mask & (1 << i) == 0 {
                                TaFunc::Rma
                            } else {
                                TaFunc::ValueWhenColor
                            },
                            vec![2],
                        )
                    })
                    .collect();
                for seed in 0..16 {
                    let rows: Vec<_> = (0..len)
                        .map(|i| {
                            (
                                cells[(seed + i) % cells.len()],
                                scratch[(seed + i) % scratch.len()],
                            )
                        })
                        .collect();
                    for [d, st, h, v] in sizes {
                        let old = TraceTable::new(rows.clone(), seed as u32, None);
                        let new = TraceTable::new(rows.clone(), seed as u32, None);
                        assert_eq!(
                            ta_alloc::allocation_bytes_with_color(&new, &decls, d, st, h, v),
                            reference_color_bytes(&old, &decls, d, st, h, v),
                            "len={len} mask={mask} seed={seed} sizes={d}/{st}/{h}/{v}",
                        );
                        assert_eq!(*new.trace.borrow(), *old.trace.borrow());
                        checked += 1;
                    }
                }
            }
        }
        assert_eq!(checked, 5_952);
    }

    #[test]
    fn headers_cover_all_rows_and_only_colors_read_twice() {
        let decls = vec![
            decl(TaFunc::Rma, vec![]),
            decl(TaFunc::ValueWhenColor, vec![]),
            decl(TaFunc::Tr, vec![]),
        ];
        let table = TraceTable::new(vec![(1, 0), (2, 1), (3, 0)], 4, None);
        // Base = 6*8 + 1*8; headers = 3*7; capture uses SECOND cells = 6.
        assert_eq!(
            ta_alloc::allocation_bytes_with_color(&table, &decls, 0, 0, 7, 11),
            Some(143)
        );
        assert_eq!(
            *table.trace.borrow(),
            vec![
                Read::Count,
                Read::Footprint(0, 0),
                Read::Footprint(1, 0),
                Read::Footprint(2, 0),
                Read::Footprint(1, 1)
            ]
        );
        let plain = vec![decl(TaFunc::Rma, vec![]), decl(TaFunc::Tr, vec![])];
        let table = TraceTable::new(vec![(1, 0), (2, 1)], 4, None);
        assert_eq!(
            ta_alloc::allocation_bytes_with_color(&table, &plain, 0, 0, usize::MAX, usize::MAX),
            Some(32)
        );
        assert_eq!(
            *table.trace.borrow(),
            vec![Read::Count, Read::Footprint(0, 0), Read::Footprint(1, 0)]
        );
    }

    #[test]
    fn checked_failures_keep_callback_prefix() {
        let decls = vec![
            decl(TaFunc::ValueWhenColor, vec![]),
            decl(TaFunc::ValueWhenColor, vec![]),
        ];
        let cases = [
            (vec![(0, 0), (0, 0)], [usize::MAX, 0, 0, 0], 3), // Base header multiply.
            (vec![(0, 0), (0, 0)], [0, 0, usize::MAX, 0], 3), // Color headers, before second reads.
            (vec![(2, 0), (2, 0)], [0, 0, 0, usize::MAX], 4), // First capture multiply.
            (vec![(1, 0), (1, 0)], [0, 0, 0, usize::MAX], 5), // Capture prefix addition.
            (vec![(1, 0), (1, 0)], [0, 0, usize::MAX / 2, 0], 5), // Base plus Color headers.
            (vec![(1, 0), (1, 0)], [0, 0, 0, usize::MAX / 2], 5), // Final capture addition.
        ];
        for (rows, [d, st, h, v], calls) in cases {
            let old = TraceTable::new(rows.clone(), 0, None);
            let new = TraceTable::new(rows, 0, None);
            assert_eq!(
                ta_alloc::allocation_bytes_with_color(&new, &decls, d, st, h, v),
                None
            );
            assert_eq!(reference_color_bytes(&old, &decls, d, st, h, v), None);
            assert_eq!(*new.trace.borrow(), *old.trace.borrow());
            assert_eq!(new.trace.borrow().len(), calls);
        }
    }

    #[test]
    fn callback_panics_keep_the_same_prefix_and_payload() {
        let decls = vec![
            decl(TaFunc::ValueWhenColor, vec![]),
            decl(TaFunc::Rma, vec![]),
            decl(TaFunc::ValueWhenColor, vec![]),
        ];
        for panic_at in 0..=5 {
            let old = TraceTable::new(vec![(1, 0); 3], 7, Some(panic_at));
            let new = TraceTable::new(vec![(1, 0); 3], 7, Some(panic_at));
            let actual =
                observed(|| ta_alloc::allocation_bytes_with_color(&new, &decls, 3, 5, 7, 11));
            let expected = observed(|| reference_color_bytes(&old, &decls, 3, 5, 7, 11));
            assert_eq!(actual, expected);
            assert_eq!(*new.trace.borrow(), *old.trace.borrow());
            assert_eq!(actual.is_err(), panic_at < 5);
        }
    }

    #[test]
    fn arbitrary_helper_count_is_not_assumed_to_equal_decl_count() {
        let decls = vec![
            decl(TaFunc::ValueWhenColor, vec![]),
            decl(TaFunc::Rma, vec![]),
            decl(TaFunc::Tr, vec![]),
        ];
        let table = TraceTable::new(vec![(2, 0)], 0, None);
        assert_eq!(
            ta_alloc::allocation_bytes_with_color(&table, &decls, 3, 5, 7, 11),
            Some(67)
        );
        let decls = vec![
            decl(TaFunc::Rma, vec![]),
            decl(TaFunc::ValueWhenColor, vec![]),
        ];
        let old = TraceTable::new(vec![(2, 0)], 0, None);
        let new = TraceTable::new(vec![(2, 0)], 0, None);
        assert_eq!(
            observed(|| ta_alloc::allocation_bytes_with_color(&new, &decls, 3, 5, 7, 11)),
            observed(|| reference_color_bytes(&old, &decls, 3, 5, 7, 11)),
        );
        assert_eq!(*new.trace.borrow(), *old.trace.borrow());
    }

    #[test]
    fn frozen_full_wrapper_matches_all_tags_widths_bits_and_stateful_reads() {
        let values = [
            i64::MIN,
            i64::MIN + 1,
            -1,
            0,
            1,
            2,
            100_001,
            i64::MAX,
            2.5f64.to_bits() as i64,
        ];
        let mut checked = 0;
        for func in TaFunc::ALL {
            for width in 0..=7 {
                for seed in 0..values.len() {
                    let params = (0..width)
                        .map(|i| values[(seed + i) % values.len()])
                        .collect();
                    let decls = vec![
                        decl(func, params),
                        decl(TaFunc::ValueWhenColor, vec![2]),
                        decl(TaFunc::Rma, vec![3]),
                    ];
                    let old_trace = RefCell::new(Vec::new());
                    let new_trace = RefCell::new(Vec::new());
                    let reader =
                        |trace: &RefCell<Vec<(usize, usize, i64, i64)>>, state, param, default| {
                            let call = trace.borrow().len();
                            let value = if call % 3 == 0 {
                                values[(seed + call) % values.len()]
                            } else {
                                default
                            };
                            trace.borrow_mut().push((state, param, default, value));
                            value
                        };
                    assert_eq!(
                        TaStates::allocation_bytes_with_params(&decls, |st, pa, d| reader(
                            &new_trace, st, pa, d
                        )),
                        reference_allocation_bytes_with_params(&decls, |st, pa, d| reader(
                            &old_trace, st, pa, d
                        )),
                        "func={func:?} width={width} seed={seed}",
                    );
                    assert_eq!(*new_trace.borrow(), *old_trace.borrow());
                    checked += 1;
                }
            }
        }
        assert_eq!(checked, TaFunc::ALL.len() * 8 * values.len());
        assert_eq!(TaFunc::ALL.len(), 79);
    }

    #[test]
    fn full_wrapper_callback_panics_and_oversize_suppression_match() {
        let decls = vec![
            decl(TaFunc::ValueWhenColor, vec![2]),
            decl(TaFunc::Rma, vec![3]),
            decl(TaFunc::ValueWhenColor, vec![i64::MIN; 5]),
        ];
        for fail in 0..=3 {
            let old_trace = RefCell::new(Vec::new());
            let new_trace = RefCell::new(Vec::new());
            let reader = |trace: &RefCell<Vec<(usize, usize, i64)>>, state, param, default| {
                let call = trace.borrow().len();
                trace.borrow_mut().push((state, param, default));
                assert_ne!(call, fail, "actual param reader panic");
                default
            };
            let actual = observed(|| {
                TaStates::allocation_bytes_with_params(&decls, |st, pa, d| {
                    reader(&new_trace, st, pa, d)
                })
            });
            let expected = observed(|| {
                reference_allocation_bytes_with_params(&decls, |st, pa, d| {
                    reader(&old_trace, st, pa, d)
                })
            });
            assert_eq!(actual, expected);
            assert_eq!(*new_trace.borrow(), *old_trace.borrow());
            assert!(new_trace.borrow().iter().all(|(state, _, _)| *state != 2));
            assert_eq!(actual.is_err(), fail < 3);
        }
    }

    #[test]
    fn predicted_bytes_match_small_real_table_payloads() {
        let cases = [
            vec![],
            vec![decl(TaFunc::Sma, vec![3]), decl(TaFunc::Rma, vec![4])],
            vec![decl(TaFunc::ValueWhenColor, vec![0])],
            vec![
                decl(TaFunc::Sma, vec![3]),
                decl(TaFunc::ValueWhenColor, vec![2]),
                decl(TaFunc::Rma, vec![4]),
            ],
            vec![
                decl(TaFunc::ValueWhenColor, vec![0; 5]),
                decl(TaFunc::ValueWhenColor, vec![3]),
            ],
            vec![
                decl(TaFunc::ValueWhenColor, vec![i64::MIN]),
                decl(TaFunc::ValueWhenColor, vec![i64::MAX]),
            ],
        ];
        for decls in cases {
            let table = TaStates::new(&decls);
            let base = table.decls.len() * std::mem::size_of::<Decl>()
                + table.states.len() * pinewasm_ta::STATE_BYTES as usize
                + (table.cells.len() + table.scratch.len()) * std::mem::size_of::<f64>();
            let headers = table.color_captures.len() * std::mem::size_of::<Option<ColorCaptures>>();
            let captures: usize = table
                .color_captures
                .iter()
                .flatten()
                .map(|capture| capture.values.len() * std::mem::size_of::<Value>())
                .sum();
            assert_eq!(
                TaStates::allocation_bytes(&decls),
                Some(base + headers + captures)
            );
            assert_eq!(
                TaStates::allocation_bytes(&decls),
                reference_allocation_bytes_with_params(&decls, |_, _, d| d)
            );
            assert_eq!(std::mem::size_of::<Decl>(), 40);
            assert!(std::mem::size_of::<State>() <= pinewasm_ta::STATE_BYTES as usize);
        }
    }
    fn reference_allocation_bytes_with_params(
        decls: &[TaStateDecl],
        value_at: impl Fn(usize, usize, i64) -> i64,
    ) -> Option<usize> {
        let footprints = ParamFootprints {
            decls,
            value_at: &value_at,
        };
        let base = ta_alloc::allocation_bytes(
            &footprints,
            std::mem::size_of::<Decl>(),
            pinewasm_ta::STATE_BYTES as usize,
        )?;
        let headers = if decls.iter().any(|decl| decl.func == TaFunc::ValueWhenColor) {
            decls
                .len()
                .checked_mul(std::mem::size_of::<Option<ColorCaptures>>())?
        } else {
            0
        };
        let mut captures = 0usize;
        for (index, decl) in decls.iter().enumerate() {
            if decl.func == TaFunc::ValueWhenColor {
                let (cells, _) = ta_alloc::Footprints::footprint(&footprints, index);
                captures = captures
                    .checked_add((cells as usize).checked_mul(std::mem::size_of::<Value>())?)?;
            }
        }
        base.checked_add(headers)?.checked_add(captures)
    }
}
