//! Pure argument-binding kernel.
//!
//! These functions decide *which* argument (or default) feeds each callee
//! parameter, over plain indices only: the analyzer first resolves every named
//! argument to a parameter index ([`ArgShape`]), calls the kernel, and then
//! maps the result back to AST expressions and diagnostics. Keeping names,
//! spans, `format!` and AST cloning out of this module keeps it inside the
//! subset of Rust that Aeneas can translate to Lean, so its properties can be
//! proved about this exact code.
//!
//! The code deliberately sticks to `while` loops over indices, `Vec::new` /
//! `push` / indexing and `Option` / `Result`: no iterator adapters, closures,
//! early `return` out of loops, or unchecked arithmetic that could overflow for
//! any slice that fits in memory.
//!
//! The `#[cfg_attr(charon, charon::rename(..))]` attributes on the seven enums
//! only rename them in the Charon/Aeneas translation (the `charon` and
//! `charon_glue` cfgs are set only by the extraction command, never by cargo).
//! Aeneas names the `Discriminant` instance of an enum after its last path
//! segment in the Lean root namespace, so without the rename a translation
//! could not be loaded next to another translation of the same enums in the
//! proof library. `--cfg=charon` gives the `Fix*` names; adding
//! `--cfg=charon_glue` (the translation rooted at [`arg_shapes`]) names
//! [`ArgShape`] `GlueArgShape` instead. They do not affect rustc builds.

/// How one call-site argument was written, with its name already resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(all(charon, not(charon_glue)), charon::rename("FixArgShape"))]
#[cfg_attr(all(charon, charon_glue), charon::rename("GlueArgShape"))]
pub(crate) enum ArgShape {
    /// `f(value)`.
    Positional,
    /// `f(name = value)`: `Some(i)` when `name` is parameter `i`, `None` when
    /// it names no parameter.
    Named(Option<usize>),
}

/// A binding error for one argument, identified by its index in the call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(charon, charon::rename("FixSlotErr"))]
pub(crate) enum SlotErr {
    /// A named argument whose parameter already has an argument.
    Duplicate(usize),
    /// A named argument that names no parameter.
    UnknownName(usize),
}

/// The result of binding arguments to parameter slots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Slots {
    /// `slots[i]` is the index of the argument bound to parameter `i`.
    pub(crate) slots: Vec<Option<usize>>,
    /// Positional arguments past the last parameter (variadic callees), in
    /// call order.
    pub(crate) overflow: Vec<usize>,
}

/// Bind arguments to parameter slots by position, then by name.
///
/// Positional arguments fill slots left to right (surplus ones go to
/// `overflow`); a named argument fills the slot it names. The first argument
/// bound to a slot wins; every later named argument for that slot is a
/// [`SlotErr::Duplicate`]. Errors are returned in call order, and binding
/// continues past an error so every error is reported.
pub(crate) fn bind_slots(n_params: usize, args: &[ArgShape]) -> Result<Slots, Vec<SlotErr>> {
    let mut slots: Vec<Option<usize>> = vec![None; n_params];
    let mut overflow: Vec<usize> = Vec::new();
    let mut errors: Vec<SlotErr> = Vec::new();
    let mut positional: usize = 0;
    let mut j: usize = 0;
    while j < args.len() {
        let shape: ArgShape = args[j];
        match shape {
            ArgShape::Positional => {
                if positional < n_params {
                    slots[positional] = Some(j);
                } else {
                    overflow.push(j);
                }
                // `positional <= j < args.len()`, so this cannot overflow.
                positional += 1;
            }
            ArgShape::Named(Some(idx)) => {
                if idx >= n_params {
                    // Callers only resolve to valid indices; treat anything
                    // else as unknown rather than indexing out of bounds.
                    errors.push(SlotErr::UnknownName(j));
                } else if slots[idx].is_some() {
                    errors.push(SlotErr::Duplicate(j));
                } else {
                    slots[idx] = Some(j);
                }
            }
            ArgShape::Named(None) => errors.push(SlotErr::UnknownName(j)),
        }
        j += 1;
    }
    if errors.is_empty() {
        Ok(Slots { slots, overflow })
    } else {
        Err(errors)
    }
}

/// Where one entry of a dense positional argument list comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(charon, charon::rename("FixChoice"))]
pub(crate) enum Choice {
    /// The call's argument at this index.
    Arg(usize),
    /// The default of the parameter at this index.
    Default(usize),
}

