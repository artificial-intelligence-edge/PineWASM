//! The stateful `ta.*` engine, shared by the interpreter and every
//! emitted artifact.
//!
//! # Why this is its own crate
//!
//! The engine used to live in `pinewasm_vm`, and a compiled artifact
//! reached it through a `host.ta` import — which meant an artifact could not
//! run anywhere but inside a host that provided it. Retiring that import
//! required the engine *inside* the module, and the one thing worse
//! than an imported engine is two engines: a second implementation compiled
//! into artifacts would drift from the interpreter one ulp at a time, and the
//! drift would surface as "the chart and the backtest disagree".
//!
//! So the engine moved here, `no_std` and allocation-free, and both sides
//! call it:
//!
//! * `pinewasm_vm::ta` wraps it with the interpreter's `Value` typing and
//!   error messages.
//! * `pinewasm_aot_runtime` compiles it to WebAssembly;
//!   `pinewasm_aot` splices that module into every artifact that needs it.
//!
//! # Allocation-free by construction
//!
//! Every ring capacity comes from a baked const param, so [`Layout::plan`]
//! sizes a program's whole engine region at compile time and the emitter
//! reserves exactly that much linear memory. Nothing here allocates, indexes
//! without a bounds check, or panics: a reachable panic in the artifact build
//! would pull `core::fmt`'s vtables (and an element section) into every
//! module, and in the browser an aborting artifact is a hung worker.
//!
//! # Numerics
//!
//! `sqrt`/`floor`/`ceil`/`round` come from `libm` because `std`'s versions
//! are unavailable in `no_std`; all four are exactly-rounded operations, so
//! the software and hardware results are bit-identical. The one transcendental
//! the engine reaches (`ta.alma`'s Gaussian kernel) goes through
//! [`pinewasm_num`], the same `exp` implementation that `math.exp` uses.

#![no_std]
#![forbid(unsafe_code)]

mod arena;
mod engine;
mod func;
mod state;

pub use engine::{Engine, Lanes, Out};
pub use func::{TaCost, TaFunc, TaOperand, TaParamRule, TaResult};
pub use state::{cells_for, scratch_for, Layout, Smoother, State, STATE_BYTES};

/// Charon/Aeneas extraction roots (see `arena::aeneas_roots` and
/// `state::aeneas_roots`). Compiled only with `--features aeneas`.
#[cfg(feature = "aeneas")]
pub mod aeneas_roots {
    pub use crate::arena::aeneas_roots::*;
    pub use crate::arena::aeneas_roots_ta_arena2::*;
    pub use crate::state::aeneas_roots::*;
}

/// Aeneas extraction roots (`Engine::reset` and the per-site state
/// footprints). Compiled only with `--features aeneas`.
#[cfg(feature = "aeneas")]
pub mod aeneas_roots_ta_engine;

/// Extraction-only entry point for the const-parameter rule table.
#[cfg(feature = "aeneas")]
pub mod aeneas_roots_param_rules;

/// Const params one declaration can carry.
///
/// Four is the widest engine signature (`ta.alma`'s length/offset/sigma/floor).
/// It is a fixed array rather than a slice so a [`Decl`] is plain data an
/// artifact can hold in a data segment; [`Decl::new`] refuses anything wider
/// rather than truncating, and `every_engine_signature_fits_the_decl` pins the
/// bound against the function table.
pub const MAX_PARAMS: usize = 4;

/// One engine-backed call site as plain data: the function tag plus its
/// compile-time const params.
///
/// This is the `no_std` shape of `pinewasm_hir::TaStateDecl`, whose params
/// are a `Vec`. The emitter converts once and writes the result into the
/// artifact as a data segment, so the module carries its own declaration
/// table and needs nothing from a host.
///
/// `tag` is a [`TaFunc`] discriminant rather than the enum itself: the field
/// is read back out of linear memory, and a value that never round-trips
/// through a checked [`TaFunc::from_tag`] could not be trusted to name a real
/// variant.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decl {
    tag: u16,
    nparams: u16,
    _pad: u32,
    params: [i64; MAX_PARAMS],
}

