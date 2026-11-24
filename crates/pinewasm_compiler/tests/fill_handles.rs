//! End-to-end tests for how `fill(...)` resolves its plot-id endpoints.
//!
//! A plot id is a compile-time handle: `fill` needs the two plot slots it
//! spans before the first bar runs. These tests cover endpoints that reach
//! `fill` through another variable, and the endpoints the compiler cannot
//! resolve, which must be reported instead of dropping the band.
//!
//! PineWASM accepts `plot()` and `hline()` IDs that are assigned to a variable
//! and passed to `fill()`. The alias and rejection cases below follow from that
//! rule and from PineWASM's diagnostics.

use pinewasm_compiler::{compile, compile_and_run, CompileOptions, MarketData, RunOptions, Value};

fn data() -> MarketData {
    MarketData {
        open: vec![9.0, 11.0, 12.0, 10.5],
        high: vec![10.5, 12.5, 12.0, 11.0],
        low: vec![8.5, 10.5, 10.0, 9.5],
        close: vec![10.0, 12.0, 11.0, 10.75],
        volume: vec![1000.0, 1500.0, 800.0, 1200.0],
        ..MarketData::default()
    }
}

fn run_ok(source: &str) -> pinewasm_compiler::RunResult {
    let run = compile_and_run(
        source,
        &data(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        run.diagnostics.iter().all(|d| !d.is_error()),
        "unexpected compile errors: {:?}",
        run.diagnostics
    );
    assert!(
        run.runtime_errors.is_empty(),
        "unexpected runtime errors: {:?}",
        run.runtime_errors
    );
    run
}

/// The `(plot1, plot2)` slots of every band, in declaration order.
fn endpoints(run: &pinewasm_compiler::RunResult) -> Vec<(u32, u32)> {
    run.fills
        .iter()
        .map(|band| (band.plot1, band.plot2))
        .collect()
}

/// Compile a script that must fail, returning `(code, message)` per error.
fn error_messages(source: &str) -> Vec<(String, String)> {
    let result = compile(source, CompileOptions::default());
    assert!(!result.success, "the script must not compile");
    result
        .diagnostics
        .iter()
        .filter(|d| d.is_error())
        .map(|d| (d.code.clone().unwrap_or_default(), d.message.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// Aliased endpoints
// ---------------------------------------------------------------------------

#[test]
fn fill_resolves_an_aliased_plot_endpoint() {
    let run = run_ok(
        r"//@version=6
indicator('alias')
p1 = plot(close, 'Close')
p2 = plot(open, 'Open')
q = p1
fill(q, p2, color.red, 'Band')
",
    );
    // The alias adds no plot of its own.
    assert_eq!(run.plots.len(), 2);
    assert_eq!(endpoints(&run), vec![(0, 1)]);
    let band = &run.fills[0];
    assert_eq!(band.title.as_deref(), Some("Band"));
    assert_eq!(
        run.plots[band.plot1 as usize].title.as_deref(),
        Some("Close")
    );
    assert_eq!(band.colors[0].values.len(), 4, "one color per bar");
}

#[test]
fn fill_resolves_an_aliased_hline_endpoint() {
    let run = run_ok(
        r"//@version=6
indicator('alias')
h1 = hline(1.0, 'Upper')
h2 = hline(-1.0, 'Lower')
g = h1
fill(h2, g, color.blue)
",
    );
    assert_eq!(run.plots.len(), 2);
    assert_eq!(endpoints(&run), vec![(1, 0)]);
    let band = &run.fills[0];
    assert_eq!(run.plots[band.plot1 as usize].values[0], Value::Float(-1.0));
    assert_eq!(run.plots[band.plot2 as usize].values[0], Value::Float(1.0));
}

#[test]
fn fill_resolves_two_aliased_endpoints_next_to_a_direct_fill() {
    let run = run_ok(
        r"//@version=6
indicator('alias')
p1 = plot(close)
p2 = plot(open)
p3 = plot(low)
a = p1
b = p3
fill(a, b, color.red)
fill(p1, p2, color.blue)
",
    );
    assert_eq!(run.plots.len(), 3);
    assert_eq!(endpoints(&run), vec![(0, 2), (0, 1)]);
    assert_eq!((run.fills[0].slot, run.fills[1].slot), (0, 1));
}

#[test]
fn fill_resolves_alias_chains_and_declaration_forms() {
    // An alias of an alias, a parenthesised initializer and a `var`
    // declaration all name the same plot.
    let run = run_ok(
        r"//@version=6
indicator('alias')
p1 = plot(close)
p2 = plot(open)
a = p1
b = (a)
c = p2
var d = c
fill((b), d, color.red)
",
    );
    assert_eq!(run.plots.len(), 2);
    assert_eq!(endpoints(&run), vec![(0, 1)]);
}

#[test]
fn fill_through_aliases_matches_the_direct_form() {
    let direct = run_ok(
        r"//@version=6
indicator('same')
p1 = plot(high)
p2 = plot(low)
fill(p1, p2, close > open ? color.green : color.red)
",
    );
    let aliased = run_ok(
        r"//@version=6
indicator('same')
p1 = plot(high)
p2 = plot(low)
a = p1
b = p2
fill(a, b, close > open ? color.green : color.red)
",
    );
    assert_eq!(endpoints(&aliased), endpoints(&direct));
    assert_eq!(
        aliased.fills[0].colors[0].values,
        direct.fills[0].colors[0].values
    );
    assert_eq!(aliased.plots.len(), direct.plots.len());
}

// ---------------------------------------------------------------------------
// Endpoints that cannot be resolved
// ---------------------------------------------------------------------------

#[test]
fn fill_reports_a_conditional_plot_endpoint() {
    let errors = error_messages(
        r"//@version=6
indicator('ternary')
p1 = close > open ? plot(close) : plot(open)
p2 = plot(low)
fill(p1, p2, color.red)
",
    );
    // A plot call may not be a ternary operand, so neither branch makes an
    // endpoint for `fill`.
    let ternary = (
        "E0254".to_string(),
        "Cannot use \"plot\" in a ternary operand".to_string(),
    );
    assert_eq!(errors, vec![ternary.clone(), ternary]);
}

#[test]
fn fill_reports_a_reassigned_plot_endpoint() {
    let errors = error_messages(
        r"//@version=6
indicator('reassigned')
var p1 = plot(close)
p2 = plot(open)
p1 := p2
fill(p2, p1, color.red)
",
    );
    assert_eq!(
        errors,
        vec![(
            "E0205".to_string(),
            "`fill` argument `plot2` does not name one `plot` call".to_string()
        )]
    );
}

#[test]
fn fill_reports_an_alias_of_an_unresolved_hline() {
    // The alias inherits nothing from a reassigned source, and each
    // unresolved endpoint is reported on its own.
    let errors = error_messages(
        r"//@version=6
indicator('reassigned')
var h1 = hline(1.0)
h2 = hline(2.0)
h1 := h2
g = h1
fill(g, h1, color.red)
",
    );
    assert_eq!(
        errors,
        vec![
            (
                "E0205".to_string(),
                "`fill` argument `hline1` does not name one `hline` call".to_string()
            ),
            (
                "E0205".to_string(),
                "`fill` argument `hline2` does not name one `hline` call".to_string()
            ),
        ]
    );
}

#[test]
fn fill_reports_an_alias_of_a_persistent_handle() {
    // `p := other` runs after `q = p` on bar 0, but `var` carries it into the
    // next bar, so `q` names `other` from bar 1 on.
    let errors = error_messages(
        r"//@version=6
indicator('persistent')
var p = plot(close)
other = plot(open)
q = p
p := other
fill(q, other, color.red)
",
    );
    assert_eq!(
        errors,
        vec![(
            "E0205".to_string(),
            "`fill` argument `plot1` does not name one `plot` call".to_string()
        )]
    );
}

// ---------------------------------------------------------------------------
// Aliases of `var`/`varip` handles
//
// `var` and `varip` initialize once, on the first bar, and keep their value
// until a `:=` changes it. A per-bar alias `q = p` reads whatever `p` holds on the current bar, so it
// names one plot exactly when `p` is never reassigned. A `:=` anywhere in the
// script, even below the `fill`, reaches the alias from the next bar on.
// ---------------------------------------------------------------------------

#[test]
fn fill_resolves_an_alias_of_a_var_handle_that_is_never_reassigned() {
    let run = run_ok(
        r"//@version=6
indicator('stable var')
var p = plot(close, 'Close')
other = plot(open, 'Open')
q = p
fill(q, other, color.red)
",
    );
    assert_eq!(run.plots.len(), 2);
    assert_eq!(endpoints(&run), vec![(0, 1)]);
}

#[test]
fn fill_resolves_an_alias_of_a_varip_handle_that_is_never_reassigned() {
    let run = run_ok(
        r"//@version=6
indicator('stable varip')
other = plot(open)
varip p = plot(close)
a = p
q = (a)
fill(other, q, color.red)
",
    );
    assert_eq!(run.plots.len(), 2);
    assert_eq!(endpoints(&run), vec![(0, 1)]);
}

#[test]
fn fill_reports_an_alias_whose_var_source_is_reassigned_below_the_fill() {
    // On bar 0 `q` names the first plot; `p := other` then carries into bar 1,
    // where `q = p` names `other`.
    let errors = error_messages(
        r"//@version=6
indicator('reassigned later')
var p = plot(close)
other = plot(open)
q = p
fill(q, other, color.red)
p := other
",
    );
    assert_eq!(
        errors,
        vec![(
            "E0205".to_string(),
            "`fill` argument `plot1` does not name one `plot` call".to_string()
        )]
    );
}

#[test]
fn fill_reports_a_var_handle_reassigned_below_the_fill() {
    let errors = error_messages(
        r"//@version=6
indicator('reassigned later')
var p = plot(close)
other = plot(open)
fill(other, p, color.red)
p := other
",
    );
    assert_eq!(
        errors,
        vec![(
            "E0205".to_string(),
            "`fill` argument `plot2` does not name one `plot` call".to_string()
        )]
    );
}

#[test]
fn fill_reports_an_alias_chain_whose_varip_root_is_reassigned() {
    let errors = error_messages(
        r"//@version=6
indicator('chain')
varip h = hline(1.0)
other = hline(2.0)
a = h
b = a
h := other
fill(b, other, color.red)
",
    );
    assert_eq!(
        errors,
        vec![(
            "E0205".to_string(),
            "`fill` argument `hline1` does not name one `hline` call".to_string()
        )]
    );
}

#[test]
fn a_var_copy_keeps_the_first_bar_handle_of_a_reassigned_source() {
    // `var d = p` runs once, on bar 0, before `p := other`, so `d` names the
    // first plot on every bar even though its source changes.
    let run = run_ok(
        r"//@version=6
indicator('var copy')
var p = plot(close)
other = plot(open)
var d = p
p := other
fill(d, other, color.red)
",
    );
    assert_eq!(endpoints(&run), vec![(0, 1)]);
}

#[test]
fn reassigning_an_alias_leaves_its_source_usable() {
    let run = run_ok(
        r"//@version=6
indicator('alias')
p1 = plot(close)
p2 = plot(open)
var q = p1
q := p2
fill(p1, p2, color.red)
",
    );
    assert_eq!(endpoints(&run), vec![(0, 1)]);
}

// ---------------------------------------------------------------------------
// Neighbouring rejections are unchanged
// ---------------------------------------------------------------------------

#[test]
fn aliases_do_not_relax_the_endpoint_kind_check() {
    // A plot alias paired with an hline is still a mixed-overload error, and
    // it is reported once: by the kind check, not as an unresolved endpoint.
    let errors = error_messages(
        r"//@version=6
indicator('mixed')
p1 = plot(close)
h1 = hline(1.0)
q = p1
fill(q, h1, color.red)
",
    );
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].0, "E0205");
    assert!(
        !errors[0].1.contains("does not name"),
        "the kind mismatch is the reported error: {errors:?}"
    );
}

#[test]
fn aliases_do_not_allow_fill_in_local_scope() {
    let errors = error_messages(
        r"//@version=6
indicator('local')
p1 = plot(close)
p2 = plot(open)
q = p1
if close > open
    fill(q, p2, color.red)
",
    );
    assert_eq!(
        errors,
        vec![(
            "E0254".to_string(),
            "Cannot use \"fill\" in local scope".to_string()
        )]
    );
}

#[test]
fn a_non_plot_endpoint_is_reported_once() {
    let errors = error_messages(
        r"//@version=6
indicator('bad')
top = plot(high)
fill(top, close, color = color.red)
",
    );
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        !errors[0].1.contains("does not name"),
        "a series endpoint is a type error: {errors:?}"
    );
}
