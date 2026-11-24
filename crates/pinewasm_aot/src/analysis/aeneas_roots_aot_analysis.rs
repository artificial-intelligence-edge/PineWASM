//! Free-function extraction roots for Charon/Aeneas (`AotAnalysis` extraction),
//! which cannot start from inherent methods at the pinned Charon. Each root
//! only delegates; production builds never enable the `aeneas` feature.
#![allow(dead_code)]

use super::{Elem, NativeCollOp};

pub(crate) fn elem_join(a: Elem, b: Elem) -> Elem {
    a.join(b)
}

pub(crate) fn elem_maybe_native(e: Elem) -> bool {
    e.maybe_native()
}

pub(crate) fn coll_op_linear_cost(op: NativeCollOp) -> bool {
    op.linear_cost()
}

// Cross-crate table roots: the `pinewasm_hir` metering and CSIL arity tables
// that the native-lowering tables above must agree with.

pub(crate) fn coll_func_linear_cost(func: pinewasm_csil::CollectionFunc) -> bool {
    func.is_linear_cost()
}

pub(crate) fn coll_func_argc_bounds(func: pinewasm_csil::CollectionFunc) -> (u8, u8) {
    func.argc_bounds()
}

pub(crate) fn pure_func_argc_bounds(func: pinewasm_csil::PureFunc) -> (u8, u8) {
    func.argc_bounds()
}