/// Why [`bind`] produced no dense argument list.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(charon, charon::rename("FixBindErr"))]
pub(crate) enum BindErr {
    /// [`bind_slots`] failed; errors in call order.
    Slots(Vec<SlotErr>),
    /// These parameters (ascending) were left unbound before a later bound
    /// one and have no default to fill the gap.
    Gaps(Vec<usize>),
}

/// Bind arguments, then flatten to a dense positional list.
///
/// The dense list covers parameters up to and including the last bound one:
/// a bound parameter contributes its argument, an unbound one with
/// `has_default[i]` contributes its default, and an unbound one without a
/// default is a gap (an error). Parameters past `has_default.len()` count as
/// having no default. Overflow arguments are appended at the end.
pub(crate) fn bind(
    n_params: usize,
    args: &[ArgShape],
    has_default: &[bool],
) -> Result<Vec<Choice>, BindErr> {
    let bound = match bind_slots(n_params, args) {
        Ok(bound) => bound,
        Err(errors) => return Err(BindErr::Slots(errors)),
    };
    // One past the last bound slot: the dense prefix length.
    let mut end: usize = 0;
    let mut i: usize = 0;
    while i < bound.slots.len() {
        if bound.slots[i].is_some() {
            end = i + 1;
        }
        i += 1;
    }
    let mut dense: Vec<Choice> = Vec::new();
    let mut gaps: Vec<usize> = Vec::new();
    let mut i: usize = 0;
    while i < end {
        // `end` is derived from this vector's length in the loop above.
        let slot = bound.slots[i];
        match slot {
            Some(j) => dense.push(Choice::Arg(j)),
            None => {
                if i < has_default.len() && has_default[i] {
                    dense.push(Choice::Default(i));
                } else {
                    gaps.push(i);
                }
            }
        }
        i += 1;
    }
    if !gaps.is_empty() {
        return Err(BindErr::Gaps(gaps));
    }
    let mut k: usize = 0;
    while k < bound.overflow.len() {
        dense.push(Choice::Arg(bound.overflow[k]));
        k += 1;
    }
    Ok(dense)
}

/// Where a user-function parameter's value comes from when the call is
/// inlined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(charon, charon::rename("FixParamSource"))]
pub(crate) enum ParamSource {
    /// The dense argument at this index.
    Arg(usize),
    /// The parameter's own default.
    Default,
}

/// Why [`plan_def_args`] rejected a user-function call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(charon, charon::rename("FixArityErr"))]
pub(crate) enum ArityErr {
    /// The argument count is outside `required..=total` (reported as E0224).
    Count { required: usize, total: usize },
    /// Parameter `param` is omitted (it lies past the last argument) and has
    /// no default. The count check cannot catch this when a parameter without
    /// a default follows one with a default: `f(a = 1.0, b)` called as
    /// `f(5.0)`. The inliner reports it as E0224 naming the parameter.
    OmittedWithoutDefault { param: usize },
}

/// Which parameter of a `n_args`-argument call is fed by index `i`: argument
/// `i` if one was passed, otherwise the parameter's default.
pub(crate) fn param_source(n_args: usize, i: usize) -> ParamSource {
    if i < n_args {
        ParamSource::Arg(i)
    } else {
        ParamSource::Default
    }
}

/// Plan how an inlined user-function call feeds its parameters from `n_args`
/// dense arguments, where `has_default[i]` says whether parameter `i` has a
/// default. `required` counts the parameters without a default.
///
/// Returns [`ArityErr::Count`] first when the count is out of range, then
/// [`ArityErr::OmittedWithoutDefault`] for the first omitted parameter that
/// has no default; otherwise one [`ParamSource`] per parameter.
pub(crate) fn plan_def_args(
    n_args: usize,
    has_default: &[bool],
) -> Result<Vec<ParamSource>, ArityErr> {
    let total = has_default.len();
    let mut required: usize = 0;
    let mut i: usize = 0;
    while i < total {
        if !has_default[i] {
            // `required <= i < total`, so this cannot overflow.
            required += 1;
        }
        i += 1;
    }
    if n_args < required || n_args > total {
        return Err(ArityErr::Count { required, total });
    }
    // Omitted parameters are exactly those at `i >= n_args`; the first one
    // without a default is an error.
    let mut i: usize = n_args;
    while i < total && has_default[i] {
        i += 1;
    }
    if i < total {
        return Err(ArityErr::OmittedWithoutDefault { param: i });
    }
    let mut plan: Vec<ParamSource> = Vec::new();
    let mut i: usize = 0;
    while i < total {
        plan.push(param_source(n_args, i));
        i += 1;
    }
    Ok(plan)
}