impl Decl {
    /// Bytes one entry occupies in an artifact's declaration table.
    pub const SIZE: usize = 8 + 8 * MAX_PARAMS;

    /// `None` when the declaration carries more const params than
    /// [`MAX_PARAMS`] — impossible for any current signature, and a refusal
    /// rather than a silent truncation if that ever changes.
    pub fn new(func: TaFunc, params: &[i64]) -> Option<Decl> {
        if params.len() > MAX_PARAMS {
            return None;
        }
        // Keep the four ordered copies explicit for the extracted constructor.
        let stored = [
            if !params.is_empty() { params[0] } else { 0 },
            if params.len() > 1 { params[1] } else { 0 },
            if params.len() > 2 { params[2] } else { 0 },
            if params.len() > 3 { params[3] } else { 0 },
        ];
        Some(Decl {
            tag: func.tag(),
            nparams: params.len() as u16,
            _pad: 0,
            params: stored,
        })
    }

    /// The declared function, or `None` for a tag no variant claims.
    pub fn func(&self) -> Option<TaFunc> {
        TaFunc::from_tag(self.tag)
    }

    /// Const param `i`, or `None` when the declaration has fewer.
    pub fn param(&self, i: usize) -> Option<i64> {
        if i >= self.nparams as usize {
            return None;
        }
        self.params.get(i).copied()
    }

    /// Little-endian bytes, the form an artifact's data segment carries.
    pub fn to_le_bytes(&self) -> [u8; Decl::SIZE] {
        let mut out = [0u8; Decl::SIZE];
        let (head, tail) = out.split_at_mut(8);
        head[0..2].copy_from_slice(&self.tag.to_le_bytes());
        head[2..4].copy_from_slice(&self.nparams.to_le_bytes());
        for (i, param) in self.params.iter().enumerate() {
            if let Some(slot) = tail.get_mut(i * 8..i * 8 + 8) {
                slot.copy_from_slice(&param.to_le_bytes());
            }
        }
        out
    }

    /// Read one entry back from an artifact's declaration table. `None` when
    /// the slice is short or the tag names no variant.
    pub fn from_le_bytes(bytes: &[u8]) -> Option<Decl> {
        let tag = u16::from_le_bytes([*bytes.first()?, *bytes.get(1)?]);
        let nparams = u16::from_le_bytes([*bytes.get(2)?, *bytes.get(3)?]);
        if nparams as usize > MAX_PARAMS || TaFunc::from_tag(tag).is_none() {
            return None;
        }
        let mut params = [0i64; MAX_PARAMS];
        for (i, slot) in params.iter_mut().enumerate() {
            let start = 8 + i * 8;
            let chunk = bytes.get(start..start + 8)?;
            let mut buf = [0u8; 8];
            buf.copy_from_slice(chunk);
            *slot = i64::from_le_bytes(buf);
        }
        Some(Decl {
            tag,
            nparams,
            _pad: 0,
            params,
        })
    }
}

// The artifact's engine copies the table into its own region, stepping by
// `size_of::<Decl>()` while the emitter reserved `count * Decl::SIZE`. They
// have to be the same number, and `#[repr(C)]` is what makes that checkable.
const _: () = assert!(
    core::mem::size_of::<Decl>() == Decl::SIZE,
    "Decl's in-memory size drifted from its wire size"
);

impl Default for Decl {
    /// A `Stateless` declaration with no params — what an untouched slot in a
    /// state array reads as.
    fn default() -> Decl {
        Decl {
            tag: TaFunc::Tr.tag(),
            nparams: 0,
            _pad: 0,
            params: [0; MAX_PARAMS],
        }
    }
}

#[cfg(test)]
mod decl_scalar_slots_tests {
    use super::*;

