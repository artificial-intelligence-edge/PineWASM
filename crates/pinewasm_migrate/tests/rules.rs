//! Per-rule coverage. For every rule in docs/migration.md this file adds
//! what `golden.rs` and `findings.rs` did not already pin: a negative case
//! showing the rule leaves unaffected code alone, the rule applied inside
//! nested constructs (functions, methods, `if`/`switch` expressions, loop
//! bodies, ternary branches, `request.security` expressions, library
//! exports), and, for review-only rules, the exact source span of the
//! finding. The coverage table in docs/migration.md lists these tests by
//! rule. Test inputs are small hand-written Pine sources for these rules.

use pinewasm_compiler::{compile, CompileOptions};
use pinewasm_migrate::{migrate, MigrationOptions, Profile, RuleClass, Severity, Status};
use pretty_assertions::assert_eq;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Compile {
    Yes,
    No,
}

/// Exact output, edit replay, idempotence, and compilation where
/// PineWASM supports every construct.
fn golden(input: &str, expected: &str, compiles: Compile) {
    golden_with(input, expected, compiles, &MigrationOptions::default());
}

fn golden_with(input: &str, expected: &str, compiles: Compile, options: &MigrationOptions) {
    let result = migrate(input, options);
    assert_eq!(result.status, Status::Migrated, "{:?}", result.parse_errors);
    assert!(
        !result.findings.iter().any(|f| f.rule == "internal"),
        "{:?}",
        result.findings
    );
    assert_eq!(result.output, expected);
    let mut applied = input.to_string();
    for edit in result.edits.iter().rev() {
        applied.replace_range(edit.start as usize..edit.end as usize, &edit.new_text);
    }
    assert_eq!(applied, result.output);
    let again = migrate(&result.output, options);
    assert_eq!(again.status, Status::AlreadyV6);
    assert_eq!(again.output, result.output);
    if compiles == Compile::Yes {
        let errors: Vec<_> = compile(&result.output, CompileOptions::default())
            .diagnostics
            .into_iter()
            .filter(|d| d.severity == pinewasm_span::Severity::Error)
            .collect();
        assert!(errors.is_empty(), "{errors:?}\n{}", result.output);
    }
}

/// The rule does not fire: the only change is the version directive and
/// no finding names `rule`.
fn untouched(input: &str, rule: &str) {
    let expected = input.replacen("//@version=5", "//@version=6", 1);
    golden(input, &expected, Compile::Yes);
    let findings = migrate(input, &MigrationOptions::default()).findings;
    assert!(
        !findings.iter().any(|f| f.rule == rule),
        "{rule} fired: {findings:?}"
    );
}

/// A finding for `rule` covers exactly the last occurrence of `snippet` in
/// the original source, with the given class, severity, and application.
fn finding(
    input: &str,
    options: &MigrationOptions,
    rule: &str,
    snippet: &str,
    class: RuleClass,
    severity: Severity,
    applied: bool,
) {
    let result = migrate(input, options);
    assert_eq!(result.status, Status::Migrated, "{:?}", result.parse_errors);
    let start = input
        .rfind(snippet)
        .unwrap_or_else(|| panic!("{snippet:?} not in input")) as u32;
    let end = start + snippet.len() as u32;
    let found = result
        .findings
        .iter()
        .find(|f| f.rule == rule && f.start == start && f.end == end)
        .unwrap_or_else(|| {
            let spans: Vec<_> = result
                .findings
                .iter()
                .map(|f| (f.rule, &input[f.start as usize..f.end as usize]))
                .collect();
            panic!("no {rule} finding on {snippet:?}; got {spans:?}")
        });
    assert_eq!(found.class, class, "{found:?}");
    assert_eq!(found.severity, severity, "{found:?}");
    assert_eq!(found.applied, applied, "{found:?}");
}

fn default_finding(input: &str, rule: &str, snippet: &str, class: RuleClass, severity: Severity) {
    let applied = class != RuleClass::Diagnostic;
    finding(
        input,
        &MigrationOptions::default(),
        rule,
        snippet,
        class,
        severity,
        applied,
    );
}

// -- B2 ----------------------------------------------------------------------

#[test]
fn b2_repeat_inside_a_function_keeps_the_first() {
    golden(
        "//@version=5\nindicator(\"B2 local\")\nf() => label.new(bar_index, high, text = \"a\", text = \"b\", textcolor = color.white)\nif close > open\n    f()\n",
        "//@version=6\nindicator(\"B2 local\")\nf() => label.new(bar_index, high, text = \"a\", textcolor = color.white)\nif close > open\n    f()\n",
        Compile::Yes,
    );
}

#[test]
fn b2_positional_then_named_title_keeps_the_positional() {
    golden(
        "//@version=5\nindicator(\"B2 positional\")\nplotshape(close > open, \"Up\", shape.circle, title = \"Other\")\n",
        "//@version=6\nindicator(\"B2 positional\")\nplotshape(close > open, \"Up\", shape.circle)\n",
        Compile::Yes,
    );
}

#[test]
fn b2_distinct_arguments_are_untouched() {
    untouched(
        "//@version=5\nindicator(\"B2 none\")\nplot(close, title = \"a\", color = color.blue, linewidth = 2)\n",
        "B2",
    );
}

// -- B3 ----------------------------------------------------------------------

#[test]
fn b3_time_bars_back_in_ternary_branches_on_the_chart_timeframe() {
    // Neither call names another timeframe, so no D-8 review is needed.
    let input = "//@version=5\nindicator(\"B3 ternary\")\nt = close > open ? time(timeframe.period, 2) : time(\"\", 1)\nplot(t)\n";
    golden(
        input,
        "//@version=6\nindicator(\"B3 ternary\")\nt = close > open ? time(timeframe.period, bars_back = 2) : time(\"\", bars_back = 1)\nplot(t)\n",
        Compile::Yes,
    );
    let findings = migrate(input, &MigrationOptions::default()).findings;
    assert!(!findings.iter().any(|f| f.rule == "D-8"), "{findings:?}");
}

#[test]
fn b3_short_positional_lists_and_named_arguments_are_untouched() {
    untouched(
        "//@version=5\nindicator(\"B3 none\")\nsrc = input.source(close, \"Source\")\nt = time(\"D\")\nu = time(\"D\", bars_back = 1)\nplot(src + t + u)\n",
        "B3",
    );
}

// -- C1 ----------------------------------------------------------------------

#[test]
fn c1_when_inside_if_loop_and_function_bodies() {
    golden(
        concat!(
            "//@version=5\nstrategy(\"Nested when\")\ngo = close > open\n",
            "if bar_index > 10\n    strategy.entry(\"L\", strategy.long, when = go)\n",
            "for i = 0 to 1\n    strategy.order(\"O\" + str.tostring(i), strategy.long, 1, when = go and i == 0)\n",
            "enter() =>\n    strategy.entry(\"S\", strategy.short, when = not go)\n    true\n",
            "x = enter()\n",
        ),
        concat!(
            "//@version=6\nstrategy(\"Nested when\", margin_long = 0, margin_short = 0)\ngo = close > open\n",
            "if bar_index > 10\n    if go\n        strategy.entry(\"L\", strategy.long)\n",
            "for i = 0 to 1 by 1\n    if go and i == 0\n        strategy.order(\"O\" + str.tostring(i), strategy.long, 1)\n",
            "enter() =>\n    if not go\n        strategy.entry(\"S\", strategy.short)\n    true\n",
            "x = enter()\n",
        ),
        Compile::Yes,
    );
}

