//! Reference argument binder used by the differential tests against the
//! kernel in [`crate::binder`], plus those tests.

use crate::*;

impl Analyzer<'_> {
    /// Bind a call's arguments to a parameter list by position then by name.
    ///
    /// `param_names` are the callee's parameters in declaration order. Returns
    /// a vector parallel to `param_names`: slot `i` holds the argument bound to
    /// parameter `i`, or `None` when that parameter was not supplied (the
    /// caller then applies the parameter's default / required rules exactly as
    /// it did for positional-only calls). Positional arguments fill slots
    /// left-to-right; a named argument (`name = value`) binds the parameter it
    /// names. Reports `E0228` for a name that does not match any parameter
    /// (with a did-you-mean) or a parameter bound twice, and returns `None`
    /// when any such error occurred so the caller can bail cleanly. The
    /// positional-after-named case is already a parse error, so `CallArg`s
    /// always arrive as `positional* named*`.
    fn reference_bind_call_args<'a>(
        &mut self,
        callee_name: &str,
        param_names: &[&str],
        args: &'a [ast::CallArg],
    ) -> Option<(Vec<Option<&'a ast::Expr>>, Vec<&'a ast::Expr>)> {
        let mut bound: Vec<Option<&'a ast::Expr>> = vec![None; param_names.len()];
        // Positional arguments beyond the fixed parameter count: variadic
        // functions (`math.max`, ...) legitimately repeat their last
        // parameter, so these are appended to the dense list rather than
        // dropped; a non-variadic callee's downstream arity check then reports
        // the surplus. Overflow only happens for all-positional calls (a named
        // argument cannot follow a positional-overflow one and still name a
        // valid distinct parameter), which is exactly the variadic case.
        let mut overflow: Vec<&'a ast::Expr> = Vec::new();
        let mut ok = true;
        let mut positional = 0usize;
        for arg in args {
            match &arg.name {
                None => {
                    if positional < bound.len() {
                        bound[positional] = Some(&arg.value);
                    } else {
                        overflow.push(&arg.value);
                    }
                    positional += 1;
                }
                Some(name) => match param_names.iter().position(|p| *p == name.name) {
                    Some(idx) => {
                        if bound[idx].is_some() {
                            self.error(
                                codes::E0228,
                                format!(
                                    "`{callee_name}` argument `{}` is bound more than once",
                                    name.name
                                ),
                                name.span,
                                "this parameter already has an argument",
                            );
                            ok = false;
                        } else {
                            bound[idx] = Some(&arg.value);
                        }
                    }
                    None => {
                        let mut diag = Diagnostic::error(
                            codes::E0228,
                            format!("`{callee_name}` has no parameter named `{}`", name.name),
                        )
                        .with_label(name.span, "unknown parameter name");
                        if let Some(suggestion) = self
                            .suggestions
                            .among(&name.name, || param_names.iter().copied().map(Some))
                        {
                            diag = diag.with_note(format!("did you mean `{suggestion}`?"));
                        }
                        self.diagnostics.push(diag);
                        ok = false;
                    }
                },
            }
        }
        if ok {
            Some((bound, overflow))
        } else {
            None
        }
    }

    fn reference_bind_positional_with_defaults(
        &mut self,
        callee_name: &str,
        param_names: &[&str],
        defaults: &[Option<&ast::Expr>],
        args: &[ast::CallArg],
        callee_span: Span,
    ) -> Option<Vec<ast::Expr>> {
        let (bound, overflow) = self.reference_bind_call_args(callee_name, param_names, args)?;
        // The last supplied slot marks the dense prefix length.
        let last_supplied = bound.iter().rposition(Option::is_some);
        let mut dense = Vec::new();
        let mut ok = true;
        if let Some(last) = last_supplied {
            dense.reserve(last + 1 + overflow.len());
            for (i, slot) in bound.iter().enumerate().take(last + 1) {
                match slot {
                    Some(expr) => dense.push((*expr).clone()),
                    None => match defaults.get(i).copied().flatten() {
                        // An interior optional parameter with a default: fill
                        // it so the dense positional list stays aligned.
                        Some(default) => dense.push(default.clone()),
                        None => {
                            self.error(
                                codes::E0204,
                                format!(
                                    "`{callee_name}` is missing a value for parameter `{}`",
                                    param_names[i]
                                ),
                                callee_span,
                                "a later argument was given by name, but this earlier parameter was not supplied",
                            );
                            ok = false;
                        }
                    },
                }
            }
        }
        // Append variadic overflow positional arguments (empty for
        // fixed-arity callees; a surplus there is caught by arity checking).
        dense.extend(overflow.into_iter().cloned());
        ok.then_some(dense)
    }
}

#[cfg(test)]
mod differential {
    use crate::binder::{self, ArityErr, ParamSource};
    use crate::{Analyzer, BuiltinRegistry, ImportedLibraries};
    use pinewasm_span::{Diagnostic, SourceId, Span};
    use pinewasm_syntax as ast;

    const PARAMS: [&str; 4] = ["alpha", "beta", "gamma", "delta"];

    /// One call argument in the sweep alphabet.
    #[derive(Clone, Copy)]
    enum Tok {
        Pos,
        Name(&'static str),
    }

    /// Positional, every parameter name, an unknown name with a close
    /// suggestion, and one with none.
    const ALPHABET: [Tok; 7] = [
        Tok::Pos,
        Tok::Name("alpha"),
        Tok::Name("beta"),
        Tok::Name("gamma"),
        Tok::Name("delta"),
        Tok::Name("betta"),
        Tok::Name("zzzzzzzz"),
    ];

    fn span(k: u32) -> Span {
        Span::new(SourceId::MAIN, 10 * k, 10 * k + 5)
    }