/// One call-site argument as name resolution sees it. The only
/// implementation is `ast::CallArg`'s against `&str` parameter names; keeping
/// [`arg_shapes`] generic over it keeps strings out of the extracted code.
pub(crate) trait ArgName<P> {
    /// Whether the argument is written `name = value`.
    fn is_named(&self) -> bool;
    /// Whether the argument's name is the name of `param`.
    fn names(&self, param: &P) -> bool;
}

/// Index of the first parameter `arg` names (what `Iterator::position`
/// returns).
fn position_of<P, A: ArgName<P>>(params: &[P], arg: &A) -> Option<usize> {
    let mut found = None;
    let mut searching = true;
    let mut i = 0;
    while searching && i < params.len() {
        if arg.names(&params[i]) {
            found = Some(i);
            searching = false;
        }
        i += 1;
    }
    found
}

/// Resolve each call argument's name to a parameter index for [`bind`].
pub(crate) fn arg_shapes<P, A: ArgName<P>>(params: &[P], args: &[A]) -> Vec<ArgShape> {
    let mut shapes = Vec::with_capacity(args.len());
    let mut k = 0;
    while k < args.len() {
        let shape = if args[k].is_named() {
            ArgShape::Named(position_of(params, &args[k]))
        } else {
            ArgShape::Positional
        };
        shapes.push(shape);
        k += 1;
    }
    shapes
}

/// How [`expand_protocol`] ends.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(charon, charon::rename("FixExpansion"))]
pub(crate) enum Expansion {
    /// A diagnostic was reported (or the expansion budget was already blown
    /// by an earlier call): the call becomes an error expression.
    Error,
    /// Inline the call: the dense arguments ([`bind`]) and the parameter plan
    /// ([`plan_def_args`]).
    Inline(Vec<Choice>, Vec<ParamSource>),
}

/// What [`expand_protocol`] needs from the analyzer: the two expansion
/// counters, the call-stack queries and the diagnostics. The only
/// implementation is `calls.rs`'s `ExpandCtx`.
pub(crate) trait ExpandHost {
    /// The "expansion budget already blown" flag.
    fn aborted(&mut self) -> &mut bool;
    /// The number of function-body statements expanded so far.
    fn nodes(&mut self) -> &mut usize;
    /// Whether the callee is already being inlined further up the stack.
    fn is_recursive(&mut self) -> bool;
    /// The current inline nesting depth.
    fn depth(&mut self) -> usize;
    /// Report a [`bind`] error (`E0228` / `E0204`).
    fn report_bind(&mut self, err: BindErr);
    /// Report direct or mutual recursion (`E0224`) and analyze the arguments.
    fn report_recursive(&mut self, dense: &[Choice]);
    /// Report the nesting-depth limit (`E0224`) and analyze the arguments.
    fn report_depth(&mut self, dense: &[Choice]);
    /// Report an arity error (`E0224`): an argument count outside
    /// `required..=total`, or an omitted parameter without a default; then
    /// analyze the arguments.
    fn report_arity(&mut self, err: ArityErr, dense: &[Choice]);
    /// Report the expansion-budget limit (`E0224`).
    fn report_budget(&mut self);
}

/// The front of `expand_def_scoped` (inlining a user-function call): the
/// abort short-circuit, argument binding, the recursion and nesting-depth
/// guards, the arity plan and the expansion budget, in that order. The
/// per-parameter argument evaluation and the body analysis stay in
/// `expand_def_scoped`. Generic over [`ExpandHost`] so Charon/Aeneas can
/// extract the order of these steps together with the kernel they call.
pub(crate) fn expand_protocol<H: ExpandHost>(
    host: &mut H,
    n_params: usize,
    shapes: &[ArgShape],
    has_default: &[bool],
    body_len: usize,
) -> Expansion {
    if *host.aborted() {
        return Expansion::Error;
    }
    let dense = match bind(n_params, shapes, has_default) {
        Ok(dense) => dense,
        Err(err) => {
            host.report_bind(err);
            return Expansion::Error;
        }
    };
    if host.is_recursive() {
        host.report_recursive(&dense);
        return Expansion::Error;
    }
    if host.depth() >= crate::MAX_INLINE_DEPTH {
        *host.aborted() = true;
        host.report_depth(&dense);
        return Expansion::Error;
    }
    let plan = match plan_def_args(dense.len(), has_default) {
        Ok(plan) => plan,
        Err(err) => {
            host.report_arity(err, &dense);
            return Expansion::Error;
        }
    };
    let nodes = host.nodes();
    *nodes = nodes.saturating_add(body_len);
    if *nodes > crate::MAX_EXPANSION_NODES {
        *host.aborted() = true;
        host.report_budget();
        return Expansion::Error;
    }
    Expansion::Inline(dense, plan)
}