    fn reference_decl_new(func: TaFunc, params: &[i64]) -> Option<Decl> {
        if params.len() > MAX_PARAMS {
            return None;
        }
        let mut stored = [0i64; MAX_PARAMS];
        for (slot, value) in stored.iter_mut().zip(params) {
            *slot = *value;
        }
        Some(Decl {
            tag: func.tag(),
            nparams: params.len() as u16,
            _pad: 0,
            params: stored,
        })
    }

    #[test]
    fn scalar_constructor_matches_complete_frozen_body() {
        assert_eq!(MAX_PARAMS, 4);
        assert_eq!(TaFunc::COUNT, 79);
        let inputs = [
            i64::MIN,
            i64::MIN + 1,
            -100_001,
            -1,
            0,
            1,
            2,
            99_999,
            100_000,
            100_001,
            i64::MAX - 1,
            i64::MAX,
            2.5f64.to_bits() as i64,
            (-0.0f64).to_bits() as i64,
            0x7ff8_1234_5678_9abcu64 as i64,
            f64::INFINITY.to_bits() as i64,
        ];
        let mut cases = 0;
        for func in TaFunc::ALL {
            for width in 0..=17 {
                for rotation in 0..inputs.len() {
                    let mut source = [0; 17];
                    for (i, value) in source.iter_mut().enumerate() {
                        *value = inputs[(i + rotation) % inputs.len()];
                    }
                    let actual = Decl::new(func, &source[..width]);
                    let previous = reference_decl_new(func, &source[..width]);
                    assert_eq!(
                        actual, previous,
                        "{func:?} width={width} rotation={rotation}"
                    );
                    match actual {
                        Some(decl) => {
                            assert!(width <= MAX_PARAMS);
                            assert_eq!(decl.tag, func.tag());
                            assert_eq!(decl.nparams as usize, width);
                            assert_eq!(&decl.to_le_bytes()[4..8], &[0; 4]);
                            assert_eq!(&decl.params[..width], &source[..width]);
                            assert!(decl.params[width..].iter().all(|value| *value == 0));
                            assert_eq!(decl.to_le_bytes(), previous.unwrap().to_le_bytes());
                            assert_eq!(Decl::from_le_bytes(&decl.to_le_bytes()), Some(decl));
                        }
                        None => assert!(width > MAX_PARAMS),
                    }
                    cases += 1;
                }
            }
        }
        assert_eq!(cases, 79 * 18 * 16);
    }

    #[test]
    fn scalar_constructor_keeps_all_slots_bits_and_padding() {
        for func in TaFunc::ALL {
            let values = [
                i64::MIN,
                (-0.0f64).to_bits() as i64,
                0x7ff8_9876_5432_1001u64 as i64,
                i64::MAX,
            ];
            let full = Decl::new(func, &values).unwrap();
            assert_eq!(full.params, values);
            assert_eq!(full.nparams, 4);
            assert_eq!(&full.to_le_bytes()[4..8], &[0; 4]);
            for width in 0..=4 {
                let partial = Decl::new(func, &values[..width]).unwrap();
                for (i, value) in partial.params.iter().enumerate() {
                    assert_eq!(*value, if i < width { values[i] } else { 0 });
                }
            }
        }
    }

    #[test]
    fn scalar_constructor_refuses_large_width_without_truncation() {
        let wide = [i64::MIN; 1_024];
        for func in TaFunc::ALL {
            for width in [5, 6, 7, 8, 16, 1_024] {
                assert_eq!(Decl::new(func, &wide[..width]), None);
                assert_eq!(reference_decl_new(func, &wide[..width]), None);
            }
            let empty = Decl::new(func, &[]).unwrap();
            assert_eq!(empty.params, [0; MAX_PARAMS]);
            assert_eq!(empty.nparams, 0);
            assert_eq!(empty.tag, func.tag());
            assert_eq!(&empty.to_le_bytes()[4..8], &[0; 4]);
        }
    }
}
