//! Pinning tests for the aot-analysis tables (`native_coll_op` as one flat
//! match; the `Elem` lattice, `pure_plan` and `NativeCollOp::linear_cost`
//! pinned exhaustively).

use super::{native_coll_op, pure_plan, Elem, MapValue, NativeCollOp, VTy};
use pinewasm_hir::{CollectionFunc, PureFunc};

/// Every unit variant name (as a JSON string) of `pub enum <name>` in `pinewasm_hir`'s builtins
/// source; callers decode them through serde so a renamed or added variant
/// cannot be silently skipped.
fn variant_names(name: &str) -> Vec<String> {
    let src = include_str!("../../../pinewasm_hir/src/builtins.rs");
    let head = format!("pub enum {name} {{");
    let start = src.find(&head).expect("enum present") + head.len();
    let body = &src[start..start + src[start..].find("\n}\n").expect("enum end")];
    let out: Vec<String> = body
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with(|c: char| c.is_ascii_uppercase()))
        // Data-carrying variants (`StrategyClosedTrade(..)`) are not unit
        // variants; `pure_plan` sends them to its `_ => None` arm.
        .filter(|l| !l.contains(['(', '{']))
        .map(|l| format!("\"{}\"", l.split([',', ' ']).next().unwrap_or(l)))
        .collect();
    assert!(out.len() > 20, "{name}: parsed only {} variants", out.len());
    out
}

fn all_collection_funcs() -> Vec<CollectionFunc> {
    variant_names("CollectionFunc")
        .iter()
        .map(|v| serde_json::from_str(v).expect("variant decodes"))
        .collect()
}

fn all_pure_funcs() -> Vec<PureFunc> {
    variant_names("PureFunc")
        .iter()
        .map(|v| serde_json::from_str(v).expect("variant decodes"))
        .collect()
}

/// Baseline table of the core native collection shapes.
fn native_coll_op_old(func: CollectionFunc, argc: u8, default_tail: bool) -> Option<NativeCollOp> {
    use CollectionFunc as F;
    if matches!(func, F::ArrayMin | F::ArrayMax) && argc == 2 && default_tail {
        return Some(if func == F::ArrayMin {
            NativeCollOp::Min
        } else {
            NativeCollOp::Max
        });
    }
    Some(match (func, argc) {
        (F::ArrayNewFloat | F::ArrayNewInt | F::ArrayNewBool, 2) => NativeCollOp::NewArray,
        (F::ArrayFrom, n) if n >= 1 => NativeCollOp::FromArgs,
        (F::ArrayPush, 2) => NativeCollOp::Push,
        (F::ArrayGet, 2) => NativeCollOp::Get,
        (F::ArraySet, 3) => NativeCollOp::Set,
        (F::ArraySize, 1) => NativeCollOp::Size,
        (F::ArrayShift, 1) => NativeCollOp::Shift,
        (F::ArrayPop, 1) => NativeCollOp::PopLast,
        (F::ArrayFirst, 1) => NativeCollOp::First,
        (F::ArrayLast, 1) => NativeCollOp::Last,
        (F::ArrayClear, 1) => NativeCollOp::Clear,
        (F::ArraySum, 1) => NativeCollOp::Sum,
        (F::ArrayAvg, 1) => NativeCollOp::Avg,
        (F::ArrayMin, 1) => NativeCollOp::Min,
        (F::ArrayMax, 1) => NativeCollOp::Max,
        _ => return None,
    })
}

/// The color-lane and variance shapes (`array.new_color`, `array.stdev`,
/// `array.variance`) that extend the baseline table; every other shape must
/// match the baseline.
fn native_coll_op_added(func: CollectionFunc, argc: u8) -> Option<NativeCollOp> {
    use CollectionFunc as F;
    match (func, argc) {
        (F::ArrayNewColor, 2) => Some(NativeCollOp::NewArray),
        (F::ArrayStdev, 2) => Some(NativeCollOp::Stdev),
        (F::ArrayVariance, 2) => Some(NativeCollOp::Variance),
        _ => None,
    }
}