#[test]
fn c1_positional_when_on_cancel_and_close_all() {
    golden(
        "//@version=5\nstrategy(\"Positional when\", margin_long = 0, margin_short = 0)\nstrategy.cancel(\"L\", close < open)\nstrategy.close_all(close < open, comment = \"flat\")\nstrategy.cancel_all(when = high > high[1])\n",
        "//@version=6\nstrategy(\"Positional when\", margin_long = 0, margin_short = 0)\nif close < open\n    strategy.cancel(\"L\")\nif close < open\n    strategy.close_all(comment = \"flat\")\nif high > high[1]\n    strategy.cancel_all()\n",
        Compile::Yes,
    );
}

#[test]
fn c1_orders_without_when_are_untouched() {
    untouched(
        "//@version=5\nstrategy(\"No when\", margin_long = 0, margin_short = 0)\nif close > open\n    strategy.entry(\"L\", strategy.long, qty = 2)\nstrategy.close(\"L\", comment = \"x\")\n",
        "C1",
    );
}

// -- C2 ----------------------------------------------------------------------

#[test]
fn c2_pairs_inside_an_if_block_and_a_function() {
    // PineWASM does not yet compile a user function whose body is a void
    // strategy call, so this output is checked for the rewrite only.
    golden(
        concat!(
            "//@version=5\nstrategy(\"C2 nest\", margin_long = 0, margin_short = 0)\n",
            "if close > open\n    strategy.exit(\"X\", \"L\", profit = 20, limit = high + 1, loss = 10, stop = low - 1)\n",
            "f(float tp) =>\n    strategy.exit(\"Y\", \"L\", limit = tp, profit = 30)\n",
            "f(high)\n",
        ),
        concat!(
            "//@version=6\nstrategy(\"C2 nest\", margin_long = 0, margin_short = 0)\n",
            "if close > open\n    strategy.exit(\"X\", \"L\", profit = na(high + 1) ? 20 : na, limit = high + 1, loss = na(low - 1) ? 10 : na, stop = low - 1)\n",
            "f(float tp) =>\n    strategy.exit(\"Y\", \"L\", limit = tp, profit = na(tp) ? 30 : na)\n",
            "f(high)\n",
        ),
        Compile::No,
    );
}

#[test]
fn c2_explicit_na_absolute_is_dropped() {
    golden(
        "//@version=5\nstrategy(\"C2 na\", margin_long = 0, margin_short = 0)\nstrategy.exit(\"X\", \"L\", limit = na, profit = 40)\n",
        "//@version=6\nstrategy(\"C2 na\", margin_long = 0, margin_short = 0)\nstrategy.exit(\"X\", \"L\", profit = 40)\n",
        Compile::Yes,
    );
}

#[test]
fn c2_unpaired_levels_are_untouched() {
    untouched(
        "//@version=5\nstrategy(\"C2 none\", margin_long = 0, margin_short = 0)\nstrategy.exit(\"X\", \"L\", profit = 10, loss = 5)\nstrategy.exit(\"Y\", \"L\", limit = high, stop = low)\n",
        "C2",
    );
}

// -- C3 ----------------------------------------------------------------------

#[test]
fn c3_only_the_missing_margin_is_added() {
    golden(
        "//@version=5\nstrategy(\"C3 short only\", margin_short = 30)\n",
        "//@version=6\nstrategy(\"C3 short only\", margin_short = 30, margin_long = 0)\n",
        Compile::Yes,
    );
}

#[test]
fn c3_explicit_margins_and_indicators_are_untouched() {
    untouched(
        "//@version=5\nstrategy(\"C3 none\", margin_long = 50, margin_short = 25)\nplot(close)\n",
        "C3",
    );
    untouched(
        "//@version=5\nindicator(\"C3 indicator\")\nplot(close)\n",
        "C3",
    );
}

// -- D1 ----------------------------------------------------------------------

#[test]
fn d1_numbers_used_as_conditions_in_functions_loops_and_switches() {
    golden(
        concat!(
            "//@version=5\nindicator(\"D1 nest\")\n",
            "f(float x) => x ? 1 : 0\n",
            "v = volume\ng = v and close > open\n",
            "k = 0\nwhile k < 3 and bar_index\n    k += 1\n",
            "m = switch\n    volume => 1\n    => 0\n",
            "plot(f(1) + (g ? 1 : 0) + k + m)\n",
        ),
        concat!(
            "//@version=6\nindicator(\"D1 nest\")\n",
            "f(float x) => bool(x) ? 1 : 0\n",
            "v = volume\ng = bool(v) and close > open\n",
            "k = 0\nwhile k < 3 and bool(bar_index)\n    k := k + 1\n    k\n",
            "m = switch\n    bool(volume) => 1\n    => 0\n",
            "plot(f(1) + (g ? 1 : 0) + k + m)\n",
        ),
        Compile::Yes,
    );
}

#[test]
fn d1_boolean_conditions_are_untouched() {
    untouched(
        "//@version=5\nindicator(\"D1 none\")\nup = close > open\nif up and not (volume > 0)\n    log.info(\"x\")\nplot(up ? 1 : 0)\n",
        "D1",
    );
}

// -- D2 ----------------------------------------------------------------------

#[test]
fn d2_bool_na_in_locals_methods_switches_and_arrays() {
    golden(
        concat!(
            "//@version=5\nindicator(\"D2 nest\")\n",
            "f(bool on) =>\n    bool r = na\n    if on\n        r := true\n    r\n",
            "method flip(bool self) =>\n    self ? na : true\n",
            "x = close > open ? na : true\n",
            "y = switch\n    close > open => true\n    => na\n",
            "arr = array.new_bool(3, na)\n",
            "plotshape(f(true) or x or y or array.get(arr, 0))\n",
            "plotshape((close > open).flip())\n",
        ),
        concat!(
            "//@version=6\nindicator(\"D2 nest\")\n",
            "f(bool on) =>\n    bool r = false\n    if on\n        r := true\n    r\n",
            "method flip(bool self) =>\n    self ? false : true\n",
            "x = close > open ? false : true\n",
            "y = switch\n    close > open => true\n    => false\n",
            "arr = array.new_bool(3, false)\n",
            "plotshape(f(true) or x or y or array.get(arr, 0))\n",
            "plotshape((close > open).flip())\n",
        ),
        Compile::Yes,
    );
}

