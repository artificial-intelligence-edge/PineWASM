//! Proof-extraction roots for the concrete Pine call-argument binder adapter.
//! This module is only compiled by Charon (`cfg(charon)`).
#![allow(dead_code)]

use crate::{ast, binder};

pub(crate) fn call_arg_is_named(arg: &ast::CallArg) -> bool {
    <ast::CallArg as binder::ArgName<&str>>::is_named(arg)
}

pub(crate) fn call_arg_names(arg: &ast::CallArg, parameter: &&str) -> bool {
    <ast::CallArg as binder::ArgName<&str>>::names(arg, parameter)
}