#[cfg(test)]
mod tests {
    /// `arg_shapes` agrees with the `Iterator::position` form it replaced.
    #[test]
    fn arg_shapes_matches_iterator_position() {
        struct A(Option<&'static str>);
        impl ArgName<&'static str> for A {
            fn is_named(&self) -> bool {
                self.0.is_some()
            }
            fn names(&self, p: &&'static str) -> bool {
                self.0 == Some(*p)
            }
        }
        let pools: [&[&str]; 4] = [&[], &["a"], &["a", "b", "a"], &["x", "y", "z", "w"]];
        let names = [None, Some("a"), Some("b"), Some("z"), Some("q")];
        for params in pools {
            for n1 in names {
                for n2 in names {
                    for n3 in names {
                        let args = [A(n1), A(n2), A(n3)];
                        let got = arg_shapes(params, &args);
                        let want: Vec<ArgShape> = args
                            .iter()
                            .map(|a| match a.0 {
                                None => ArgShape::Positional,
                                Some(n) => ArgShape::Named(params.iter().position(|p| *p == n)),
                            })
                            .collect();
                        assert_eq!(got, want);
                    }
                }
            }
        }
    }

    use super::*;

    #[test]
    fn positional_then_named() {
        let args = [
            ArgShape::Positional,
            ArgShape::Named(Some(2)),
            ArgShape::Named(Some(1)),
        ];
        assert_eq!(
            bind_slots(3, &args),
            Ok(Slots {
                slots: vec![Some(0), Some(2), Some(1)],
                overflow: vec![],
            })
        );
    }

    #[test]
    fn errors_in_call_order_first_binding_wins() {
        let args = [
            ArgShape::Positional,
            ArgShape::Named(None),
            ArgShape::Named(Some(0)),
            ArgShape::Named(Some(5)),
        ];
        assert_eq!(
            bind_slots(2, &args),
            Err(vec![
                SlotErr::UnknownName(1),
                SlotErr::Duplicate(2),
                SlotErr::UnknownName(3),
            ])
        );
    }

    #[test]
    fn dense_fills_defaults_and_reports_gaps() {
        let args = [ArgShape::Positional, ArgShape::Named(Some(3))];
        assert_eq!(
            bind(4, &args, &[false, true, false, false]),
            Err(BindErr::Gaps(vec![2]))
        );
        assert_eq!(
            bind(4, &args, &[false, true, true, false]),
            Ok(vec![
                Choice::Arg(0),
                Choice::Default(1),
                Choice::Default(2),
                Choice::Arg(1),
            ])
        );
        // An empty default table means no parameter has a default.
        assert_eq!(bind(4, &args, &[]), Err(BindErr::Gaps(vec![1, 2])));
    }

    #[test]
    fn overflow_is_appended() {
        let args = [ArgShape::Positional; 4];
        assert_eq!(
            bind(2, &args, &[]),
            Ok(vec![
                Choice::Arg(0),
                Choice::Arg(1),
                Choice::Arg(2),
                Choice::Arg(3),
            ])
        );
    }

    #[test]
    fn def_plan_cases() {
        assert_eq!(
            plan_def_args(1, &[false, true]),
            Ok(vec![ParamSource::Arg(0), ParamSource::Default])
        );
        assert_eq!(
            plan_def_args(0, &[false, true]),
            Err(ArityErr::Count {
                required: 1,
                total: 2
            })
        );
        assert_eq!(
            plan_def_args(3, &[false, true]),
            Err(ArityErr::Count {
                required: 1,
                total: 2
            })
        );
        // `f(a = 1.0, b)` called as `f(5.0)`: the count check passes.
        assert_eq!(
            plan_def_args(1, &[true, false]),
            Err(ArityErr::OmittedWithoutDefault { param: 1 })
        );
    }

    /// A host that records the protocol's calls.
    struct Trace {
        aborted: bool,
        nodes: usize,
        recursive: bool,
        depth: usize,
        log: Vec<String>,
    }