#[test]
fn d2_na_of_other_types_is_untouched() {
    untouched(
        "//@version=5\nindicator(\"D2 none\")\nfloat x = na\nvar label l = na\nint n = na\nplot(nz(x) + nz(n))\n",
        "D2",
    );
}

// -- D2g ---------------------------------------------------------------------

#[test]
fn d2g_fixnan_of_a_bool_inside_a_function() {
    golden(
        "//@version=5\nindicator(\"D2g\")\nf(bool b) => fixnan(b)\nplotshape(f(close > open))\n",
        "//@version=6\nindicator(\"D2g\")\nf(bool b) => b\nplotshape(f(close > open))\n",
        Compile::Yes,
    );
}

#[test]
fn d2g_nz_and_fixnan_of_numbers_are_untouched() {
    untouched(
        "//@version=5\nindicator(\"D2g none\")\nplot(nz(close[1]) + fixnan(open) + nz(volume, 1))\n",
        "D2g",
    );
}

// -- D2h ---------------------------------------------------------------------

#[test]
fn d2h_comparison_test_inside_a_ternary_branch() {
    golden(
        "//@version=5\nindicator(\"D2h\")\nm = close > open ? na(high >= low) : false\nplotshape(m)\n",
        "//@version=6\nindicator(\"D2h\")\nm = close > open ? (na(high) or na(low)) : false\nplotshape(m)\n",
        Compile::Yes,
    );
}

#[test]
fn d2h_na_of_a_number_is_untouched() {
    untouched(
        "//@version=5\nindicator(\"D2h none\")\nplotshape(na(close) or na(close[1]))\n",
        "D2h",
    );
}

// -- D2i ---------------------------------------------------------------------

#[test]
fn d2i_history_of_a_never_na_bool_becomes_a_bar_count() {
    golden(
        "//@version=5\nindicator(\"D2i\")\nup = ta.crossover(close, open)\nfirst = barstate.isfirst\nplotshape(na(up[3]) or na(first[1]))\n",
        "//@version=6\nindicator(\"D2i\")\nup = ta.crossover(close, open)\nfirst = barstate.isfirst\nplotshape((bar_index < 3) or (bar_index < 1))\n",
        Compile::Yes,
    );
}

#[test]
fn d2i_does_not_apply_to_a_comparison_that_can_be_na() {
    // `close > open` is `na` whenever a price is `na`, so the history
    // test is not a bar count; it is approximated and reported instead.
    let input = "//@version=5\nindicator(\"D2i none\")\nb = close > open\nplotshape(na(b[2]))\n";
    default_finding(
        input,
        "D-1",
        "na(b[2])",
        RuleClass::Approximate,
        Severity::Warning,
    );
    let findings = migrate(input, &MigrationOptions::default()).findings;
    assert!(!findings.iter().any(|f| f.rule == "D2i"), "{findings:?}");
}

// -- D3 ----------------------------------------------------------------------

#[test]
fn d3_na_comparisons_in_functions_and_ternary_branches() {
    golden(
        "//@version=5\nindicator(\"D3 nest\")\nf(x) => x != na ? x : 0\ng = close > open ? (volume == na) : false\nplot(f(close))\nplotshape(g)\n",
        "//@version=6\nindicator(\"D3 nest\")\nf(x) => false ? x : 0\ng = close > open ? (false) : false\nplot(f(close))\nplotshape(g)\n",
        Compile::Yes,
    );
}

#[test]
fn d3_idiomatic_profile_writes_na_tests() {
    golden_with(
        "//@version=5\nindicator(\"D3 idiom\")\nx = close == na\ny = na == open\nplotshape(x or y)\n",
        "//@version=6\nindicator(\"D3 idiom\")\nx = na(close)\ny = na(open)\nplotshape(x or y)\n",
        Compile::Yes,
        &MigrationOptions {
            profile: Profile::Idiomatic,
            ..MigrationOptions::default()
        },
    );
}

#[test]
fn d3_na_calls_and_ordinary_comparisons_are_untouched() {
    untouched(
        "//@version=5\nindicator(\"D3 none\")\nplotshape(na(close) or close == 0 or open != high)\n",
        "D3",
    );
}

// -- D4a ---------------------------------------------------------------------

#[test]
fn d4a_na_scale_is_removed_from_the_declaration() {
    golden(
        "//@version=5\nindicator(\"D4a scale\", scale = na)\nplot(close)\n",
        "//@version=6\nindicator(\"D4a scale\")\nplot(close)\n",
        Compile::Yes,
    );
}

#[test]
fn d4a_explicit_constants_are_untouched() {
    untouched(
        "//@version=5\nindicator(\"D4a none\")\nplot(close, style = plot.style_line, display = display.all)\n",
        "D4a",
    );
}

// -- D4b ---------------------------------------------------------------------

#[test]
fn d4b_na_branch_in_a_function_ternary() {
    golden(
        "//@version=5\nindicator(\"D4b nest\")\nf(bool dots) =>\n    dots ? plot.style_circles : na\nplot(close, style = f(input.bool(true)))\n",
        "//@version=6\nindicator(\"D4b nest\")\nf(bool dots) =>\n    dots ? plot.style_circles : plot.style_line\nplot(close, style = f(input.bool(true)))\n",
        Compile::Yes,
    );
}

#[test]
fn d4b_hline_style_na_branch_assumes_the_solid_style() {
    // An `na` hline style branch becomes `hline.style_solid`; the same
    // assumption as D4c's added branch applies and is reported.
    let input = "//@version=5\nindicator(\"D4b hline\")\nname = input.string(\"Dashed\")\nst = name == \"Dashed\" ? hline.style_dashed : na\nhline(1, linestyle = st)\n";
    golden(
        input,
        "//@version=6\nindicator(\"D4b hline\")\nname = input.string(\"Dashed\")\nst = name == \"Dashed\" ? hline.style_dashed : hline.style_solid\nhline(1, linestyle = st)\n",
        Compile::Yes,
    );
    default_finding(
        input,
        "D4b",
        "na",
        RuleClass::Approximate,
        Severity::Warning,
    );
}

// -- D4c ---------------------------------------------------------------------

#[test]
fn d4c_function_ending_in_a_statement_switch_or_if() {
    // The body's final `switch` or `if` statement is the function's value,
    // the same as the expression form assigned to a variable.
    golden(
        concat!(
            "//@version=5\nindicator(\"D4c tail\")\n",
            "st(x) =>\n    switch x\n        1 => hline.style_dashed\n        2 => hline.style_dotted\n",
            "pl(bool c) =>\n    if c\n        plot.style_histogram\n",
            "g = input.int(1)\n",
            "hline(1, linestyle = st(g))\n",
            "plot(close, style = pl(input.bool(true)))\n",
        ),
        concat!(
            "//@version=6\nindicator(\"D4c tail\")\n",
            "st(x) =>\n    switch x\n        1 => hline.style_dashed\n        2 => hline.style_dotted\n        => hline.style_solid\n",
            "pl(bool c) =>\n    if c\n        plot.style_histogram\n    else\n        plot.style_line\n",
            "g = input.int(1)\n",
            "hline(1, linestyle = st(g))\n",
            "plot(close, style = pl(input.bool(true)))\n",
        ),
        Compile::Yes,
    );
}