#[test]
fn native_coll_op_matches_pre_refactor_exhaustively() {
    let funcs = all_collection_funcs();
    let mut some = 0usize;
    for &func in &funcs {
        for argc in 0..=u8::MAX {
            for default_tail in [false, true] {
                let new = native_coll_op(func, argc, default_tail);
                let old = native_coll_op_old(func, argc, default_tail);
                let added = native_coll_op_added(func, argc);
                assert!(old.is_none() || added.is_none(), "{func:?} argc={argc}");
                assert_eq!(
                    new,
                    added.or(old),
                    "{func:?} argc={argc} default_tail={default_tail}"
                );
                some += usize::from(new.is_some());
            }
        }
    }
    // array.from admits every argc >= 1 (255 x 2); the rest are fixed shapes.
    assert!(some > 510, "suspiciously few native shapes: {some}");
}

#[test]
fn native_linear_cost_is_collection_func_parity() {
    for func in all_collection_funcs() {
        for argc in 0..=u8::MAX {
            for default_tail in [false, true] {
                if let Some(op) = native_coll_op(func, argc, default_tail) {
                    assert_eq!(
                        op.linear_cost(),
                        func.is_linear_cost(),
                        "{func:?} -> {op:?}"
                    );
                }
            }
        }
    }
}

fn elem_carrier() -> Vec<Elem> {
    let mut v = vec![Elem::Unset, Elem::AnyLane, Elem::Opaque, Elem::Mixed];
    v.extend(
        [
            VTy::F64,
            VTy::I64,
            VTy::Bool3,
            VTy::Color,
            VTy::Str,
            VTy::Ref,
        ]
        .map(Elem::Known),
    );
    v.extend([0, 1, 7, u32::MAX].map(Elem::Obj));
    v.extend([
        Elem::Map(VTy::I64, MapValue::Lane(VTy::F64)),
        Elem::Map(VTy::Str, MapValue::Lane(VTy::F64)),
        Elem::Map(VTy::Str, MapValue::Lane(VTy::Str)),
        Elem::Map(VTy::Str, MapValue::Obj(0)),
        Elem::Map(VTy::Str, MapValue::Obj(1)),
    ]);
    v
}

#[test]
fn elem_join_is_a_join_semilattice_with_unset_bottom_mixed_top() {
    let c = elem_carrier();
    for &a in &c {
        assert_eq!(a.join(a), a);
        assert_eq!(a.join(Elem::Unset), a);
        assert_eq!(Elem::Unset.join(a), a);
        assert_eq!(a.join(Elem::Mixed), Elem::Mixed);
        for &b in &c {
            assert_eq!(a.join(b), b.join(a), "{a:?} {b:?}");
            // Distinct non-bottom cells always go to top.
            if a != b && a != Elem::Unset && b != Elem::Unset {
                assert_eq!(a.join(b), Elem::Mixed, "{a:?} {b:?}");
            }
            assert_eq!(
                a.join(b).maybe_native(),
                a.maybe_native() || b.maybe_native(),
                "{a:?} {b:?}"
            );
            for &d in &c {
                assert_eq!(a.join(b).join(d), a.join(b.join(d)), "{a:?} {b:?} {d:?}");
            }
        }
    }
    let native: Vec<Elem> = c.into_iter().filter(|e| !e.maybe_native()).collect();
    assert_eq!(native, vec![Elem::Unset, Elem::Opaque]);
}

#[test]
fn pure_plan_depends_on_argc_only_for_math_round() {
    for func in all_pure_funcs() {
        let base = pure_plan(func, 1);
        for argc in 0..=u8::MAX {
            let got = pure_plan(func, argc);
            if func == PureFunc::MathRound && argc == 2 {
                assert_eq!(got, Some(super::PurePlan::Round2));
            } else {
                assert_eq!(got, base, "{func:?} argc={argc}");
            }
        }
    }
}