    impl ExpandHost for Trace {
        fn aborted(&mut self) -> &mut bool {
            &mut self.aborted
        }
        fn nodes(&mut self) -> &mut usize {
            &mut self.nodes
        }
        fn is_recursive(&mut self) -> bool {
            self.recursive
        }
        fn depth(&mut self) -> usize {
            self.depth
        }
        fn report_bind(&mut self, err: BindErr) {
            self.log.push(format!("bind {err:?}"));
        }
        fn report_recursive(&mut self, dense: &[Choice]) {
            self.log.push(format!("recursive {}", dense.len()));
        }
        fn report_depth(&mut self, dense: &[Choice]) {
            self.log
                .push(format!("depth {} aborted={}", dense.len(), self.aborted));
        }
        fn report_arity(&mut self, err: ArityErr, dense: &[Choice]) {
            self.log.push(format!("arity {err:?} {}", dense.len()));
        }
        fn report_budget(&mut self) {
            self.log.push(format!("budget aborted={}", self.aborted));
        }
    }

    fn trace(aborted: bool, nodes: usize, recursive: bool, depth: usize) -> Trace {
        Trace {
            aborted,
            nodes,
            recursive,
            depth,
            log: Vec::new(),
        }
    }

    #[test]
    fn expand_protocol_guard_order() {
        let pos = [ArgShape::Positional];
        // Aborted: nothing else runs, not even binding errors.
        let mut h = trace(true, 0, true, 0);
        let bad = [ArgShape::Named(None)];
        assert_eq!(
            expand_protocol(&mut h, 1, &bad, &[false], 3),
            Expansion::Error
        );
        assert!(h.log.is_empty());
        // A binding error wins over recursion.
        let mut h = trace(false, 0, true, 0);
        assert_eq!(
            expand_protocol(&mut h, 1, &bad, &[false], 3),
            Expansion::Error
        );
        assert_eq!(h.log, ["bind Slots([UnknownName(0)])"]);
        // Recursion wins over depth.
        let mut h = trace(false, 0, true, crate::MAX_INLINE_DEPTH);
        assert_eq!(
            expand_protocol(&mut h, 1, &pos, &[false], 3),
            Expansion::Error
        );
        assert_eq!(h.log, ["recursive 1"]);
        assert!(!h.aborted);
        // Depth sets the abort flag before reporting, and wins over arity.
        let mut h = trace(false, 0, false, crate::MAX_INLINE_DEPTH);
        assert_eq!(
            expand_protocol(&mut h, 2, &pos, &[false, false], 3),
            Expansion::Error
        );
        assert_eq!(h.log, ["depth 1 aborted=true"]);
        // Arity wins over the budget and leaves the node count alone.
        let mut h = trace(false, crate::MAX_EXPANSION_NODES, false, 0);
        assert_eq!(
            expand_protocol(&mut h, 2, &pos, &[false, false], 3),
            Expansion::Error
        );
        assert_eq!(h.log, ["arity Count { required: 2, total: 2 } 1"]);
        assert_eq!(h.nodes, crate::MAX_EXPANSION_NODES);
        // The budget saturates, sets the abort flag, then reports.
        let mut h = trace(false, usize::MAX, false, 0);
        assert_eq!(
            expand_protocol(&mut h, 1, &pos, &[false], 3),
            Expansion::Error
        );
        assert_eq!(h.log, ["budget aborted=true"]);
        assert_eq!(h.nodes, usize::MAX);
        // An omitted parameter without a default is an arity error too, before
        // the budget and leaving the node count alone.
        let mut h = trace(false, crate::MAX_EXPANSION_NODES, false, 0);
        assert_eq!(
            expand_protocol(&mut h, 2, &pos, &[true, false], 3),
            Expansion::Error
        );
        assert_eq!(h.log, ["arity OmittedWithoutDefault { param: 1 } 1"]);
        assert_eq!(h.nodes, crate::MAX_EXPANSION_NODES);
        // Otherwise: inline, with the plan.
        let mut h = trace(false, 5, false, 0);
        assert_eq!(
            expand_protocol(&mut h, 2, &pos, &[false, true], 3),
            Expansion::Inline(
                vec![Choice::Arg(0)],
                vec![ParamSource::Arg(0), ParamSource::Default]
            )
        );
        assert!(h.log.is_empty());
        assert_eq!(h.nodes, 8);
    }
}