#[test]
fn d4c_statement_tail_findings_carry_their_class() {
    let input = "//@version=5\nindicator(\"D4c classes\")\nst(x) =>\n    switch x\n        1 => hline.style_dashed\npl(bool c) =>\n    if c\n        plot.style_histogram\nhline(1, linestyle = st(input.int(1)))\nplot(close, style = pl(input.bool(true)))\n";
    default_finding(
        input,
        "D4c",
        "switch x\n        1 => hline.style_dashed",
        RuleClass::Approximate,
        Severity::Warning,
    );
    default_finding(
        input,
        "D4c",
        "if c\n        plot.style_histogram",
        RuleClass::Exact,
        Severity::Info,
    );
}

#[test]
fn d4c_complete_conditionals_and_other_types_are_untouched() {
    untouched(
        concat!(
            "//@version=5\nindicator(\"D4c none\")\n",
            "st(x) =>\n    switch x\n        1 => hline.style_dashed\n        => hline.style_dotted\n",
            "n(bool c) =>\n    if c\n        1.0\n",
            "hline(1, linestyle = st(input.int(1)))\n",
            "plot(n(close > open))\n",
        ),
        "D4c",
    );
}

// -- D5 ----------------------------------------------------------------------

#[test]
fn d5_const_divisions_in_functions_methods_branches_loops_and_requests() {
    // PineWASM does not yet resolve a method called on a built-in variable
    // (E0202) or evaluate this compound request expression (E0244), so this
    // output is checked for the rewrite only.
    golden(
        concat!(
            "//@version=5\nindicator(\"Division nesting\")\n",
            "f() =>\n    q = 9 / 4\n    q\n",
            "method scaled(float self) =>\n    self * (10 / 4)\n",
            "g = close > open ? 7 / 2 : 3 / 2\n",
            "h = switch\n    close > open => 11 / 3\n    => 1\n",
            "s = 0\nfor i = 0 to 3\n    s += 5 / 2\n",
            "lo = request.security(syminfo.tickerid, \"D\", close / 2 + 3 / 2)\n",
            "plot(f() + close.scaled() + g + h + s + lo)\n",
        ),
        concat!(
            "//@version=6\nindicator(\"Division nesting\")\n",
            "f() =>\n    q = int(9 / 4)\n    q\n",
            "method scaled(float self) =>\n    self * int(10 / 4)\n",
            "g = close > open ? int(7 / 2) : int(3 / 2)\n",
            "h = switch\n    close > open => int(11 / 3)\n    => 1\n",
            "s = 0\nfor i = 0 to 3 by 1\n    s := s + int(5 / 2)\n    s\n",
            "lo = request.security(syminfo.tickerid, \"D\", close / 2 + int(3 / 2))\n",
            "plot(f() + close.scaled() + g + h + s + lo)\n",
        ),
        Compile::No,
    );
}

#[test]
fn d5_reuses_existing_parentheses_and_nests_truncations() {
    golden(
        "//@version=5\nindicator(\"D5 groups\")\na = (5 / 2)\nb = 3 * ((7 / 2))\nd = (5 / 2) / 2\nplot(a + b + d)\n",
        "//@version=6\nindicator(\"D5 groups\")\na = int(5 / 2)\nb = 3 * (int(7 / 2))\nd = int(int(5 / 2) / 2)\nplot(a + b + d)\n",
        Compile::Yes,
    );
}

#[test]
fn d5_leaves_parentheses_after_a_keyword_in_place() {
    // `and(` must not become `andint(`.
    golden(
        "//@version=5\nindicator(\"D5 keyword\")\nc = close > open and(9 / 4) > 1\nplotshape(c)\n",
        "//@version=6\nindicator(\"D5 keyword\")\nc = close > open and(int(9 / 4)) > 1\nplotshape(c)\n",
        Compile::Yes,
    );
}

#[test]
fn d5_library_export_with_a_typed_parameter_keeps_its_fraction() {
    // A typed `int` parameter is at least `simple`, so the division keeps its fraction.
    untouched(
        "//@version=5\n// @description Division helpers.\nlibrary(\"DivisionHelpers\")\nexport half(int n) =>\n    n / 2\n",
        "D5",
    );
}

#[test]
fn d5_divisions_that_are_not_const_int_are_untouched() {
    untouched(
        "//@version=5\nindicator(\"D5 none\")\nfl = 5.0 / 2\nse = close / 2\niv = input.int(5) / 2\nbi = bar_index / 2\nplot(fl + se + iv + bi)\n",
        "D5",
    );
}

#[test]
fn d5_function_called_with_const_and_series_is_reported() {
    default_finding(
        "//@version=5\nindicator(\"D5 mixed\")\nhalf(x) => x / 2\nplot(half(5) + half(bar_index))\n",
        "D5",
        "x / 2",
        RuleClass::Diagnostic,
        Severity::Warning,
    );
}

// -- D6 ----------------------------------------------------------------------

#[test]
fn d6_first_bar_value_inside_a_function() {
    golden(
        "//@version=5\nindicator(\"D6 nest\")\nvar n = 3\nn := 4\nf() => ta.sma(close, n)\nplot(f())\n",
        "//@version=6\nindicator(\"D6 nest\")\nvar n = 3\nn := 4\nf() => ta.sma(close, 4)\nplot(f())\n",
        Compile::Yes,
    );
}

#[test]
fn d6_lengths_that_are_never_reassigned_are_untouched() {
    untouched(
        "//@version=5\nindicator(\"D6 none\")\nn = 3\nlen = input.int(5)\nplot(ta.sma(close, n) + ta.ema(close, len))\n",
        "D6",
    );
}

// -- D8 ----------------------------------------------------------------------

#[test]
fn d8_width_in_a_ternary_and_an_hline() {
    golden(
        "//@version=5\nindicator(\"D8 nest\")\nthin = input.bool(false)\nw = input.int(0, \"Width\")\nplot(close, linewidth = thin ? 0 : 2)\nhline(1, linewidth = w)\n",
        "//@version=6\nindicator(\"D8 nest\")\nthin = input.bool(false)\nw = input.int(0, \"Width\")\nplot(close, linewidth = math.max(1, thin ? 0 : 2))\nhline(1, linewidth = math.max(1, w))\n",
        Compile::Yes,
    );
}

#[test]
fn d8_widths_known_to_be_at_least_one_are_untouched() {
    untouched(
        "//@version=5\nindicator(\"D8 none\")\nw = input.int(2, \"W\", minval = 1)\nplot(close, linewidth = w)\nplot(open, linewidth = 3)\n",
        "D8",
    );
}

// -- E1 ----------------------------------------------------------------------

