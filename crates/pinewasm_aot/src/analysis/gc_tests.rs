//! What the collector plan makes of each kind of cell ([`super::gc_cell`]).
//!
//! An array of references or a map is not native today: such a program keeps
//! its whole heap on the host, so no program reaches `gc_plan` with a
//! reference lane, and the differential tests cannot exercise its refusal.
//! These pin it directly, for the day a reference lane becomes native.

use super::{gc_cell, Elem, GcCell, GcRef, VTy};
use std::collections::BTreeSet;

#[test]
fn a_scalar_array_is_a_leaf_and_a_reference_lane_refuses_the_plan() {
    let none = BTreeSet::new();
    for lane in [VTy::F64, VTy::I64, VTy::Bool3, VTy::Color] {
        assert_eq!(gc_cell(Elem::Known(lane), &none), GcCell::Ref(GcRef::Array));
    }
    // `array.from`: native only with a scalar lane.
    assert_eq!(gc_cell(Elem::AnyLane, &none), GcCell::Ref(GcRef::Array));
    // The collector counts an array's elements without reading them: an
    // array of handles would keep what they refer to alive untraced.
    for lane in [VTy::Ref, VTy::Str] {
        assert_eq!(
            gc_cell(Elem::Known(lane), &none),
            GcCell::Untraceable,
            "{lane:?}"
        );
    }
}

#[test]
fn objects_are_traced_natively_and_drawings_through_the_host() {
    let drawings = BTreeSet::from([4]);
    assert_eq!(
        gc_cell(Elem::Obj(3), &drawings),
        GcCell::Ref(GcRef::Object(3))
    );
    assert_eq!(gc_cell(Elem::Opaque, &drawings), GcCell::Ref(GcRef::Hosted));
    // A drawing-backing type is never a native object.
    assert_eq!(gc_cell(Elem::Obj(4), &drawings), GcCell::Untraceable);
    // A cell that can hold references of more than one kind is untraceable;
    // one that only ever holds `na` is no reference at all.
    assert_eq!(gc_cell(Elem::Mixed, &drawings), GcCell::Untraceable);
    assert_eq!(gc_cell(Elem::Unset, &drawings), GcCell::Na);
}