    fn make_args(toks: &[Tok]) -> Vec<ast::CallArg> {
        toks.iter()
            .enumerate()
            .map(|(k, tok)| {
                let k = k as u32 + 1;
                ast::CallArg {
                    name: match tok {
                        Tok::Pos => None,
                        Tok::Name(n) => Some(ast::Ident {
                            name: (*n).to_string(),
                            span: span(100 + k),
                        }),
                    },
                    value: ast::Expr::Int {
                        value: i64::from(k),
                        span: span(k),
                    },
                    span: span(200 + k),
                }
            })
            .collect()
    }

    fn defaults_exprs() -> Vec<ast::Expr> {
        (0..PARAMS.len())
            .map(|i| ast::Expr::Int {
                value: -1 - i as i64,
                span: span(300 + i as u32),
            })
            .collect()
    }

    type Outcome<T> = (T, Vec<Diagnostic>);

    type Borrowed<'a> = (Vec<Option<&'a ast::Expr>>, Vec<&'a ast::Expr>);

    /// Clone a `bind_call_args` result so two runs can be compared.
    fn owned((bound, overflow): Borrowed<'_>) -> (Vec<Option<ast::Expr>>, Vec<ast::Expr>) {
        let mut slots = Vec::new();
        for slot in bound {
            slots.push(slot.cloned());
        }
        (slots, overflow.into_iter().cloned().collect())
    }

    fn run<T>(f: impl FnOnce(&mut Analyzer<'_>) -> T) -> Outcome<T> {
        let mut analyzer = Analyzer::new(
            BuiltinRegistry::standard_ref(),
            ImportedLibraries::default(),
        );
        let out = f(&mut analyzer);
        (out, analyzer.diagnostics.finish())
    }

    /// Every argument list over [`ALPHABET`] up to `max_len` long.
    fn all_calls(max_len: usize) -> Vec<Vec<Tok>> {
        let mut out: Vec<Vec<Tok>> = vec![vec![]];
        let mut frontier = out.clone();
        for _ in 0..max_len {
            let mut next = Vec::new();
            for call in &frontier {
                for tok in ALPHABET {
                    let mut c = call.clone();
                    c.push(tok);
                    next.push(c);
                }
            }
            out.extend(next.iter().cloned());
            frontier = next;
        }
        out
    }

    /// Deterministic sweep: every parameter count 0..=4, every argument list
    /// up to 4 long (including positional-after-named, which the parser
    /// rejects but the binder must still treat identically), every default
    /// pattern, and the empty default table. The reference and the kernel
    /// must agree on the result and on every diagnostic.
    #[test]
    fn sweep_matches_baseline() {
        let calls = all_calls(4);
        let default_exprs = defaults_exprs();
        let mut compared = 0usize;
        for n in 0..=PARAMS.len() {
            let names = &PARAMS[..n];
            // `None` = the empty `defaults` slice `bind_positional` passes.
            let mut patterns: Vec<Option<u32>> = vec![None];
            patterns.extend((0..(1u32 << n)).map(Some));
            for toks in &calls {
                let args = make_args(toks);

                let new = run(|a| a.bind_call_args("f", names, &args).map(owned));
                let old = run(|a| a.reference_bind_call_args("f", names, &args).map(owned));
                assert_eq!(new, old, "bind_call_args n={n} args={args:?}");

                for pattern in &patterns {
                    let defaults: Vec<Option<&ast::Expr>> = match pattern {
                        None => Vec::new(),
                        Some(bits) => (0..n)
                            .map(|i| (bits >> i & 1 == 1).then(|| &default_exprs[i]))
                            .collect(),
                    };
                    let span = span(999);
                    let new = run(|a| {
                        a.bind_positional_with_defaults("f", names, &defaults, &args, span)
                    });
                    let old = run(|a| {
                        a.reference_bind_positional_with_defaults(
                            "f", names, &defaults, &args, span,
                        )
                    });
                    assert_eq!(
                        new, old,
                        "bind_positional_with_defaults n={n} defaults={pattern:?} args={args:?}"
                    );
                    compared += 1;
                }
            }
        }
        assert!(compared > 100_000, "sweep too small: {compared}");
    }

    /// Reference arity rule in iterator form: the count check, then per
    /// parameter `args.get(i)` or the default. A missing default is reported
    /// here by its parameter index, and the inliner reports it as E0224.
    fn reference_def_plan(
        n_args: usize,
        has_default: &[bool],
    ) -> Result<Vec<ParamSource>, ArityErr> {
        let required = has_default.iter().filter(|d| !**d).count();
        let total = has_default.len();
        if n_args < required || n_args > total {
            return Err(ArityErr::Count { required, total });
        }
        let mut plan = Vec::new();
        for (i, has) in has_default.iter().enumerate() {
            match (0..n_args).nth(i) {
                Some(j) => plan.push(ParamSource::Arg(j)),
                None if *has => plan.push(ParamSource::Default),
                None => return Err(ArityErr::OmittedWithoutDefault { param: i }),
            }
        }
        Ok(plan)
    }

    #[test]
    fn def_plan_sweep_matches_baseline() {
        for total in 0..=6usize {
            for bits in 0..(1u32 << total) {
                let has_default: Vec<bool> = (0..total).map(|i| bits >> i & 1 == 1).collect();
                for n_args in 0..=total + 2 {
                    let new = binder::plan_def_args(n_args, &has_default);
                    assert_eq!(
                        new,
                        reference_def_plan(n_args, &has_default),
                        "n_args={n_args} has_default={has_default:?}"
                    );
                    // Wherever the plan exists, each entry is `param_source`.
                    if let Ok(plan) = new {
                        for (i, source) in plan.iter().enumerate() {
                            assert_eq!(*source, binder::param_source(n_args, i));
                        }
                    }
                }
            }
        }
    }
}