#[test]
fn e1_operands_in_functions_methods_and_loop_bodies() {
    // PineWASM rejects the stateful call in the loop body and does not yet
    // resolve a method on a built-in variable, so this output is checked
    // for the rewrite only.
    golden(
        concat!(
            "//@version=5\nindicator(\"Lazy nesting\")\n",
            "f(x) =>\n    x > 0 and ta.rsi(close, 14) > 50\n",
            "method up(float self) =>\n    self > open or ta.change(self) > 0\n",
            "type Bar\n    float v\n",
            "method rising(Bar self) =>\n    self.v > 0 and ta.sma(self.v, 3) > self.v\n",
            "c = 0\nfor i = 0 to 2\n    if i > 0 and ta.cum(1) > i\n        c += 1\n",
            "b = Bar.new(close)\n",
            "plotshape(f(close) or close.up() or b.rising())\n",
            "plot(c)\n",
        ),
        concat!(
            "//@version=6\nindicator(\"Lazy nesting\")\n",
            "f(x) =>\n    __v6_1 = ta.rsi(close, 14) > 50\n    x > 0 and __v6_1\n",
            "method up(float self) =>\n    __v6_2 = ta.change(self) > 0\n    self > open or __v6_2\n",
            "type Bar\n    float v\n",
            "method rising(Bar self) =>\n    __v6_3 = ta.sma(self.v, 3) > self.v\n    self.v > 0 and __v6_3\n",
            "c = 0\nfor i = 0 to 2 by 1\n    __v6_4 = ta.cum(1) > i\n    if i > 0 and __v6_4\n        c := c + 1\n        c\n",
            "b = Bar.new(close)\n",
            "__v6_5 = f(close)\n__v6_6 = close.up()\n__v6_7 = b.rising()\n",
            "plotshape(__v6_5 or __v6_6 or __v6_7)\n",
            "plot(c)\n",
        ),
        Compile::No,
    );
}

#[test]
fn e1_ternary_branch_inside_a_library_method() {
    golden(
        concat!(
            "//@version=5\n// @description Swing helpers.\nlibrary(\"SwingHelpers\")\n",
            "export type Swing\n    float price\n",
            "export method holds(Swing self) =>\n    na(self) ? false : close[1] >= self.price or ta.cross(close, self.price)\n",
        ),
        concat!(
            "//@version=6\n// @description Swing helpers.\nlibrary(\"SwingHelpers\")\n",
            "export type Swing\n    float price\n",
            "export method holds(Swing self) =>\n    bool __v6_1 = if na(self)\n        false\n    else\n        __v6_2 = ta.cross(close, self.price)\n        close[1] >= self.price or __v6_2\n    __v6_1\n",
        ),
        Compile::Yes,
    );
}

#[test]
fn e1_history_references_and_pure_calls_are_untouched() {
    untouched(
        concat!(
            "//@version=5\nindicator(\"Pure operands\")\n",
            "b = close > open or na(close[1])\n",
            "c = close > open and close[1] > open[1]\n",
            "f(x) => x > 1\n",
            "d = close > open and f(close)\n",
            "plotshape(b or c or d)\n",
        ),
        "E1",
    );
}

// -- E2 ----------------------------------------------------------------------

#[test]
fn e2_descending_loop_with_a_changing_start_bound() {
    // The unit step is a magnitude, so the loop still counts down.
    golden(
        "//@version=5\nindicator(\"E2 start\")\na = array.from(1.0, 2.0, 3.0)\nfor i = array.size(a) - 1 to 0\n    array.remove(a, i)\nplot(array.size(a))\n",
        "//@version=6\nindicator(\"E2 start\")\na = array.from(1.0, 2.0, 3.0)\nfor i = array.size(a) - 1 to 0 by 1\n    array.remove(a, i)\nplot(array.size(a))\n",
        Compile::Yes,
    );
}

#[test]
fn e2_explicit_steps_and_for_in_loops_are_untouched() {
    untouched(
        "//@version=5\nindicator(\"E2 none\")\na = array.from(1.0, 2.0)\nb = array.new_float(0)\nfor v in a\n    array.push(b, v)\nfor i = 0 to 4 by 2\n    array.push(b, i)\nplot(array.sum(b))\n",
        "E2",
    );
}

// -- E3 ----------------------------------------------------------------------

#[test]
fn e3_fields_methods_and_for_in_bodies() {
    golden(
        concat!(
            "//@version=5\nindicator(\"E3 fields\")\n",
            "type Counter\n    int n = 0\n",
            "var c = Counter.new()\nc.n += 1\n",
            "method bump(Counter self, int k) =>\n    self.n *= k\n    self.n\n",
            "s = 0.0\nfor [i, v] in array.from(1.0, 2.0)\n    s += i * v\n",
            "plot(c.bump(2) + s)\n",
        ),
        concat!(
            "//@version=6\nindicator(\"E3 fields\")\n",
            "type Counter\n    int n = 0\n",
            "var c = Counter.new()\nc.n := c.n + 1\n",
            "method bump(Counter self, int k) =>\n    self.n := self.n * k\n    self.n\n",
            "s = 0.0\nfor [i, v] in array.from(1.0, 2.0)\n    s := s + i * v\n    s\n",
            "plot(c.bump(2) + s)\n",
        ),
        Compile::Yes,
    );
}

#[test]
fn e3_plain_reassignments_are_untouched() {
    untouched(
        "//@version=5\nindicator(\"E3 none\")\nx = 0.0\nx := x + close\nplot(x)\n",
        "E3",
    );
}

// -- E4 ----------------------------------------------------------------------

#[test]
fn e4_bounds_in_functions_nested_loops_and_conditional_mutations() {
    // The migrated loop fixes each end bound at loop entry. A bound the body can
    // change, through a conditional collection mutation or a reassignment in a
    // nested loop, is saved once before its own loop; the inner loop's temporary
    // is recomputed on each outer iteration, so each re-entry takes a fresh
    // value. Loop bodies ending in a reassignment also gain the explicit tail
    // read (D9).
    golden(
        concat!(
            "//@version=5\nindicator(\"E4 nest\")\n",
            "f(float[] a) =>\n    s = 0.0\n    for i = 0 to array.size(a) - 1\n        if array.get(a, i) > 0\n            array.push(a, 0.0)\n        s += 1\n    s\n",
            "n = 3\nfor i = 0 to n\n    for j = 0 to n\n        n -= 1\n",
            "plot(f(array.new_float(2, 1.0)) + n)\n",
        ),
        concat!(
            "//@version=6\nindicator(\"E4 nest\")\n",
            "f(float[] a) =>\n    s = 0.0\n    __v6_1 = array.size(a) - 1\n    for i = 0 to __v6_1 by 1\n        if array.get(a, i) > 0\n            array.push(a, 0.0)\n        s := s + 1\n        s\n    s\n",
            "n = 3\n__v6_2 = n\nfor i = 0 to __v6_2 by 1\n    __v6_3 = n\n    for j = 0 to __v6_3 by 1\n        n := n - 1\n        n\n",
            "plot(f(array.new_float(2, 1.0)) + n)\n",
        ),
        Compile::Yes,
    );
}

#[test]
fn e4_bounds_the_body_cannot_change_stay_inline() {
    // Constants, inputs, immutable arithmetic, and collections the body only
    // reads keep their inline bound in the migrated loop; only the unit step
    // (E2) is written.
    let input = concat!(
        "//@version=5\nindicator(\"E4 none\")\n",
        "len = input.int(3)\nlast = len - 1\na = array.from(1.0, 2.0)\nb = array.new_float(0)\ns = 0.0\n",
        "for i = 0 to last\n    s += i\n",
        "for i = 0 to array.size(a) - 1\n    array.push(b, array.get(a, i))\n",
        "for i = 0 to 4\n    array.push(a, i)\n",
        "plot(s + array.sum(b))\n",
    );
    golden(
        input,
        concat!(
            "//@version=6\nindicator(\"E4 none\")\n",
            "len = input.int(3)\nlast = len - 1\na = array.from(1.0, 2.0)\nb = array.new_float(0)\ns = 0.0\n",
            "for i = 0 to last by 1\n    s := s + i\n    s\n",
            "for i = 0 to array.size(a) - 1 by 1\n    array.push(b, array.get(a, i))\n",
            "for i = 0 to 4 by 1\n    array.push(a, i)\n",
            "plot(s + array.sum(b))\n",
        ),
        Compile::Yes,
    );
    let findings = migrate(input, &MigrationOptions::default()).findings;
    assert!(!findings.iter().any(|f| f.rule == "E4"), "{findings:?}");
}

// -- F1 ----------------------------------------------------------------------

#[test]
fn f1_constants_in_functions_ternaries_and_colors() {
    golden(
        concat!(
            "//@version=5\nindicator(\"F1 nest\")\n",
            "f() => 3[1]\n",
            "g = close > open ? 5[2] : 0\n",
            "h = color.red[1]\n",
            "k = true[1]\n",
            "plot(f() + g + (k ? 1 : 0), color = h)\n",
        ),
        concat!(
            "//@version=6\nindicator(\"F1 nest\")\n",
            "f() =>\n    __v6_4 = 3\n    __v6_4[1]\n",
            "__v6_1 = 5\ng = close > open ? __v6_1[2] : 0\n",
            "__v6_2 = #FF5252\nh = __v6_2[1]\n",
            "__v6_3 = true\nk = __v6_3[1]\n",
            "plot(f() + g + (k ? 1 : 0), color = h)\n",
        ),
        Compile::Yes,
    );
}

#[test]
fn f1_history_of_variables_is_untouched() {
    untouched(
        "//@version=5\nindicator(\"F1 none\")\nx = 6\nplot(close[1] + x[1] + nz(bar_index[2]))\n",
        "F1",
    );
}

// -- F2 ----------------------------------------------------------------------

#[test]
fn f2_persistent_object_is_snapshotted_and_fresh_object_is_indexed() {
    let input = "//@version=5\nindicator(\"F2 both\")\ntype P\n    float v\nvar p = P.new(close)\np.v := close\nq = P.new(close)\nplot(p.v[1] + q.v + q.v[2])\n";
    golden(
        input,
        "//@version=6\nindicator(\"F2 both\")\ntype P\n    float v\nvar p = P.new(close)\np.v := close\nq = P.new(close)\n__v6_1 = p.v\n__v6_2 = q[2]\nplot(__v6_1[1] + q.v + __v6_2.v)\n",
        Compile::Yes,
    );
    default_finding(
        input,
        "F2",
        "p.v[1]",
        RuleClass::Approximate,
        Severity::Warning,
    );
    default_finding(input, "F2", "q.v[2]", RuleClass::Exact, Severity::Info);
}

#[test]
fn f2_fields_of_parameters_in_functions_and_methods() {
    golden(
        "//@version=5\nindicator(\"F2 param\")\ntype P\n    float v\nf(P p) => p.v[1]\nmethod prev(P self) => self.v[1]\nq = P.new(close)\nplot(f(q) + q.prev())\n",
        "//@version=6\nindicator(\"F2 param\")\ntype P\n    float v\nf(P p) =>\n    __v6_1 = p.v\n    __v6_1[1]\nmethod prev(P self) =>\n    __v6_2 = self.v\n    __v6_2[1]\nq = P.new(close)\nplot(f(q) + q.prev())\n",
        Compile::Yes,
    );
}

#[test]
fn f2_field_reads_without_history_are_untouched() {
    untouched(
        "//@version=5\nindicator(\"F2 none\")\ntype P\n    float v\nq = P.new(close)\nplot(q.v)\n",
        "F2",
    );
}

// -- G1 ----------------------------------------------------------------------

#[test]
fn g1_comparisons_and_text_in_functions_switches_and_strings() {
    golden(
        concat!(
            "//@version=5\nindicator(\"tf\")\n",
            "f() => timeframe.period == \"W\" ? 1 : 0\n",
            "s = switch timeframe.period\n    \"D\" => 1\n    \"M\" => 2\n    => 3\n",
            "t = timeframe.period != \"S\"\n",
            "lbl = \"tf: \" + timeframe.period\n",
            "plot(f() + s + (t ? 1 : 0))\n",
            "label.new(bar_index, high, lbl)\n",
        ),
        concat!(
            "//@version=6\nindicator(\"tf\")\n",
            "// v5 spelled a multiplier of 1 without the number (\"D\", not \"1D\").\n",
            "__v6_timeframePeriodV5() =>\n",
            "    timeframe.multiplier != 1 ? timeframe.period : timeframe.isdaily ? \"D\" : timeframe.isweekly ? \"W\" : timeframe.ismonthly ? \"M\" : timeframe.isseconds ? \"S\" : timeframe.period\n",
            "f() => timeframe.period == \"1W\" ? 1 : 0\n",
            "s = switch __v6_timeframePeriodV5()\n    \"D\" => 1\n    \"M\" => 2\n    => 3\n",
            "t = timeframe.period != \"1S\"\n",
            "lbl = \"tf: \" + __v6_timeframePeriodV5()\n",
            "plot(f() + s + (t ? 1 : 0))\n",
            "label.new(bar_index, high, lbl, textcolor = color.black)\n",
        ),
        Compile::Yes,
    );
}

#[test]
fn g1_numeric_timeframes_and_predicates_are_untouched() {
    untouched(
        "//@version=5\nindicator(\"G1 none\")\np = timeframe.period == \"60\"\nq = timeframe.isdaily\nt = time(timeframe.period)\nplotshape(p or q)\nplot(t)\n",
        "G1",
    );
}

#[test]
fn g1_idiomatic_text_use_is_reported_with_its_span() {
    finding(
        "//@version=5\nindicator(\"D-5\")\nlabel.new(bar_index, high, \"tf \" + timeframe.period)\n",
        &MigrationOptions {
            profile: Profile::Idiomatic,
            ..MigrationOptions::default()
        },
        "D-5",
        "timeframe.period",
        RuleClass::Diagnostic,
        Severity::Warning,
        false,
    );
}

// -- G2 ----------------------------------------------------------------------

#[test]
fn g2_constants_in_functions_switches_inputs_and_tables() {
    golden(
        concat!(
            "//@version=5\nindicator(\"G2 nest\")\n",
            "f(up) => up ? color.red : color.teal\n",
            "c = switch\n    close > open => color.yellow\n    => color.gray\n",
            "bg = input.color(color.red, \"Background\")\n",
            "plot(close, color = f(close > open))\n",
            "bgcolor(c)\n",
            "plot(open, color = bg)\n",
            "var t = table.new(position.top_right, 1, 1, bgcolor = color.teal)\n",
        ),
        concat!(
            "//@version=6\nindicator(\"G2 nest\")\n",
            "f(up) => up ? #FF5252 : #00897B\n",
            "c = switch\n    close > open => #FFEB3B\n    => color.gray\n",
            "bg = input.color(#FF5252, \"Background\")\n",
            "plot(close, color = f(close > open))\n",
            "bgcolor(c)\n",
            "plot(open, color = bg)\n",
            "var t = table.new(position.top_right, 1, 1, bgcolor = #00897B)\n",
        ),
        Compile::Yes,
    );
}

#[test]
fn g2_unchanged_constants_are_untouched() {
    untouched(
        "//@version=5\nindicator(\"G2 none\")\nplot(close, color = close > open ? color.green : color.maroon)\nbgcolor(color.new(color.blue, 90))\n",
        "G2",
    );
}

// -- G3 ----------------------------------------------------------------------

#[test]
fn g3_labels_inside_functions_and_blocks() {
    golden(
        "//@version=5\nindicator(\"G3 nest\")\nmark(string txt) =>\n    label.new(bar_index, high, txt, color = color.gray)\nif close > open\n    mark(\"up\")\nelse\n    label.new(bar_index, low, \"down\", style = label.style_label_up)\n",
        "//@version=6\nindicator(\"G3 nest\")\nmark(string txt) =>\n    label.new(bar_index, high, txt, color = color.gray, textcolor = color.black)\nif close > open\n    mark(\"up\")\nelse\n    label.new(bar_index, low, \"down\", style = label.style_label_up, textcolor = color.black)\n",
        Compile::Yes,
    );
}

// -- G4 ----------------------------------------------------------------------

#[test]
fn g4_na_branch_keeps_its_transparency_on_the_color_branch() {
    // `na` draws nothing, so `transp` applies only to the plain color beside
    // it.
    let input = "//@version=5\nindicator(\"G4 na branch\")\nspike = volume > ta.sma(volume, 20)\nbgcolor(spike ? color.yellow : na, transp = 85)\nbgcolor(spike ? na : color.new(color.blue, 40), transp = 60)\n";
    golden(
        input,
        "//@version=6\nindicator(\"G4 na branch\")\nspike = volume > ta.sma(volume, 20)\nbgcolor(spike ? color.new(#FFEB3B, 85) : na)\nbgcolor(spike ? na : color.new(color.blue, 40))\n",
        Compile::Yes,
    );
    default_finding(input, "G4", "transp = 85", RuleClass::Exact, Severity::Info);
}

#[test]
fn g4_transp_on_fill_and_on_a_plotchar_without_a_color() {
    golden(
        "//@version=5\nindicator(\"G4 fill\")\np1 = plot(high)\np2 = plot(low)\nfill(p1, p2, color = color.aqua, transp = 80)\nplotchar(close > open, char = \"x\", transp = 30)\n",
        "//@version=6\nindicator(\"G4 fill\")\np1 = plot(high)\np2 = plot(low)\nfill(p1, p2, color = color.new(color.aqua, 80))\nplotchar(close > open, char = \"x\", color = color.new(color.blue, 30))\n",
        Compile::Yes,
    );
}

#[test]
fn g4_calls_without_transp_are_untouched() {
    untouched(
        "//@version=5\nindicator(\"G4 none\")\nplot(close, color = color.orange)\nbgcolor(close > open ? color.new(color.lime, 80) : na)\n",
        "G4",
    );
}

#[test]
fn g4_runtime_color_alias_is_reported_with_its_span() {
    default_finding(
        "//@version=5\nindicator(\"G4 alias\")\nhl = close > open ? color.lime : na\nbgcolor(hl, transp = 85)\n",
        "G4",
        "transp = 85",
        RuleClass::Approximate,
        Severity::Warning,
    );
}

// -- H1 ----------------------------------------------------------------------

#[test]
fn h1_request_in_an_if_expression_branch_keeps_dynamic_requests() {
    untouched(
        "//@version=5\nindicator(\"H1 local\")\nx = if close > open\n    request.security(syminfo.tickerid, \"D\", close)\nelse\n    0.0\nplot(x)\n",
        "H1",
    );
}

#[test]
fn h1_scripts_without_requests_are_untouched() {
    untouched(
        "//@version=5\nindicator(\"No request\")\nplot(close)\n",
        "H1",
    );
}

#[test]
fn h1_library_exporting_a_request_keeps_the_default() {
    // A library's exported requests always evaluate dynamically, and
    // the declaration default is left omitted like any other script.
    untouched(
        "//@version=5\n// @description Requests.\nlibrary(\"RequestHelpers\")\nexport fetch(simple string tf) =>\n    request.security(syminfo.tickerid, tf, close)\n",
        "H1",
    );
}

#[test]
fn h1_explicit_false_is_removed_when_a_request_runs_locally() {
    let input = "//@version=5\nindicator(\"H1 explicit\", dynamic_requests = false)\nf() => request.security(syminfo.tickerid, \"D\", close)\nfloat x = na\nif bar_index % 2 == 0\n    x := f()\nplot(x)\n";
    golden(
        input,
        "//@version=6\nindicator(\"H1 explicit\")\nf() => request.security(syminfo.tickerid, \"D\", close)\nfloat x = na\nif bar_index % 2 == 0\n    x := f()\nplot(x)\n",
        Compile::Yes,
    );
    default_finding(
        input,
        "H1",
        "indicator(\"H1 explicit\", dynamic_requests = false)",
        RuleClass::Exact,
        Severity::Info,
    );
}

// -- Review findings: exact spans ---------------------------------------------

const H: &str = "//@version=5\nindicator(\"t\")\n";

#[test]
fn review_findings_point_at_their_sites() {
    let d = MigrationOptions::default();
    default_finding(
        &format!("{H}b = close > open\nx = b[1] == false\nplotshape(x)\n"),
        "D-1",
        "b[1] == false",
        RuleClass::Diagnostic,
        Severity::Warning,
    );
    default_finding(
        &format!("{H}o = bar_index % 3\nplot(close, offset = o)\n"),
        "D-2",
        "o",
        RuleClass::Diagnostic,
        Severity::Error,
    );
    default_finding(
        &format!("{H}var n = input.int(3)\nn += 1\nplot(ta.sma(close, n))\n"),
        "D-3",
        "n",
        RuleClass::Diagnostic,
        Severity::Error,
    );
    default_finding(
        "//@version=5\nstrategy(\"t\")\nstrategy.exit(\"X\", \"L\", trail_points = -5, trail_offset = 2)\n",
        "D-7",
        "-5",
        RuleClass::Diagnostic,
        Severity::Warning,
    );
    default_finding(
        &format!("{H}plot(time(\"W\", 2))\n"),
        "D-8",
        "time(\"W\", 2)",
        RuleClass::Diagnostic,
        Severity::Warning,
    );
    default_finding(
        &format!("{H}a = array.from(1.0, 2.0)\nplot(array.get(a, -1))\n"),
        "D-11",
        "-1",
        RuleClass::Diagnostic,
        Severity::Warning,
    );
    finding(
        &format!(
            "{H}c = input.color(color.blue, \"Line\")\nplot(close, color = color.new(c, 50))\n"
        ),
        &d,
        "D-12",
        "color.new(c, 50)",
        RuleClass::Diagnostic,
        Severity::Info,
        false,
    );
    default_finding(
        "//@version=5\nstrategy(\"t\")\nd = close > open ? strategy.long : na\nstrategy.entry(\"L\", d)\n",
        "D-13",
        "na",
        RuleClass::Diagnostic,
        Severity::Error,
    );
}

#[test]
fn nested_request_finding_points_at_the_call_that_needs_dynamic_requests() {
    // The nested request is only a problem because another request runs
    // from a local block, so the review site is that local call.
    default_finding(
        &format!(
            "{H}f() => request.security(syminfo.tickerid, \"D\", close)\nv = request.security(syminfo.tickerid, \"W\", request.security(syminfo.tickerid, \"D\", close))\nif close > open\n    x = f()\nplot(v)\n"
        ),
        "D-4",
        "f()",
        RuleClass::Diagnostic,
        Severity::Error,
    );
}

#[test]
fn modernize_findings_point_at_the_legacy_spellings() {
    let options = MigrationOptions {
        modernize: true,
        ..MigrationOptions::default()
    };
    let input = format!("{H}float[] a = array.new_float(2)\nb = bool(na)\nplot(array.size(a))\n");
    for (rule, snippet) in [
        ("I2", "float[]"),
        ("I3", "array.new_float"),
        ("I4", "bool(na)"),
    ] {
        finding(
            &input,
            &options,
            rule,
            snippet,
            RuleClass::Exact,
            Severity::Info,
            true,
        );
    }
}

#[test]
fn review_findings_do_not_fire_on_safe_sites() {
    let cases: [(&str, String); 10] = [
        (
            "D-1",
            format!("{H}b = close > open\nx = b[1] and b\nplotshape(x)\n"),
        ),
        ("D-2", format!("{H}plot(close, offset = -2)\n")),
        (
            "D-3",
            format!("{H}n = input.int(3)\nplot(ta.sma(close, n))\n"),
        ),
        (
            "D-4",
            format!("{H}v = request.security(syminfo.tickerid, \"W\", request.security(syminfo.tickerid, \"D\", close))\nplot(v)\n"),
        ),
        (
            "D-5",
            format!("{H}label.new(bar_index, high, timeframe.period)\n"),
        ),
        (
            "D-7",
            "//@version=5\nstrategy(\"t\")\nstrategy.exit(\"X\", \"L\", trail_points = 5, trail_offset = 2)\n"
                .to_string(),
        ),
        ("D-8", format!("{H}plot(time(\"\", 2) + time(\"W\"))\n")),
        (
            "D-11",
            format!("{H}a = array.from(1.0, 2.0)\nplot(array.get(a, 1))\n"),
        ),
        (
            "D-12",
            format!("{H}c = input.color(color.blue, \"Line\")\nplot(close, color = c)\n"),
        ),
        (
            "D-13",
            "//@version=5\nstrategy(\"t\")\nd = close > open ? strategy.long : strategy.short\nstrategy.entry(\"L\", d)\n"
                .to_string(),
        ),
    ];
    for (rule, input) in cases {
        let result = migrate(&input, &MigrationOptions::default());
        assert_eq!(result.status, Status::Migrated, "{:?}", result.parse_errors);
        assert!(
            !result.findings.iter().any(|f| f.rule == rule),
            "{rule} fired on {input:?}: {:?}",
            result.findings
        );
    }
}

/// The coverage table in docs/migration.md names tests that exist, and
/// every rule identifier the migrator can report has a row.
#[test]
fn coverage_table_is_accurate() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let doc = std::fs::read_to_string(root.join("../../docs/migration.md")).unwrap();
    let section = doc
        .split("### Test coverage")
        .nth(1)
        .expect("docs/migration.md has a Test coverage section")
        .split("\n#")
        .next()
        .unwrap_or_default();
    let mut rows = std::collections::BTreeSet::new();
    for line in section.lines().filter(|l| l.starts_with("| ")) {
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        let rule = cells[1];
        if rule == "Rule" || rule.starts_with("---") {
            continue;
        }
        rows.insert(rule.to_string());
        for cell in &cells[2..cells.len() - 1] {
            assert!(!cell.is_empty(), "{rule} has an empty column");
            for name in cell.split(',').map(|n| n.trim().trim_matches('`')) {
                let (file, test) = name
                    .split_once("::")
                    .unwrap_or_else(|| panic!("{rule}: {name:?} is not file::test"));
                let source = std::fs::read_to_string(root.join(format!("tests/{file}.rs")))
                    .unwrap_or_else(|e| panic!("{rule}: tests/{file}.rs: {e}"));
                assert!(
                    source.contains(&format!("fn {test}()")),
                    "{rule}: tests/{file}.rs has no test {test}"
                );
            }
        }
    }
    // Rule identifiers are string literals passed to `note`: `A1`, a letter
    // and a number with an optional suffix (`D2g`), or a review code (`D-4`).
    let mut reported = std::collections::BTreeSet::new();
    for entry in std::fs::read_dir(root.join("src/rules")).unwrap() {
        let source = std::fs::read_to_string(entry.unwrap().path()).unwrap();
        for piece in source.split('"').skip(1).step_by(2) {
            let mut chars = piece.chars();
            let letter = chars.next().is_some_and(|c| c.is_ascii_uppercase());
            let rest: String = chars.collect();
            let body = rest.strip_prefix('-').unwrap_or(&rest);
            let digits = body.trim_end_matches(|c: char| c.is_ascii_lowercase());
            if letter
                && !digits.is_empty()
                && digits.len() <= 2
                && digits.chars().all(|c| c.is_ascii_digit())
                && body.len() - digits.len() <= 1
            {
                reported.insert(piece.to_string());
            }
        }
    }
    let missing: Vec<_> = reported.difference(&rows).collect();
    assert!(
        missing.is_empty(),
        "rules without a coverage row: {missing:?}"
    );
    let stale: Vec<_> = rows.difference(&reported).collect();
    assert!(
        stale.is_empty(),
        "coverage rows for unknown rules: {stale:?}"
    );
}
