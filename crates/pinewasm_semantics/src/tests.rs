use crate::analyze;
use pinewasm_hir::{
    dump_hir, DisplayKind as OutputDisplayKind, HirExprKind, HirStmt, InputActiveBinaryOp,
    InputActiveExpr, InputValue, MarkerLocation, MarkerShape, OhlcField, OrderKind, PlotColorRole,
    PlotDisplay, PlotLineStyle, PlotStyleKind, Type,
};
use pinewasm_parse::parse;
use pinewasm_span::{has_errors, Diagnostic, Severity, SourceFile, SourceId};

fn check(source: &str) -> (String, Vec<Diagnostic>) {
    let parsed = parse(source);
    assert!(
        !has_errors(&parsed.diagnostics),
        "test source must parse cleanly: {:?}",
        parsed.diagnostics
    );
    let result = analyze(&parsed.program);
    (dump_hir(&result.hir), result.diagnostics)
}

fn error_codes(diagnostics: &[Diagnostic]) -> Vec<&str> {
    diagnostics
        .iter()
        .filter(|d| d.is_error())
        .filter_map(|d| d.code.as_deref())
        .collect()
}

fn diagnostic_with_code<'a>(diagnostics: &'a [Diagnostic], code: &str) -> &'a Diagnostic {
    diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code.as_deref() == Some(code))
        .unwrap_or_else(|| panic!("missing diagnostic {code}: {diagnostics:?}"))
}

fn assert_diagnostic_source(
    source: &str,
    diagnostic: &Diagnostic,
    expected_source: &str,
    expected_line: u32,
    expected_col: u32,
) {
    let span = diagnostic
        .primary_span()
        .expect("diagnostic must have a primary span");
    assert_eq!(
        &source[span.start as usize..span.end as usize],
        expected_source
    );
    let file = SourceFile::new(SourceId::MAIN, "main.pine", source);
    let start = file.line_col(span.start);
    assert_eq!((start.line, start.col), (expected_line, expected_col));
}

#[test]
fn core_example_hir_snapshot() {
    let (dump, diagnostics) =
        check("//@version=6\nindicator(\"Example\")\nx = close - close[1]\nplot(x)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    insta::assert_snapshot!(dump);
}

#[test]
fn loop_result_expression_reports_compiler_lowering_gap() {
    let (_, diagnostics) = check(
        "//@version=6\nindicator(\"Loop result\")\nvalues = array.from(1, 2)\nresult = for value in values\n    value * 2\nplot(result)\n",
    );
    assert!(
        error_codes(&diagnostics).contains(&"E0250"),
        "{diagnostics:?}"
    );
}

#[test]
fn builtins_resolve_as_series() {
    let (dump, diagnostics) =
        check("indicator(\"B\")\nx = open + high + low + close + volume + bar_index\nplot(x)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(dump.contains("ReadBuiltin open[0] : series float"));
    // `bar_index` is series int; adding it to a float sum
    // goes through an explicit coercion.
    assert!(dump.contains("ReadBuiltin bar_index[0] : series int"));
    assert!(dump.contains("IntToFloat : series float"));
}

#[test]
fn if_else_hir_snapshot() {
    let (dump, diagnostics) = check(
        "indicator(\"Branch\")\nvalue = 0.0\nif close > open\n    value := close\nelse\n    value := open\nplot(value)\n",
    );
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    insta::assert_snapshot!(dump);
}

#[test]
fn plot_family_calls_in_local_scope_are_rejected_with_e0254_and_full_call_spans() {
    let cases = [
        ("plot", "plot(close)"),
        ("plotshape", "plotshape(close > open)"),
        ("plotchar", "plotchar(close > open)"),
        ("plotarrow", "plotarrow(close - open)"),
        ("plotcandle", "plotcandle(open, high, low, close)"),
        ("plotbar", "plotbar(open, high, low, close)"),
        ("hline", "hline(50)"),
        ("bgcolor", "bgcolor(color.red)"),
        ("barcolor", "barcolor(color.green)"),
        ("fill", "fill(1, 2, color.blue)"),
    ];

    for (function, call) in cases {
        let source = format!("indicator(\"X\")\nif close > open\n    {call}\n");
        let (dump, diagnostics) = check(&source);
        let diagnostic = diagnostic_with_code(&diagnostics, "E0254");
        assert_eq!(diagnostic.severity, Severity::Error);
        assert_eq!(
            diagnostic.message,
            format!("Cannot use \"{function}\" in local scope")
        );
        assert_diagnostic_source(&source, diagnostic, call, 3, 5);
        assert!(
            !dump.contains("Plot slot") && !dump.contains("Visual"),
            "local {function} allocated output metadata:\n{dump}"
        );
    }
}

#[test]
fn plot_family_calls_at_global_scope_remain_valid() {
    let (_, diagnostics) = check(
        "indicator(\"X\")\n\
         plot(close)\n\
         plotshape(close > open)\n\
         plotchar(close > open)\n\
         plotarrow(close - open)\n\
         plotcandle(open, high, low, close)\n\
         plotbar(open, high, low, close)\n\
         upper = hline(50)\n\
         lower = hline(25)\n\
         bgcolor(color.red)\n\
         barcolor(color.green)\n\
         fill(upper, lower, color.blue)\n",
    );
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
}

#[test]
fn local_plot_rejection_still_analyzes_arguments() {
    let (_, diagnostics) = check("indicator(\"X\")\nif close > open\n    plot(cloze)\n");
    assert!(error_codes(&diagnostics).contains(&"E0254"));
    assert!(error_codes(&diagnostics).contains(&"E0201"));
}

#[test]
fn qualifier_of_constant_expression_is_const() {
    let (dump, diagnostics) = check("indicator(\"C\")\nk = 1 + 2 * 3\nplot(k)\n");
    assert!(diagnostics.is_empty());
    assert!(dump.contains("symbol 0 `k`: const int"));
}

#[test]
fn unknown_identifier_is_rejected_with_suggestion() {
    let (_, diagnostics) = check("indicator(\"X\")\nplot(cloze)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0201"]);
    assert!(diagnostics[0].notes.iter().any(|n| n.contains("close")));
}

#[test]
fn unknown_function_is_rejected() {
    let (_, diagnostics) = check("indicator(\"X\")\nplot(sma(close))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0202"]);
}

#[test]
fn plot_of_string_is_rejected() {
    let (_, diagnostics) = check("indicator(\"X\")\nplot(\"hello\")\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0205"]);
}

#[test]
fn history_offset_must_be_numeric() {
    let (_, diagnostics) = check("indicator(\"X\")\nx = close[\"x\"]\nplot(close)\n");
    assert!(error_codes(&diagnostics).contains(&"E0210"));
}

#[test]
fn history_offset_accepts_fractional_numeric_values() {
    let (_, diagnostics) = check("//@version=6\nindicator(\"fractional history\")\noff = input.float(1.9)\nplot(bar_index[1.9] - bar_index[1])\nplot(bar_index[off] - bar_index[1])\nplot(close[volume])\n");
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
}

#[test]
fn history_offset_may_be_an_int_expression() {
    // A series or simple `int`-typed offset is allowed. A const int variable, a
    // `series int` builtin (`bar_index`), and an int arithmetic expression are
    // all accepted with no diagnostics; the read lowers to a dynamic-offset
    // path (`ReadBuiltinDynamic`/`ReadVarDynamic`), not the literal fast path.
    let (dump, diagnostics) = check("indicator(\"X\")\nn = 1\ny = close[n]\nplot(y)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(
        dump.contains("ReadBuiltinDynamic"),
        "expected a dynamic builtin read in the dump:\n{dump}"
    );

    let (_, diagnostics) = check("indicator(\"X\")\ny = close[bar_index]\nplot(y)\n");
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");

    let (_, diagnostics) = check("indicator(\"X\")\ny = close[bar_index - 1]\nplot(y)\n");
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");

    // A dynamic offset on a user variable lowers to `ReadVarDynamic`.
    let (dump, diagnostics) =
        check("indicator(\"X\")\nn = 2\nx = close * 2.0\ny = x[n]\nplot(y)\n");
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
    assert!(
        dump.contains("ReadVarDynamic"),
        "expected a dynamic user read in the dump:\n{dump}"
    );

    // A compound target with a dynamic offset materializes into a hidden slot.
    let (dump, diagnostics) = check("indicator(\"X\")\nn = 1\ny = (close - open)[n]\nplot(y)\n");
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
    assert!(
        dump.contains("MaterializeHistoryDynamic"),
        "expected a dynamic materialized history in the dump:\n{dump}"
    );
}

#[test]
fn history_of_non_series_is_rejected() {
    let (_, diagnostics) = check("indicator(\"X\")\nx = 5[1]\nplot(close)\n");
    assert!(error_codes(&diagnostics).contains(&"E0209"));
}

#[test]
fn history_of_compound_expression_materializes_a_hidden_slot() {
    // The value expression is analyzed before `x` is declared, so the
    // hidden slot is symbol 0 and `x` is symbol 1.
    let (dump, diagnostics) = check("indicator(\"X\")\nx = (close - open)[1]\nplot(x)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(
        dump.contains("symbol 0 `%h0`: series float (slot 0, synthetic)"),
        "dump:\n{dump}"
    );
    assert!(
        dump.contains("MaterializeHistory slot0[1] : series float"),
        "dump:\n{dump}"
    );
    insta::assert_snapshot!(dump);
}

#[test]
fn history_of_history_composes_through_a_hidden_slot() {
    // `close[1][2]`: the inner read keeps the direct builtin-offset path;
    // the outer history materializes it into a hidden slot.
    let (dump, diagnostics) = check("indicator(\"X\")\nx = close[1][2]\nplot(x)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(
        dump.contains("MaterializeHistory slot0[2] : series float"),
        "dump:\n{dump}"
    );
    assert!(
        dump.contains("ReadBuiltin close[1] : series float"),
        "dump:\n{dump}"
    );
}

#[test]
fn each_materialize_site_gets_its_own_hidden_slot() {
    let (dump, diagnostics) =
        check("indicator(\"X\")\na = (close - open)[1]\nb = (close + open)[2]\nplot(a)\nplot(b)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    // Symbols: %h0, a, %h1, b (value expressions analyzed before the
    // declarations that consume them).
    assert!(
        dump.contains("symbol 0 `%h0`: series float (slot 0, synthetic)"),
        "dump:\n{dump}"
    );
    assert!(
        dump.contains("symbol 2 `%h1`: series float (slot 2, synthetic)"),
        "dump:\n{dump}"
    );
}

#[test]
fn hidden_slot_names_are_not_reachable_from_source() {
    // `%` cannot start an identifier, and synthetic symbols never enter a
    // scope, so a user variable can shadow nothing and collide with nothing.
    let (dump, diagnostics) =
        check("indicator(\"X\")\nh = (close - open)[1]\nh2 = h[1]\nplot(h2)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    // `h[1]` is an identifier target: the direct offset-read path, no
    // second hidden slot.
    assert!(dump.contains("ReadVar `h`[1]"), "dump:\n{dump}");
    assert!(!dump.contains("%h1"), "dump:\n{dump}");
}

#[test]
fn compound_history_inside_a_branch_warns_w0004() {
    let (_, diagnostics) =
        check("indicator(\"W\")\nd = 0.0\nif close > open\n    d := (close - open)[1]\nplot(d)\n");
    assert!(
        !has_errors(&diagnostics),
        "unexpected errors: {diagnostics:?}"
    );
    let w0004: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.code.as_deref() == Some("W0004"))
        .collect();
    assert_eq!(w0004.len(), 1, "diagnostics: {diagnostics:?}");
}

#[test]
fn compound_history_at_top_level_does_not_warn_w0004() {
    let (_, diagnostics) = check("indicator(\"W\")\nd = (close - open)[1]\nplot(d)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
}

#[test]
fn identifier_history_inside_a_branch_does_not_warn_w0004() {
    // Only materializing sites warn; the direct offset-read path on an
    // identifier target stores nothing and has no gap behavior of its own.
    let (_, diagnostics) =
        check("indicator(\"W\")\nd = 0.0\nif close > open\n    d := close[1]\nplot(d)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
}

#[test]
fn compound_history_in_var_initializer_warns_w0004() {
    let (_, diagnostics) = check("indicator(\"W\")\nvar d = (close - open)[1]\nplot(d)\n");
    assert!(
        !has_errors(&diagnostics),
        "unexpected errors: {diagnostics:?}"
    );
    let w0004: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.code.as_deref() == Some("W0004"))
        .collect();
    assert_eq!(w0004.len(), 1, "diagnostics: {diagnostics:?}");
    assert!(
        w0004[0].message.contains("executes only once"),
        "diagnostics: {diagnostics:?}"
    );
}

#[test]
fn identifier_history_in_var_initializer_does_not_warn_w0004() {
    // Only materializing sites warn; `close[1]` reads the builtin's own
    // history and materializes nothing.
    let (_, diagnostics) = check("indicator(\"W\")\nvar d = close[1]\nplot(d)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
}

#[test]
fn const_compound_history_materializes_a_series_slot() {
    // Computed values have a per-bar history even when their inputs are const.
    let (dump, diagnostics) = check("indicator(\"X\")\nx = (1 + 2)[1]\nplot(close)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(dump.contains("MaterializeHistory"), "dump:\n{dump}");
}

#[test]
fn direct_and_grouped_literals_still_have_no_history() {
    for target in ["1.0", "(1.0)"] {
        let source = format!("indicator(\"X\")\nx = {target}[1]\nplot(close)\n");
        let (_, diagnostics) = check(&source);
        assert!(
            error_codes(&diagnostics).contains(&"E0209"),
            "{source}: {diagnostics:?}"
        );
    }
}

#[test]
fn history_of_user_variable_works() {
    let (dump, diagnostics) = check("indicator(\"X\")\nx = close\ny = x[1]\nplot(y)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(dump.contains("ReadVar `x`[1] (sym 0)"));
}

#[test]
fn indicator_with_non_string_title_is_rejected() {
    let (_, diagnostics) = check("indicator(123)\nplot(close)\n");
    assert!(error_codes(&diagnostics).contains(&"E0205"));
}

#[test]
fn duplicate_declaration_is_rejected() {
    let (_, diagnostics) = check("indicator(\"X\")\nx = 1\nx = 2\nplot(close)\n");
    assert!(error_codes(&diagnostics).contains(&"E0211"));
}

#[test]
fn shadowing_in_branch_scope_is_allowed() {
    let (_, diagnostics) = check("indicator(\"X\")\nx = 1\nif close > open\n    x = 2\nplot(x)\n");
    assert!(
        !has_errors(&diagnostics),
        "unexpected errors: {diagnostics:?}"
    );
}

#[test]
fn branch_local_variable_does_not_escape() {
    let (_, diagnostics) = check("indicator(\"X\")\nif close > open\n    y = 2\nplot(y)\n");
    assert!(error_codes(&diagnostics).contains(&"E0201"));
}

#[test]
fn condition_must_be_bool() {
    let (_, diagnostics) = check("indicator(\"X\")\nif close\n    plot(close)\n");
    assert!(error_codes(&diagnostics).contains(&"E0208"));
}

#[test]
fn arithmetic_on_bool_is_rejected() {
    let (_, diagnostics) = check("indicator(\"X\")\nx = true + 1\nplot(close)\n");
    assert!(error_codes(&diagnostics).contains(&"E0207"));
}

#[test]
fn assigning_void_is_rejected() {
    let (_, diagnostics) = check("indicator(\"X\")\nx = bgcolor(color.red)\n");
    assert!(error_codes(&diagnostics).contains(&"E0212"));
}

#[test]
fn shadowing_a_builtin_warns_unless_the_script_also_uses_it() {
    let (_, diagnostics) = check("indicator(\"X\")\nclose = 1\nplot(close)\n");
    assert!(
        !error_codes(&diagnostics).contains(&"E0211"),
        "{diagnostics:?}"
    );
    assert!(
        diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("W0006")),
        "{diagnostics:?}"
    );
    let (_, diagnostics) = check("indicator(\"X\")\nplot(close)\nclose = 1\n");
    assert!(error_codes(&diagnostics).contains(&"E0211"));
}

#[test]
fn function_parameter_can_shadow_builtin_volume() {
    let source = "//@version=6\nindicator(\"volume parameter shadow check\")\npine_vwma(x, y, volume) =>\n    ta.sma(x * volume, y) / ta.sma(volume, y)\nplot(pine_vwma(hl2, 3, volume))\n";
    let (_, diagnostics) = check(source);
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
}

#[test]
fn method_parameter_can_shadow_builtin_volume() {
    let source = "//@version=6\nindicator(\"method volume shadow check\")\nmethod scaled(array<float> this, float volume) =>\n    array.size(this) + volume * 2\na = array.new<float>()\nplot(a.scaled(close))\n";
    let (_, diagnostics) = check(source);
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
}

#[test]
fn missing_indicator_is_a_warning_not_error() {
    let (_, diagnostics) = check("plot(close)\n");
    assert!(!has_errors(&diagnostics));
    assert!(diagnostics
        .iter()
        .any(|d| d.code.as_deref() == Some("W0001")));
}

#[test]
fn discarded_expression_warns() {
    let (_, diagnostics) = check("indicator(\"X\")\nclose + 1\nplot(close)\n");
    assert!(!has_errors(&diagnostics));
    assert!(diagnostics
        .iter()
        .any(|d| d.code.as_deref() == Some("W0002")));
}

#[test]
fn effectful_value_calls_may_ignore_their_results_like_pine() {
    let (_, diagnostics) = check(
        "indicator(\"X\")\na = array.new_float()\narray.push(a, 1.0)\narray.shift(a)\nlabel.new(bar_index, close)\nplot(close)\n",
    );
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
}

#[test]
fn a_loop_bodys_last_expression_is_its_value_not_a_discarded_one() {
    // A loop is an expression: its value is the last expression of its
    // body, as for an `if` block. A library function that ends a `for` body
    // with a call to its own helper must not warn, just as the same statement
    // does not warn at the end of an `if` block, even when the caller cannot
    // edit the library.
    let queue = "queue(id, int size, value) =>\n    id.push(value)\n    if id.size() > size\n        id.shift()\n";
    for body in [
        "if close > open\n    queue(a, 3, close)\n",
        "switch\n    close > open =>\n        queue(a, 3, close)\n",
        "for i = 0 to 2\n    queue(a, 3, close)\n",
        "for element in a\n    queue(b, 3, element)\n",
        "for [index, element] in a\n    queue(b, 3, element)\n",
        "while array.size(a) < 2\n    queue(a, 3, close)\n",
    ] {
        let source = format!(
            "indicator(\"X\")\n{queue}var a = array.new_float()\nvar b = array.new_float()\n{body}plot(close)\n"
        );
        let (_, diagnostics) = check(&source);
        assert!(diagnostics.is_empty(), "{body}: {diagnostics:?}");
    }
    // Anywhere but last, the value really is thrown away.
    for body in [
        "for i = 0 to 2\n    math.max(close[i], open[i])\n    total += close[i]\n",
        "while total < 3\n    math.max(close, open)\n    total += 1\n",
    ] {
        let (_, diagnostics) = check(&format!(
            "indicator(\"X\")\nfloat total = 0.0\n{body}plot(total)\n"
        ));
        assert!(
            diagnostics
                .iter()
                .any(|d| d.code.as_deref() == Some("W0002")),
            "{body}: {diagnostics:?}"
        );
    }
}

#[test]
fn plot_with_title_and_slot_allocation() {
    let (dump, diagnostics) = check("indicator(\"X\")\nplot(close, \"Close\")\nplot(open)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(dump.contains("plot slot 0 title=\"Close\""));
    assert!(dump.contains("plot slot 1"));
}

#[test]
fn function_name_used_as_value_is_rejected() {
    let (_, diagnostics) = check("indicator(\"X\")\nx = plot\n");
    assert!(error_codes(&diagnostics).contains(&"E0216"));
}

#[test]
fn calling_a_series_is_rejected() {
    let (_, diagnostics) = check("indicator(\"X\")\nx = close(1)\nplot(x)\n");
    assert!(error_codes(&diagnostics).contains(&"E0203"));
}

// -- adversarial regression tests ---------------------------------------------

#[test]
fn plot_title_accepts_const_string_variable() {
    let parsed = parse("indicator(\"X\")\ns = \"My Title\"\nplot(close, s)\n");
    let result = analyze(&parsed.program);
    assert!(!has_errors(&result.diagnostics), "{:?}", result.diagnostics);
    assert_eq!(result.hir.plots[0].title.as_deref(), Some("My Title"));
}

// -- int/float numeric tower ---------------------------------------------------

#[test]
fn int_literal_types_as_int() {
    let (dump, diagnostics) = check("indicator(\"I\")\nx = 42\nplot(x)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(dump.contains("symbol 0 `x`: const int"), "dump:\n{dump}");
    assert!(dump.contains("ConstInt 42 : const int"), "dump:\n{dump}");
    // plot(x) coerces the int value to float.
    assert!(dump.contains("IntToFloat : const float"), "dump:\n{dump}");
}

#[test]
fn float_literal_types_as_float() {
    let (dump, diagnostics) = check("indicator(\"F\")\nx = 42.0\nplot(x)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(dump.contains("symbol 0 `x`: const float"), "dump:\n{dump}");
    assert!(
        dump.contains("ConstFloat 42 : const float"),
        "dump:\n{dump}"
    );
}

#[test]
fn mixed_arithmetic_types_as_float_with_visible_coercion() {
    let (dump, diagnostics) = check("indicator(\"M\")\nx = 1 + 2.5\nplot(x)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    insta::assert_snapshot!(dump);
}

#[test]
fn int_arithmetic_stays_int() {
    let (dump, diagnostics) = check("indicator(\"II\")\nx = 7 % 3 + 4 * 2 - 1\nplot(x)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(dump.contains("Binary `%` : const int"), "dump:\n{dump}");
    assert!(dump.contains("Binary `*` : const int"), "dump:\n{dump}");
    assert!(dump.contains("Binary `+` : const int"), "dump:\n{dump}");
    assert!(dump.contains("Binary `-` : const int"), "dump:\n{dump}");
}

#[test]
fn int_division_is_statically_int_for_every_qualifier() {
    // The static type of int/int stays int, while the runtime opcode can
    // retain an uneven quotient's fractional value.
    let (dump, diagnostics) = check(
        "indicator(\"DIV\")\nn = input.int(9)\nc = 7 / 2\ni = n * 3 / 4\ns = bar_index / 2\nplot(c + i + s)\n",
    );
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(dump.contains("Binary `/` : const int"), "dump:\n{dump}");
    assert!(dump.contains("Binary `/` : input int"), "dump:\n{dump}");
    assert!(dump.contains("Binary `/` : series int"), "dump:\n{dump}");
    assert!(dump.contains("symbol 1 `c`: const int"), "dump:\n{dump}");
}

#[test]
fn int_division_is_accepted_in_int_contexts() {
    for src in [
        "indicator(\"R\")\nplot(close[4 / 2])\n",
        "indicator(\"R\")\na = array.new_float(10 / 2)\nplot(array.size(a))\n",
        "indicator(\"R\")\na = array.from(1.0, 2.0, 3.0)\nplot(array.get(a, 4 / 2))\n",
        "indicator(\"R\")\nint x = 7 / 2\nplot(x)\n",
    ] {
        let (_, diagnostics) = check(src);
        assert!(diagnostics.is_empty(), "{src}: {diagnostics:?}");
    }
    for src in [
        "indicator(\"R\")\nplot(close[int(4 / 2)])\n",
        "indicator(\"R\")\na = array.new_float(int(10 / 2))\nplot(array.size(a))\n",
        "indicator(\"R\")\na = array.from(1.0, 2.0, 3.0)\nplot(array.get(a, int(4 / 2)))\n",
        "indicator(\"R\")\nint x = int(7 / 2)\nplot(x)\n",
    ] {
        let (_, diagnostics) = check(src);
        assert!(diagnostics.is_empty(), "{src}: {diagnostics:?}");
    }
}

#[test]
fn accepts_fractional_int_division_in_an_int_initializer() {
    // Checks that a fractional int division in an int initializer compiles.
    let (dump, diagnostics) = check("indicator(\"DIV\")\nint x = 5 / 2\nplot(x)\n");
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert!(dump.contains("Binary `/` : const int"), "{dump}");
}

#[test]
fn accepts_fractional_int_division_in_int_reassignment() {
    // Checks that a fractional int division in an int reassignment compiles.
    let (dump, diagnostics) =
        check("indicator(\"DIV\")\nf(int a) =>\n    int x = a\n    x /= 2\n    x\nplot(f(5))\n");
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert!(dump.contains("Binary `/` : const int"), "{dump}");
}

#[test]
fn accepts_fractional_int_division_as_an_array_index() {
    // A fractional int division used as an array index compiles; array.get reads element 30.
    let (_, diagnostics) =
        check("indicator(\"DIV\")\na = array.from(10, 20, 30, 40)\nplot(array.get(a, 5 / 2))\n");
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn int_float_equality_promotes_the_int_side() {
    let (dump, diagnostics) = check("indicator(\"EQ\")\nok = 1 == 1.0\nplot(ok ? close : open)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(dump.contains("Binary `==` : const bool"), "dump:\n{dump}");
    assert!(dump.contains("IntToFloat : const float"), "dump:\n{dump}");
}

#[test]
fn mixed_comparison_promotes_the_int_side() {
    let (dump, diagnostics) =
        check("indicator(\"CMP\")\nok = close > 1\nplot(ok ? close : open)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(dump.contains("Binary `>` : series bool"), "dump:\n{dump}");
    assert!(dump.contains("IntToFloat : const float"), "dump:\n{dump}");
}

#[test]
fn modulo_on_bools_is_rejected() {
    let (_, diagnostics) = check("indicator(\"X\")\nx = true % false\nplot(close)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0207"]);
}

#[test]
fn unary_minus_preserves_int_and_rejects_bool() {
    let (dump, diagnostics) = check("indicator(\"N\")\nx = -3\ny = -3.5\nplot(y)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(dump.contains("symbol 0 `x`: const int"), "dump:\n{dump}");
    assert!(dump.contains("symbol 1 `y`: const float"), "dump:\n{dump}");

    let (_, diagnostics) = check("indicator(\"N\")\nx = -true\nplot(close)\n");
    assert!(error_codes(&diagnostics).contains(&"E0207"));
}

#[test]
fn bar_index_types_as_series_int() {
    let (dump, diagnostics) =
        check("indicator(\"B\")\neven = bar_index % 2 == 0\nplot(even ? close : open)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(
        dump.contains("ReadBuiltin bar_index[0] : series int"),
        "dump:\n{dump}"
    );
    assert!(dump.contains("Binary `%` : series int"), "dump:\n{dump}");
    assert!(
        dump.contains("symbol 0 `even`: series bool"),
        "dump:\n{dump}"
    );
}

#[test]
fn r1_time_axis_builtins_resolve_with_correct_types() {
    // The derived averages are series float; the time and calendar builtins
    // are series int.
    let (dump, diagnostics) = check(
        "indicator(\"R\")\nplot(hl2 + ohlc4)\nt = time + time_close + dayofweek + year\nplot(t)\n",
    );
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(
        dump.contains("ReadBuiltin hl2[0] : series float"),
        "dump:\n{dump}"
    );
    assert!(
        dump.contains("ReadBuiltin ohlc4[0] : series float"),
        "dump:\n{dump}"
    );
    assert!(
        dump.contains("ReadBuiltin time[0] : series int"),
        "dump:\n{dump}"
    );
    assert!(
        dump.contains("ReadBuiltin dayofweek[0] : series int"),
        "dump:\n{dump}"
    );
    // The int builtins sum to a series int.
    assert!(dump.contains("symbol 0 `t`: series int"), "dump:\n{dump}");
}

#[test]
fn r1_dayofweek_constants_are_const_int() {
    let (dump, diagnostics) = check("indicator(\"K\")\nd = dayofweek.monday\nplot(close)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    // `dayofweek.monday` is a const int (value 2), not a float.
    assert!(dump.contains("ConstInt 2 : const int"), "dump:\n{dump}");
}

#[test]
fn plot_of_int_is_coerced_not_rejected() {
    let (dump, diagnostics) = check("indicator(\"P\")\nplot(1)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(dump.contains("IntToFloat : const float"), "dump:\n{dump}");
}

// -- var declarations and := reassignment --------------------------------------

#[test]
fn var_declaration_marks_symbol_persistent_snapshot() {
    let (dump, diagnostics) = check("indicator(\"V\")\nvar n = 0\nn := n + 1\nplot(n)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    // This recurrent write reads a carried slot, so its RHS and symbol are
    // series-qualified before the first reassignment is analyzed.
    assert!(
        dump.contains("symbol 0 `n`: series int (slot 0, var)"),
        "dump:\n{dump}"
    );
    assert!(dump.contains("VarAssign `n` (sym 0)"), "dump:\n{dump}");
    insta::assert_snapshot!(dump);
}

#[test]
fn reassign_resolves_to_the_existing_symbol() {
    let (dump, diagnostics) = check("indicator(\"R\")\nx = 1\nx := 2\nplot(x)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    // One symbol, one slot; the reassignment targets sym 0.
    assert!(dump.contains("Reassign `x` (sym 0)"), "dump:\n{dump}");
    assert!(!dump.contains("symbol 1"), "dump:\n{dump}");
}

#[test]
fn reassign_from_branch_targets_the_outer_symbol_and_promotes_to_series() {
    let (dump, diagnostics) =
        check("indicator(\"B\")\nx = 1\nif close > open\n    x := 2\nplot(x)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    // A conditionally reassigned variable is path-dependent: series.
    assert!(
        dump.contains("symbol 0 `x`: series int (slot 0)"),
        "dump:\n{dump}"
    );
    assert!(dump.contains("Reassign `x` (sym 0)"), "dump:\n{dump}");
}

#[test]
fn reassign_joins_the_value_qualifier() {
    // A top-level reassignment from a series value makes the variable series.
    let (dump, diagnostics) = check("indicator(\"Q\")\nx = 1\nx := bar_index\nplot(x)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(
        dump.contains("symbol 0 `x`: series int (slot 0)"),
        "dump:\n{dump}"
    );
}

#[test]
fn reassign_undeclared_is_rejected() {
    let (_, diagnostics) = check("indicator(\"X\")\ny := 1\nplot(close)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0217"]);
    assert!(
        diagnostics[0].message.contains("declare it first with `=`"),
        "diagnostics: {diagnostics:?}"
    );
}

#[test]
fn reassign_builtin_series_is_rejected() {
    let (_, diagnostics) = check("indicator(\"X\")\nclose := 1\nplot(close)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0217"]);
    assert!(
        diagnostics[0].message.contains("built-in series"),
        "diagnostics: {diagnostics:?}"
    );
}

#[test]
fn reassign_function_name_is_rejected() {
    let (_, diagnostics) = check("indicator(\"X\")\nplot := 1\nplot(close)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0217"]);
    assert!(
        diagnostics[0].message.contains("function"),
        "diagnostics: {diagnostics:?}"
    );
}

#[test]
fn reassign_type_mismatch_is_rejected() {
    // string into float.
    let (_, diagnostics) = check("indicator(\"X\")\nx = 1.0\nx := \"oops\"\nplot(x)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0218"]);
    // float into int.
    let (_, diagnostics) = check("indicator(\"X\")\nx = 1\nx := 1.5\nplot(x)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0218"]);
}

#[test]
fn reassign_int_into_float_var_is_coerced() {
    let (dump, diagnostics) = check("indicator(\"X\")\nx = 1.0\nx := 2\nplot(x)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(dump.contains("Reassign `x` (sym 0)"), "dump:\n{dump}");
    assert!(dump.contains("IntToFloat : const float"), "dump:\n{dump}");
    // The variable keeps its float type.
    assert!(
        dump.contains("symbol 0 `x`: const float (slot 0)"),
        "dump:\n{dump}"
    );
}

#[test]
fn var_duplicate_declaration_is_rejected() {
    let (_, diagnostics) = check("indicator(\"X\")\nvar x = 1\nvar x = 2\nplot(x)\n");
    assert!(error_codes(&diagnostics).contains(&"E0211"));
    let (_, diagnostics) = check("indicator(\"X\")\nx = 1\nvar x = 2\nplot(x)\n");
    assert!(error_codes(&diagnostics).contains(&"E0211"));
}

#[test]
fn var_shadowing_follows_the_builtin_shadowing_rule() {
    let (_, diagnostics) = check("indicator(\"X\")\nvar close = 1\nplot(close)\n");
    assert!(
        !error_codes(&diagnostics).contains(&"E0211"),
        "{diagnostics:?}"
    );
    let (_, diagnostics) = check("indicator(\"X\")\nx = close\nvar close = 1\nplot(x)\n");
    assert!(error_codes(&diagnostics).contains(&"E0211"));
}

#[test]
fn reassign_shadowed_name_targets_the_innermost_symbol() {
    // The branch declares its own `x`; `:=` inside the branch reassigns the
    // inner symbol (innermost-first lookup), leaving the outer one alone.
    let (dump, diagnostics) =
        check("indicator(\"S\")\nx = 1\nif close > open\n    x = 2\n    x := 3\nplot(x)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(dump.contains("Reassign `x` (sym 1)"), "dump:\n{dump}");
    // Outer `x` keeps its const qualifier: only the inner symbol was touched.
    assert!(
        dump.contains("symbol 0 `x`: const int (slot 0)"),
        "dump:\n{dump}"
    );
}

#[test]
fn reassign_of_void_value_is_a_type_mismatch() {
    let (_, diagnostics) = check("indicator(\"X\")\nx = 1.0\nx := bgcolor(color.red)\n");
    assert!(error_codes(&diagnostics).contains(&"E0212"));
}

// -- stateful ta.* builtins and namespaced identifiers ---------------------------

#[test]
fn ta_change_sma_hir_snapshot() {
    // Each ta call site owns its own hidden slot; ta.change's omitted
    // length defaults to 1; TaCall nodes carry slot and length.
    let (dump, diagnostics) =
        check("indicator(\"TA\")\nc = ta.change(close)\ns = ta.sma(close, 2)\nplot(c)\nplot(s)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(
        dump.contains("symbol 0 `%h0`: series float (slot 0, synthetic)"),
        "dump:\n{dump}"
    );
    assert!(
        dump.contains("symbol 2 `%h1`: series float (slot 2, synthetic)"),
        "dump:\n{dump}"
    );
    assert!(
        dump.contains("TaCall ta.change slot0 len=1 : series float"),
        "dump:\n{dump}"
    );
    assert!(
        dump.contains("TaCall ta.sma slot2 len=2 : series float"),
        "dump:\n{dump}"
    );
    insta::assert_snapshot!(dump);
}

#[test]
fn ta_source_int_is_coerced_to_float() {
    let (dump, diagnostics) = check("indicator(\"TA\")\nplot(ta.change(bar_index))\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    // The int source goes through an explicit IntToFloat under the TaCall.
    assert!(
        dump.contains("TaCall ta.change slot0 len=1 : series float"),
        "dump:\n{dump}"
    );
    assert!(dump.contains("IntToFloat : series float"), "dump:\n{dump}");
}

#[test]
fn ta_compound_source_works() {
    let (dump, diagnostics) = check("indicator(\"TA\")\nplot(ta.sma(close - open, 2))\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(
        dump.contains("TaCall ta.sma slot0 len=2 : series float"),
        "dump:\n{dump}"
    );
    assert!(dump.contains("Binary `-` : series float"), "dump:\n{dump}");
}

#[test]
fn ta_sma_missing_length_is_an_argument_count_error() {
    let (_, diagnostics) = check("indicator(\"TA\")\nplot(ta.sma(close))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0204"]);
}

#[test]
fn ta_length_violations_are_e0219() {
    // Zero.
    let (_, diagnostics) = check("indicator(\"TA\")\nplot(ta.sma(close, 0))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0219"]);
    assert!(
        diagnostics[0].message.contains("must be at least 1"),
        "diagnostics: {diagnostics:?}"
    );
    // Negative.
    let (_, diagnostics) = check("indicator(\"TA\")\nplot(ta.sma(close, -3))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0219"]);
    assert!(
        diagnostics[0].message.contains("must be at least 1"),
        "diagnostics: {diagnostics:?}"
    );
    // Too large.
    let (_, diagnostics) = check("indicator(\"TA\")\nplot(ta.sma(close, 100001))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0219"]);
    assert!(
        diagnostics[0].message.contains("must be at most 100000"),
        "diagnostics: {diagnostics:?}"
    );
    // At the bounds: fine.
    let (_, diagnostics) = check("indicator(\"TA\")\nplot(ta.sma(close, 1))\n");
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    let (_, diagnostics) = check("indicator(\"TA\")\nplot(ta.sma(close, 100000))\n");
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    // ta.sma accepts a runtime series-int length, unlike fixed-window TA
    // functions such as ta.ema.
    let (dump, diagnostics) =
        check("indicator(\"TA\")\nn = bar_index + 1\nplot(ta.sma(close, n))\n");
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    assert!(dump.contains("TaCallDynamic ta.sma"));
    // Non-literal integer expression.
    let (_, diagnostics) = check("indicator(\"TA\")\nplot(ta.sma(close, 1 + 1))\n");
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    // Float literal.
    let (_, diagnostics) = check("indicator(\"TA\")\nplot(ta.sma(close, 2.0))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0219"]);
    assert!(
        diagnostics[0].message.contains("float literal"),
        "diagnostics: {diagnostics:?}"
    );
    // ta.change's optional length obeys the same rule when given.
    let (_, diagnostics) = check("indicator(\"TA\")\nplot(ta.change(close, 0))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0219"]);
}

#[test]
fn const_and_input_variables_are_valid_ta_lengths() {
    // A `ta.*` length is a `simple int`: a literal, a const-valued
    // variable, or an `input.int(...)` (fixed for the run) all qualify. The
    // default is checked through const_int propagation; a separate binding
    // preserves input overrides for each run.
    for source in [
        "indicator(\"T\")\nn = 14\nplot(ta.sma(close, n))\n",
        "indicator(\"T\")\nn = input.int(14)\nplot(ta.rsi(close, n))\n",
        "indicator(\"T\")\nn = input.int(20, \"len\", minval=1)\nplot(ta.ema(close, n))\n",
        // Forwarded through another const-valued variable.
        "indicator(\"T\")\na = 9\nb = a\nplot(ta.wma(close, b))\n",
    ] {
        let (_dump, diagnostics) = check(source);
        assert!(
            diagnostics.is_empty(),
            "const/input length should compile: {source:?} -> {diagnostics:?}"
        );
    }

    // A reassignment clears the known value, but ta.sma accepts the resulting
    // runtime int length.
    let (_dump, diagnostics) =
        check("indicator(\"T\")\nn = 14\nn := n + 1\nplot(ta.sma(close, n))\n");
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
}

#[test]
fn function_parameter_can_take_history_regardless_of_qualifier() {
    // A function parameter is a real per-bar slot, so `p[i]` indexes the values
    // stored into it each bar — valid for a UDT field argument and even when
    // the argument is `simple`-qualified (`syminfo.mintick`). A non-slot
    // target (a const literal) still requires a series.
    let (_dump, diagnostics) = check(
        "indicator(\"H\")\ntype S\n    float src\n\
         at(float p, int i) =>\n    p[i]\n\
         s = S.new(close)\nplot(at(s.src, 2))\n",
    );
    assert!(
        !has_errors(&diagnostics),
        "history on a parameter bound to a UDT field should compile: {diagnostics:?}"
    );
    let (_dump, diagnostics) =
        check("indicator(\"H\")\nat(float p, int i) =>\n    p[i]\nplot(at(syminfo.mintick, 2))\n");
    assert!(
        !has_errors(&diagnostics),
        "history on a parameter bound to a simple argument should compile: {diagnostics:?}"
    );
}

#[test]
fn ta_source_type_mismatch_is_e0205() {
    let (_, diagnostics) = check("indicator(\"TA\")\nplot(ta.sma(\"x\", 2))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0205"]);
    let (_, diagnostics) = check("indicator(\"TA\")\nplot(ta.change(true))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0205"]);
}

#[test]
fn unknown_ta_function_is_e0202_with_namespace_note() {
    let (_, diagnostics) = check("indicator(\"TA\")\nplot(ta.foo(close, 2))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0202"]);
    assert!(
        diagnostics[0]
            .notes
            .iter()
            .any(|n| n.contains("namespaced built-ins look like `ta.sma(...)`")),
        "diagnostics: {diagnostics:?}"
    );
    // A near-miss also gets a did-you-mean suggestion.
    let (_, diagnostics) = check("indicator(\"TA\")\nplot(ta.smma(close, 2))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0202"]);
    assert!(
        diagnostics[0].notes.iter().any(|n| n.contains("ta.sma")),
        "diagnostics: {diagnostics:?}"
    );
}

#[test]
fn unknown_dotted_identifier_is_e0201_with_namespace_note() {
    let (_, diagnostics) = check("indicator(\"TA\")\nplot(x.y)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0201"]);
    assert!(
        diagnostics[0]
            .notes
            .iter()
            .any(|n| n.contains("field access is not supported")),
        "diagnostics: {diagnostics:?}"
    );
}

#[test]
fn ta_function_name_without_call_is_e0216() {
    let (_, diagnostics) = check("indicator(\"TA\")\nx = ta.sma\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0216"]);
}

#[test]
fn ta_call_inside_a_branch_warns_w0004() {
    let (_, diagnostics) =
        check("indicator(\"TA\")\nx = 0.0\nif close > open\n    x := ta.sma(close, 2)\nplot(x)\n");
    assert!(
        !has_errors(&diagnostics),
        "unexpected errors: {diagnostics:?}"
    );
    let w0004: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.code.as_deref() == Some("W0004"))
        .collect();
    assert_eq!(w0004.len(), 1, "diagnostics: {diagnostics:?}");
    assert!(
        w0004[0]
            .message
            .contains("should be executed on every bar for consistent results"),
        "diagnostics: {diagnostics:?}"
    );
}

#[test]
fn ta_call_under_run_stable_input_condition_does_not_warn_w0004() {
    let (_, diagnostics) = check(
        "indicator(\"TA\")\nenabled = input.bool(true)\nx = 0.0\nif enabled\n    x := ta.sma(close, 2)\nplot(x)\n",
    );
    assert!(
        !diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code.as_deref() == Some("W0004")),
        "unexpected W0004: {diagnostics:?}"
    );
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
}

#[test]
fn ta_call_at_top_level_does_not_warn_w0004() {
    let (_, diagnostics) = check("indicator(\"TA\")\nplot(ta.sma(close, 2))\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
}

#[test]
fn ta_call_in_var_initializer_warns_w0004() {
    let (_, diagnostics) = check("indicator(\"TA\")\nvar x = ta.sma(close, 2)\nplot(x)\n");
    assert!(
        !has_errors(&diagnostics),
        "unexpected errors: {diagnostics:?}"
    );
    let w0004: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.code.as_deref() == Some("W0004"))
        .collect();
    assert_eq!(w0004.len(), 1, "diagnostics: {diagnostics:?}");
    assert!(
        w0004[0].message.contains(
            "executes only once; the series it records will be permanently `na` after the first bar"
        ),
        "diagnostics: {diagnostics:?}"
    );
}

#[test]
fn ta_call_in_non_var_initializer_does_not_warn_w0004() {
    let (_, diagnostics) = check("indicator(\"TA\")\nx = ta.sma(close, 2)\nplot(x)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
}

#[test]
fn ta_call_in_var_initializer_inside_a_branch_warns_w0004_once() {
    // Initialize-once subsumes the branch-gap case: a `var` initializer
    // inside a branch still executes only once, so exactly one W0004 fires
    // and it carries the run-once wording.
    let (_, diagnostics) =
        check("indicator(\"TA\")\nif close > open\n    var x = ta.sma(close, 2)\n");
    assert!(
        !has_errors(&diagnostics),
        "unexpected errors: {diagnostics:?}"
    );
    let w0004: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.code.as_deref() == Some("W0004"))
        .collect();
    assert_eq!(w0004.len(), 1, "diagnostics: {diagnostics:?}");
    assert!(
        w0004[0].message.contains("executes only once"),
        "diagnostics: {diagnostics:?}"
    );
}

#[test]
fn discarded_ta_call_warns_w0002() {
    let (_, diagnostics) = check("indicator(\"TA\")\nta.change(close)\nplot(close)\n");
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    assert!(diagnostics
        .iter()
        .any(|d| d.code.as_deref() == Some("W0002")));
}

#[test]
fn plot_title_accepts_parenthesized_literal() {
    let (dump, diagnostics) = check("indicator(\"X\")\nplot(close, (\"Paren\"))\n");
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    assert!(
        dump.contains("plot slot 0 title=\"Paren\""),
        "dump:\n{dump}"
    );
}

// -- pure builtins (math.*, na, nz), namespaced constants, the na literal --------

#[test]
fn math_builtins_hir_snapshot() {
    let (dump, diagnostics) = check(
        "indicator(\"M\")\nplot(math.sqrt(close))\nplot(math.min(1, 2, bar_index))\nplot(nz(close[1]))\n",
    );
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    insta::assert_snapshot!(dump);
}

#[test]
fn math_constants_resolve_as_const_float() {
    let (dump, diagnostics) =
        check("indicator(\"C\")\nplot(math.pi)\nplot(math.e)\nplot(math.phi)\nplot(math.rphi)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    // Constants lower to plain float constants, const-qualified.
    assert!(
        dump.contains(&format!(
            "ConstFloat {} : const float",
            std::f64::consts::PI
        )),
        "dump:\n{dump}"
    );
    assert!(
        dump.contains(&format!("ConstFloat {} : const float", std::f64::consts::E)),
        "dump:\n{dump}"
    );
}

#[test]
fn history_of_a_math_constant_is_rejected() {
    let (_, diagnostics) = check("indicator(\"C\")\nplot(math.pi[1])\n");
    assert!(error_codes(&diagnostics).contains(&"E0209"));
}

#[test]
fn calling_a_math_constant_is_rejected() {
    let (_, diagnostics) = check("indicator(\"C\")\nplot(math.pi(2))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0203"]);
}

#[test]
fn unknown_math_function_gets_a_suggestion() {
    let (_, diagnostics) = check("indicator(\"C\")\nplot(math.sqr(4))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0202"]);
    assert!(
        diagnostics[0].notes.iter().any(|n| n.contains("math.sqrt")),
        "diagnostics: {diagnostics:?}"
    );
}

#[test]
fn pure_int_preservation_and_float_results() {
    // abs/sign preserve the numeric type; floor/ceil/round return int;
    // sqrt is always float; min/max are int only when every arg is int.
    let (dump, diagnostics) = check(
        "indicator(\"T\")\na = math.abs(-3)\nb = math.abs(-3.5)\nc = math.floor(2.5)\nd = math.min(1, 2)\ne = math.min(1, 2.5)\nf = math.sqrt(4)\ng = math.round(2.5)\nh = math.round(2.567, 2)\nplot(a)\nplot(b)\nplot(c)\nplot(d)\nplot(e)\nplot(f)\nplot(g)\nplot(h)\n",
    );
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(dump.contains("symbol 0 `a`: const int"), "dump:\n{dump}");
    assert!(dump.contains("symbol 1 `b`: const float"), "dump:\n{dump}");
    assert!(dump.contains("symbol 2 `c`: const int"), "dump:\n{dump}");
    assert!(dump.contains("symbol 3 `d`: const int"), "dump:\n{dump}");
    assert!(dump.contains("symbol 4 `e`: const float"), "dump:\n{dump}");
    assert!(dump.contains("symbol 5 `f`: const float"), "dump:\n{dump}");
    assert!(dump.contains("symbol 6 `g`: const int"), "dump:\n{dump}");
    assert!(dump.contains("symbol 7 `h`: const float"), "dump:\n{dump}");
}

#[test]
fn math_round_rejects_statically_known_out_of_range_precision() {
    // math.round() rejects a literal negative precision at compile time with a
    // value-specific message. The message names the value, the parameter, the
    // function and the `[0, 100]` range.
    let source = "indicator(\"P\")\nplot(math.round(1234.5678, -2))\n";
    let (_, diagnostics) = check(source);
    let diagnostic = diagnostic_with_code(&diagnostics, "E0248");
    assert_eq!(
        diagnostic.message,
        "Invalid value \"-2\" for \"precision\" parameter of the \"math.round()\" function. It must be between 0 and 100"
    );
    // The diagnostic points at the offending precision argument, not the call.
    assert_diagnostic_source(source, diagnostic, "-2", 2, 28);

    // Above the range is rejected too, with the actual value echoed back.
    let (_, over) = check("indicator(\"P\")\nplot(math.round(1.0, 101))\n");
    assert_eq!(
        diagnostic_with_code(&over, "E0248").message,
        "Invalid value \"101\" for \"precision\" parameter of the \"math.round()\" function. It must be between 0 and 100"
    );

    // A const-folded negative (parenthesized / via a const binding) is caught
    // the same way a bare literal is.
    assert!(
        error_codes(&check("indicator(\"P\")\nplot(math.round(1.0, -(1)))\n").1).contains(&"E0248")
    );
    assert!(
        error_codes(&check("indicator(\"P\")\np = -3\nplot(math.round(1.0, p))\n").1)
            .contains(&"E0248")
    );

    // The check indexes the *bound* argument list, so a named `precision` is
    // caught wherever it sits in the call.
    assert!(
        error_codes(&check("indicator(\"P\")\nplot(math.round(1.0, precision = -2))\n").1)
            .contains(&"E0248")
    );
    assert!(error_codes(
        &check("indicator(\"P\")\nplot(math.round(precision = -2, number = 1.0))\n").1
    )
    .contains(&"E0248"));
}

#[test]
fn math_round_accepts_in_range_and_runtime_precision() {
    // In-range literals compile clean, and a *runtime-computed* precision is
    // never rejected here (it is clamped at evaluation, not at compile time)
    // even when it can evaluate negative.
    for source in [
        "indicator(\"P\")\nplot(math.round(1.0, 0))\n",
        "indicator(\"P\")\nplot(math.round(1.0, 100))\n",
        "indicator(\"P\")\nplot(math.round(1.0, math.floor(-1.5)))\n",
        // An `input.int` default is NOT a compile-time precision: the host can
        // override it, so an out-of-range default must reach the runtime clamp.
        // A declaration stamps the default into `Symbol::const_int` (see
        // `analyze_declare`), so the alias form is the one at risk of a false
        // E0248 — both spellings must agree.
        "indicator(\"P\")\np = input.int(-1)\nplot(math.round(1.0, p))\n",
        "indicator(\"P\")\nplot(math.round(1.0, input.int(-1)))\n",
        "indicator(\"P\")\np = input.int(101)\nplot(math.round(1.0, p))\n",
    ] {
        let (_, diagnostics) = check(source);
        assert!(
            !error_codes(&diagnostics).contains(&"E0248"),
            "unexpected E0248 for `{source}`: {diagnostics:?}"
        );
    }
}

#[test]
fn pure_qualifier_joins_arguments_and_random_is_series() {
    let (dump, diagnostics) = check(
        "indicator(\"Q\")\nk = math.abs(-3)\ns = math.abs(close)\nr = math.random()\nplot(k)\nplot(s)\nplot(r)\n",
    );
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(dump.contains("symbol 0 `k`: const int"), "dump:\n{dump}");
    assert!(dump.contains("symbol 1 `s`: series float"), "dump:\n{dump}");
    // A random draw differs per execution even with const arguments.
    assert!(dump.contains("symbol 2 `r`: series float"), "dump:\n{dump}");
    // The omitted bounds are materialized as explicit 0.0 / 1.0 arguments.
    assert!(
        dump.contains("CallPure math.random argc=2"),
        "dump:\n{dump}"
    );
}

#[test]
fn pure_argument_count_violations_are_e0204() {
    for source in [
        "indicator(\"A\")\nplot(math.sqrt())\n",
        "indicator(\"A\")\nplot(math.sqrt(1, 2))\n",
        "indicator(\"A\")\nplot(math.min(1))\n",
        "indicator(\"A\")\nplot(math.min(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11))\n",
        "indicator(\"A\")\nplot(math.random(0, 1, 2, 3))\n",
        "indicator(\"A\")\nx = na()\nplot(close)\n",
    ] {
        let (_, diagnostics) = check(source);
        assert_eq!(
            error_codes(&diagnostics),
            vec!["E0204"],
            "source: {source:?} diagnostics: {diagnostics:?}"
        );
    }
}

#[test]
fn pure_argument_type_violations_are_e0205() {
    // Non-numeric argument to a numeric parameter.
    let (_, diagnostics) = check("indicator(\"T\")\nplot(math.sqrt(\"x\"))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0205"]);
    // Float seed: the seed parameter is declared int (no coercion).
    let (_, diagnostics) = check("indicator(\"T\")\nplot(math.random(0, 1, 1.5))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0205"]);
    // String source for nz (int, float, or bool only).
    let (_, diagnostics) = check("indicator(\"T\")\nx = nz(\"s\")\nplot(close)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0205"]);
}

#[test]
fn nz_replacement_type_mismatch_is_e0221() {
    for call in ["nz(color.red, 1)", "nz(close, color.red)"] {
        let (_, diagnostics) = check(&format!("indicator(\"N\")\nx = {call}\nplot(close)\n"));
        assert_eq!(error_codes(&diagnostics), vec!["E0221"]);
    }
    for call in [
        "nz(true)",
        "nz(close > open, 1)",
        "nz(close, false)",
        "nz(close, \"oops\")",
    ] {
        let (_, diagnostics) = check(&format!("indicator(\"N\")\nx = {call}\nplot(close)\n"));
        assert_eq!(error_codes(&diagnostics), vec!["E0205"]);
    }
}

#[test]
fn nz_default_replacement_is_typed_zero() {
    // nz(x) materializes 0 / 0.0 / transparent black to match the source type.
    let (dump, diagnostics) = check(
        "indicator(\"N\")\ni = bar_index\nf = close\nx = nz(i[1])\ny = nz(f[1])\nplot(x)\nplot(y)\n",
    );
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(dump.contains("symbol 2 `x`: series int"), "dump:\n{dump}");
    assert!(dump.contains("symbol 3 `y`: series float"), "dump:\n{dump}");
    assert!(dump.contains("ConstInt 0 : const int"), "dump:\n{dump}");
    assert!(dump.contains("ConstFloat 0 : const float"), "dump:\n{dump}");
}

#[test]
fn nz_mixed_numeric_widens_to_float() {
    let (dump, diagnostics) = check("indicator(\"N\")\nx = nz(close, 1)\nplot(x)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(dump.contains("symbol 0 `x`: series float"), "dump:\n{dump}");
    assert!(dump.contains("IntToFloat : const float"), "dump:\n{dump}");
    // Int source with float replacement widens the source side.
    let (dump, diagnostics) = check("indicator(\"N\")\nx = nz(bar_index[1], 0.5)\nplot(x)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(dump.contains("symbol 0 `x`: series float"), "dump:\n{dump}");
}

#[test]
fn na_function_accepts_nullable_scalar_types_and_returns_bool() {
    let (dump, diagnostics) = check(
        "indicator(\"NA\")\ns = \"x\"\na = na(close)\nb = na(bar_index)\nc = na(color.red)\nd = na(s[1])\nplot(a or b or c or d ? 1 : 0)\n",
    );
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(dump.contains("symbol 1 `a`: series bool"), "dump:\n{dump}");
    assert!(dump.contains("symbol 4 `d`: series bool"), "dump:\n{dump}");
}

#[test]
fn na_of_an_effect_call_is_rejected() {
    // The effect call itself is the error (E0212); the pure-call check
    // suppresses the cascade instead of piling E0221 on top.
    let (_, diagnostics) = check("indicator(\"NA\")\nx = na(bgcolor(color.red))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0212"]);
}

// -- the ternary conditional operator ---------------------------------------------

#[test]
fn ternary_hir_snapshot() {
    // Identical branch types keep the type; qualifier joins cond and both
    // branches (series cond → series result).
    let (dump, diagnostics) = check("indicator(\"T\")\nx = close > open ? close : open\nplot(x)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(dump.contains("Ternary : series float"), "dump:\n{dump}");
    assert!(dump.contains("symbol 0 `x`: series float"), "dump:\n{dump}");
    insta::assert_snapshot!(dump);
}

#[test]
fn ternary_identical_int_branches_stay_int() {
    let (dump, diagnostics) = check("indicator(\"T\")\nx = 1 == 1 ? 2 : 3\nplot(x)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(dump.contains("Ternary : const int"), "dump:\n{dump}");
    assert!(dump.contains("symbol 0 `x`: const int"), "dump:\n{dump}");
}

#[test]
fn ternary_mixed_int_float_branches_unify_to_float_with_visible_coercion() {
    let (dump, diagnostics) = check("indicator(\"T\")\nx = close > open ? 1 : 0.5\nplot(x)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(dump.contains("Ternary : series float"), "dump:\n{dump}");
    // The int branch goes through an explicit coercion node.
    assert!(dump.contains("IntToFloat : const float"), "dump:\n{dump}");
}

#[test]
fn ternary_condition_must_be_bool() {
    let (_, diagnostics) = check("indicator(\"T\")\nx = close ? 1.0 : 2.0\nplot(x)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0207"]);
    assert!(
        diagnostics[0]
            .message
            .contains("`?:` condition must be bool"),
        "diagnostics: {diagnostics:?}"
    );
}

#[test]
fn ternary_branch_type_mismatch_is_e0207() {
    let (_, diagnostics) = check("indicator(\"T\")\nx = close > open ? 1.0 : \"s\"\nplot(close)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0207"]);
    assert!(
        diagnostics[0]
            .message
            .contains("`?:` branches have mismatched types"),
        "diagnostics: {diagnostics:?}"
    );
    // bool vs numeric is just as mismatched.
    let (_, diagnostics) = check("indicator(\"T\")\nx = close > open ? true : 1\nplot(close)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0207"]);
}

#[test]
fn ternary_na_branch_adopts_the_other_branch_type() {
    let (dump, diagnostics) = check("indicator(\"T\")\nx = close > 0 ? close : na\nplot(x)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(dump.contains("ConstNa : const float"), "dump:\n{dump}");
    assert!(dump.contains("Ternary : series float"), "dump:\n{dump}");
    // The na side works on the left too, and adopts int siblings exactly.
    let (dump, diagnostics) = check("indicator(\"T\")\nx = 1 == 1 ? na : 2\nplot(x)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(dump.contains("ConstNa : const int"), "dump:\n{dump}");
    assert!(dump.contains("symbol 0 `x`: const int"), "dump:\n{dump}");
}

#[test]
fn ternary_na_condition_is_rejected_in_v6() {
    let (_, diagnostics) = check("indicator(\"T\")\nx = na ? 1.0 : 2.0\nplot(x)\n");
    assert!(error_codes(&diagnostics).contains(&"E0220"));
}

#[test]
fn ternary_with_both_branches_na_uses_the_outer_expectation() {
    // In an argument position the parameter type flows into both branches
    // (like a parenthesized na)...
    let (dump, diagnostics) = check("indicator(\"T\")\nplot(close > 0 ? na : na)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(dump.contains("ConstNa : const float"), "dump:\n{dump}");
    // ...and a `:=` target's type does too.
    let (_, diagnostics) = check("indicator(\"T\")\nx = 1.0\nx := close > 0 ? na : na\nplot(x)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    // With no expectation, each na branch is E0220.
    let (_, diagnostics) = check("indicator(\"T\")\nx = close > 0 ? na : na\nplot(close)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0220", "E0220"]);
}

#[test]
fn ternary_qualifier_is_the_join_of_condition_and_branches() {
    // All-const ternaries stay const; a series condition alone makes the
    // result series even over const branches.
    let (dump, diagnostics) =
        check("indicator(\"T\")\nk = 1 == 1 ? 1.0 : 2.0\ns = close > open ? 1.0 : 2.0\nplot(k)\nplot(s)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(dump.contains("symbol 0 `k`: const float"), "dump:\n{dump}");
    assert!(dump.contains("symbol 1 `s`: series float"), "dump:\n{dump}");
}

#[test]
fn ternary_feeds_var_and_reassignment() {
    let (dump, diagnostics) =
        check("indicator(\"T\")\nvar n = 0.0\nn := close > open ? n + 1.0 : n\nplot(n)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(dump.contains("Ternary : series float"), "dump:\n{dump}");
    assert!(dump.contains("Reassign `n` (sym 0)"), "dump:\n{dump}");
}

#[test]
fn ternary_reassign_type_rule_still_applies() {
    // The unified branch type must match the := target's type.
    let (_, diagnostics) =
        check("indicator(\"T\")\nx = 1\nx := close > open ? 1.5 : 2.0\nplot(x)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0218"]);
}

#[test]
fn series_ternary_branch_history_sites_warn_w0004() {
    // Ternaries lower to real branches, so a history site or `ta.*`
    // call in an arm skips the bars where the other arm is selected, exactly
    // like an `if`/`switch` expression branch. The source-level stateful-call
    // warning W0009 still applies to the `ta.*` call.
    let (_, diagnostics) =
        check("indicator(\"T\")\nplot(close > open ? ta.sma(close, 2) : (close - open)[1])\n");
    let w0004 = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code.as_deref() == Some("W0004"))
        .count();
    assert_eq!(w0004, 2, "diagnostics: {diagnostics:?}");
    assert!(warning_codes(&diagnostics).contains(&"W0009"));
}

#[test]
fn run_stable_ternary_branch_does_not_warn_w0004() {
    // An input-controlled condition selects the same arm on every bar, so the
    // arm's history is not gapped (matching `if` branches).
    let (_, diagnostics) = check(
        "indicator(\"T\")\nuse_sma = input.bool(true)\nplot(use_sma ? ta.sma(close, 2) : (close - open)[1])\n",
    );
    assert!(
        !diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code.as_deref() == Some("W0004")),
        "unexpected W0004: {diagnostics:?}"
    );
}

#[test]
fn lazy_series_boolean_rhs_warns_w0008_with_actionable_text_and_span() {
    let source = "indicator(\"Lazy\")\nenabled = close > open\nsignal = enabled and ta.crossover(close, open)\nplot(signal ? 1.0 : 0.0)\n";
    let (_, diagnostics) = check(source);
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    let diagnostic = diagnostic_with_code(&diagnostics, "W0008");
    assert_eq!(diagnostic.severity, Severity::Warning);
    assert_eq!(
        diagnostic.message,
        "Stateful call `ta.crossover()` is guarded by a lazy boolean condition, so some bars can skip it and its history can drift"
    );
    assert_diagnostic_source(source, diagnostic, "ta.crossover(close, open)", 3, 22);
}

#[test]
fn lazy_series_boolean_or_rhs_warns_w0008_with_actionable_text_and_span() {
    // The `or` mirror of the `and` case above: a stateful call in the
    // lazily-evaluated rhs of `or` carries the same W0008 guidance.
    let source = "indicator(\"Lazy\")\nbearish = close < open\nsignal = bearish or ta.crossunder(close, open)\nplot(signal ? 1.0 : 0.0)\n";
    let (_, diagnostics) = check(source);
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    let diagnostic = diagnostic_with_code(&diagnostics, "W0008");
    assert_eq!(diagnostic.severity, Severity::Warning);
    assert_eq!(
        diagnostic.message,
        "Stateful call `ta.crossunder()` is guarded by a lazy boolean condition, so some bars can skip it and its history can drift"
    );
    assert_diagnostic_source(source, diagnostic, "ta.crossunder(close, open)", 3, 21);
}

#[test]
fn lazy_series_boolean_warning_generalizes_and_restores_nested_context() {
    let source = "indicator(\"Lazy\")\nenabled = close > open\na = enabled or (true and ta.crossunder(close, open))\nb = ta.crossover(close, open)\nplot(a ? 1.0 : 0.0)\n";
    let (_, diagnostics) = check(source);
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    let warnings: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code.as_deref() == Some("W0008"))
        .collect();
    assert_eq!(warnings.len(), 1, "diagnostics: {diagnostics:?}");
    assert!(warnings[0]
        .message
        .starts_with("Stateful call `ta.crossunder()` is guarded by a lazy boolean condition"));
    assert_diagnostic_source(source, warnings[0], "ta.crossunder(close, open)", 3, 26);
}

#[test]
fn lazy_boolean_negative_controls_do_not_warn_w0008() {
    let source = "indicator(\"Lazy\")\nenabled = close > open\na = true and ta.crossover(close, open)\nb = ta.crossunder(close, open) and enabled\nc = enabled and (close > math.max(open, low))\ncrossed = ta.crossover(close, open)\nd = enabled and crossed\nplot(d ? 1.0 : 0.0)\n";
    let (_, diagnostics) = check(source);
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    assert!(
        !warning_codes(&diagnostics).contains(&"W0008"),
        "diagnostics: {diagnostics:?}"
    );
}

#[test]
fn series_ternary_branch_warns_w0009_with_actionable_text_and_span() {
    let source = "indicator(\"Ternary\")\nenabled = close > open\nsignal = enabled ? ta.crossover(close, open) : false\nplot(signal ? 1.0 : 0.0)\n";
    let (_, diagnostics) = check(source);
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    let diagnostic = diagnostic_with_code(&diagnostics, "W0009");
    assert_eq!(diagnostic.severity, Severity::Warning);
    assert_eq!(
        diagnostic.message,
        "Stateful call `ta.crossover()` appears in a conditional branch, so its history can advance unevenly"
    );
    assert_diagnostic_source(source, diagnostic, "ta.crossover(close, open)", 3, 20);
}

#[test]
fn ternary_warning_generalizes_and_restores_nested_context() {
    let source = "indicator(\"Ternary\")\nenabled = close > open\na = enabled ? (true ? ta.sma(close, 2) : 0.0) : 0.0\nb = ta.ema(close, 2)\nplot(a + b)\n";
    let (_, diagnostics) = check(source);
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    let warnings: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code.as_deref() == Some("W0009"))
        .collect();
    assert_eq!(warnings.len(), 1, "diagnostics: {diagnostics:?}");
    assert!(warnings[0]
        .message
        .starts_with("Stateful call `ta.sma()` appears in a conditional branch"));
    assert_diagnostic_source(source, warnings[0], "ta.sma(close, 2)", 3, 23);
}

#[test]
fn ternary_negative_controls_do_not_warn_w0009() {
    let source = "indicator(\"Ternary\")\nenabled = close > open\na = true ? ta.crossover(close, open) : false\nb = enabled ? math.max(close, open) : 0.0\ncrossed = ta.crossover(close, open)\nc = enabled ? crossed : false\nplot((a or c) ? b : 0.0)\n";
    let (_, diagnostics) = check(source);
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    assert!(
        !warning_codes(&diagnostics).contains(&"W0009"),
        "diagnostics: {diagnostics:?}"
    );
}

#[test]
fn ternary_inside_an_if_branch_still_warns_w0004_for_ta_calls() {
    // The enclosing `if` branch is what skips bars; the ternary does not
    // shield the ta call from the branch-gap warning.
    let (_, diagnostics) = check(
        "indicator(\"T\")\nx = 0.0\nif close > open\n    x := close > 0.0 ? ta.sma(close, 2) : close\nplot(x)\n",
    );
    assert!(
        !has_errors(&diagnostics),
        "unexpected errors: {diagnostics:?}"
    );
    assert!(
        diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("W0004")),
        "diagnostics: {diagnostics:?}"
    );
}

#[test]
fn ternary_of_void_effect_call_is_rejected() {
    // Effect calls inside expressions are E0212; the ternary suppresses the
    // cascade instead of piling on a branch-mismatch error.
    let (_, diagnostics) = check("indicator(\"T\")\nx = close > open ? bgcolor(color.red) : 1.0\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0212"]);
}

// -- the na literal ---------------------------------------------------------------

#[test]
fn na_literal_adopts_the_parameter_type() {
    let (dump, diagnostics) = check("indicator(\"L\")\nplot(na)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(dump.contains("ConstNa : const float"), "dump:\n{dump}");
}

#[test]
fn na_literal_adopts_the_sibling_operand_type() {
    let (dump, diagnostics) =
        check("indicator(\"L\")\na = close + na\nb = 1 + na\nc = na > close\nplot(a)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(dump.contains("symbol 0 `a`: series float"), "dump:\n{dump}");
    assert!(dump.contains("symbol 1 `b`: const int"), "dump:\n{dump}");
    assert!(dump.contains("symbol 2 `c`: series bool"), "dump:\n{dump}");
}

#[test]
fn na_literal_string_equality_types() {
    let (dump, diagnostics) =
        check("indicator(\"L\")\ns = \"x\"\nok = s == na\nplot(ok ? 1 : 0)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(dump.contains("ConstNa : const string"), "dump:\n{dump}");
}

#[test]
fn na_literal_adopts_the_reassign_target_type() {
    let (dump, diagnostics) = check("indicator(\"L\")\nx = 1.0\nx := na\nplot(x)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(dump.contains("ConstNa : const float"), "dump:\n{dump}");
}

#[test]
fn uninferable_na_literal_is_e0220() {
    // Plain declaration, var initializer, unary minus, if condition: no
    // expected type exists at any of these positions.
    for source in [
        "indicator(\"E\")\nx = na\nplot(close)\n",
        "indicator(\"E\")\nvar x = na\nplot(close)\n",
        "indicator(\"E\")\nx = -na\nplot(close)\n",
        "indicator(\"E\")\nif na\n    x = close\nplot(close)\n",
    ] {
        let (_, diagnostics) = check(source);
        assert_eq!(
            error_codes(&diagnostics),
            vec!["E0220"],
            "source: {source:?} diagnostics: {diagnostics:?}"
        );
        assert!(
            diagnostics
                .iter()
                .any(|d| d.notes.iter().any(|n| n.contains("type annotation"))),
            "source: {source:?} diagnostics: {diagnostics:?}"
        );
    }
}

#[test]
fn type_annotation_exact_match_types_the_variable() {
    // Each annotation whose initializer already has the matching type is
    // accepted with no diagnostics, and the initializer keeps its type.
    for (source, needle) in [
        (
            "indicator(\"A\")\nint n = 3\nplot(close)\n",
            "ConstInt 3 : const int",
        ),
        (
            "indicator(\"A\")\nfloat f = 1.5\nplot(close)\n",
            "ConstFloat 1.5 : const float",
        ),
        (
            "indicator(\"A\")\nbool ok = true\nplot(close)\n",
            "ConstBool true : const bool",
        ),
        (
            "indicator(\"A\")\nstring s = \"hi\"\nplot(close)\n",
            "ConstString \"hi\" : const string",
        ),
    ] {
        let (dump, diagnostics) = check(source);
        assert!(
            diagnostics.is_empty(),
            "source: {source:?} diagnostics: {diagnostics:?}"
        );
        assert!(
            dump.contains(needle),
            "expected {needle:?} in dump:\n{dump}"
        );
    }
}

#[test]
fn float_annotation_coerces_int_initializer() {
    // `float x = 3` is the one permitted widening: the int initializer is
    // wrapped in IntToFloat, and the declared type is float.
    let (dump, diagnostics) = check("indicator(\"A\")\nfloat x = 3\nplot(x)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(
        dump.contains("IntToFloat : const float"),
        "expected IntToFloat coercion in dump:\n{dump}"
    );
}

#[test]
fn annotation_with_na_initializer_types_the_variable() {
    // Initializing with na: the annotation supplies the type
    // the bare `na` literal adopts, so there is no E0220.
    for (source, needle) in [
        (
            "indicator(\"A\")\nfloat x = na\nplot(close)\n",
            "ConstNa : const float",
        ),
        (
            "indicator(\"A\")\nint n = na\nplot(close)\n",
            "ConstNa : const int",
        ),
        (
            "indicator(\"A\")\nstring s = na\nplot(close)\n",
            "ConstNa : const string",
        ),
    ] {
        let (dump, diagnostics) = check(source);
        assert!(
            diagnostics.is_empty(),
            "source: {source:?} diagnostics: {diagnostics:?}"
        );
        assert!(
            dump.contains(needle),
            "expected {needle:?} in dump:\n{dump}"
        );
    }
}

#[test]
fn annotation_declared_type_governs_later_reassignment() {
    // `float x = na` then `x := 2` widens the int to float (allowed). The
    // declared type is float, taken from the annotation, not from the na
    // initializer (which carries no runtime type of its own).
    let (dump, diagnostics) = check("indicator(\"A\")\nfloat x = na\nx := 2\nplot(x)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(
        dump.contains("IntToFloat : const float"),
        "reassignment should widen int to the declared float type:\n{dump}"
    );
}

#[test]
fn int_annotation_rejects_string_reassignment_via_declared_type() {
    // The annotated declared type (int) is what a later `:=` checks against:
    // assigning a string is E0218, proving the annotation set the type.
    let (_, diagnostics) = check("indicator(\"A\")\nint n = na\nn := \"x\"\nplot(close)\n");
    assert_eq!(
        error_codes(&diagnostics),
        vec!["E0218"],
        "diagnostics: {diagnostics:?}"
    );
}

#[test]
fn annotation_type_mismatch_is_e0226() {
    // Non-widening mismatches are E0226: int annotation with a float literal,
    // and any cross-category disagreement. (float←int is the only coercion.)
    for source in [
        "indicator(\"A\")\nint n = 1.5\nplot(close)\n",
        "indicator(\"A\")\nbool b = 1\nplot(close)\n",
        "indicator(\"A\")\nstring s = 3\nplot(close)\n",
        "indicator(\"A\")\nfloat f = true\nplot(close)\n",
        "indicator(\"A\")\nint n = \"x\"\nplot(close)\n",
    ] {
        let (_, diagnostics) = check(source);
        assert_eq!(
            error_codes(&diagnostics),
            vec!["E0226"],
            "source: {source:?} diagnostics: {diagnostics:?}"
        );
    }
}

#[test]
fn var_annotation_is_persistent_and_typed() {
    // `var float x = na` is a persistent, annotated declaration.
    let (dump, diagnostics) = check("indicator(\"A\")\nvar float x = na\nplot(close)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(
        dump.contains("VarAssign `x`"),
        "expected persistent VarAssign in dump:\n{dump}"
    );
    assert!(
        dump.contains("ConstNa : const float"),
        "expected na typed as float in dump:\n{dump}"
    );
}

#[test]
fn na_plus_na_reports_e0220_for_each_side() {
    let (_, diagnostics) = check("indicator(\"E\")\nx = na + na\nplot(close)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0220", "E0220"]);
}

#[test]
fn not_na_is_rejected_in_v6() {
    let (_, diagnostics) = check("indicator(\"L\")\nok = not na\nplot(ok ? 1 : 0)\n");
    assert!(error_codes(&diagnostics).contains(&"E0220"));
}

#[test]
fn na_literal_as_pure_argument_adopts_float() {
    // Numeric parameters expect float, so `nz(na)` and `math.sqrt(na)`
    // both type (and are permanently na at runtime).
    let (dump, diagnostics) = check("indicator(\"L\")\nplot(nz(na))\nplot(math.sqrt(na))\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(dump.contains("ConstNa : const float"), "dump:\n{dump}");
}

#[test]
fn reassigned_var_is_series_qualified() {
    // A recurrent persistent write is time-dependent, so compound history
    // is valid: the variable's qualifier is series.
    let (dump, diagnostics) =
        check("indicator(\"R\")\nvar k = 1.0\nk := k + 1.0\nplot((k + 0.0)[1])\n");
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    assert!(
        dump.contains("`k`: series float"),
        "reassigned var should be series-qualified:\n{dump}"
    );
}

// -- engine-backed stateful builtins (CALL_TA) ---------------------------------

#[test]
fn ta_engine_hir_snapshot() {
    // Engine calls become CallTaState nodes over a ta_states table:
    // ta.ema's length bakes into the state's params, valuewhen's condition
    // stays a bool operand, and highestbars is series int (coerced for the
    // plot).
    let (dump, diagnostics) = check(
        "indicator(\"Engine\")\nplot(ta.ema(close, 9))\nplot(ta.valuewhen(close > open, close, 1))\nplot(ta.highestbars(close, 5))\n",
    );
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    insta::assert_snapshot!(dump);
}

#[test]
fn valuewhen_preserves_integer_source_type_for_history_offsets() {
    let (dump, diagnostics) = check(
        "indicator(\"VW int\")\nint n = ta.valuewhen(bar_index % 2 == 0, bar_index, 0)\nplot(close[bar_index - n])\n",
    );
    assert!(error_codes(&diagnostics).is_empty(), "{diagnostics:?}");
    assert!(dump.contains("ta.valuewhen"), "{dump}");
    assert!(dump.contains("series int"), "{dump}");

    let (_, diagnostics) =
        check("indicator(\"VW float\")\nint n = ta.valuewhen(true, close, 0)\nplot(n)\n");
    assert!(!error_codes(&diagnostics).is_empty());
}

#[test]
fn fixnan_preserves_integer_source_type() {
    let (dump, diagnostics) = check(
        "indicator(\"fixnan int\")\nint x = fixnan(bar_index > 10 ? bar_index : na)\nplot(x)\n",
    );
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert!(dump.contains("series int"), "{dump}");

    let (_, diagnostics) = check("indicator(\"fixnan float\")\nint x = fixnan(close)\nplot(x)\n");
    assert!(!error_codes(&diagnostics).is_empty());
}

#[test]
fn ta_engine_state_ids_allocate_in_syntactic_order() {
    let parsed = pinewasm_parse::parse(
        "indicator(\"Order\")\nplot(ta.cum(close))\nplot(ta.ema(close, 4))\nplot(ta.stdev(close, 3))\n",
    );
    let result = analyze(&parsed.program);
    assert!(!has_errors(&result.diagnostics), "{:?}", result.diagnostics);
    let states = &result.hir.ta_states;
    assert_eq!(states.len(), 3);
    assert_eq!(states[0].func, pinewasm_hir::TaFunc::Cum);
    assert!(states[0].params.is_empty());
    assert_eq!(states[1].func, pinewasm_hir::TaFunc::Ema);
    assert_eq!(states[1].params, vec![4]);
    // stdev's optional `biased` flag defaults to true (population).
    assert_eq!(states[2].func, pinewasm_hir::TaFunc::Stdev);
    assert_eq!(states[2].params, vec![3, 1]);
}

#[test]
fn ta_engine_bakes_const_float_and_bool_params() {
    let parsed = pinewasm_parse::parse(
        "indicator(\"Bake\")\nplot(ta.alma(close, 9, 0.85, 6))\nplot(ta.tr(false))\nplot(ta.variance(close, 2, false))\n",
    );
    let result = analyze(&parsed.program);
    assert!(!has_errors(&result.diagnostics), "{:?}", result.diagnostics);
    let states = &result.hir.ta_states;
    // alma: length as-is, offset/sigma as f64 bit patterns, `floor` defaulted
    // to false.
    assert_eq!(
        states[0].params,
        vec![9, 0.85f64.to_bits() as i64, 6.0f64.to_bits() as i64, 0]
    );
    // tr(false) and variance(..., false) bake the flags as 0.
    assert_eq!(states[1].params, vec![0]);
    assert_eq!(states[2].params, vec![2, 0]);
}

#[test]
fn ta_engine_result_types_follow_the_data_table() {
    // crossover is series bool (usable as an if condition); barssince and
    // highestbars are series int (int arithmetic proves it).
    let (_, diagnostics) = check(
        "indicator(\"Ty\")\nif ta.crossover(close, open)\n    y = 1\nx = ta.barssince(close > open) % 2\nplot(x)\n",
    );
    assert!(
        !has_errors(&diagnostics),
        "unexpected errors: {diagnostics:?}"
    );
}

#[test]
fn ta_engine_arity_errors_are_e0204() {
    let (_, diagnostics) = check("indicator(\"TA\")\nplot(ta.ema(close))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0204"]);
    let (_, diagnostics) = check("indicator(\"TA\")\nplot(ta.alma(close, 9, 0.85))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0204"]);
    // stdev's biased flag is optional: 2 or 3 arguments both check.
    let (_, diagnostics) = check("indicator(\"TA\")\nplot(ta.stdev(close, 2))\n");
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
}

#[test]
fn ta_engine_operand_type_errors_are_e0205() {
    // A bool where a numeric source is required.
    let (_, diagnostics) = check("indicator(\"TA\")\nplot(ta.ema(close > open, 2))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0205"]);
    // A float where a bool condition is required.
    let (_, diagnostics) = check("indicator(\"TA\")\nplot(ta.barssince(close))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0205"]);
}

#[test]
fn ta_engine_const_argument_violations_are_e0219() {
    // Window length rules match the slot-based ta functions.
    let (_, diagnostics) = check("indicator(\"TA\")\nplot(ta.ema(close, 0))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0219"]);
    // hma needs length >= 2 for its inner wma(length/2).
    let (_, diagnostics) = check("indicator(\"TA\")\nplot(ta.hma(close, 1))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0219"]);
    assert!(
        diagnostics[0].message.contains("must be at least 2"),
        "diagnostics: {diagnostics:?}"
    );
    // A series-valued length (genuinely non-const) is rejected.
    let (_, diagnostics) = check("indicator(\"TA\")\nn = bar_index\nplot(ta.rsi(close, n))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0219"]);
    // valuewhen's occurrence must be >= 0.
    let (_, diagnostics) = check("indicator(\"TA\")\nplot(ta.valuewhen(close > 0, close, -1))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0219"]);
    // alma's sigma must be a positive number literal.
    let (_, diagnostics) = check("indicator(\"TA\")\nplot(ta.alma(close, 9, 0.85, 0))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0219"]);
    assert!(
        diagnostics[0].message.contains("greater than zero"),
        "diagnostics: {diagnostics:?}"
    );
    // alma's offset must be a number literal.
    let (_, diagnostics) = check("indicator(\"TA\")\nplot(ta.alma(close, 9, close, 6))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0219"]);
    // tr's handle_na must be a bool literal.
    let (_, diagnostics) = check("indicator(\"TA\")\nb = true\nplot(ta.tr(b))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0219"]);
    assert!(
        diagnostics[0].message.contains("must be `true` or `false`"),
        "diagnostics: {diagnostics:?}"
    );
}

#[test]
fn extrema_accept_series_integer_lengths_without_weakening_ema() {
    let (dump, diagnostics) = check(
        "indicator(\"Dynamic extrema\")\nn = bar_index % 3 + 1\nplot(ta.highest(close, n))\nplot(ta.lowest(n))\n",
    );
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    assert!(dump.contains("TaCallDynamic ta.highest"));
    assert!(dump.contains("TaCallDynamic ta.lowest"));
    let (_, diagnostics) =
        check("indicator(\"Still simple\")\nn = bar_index % 3 + 1\nplot(ta.ema(close, n))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0219"]);
}

#[test]
fn ta_engine_call_inside_a_branch_warns_w0004() {
    let (_, diagnostics) =
        check("indicator(\"TA\")\nx = 0.0\nif close > open\n    x := ta.ema(close, 2)\nplot(x)\n");
    assert!(
        !has_errors(&diagnostics),
        "unexpected errors: {diagnostics:?}"
    );
    let w0004: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.code.as_deref() == Some("W0004"))
        .collect();
    assert_eq!(w0004.len(), 1, "diagnostics: {diagnostics:?}");
    assert!(
        w0004[0]
            .message
            .contains("should be executed on every bar for consistent results"),
        "diagnostics: {diagnostics:?}"
    );
}

#[test]
fn ta_engine_call_in_var_initializer_warns_w0004() {
    let (_, diagnostics) = check("indicator(\"TA\")\nvar x = ta.cum(close)\nplot(x)\n");
    assert!(
        !has_errors(&diagnostics),
        "unexpected errors: {diagnostics:?}"
    );
    let w0004: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.code.as_deref() == Some("W0004"))
        .collect();
    assert_eq!(w0004.len(), 1, "diagnostics: {diagnostics:?}");
    assert!(
        w0004[0].message.contains("executes only once"),
        "diagnostics: {diagnostics:?}"
    );
}

#[test]
fn ta_engine_na_source_adopts_float() {
    // The na literal in a source position adopts float, like other builtin
    // arguments; fixnan(na) is degenerate but well-typed.
    let (_, diagnostics) = check("indicator(\"TA\")\nplot(fixnan(na))\n");
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
}

#[test]
fn ta_variable_style_builtins_resolve_as_bare_names() {
    // Zero-signature engine functions (`ta.obv`, `ta.vwap`, ...) are
    // referenced like variables; each mention is its own call site.
    let (dump, diagnostics) = check(
        "indicator(\"V\")\nplot(ta.obv)\nplot(ta.vwap)\nplot(ta.accdist + ta.iii)\nplot(ta.nvi - ta.pvi)\nplot(ta.pvt + ta.wad + ta.wvad)\n",
    );
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    assert!(dump.contains("CallTaState ta.obv state0"), "dump:\n{dump}");
    assert!(dump.contains("ta.vwap state1"), "dump:\n{dump}");

    // History references materialize the variable's series.
    let (dump, diagnostics) = check("indicator(\"V\")\nplot(ta.obv[2])\n");
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    assert!(dump.contains("MaterializeHistory"), "dump:\n{dump}");

    // Passing arguments to a variable-style builtin is an arity error.
    let (_, diagnostics) = check("indicator(\"V\")\nplot(ta.obv(close))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0204"]);

    // Functions with parameters still demand a call (E0216).
    let (_, diagnostics) = check("indicator(\"V\")\nplot(ta.wpr)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0216"]);
}

#[test]
fn ta_vwap_overloads_allocate_the_expected_engine_states() {
    let parsed = parse("indicator(\"VW\")\nplot(ta.vwap(close))\n");
    let result = analyze(&parsed.program);
    assert!(
        !has_errors(&result.diagnostics),
        "diagnostics: {:?}",
        result.diagnostics
    );
    assert_eq!(result.hir.ta_states.len(), 1);
    assert_eq!(
        result.hir.ta_states[0].func,
        pinewasm_hir::TaFunc::VwapSource
    );

    let parsed = parse("indicator(\"VW\")\nplot(ta.vwap(close, bar_index % 2 == 0))\n");
    let result = analyze(&parsed.program);
    assert!(
        !has_errors(&result.diagnostics),
        "diagnostics: {:?}",
        result.diagnostics
    );
    assert_eq!(result.hir.ta_states.len(), 1);
    assert_eq!(
        result.hir.ta_states[0].func,
        pinewasm_hir::TaFunc::VwapAnchored
    );

    let parsed = parse(
        "indicator(\"VW\")\n\
         [v, upper, lower] = ta.vwap(close, bar_index % 2 == 0, 2.0)\n\
         plot(v)\nplot(upper)\nplot(lower)\n",
    );
    let result = analyze(&parsed.program);
    assert!(
        !has_errors(&result.diagnostics),
        "diagnostics: {:?}",
        result.diagnostics
    );
    assert_eq!(result.hir.ta_states.len(), 1);
    assert_eq!(
        result.hir.ta_states[0].func,
        pinewasm_hir::TaFunc::VwapBands
    );
}

#[test]
fn ta_vwap_band_overload_requires_destructuring_and_bool_anchor() {
    let (_, diagnostics) = check("indicator(\"VW\")\nplot(ta.vwap(close, true, 2.0))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0225"]);

    let (_, diagnostics) = check("indicator(\"VW\")\nplot(ta.vwap(close, close))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0205"]);
}

#[test]
fn ta_pivot_source_defaults_and_arity() {
    // Both the defaulted-source and explicit-source forms typecheck.
    let (dump, diagnostics) =
        check("indicator(\"P\")\nplot(ta.pivothigh(2, 2))\nplot(ta.pivotlow(close, 2, 2))\n");
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    // The omitted source materializes as a read of `high`.
    assert!(dump.contains("ReadBuiltin high"), "dump:\n{dump}");

    // Too few / too many arguments are E0204 with the 2-to-3 range.
    let (_, diagnostics) = check("indicator(\"P\")\nplot(ta.pivothigh(2))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0204"]);
    assert!(
        diagnostics[0].message.contains("2 to 3"),
        "diagnostics: {diagnostics:?}"
    );
    let (_, diagnostics) = check("indicator(\"P\")\nplot(ta.pivotlow(close, 2, 2, 2))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0204"]);

    // Zero is accepted on either side, including a one-cell window.
    let (_, diagnostics) = check(
        "indicator(\"P\")\nplot(ta.pivothigh(close, 0, 2))\nplot(ta.pivotlow(close, 2, 0))\nplot(ta.pivotlow(close, 0, 0))\n",
    );
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    let (_, diagnostics) = check("indicator(\"P\")\nplot(ta.pivothigh(close, -1, 2))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0219"]);
}

#[test]
fn ta_extended_batch_const_argument_violations_are_e0219() {
    // Percentages are number literals within 0..=100.
    let (_, diagnostics) =
        check("indicator(\"TA\")\nplot(ta.percentile_linear_interpolation(close, 5, 150))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0219"]);
    assert!(
        diagnostics[0].message.contains("between 0 and 100"),
        "diagnostics: {diagnostics:?}"
    );
    let (_, diagnostics) =
        check("indicator(\"TA\")\nplot(ta.percentile_nearest_rank(close, 5, -1))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0219"]);
    let (_, diagnostics) =
        check("indicator(\"TA\")\np = bar_index\nplot(ta.percentile_nearest_rank(close, 5, p))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0219"]);

    // ta.sar's start/inc/max must be positive number literals.
    let (_, diagnostics) = check("indicator(\"TA\")\nplot(ta.sar(0, 0.02, 0.2))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0219"]);

    // ta.tsi's two lengths follow the int-literal window rule.
    let (_, diagnostics) = check("indicator(\"TA\")\nn = bar_index\nplot(ta.tsi(close, n, 25))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0219"]);

    // ta.kcw's useTrueRange is a bool literal.
    let (_, diagnostics) = check("indicator(\"TA\")\nplot(ta.kcw(close, 5, 2, 1))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0219"]);
}

#[test]
fn ta_extended_batch_operand_type_errors_are_e0205() {
    // A bool where ta.stoch's numeric source is required.
    let (_, diagnostics) = check("indicator(\"TA\")\nplot(ta.stoch(close > open, high, low, 2))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0205"]);
    // Both ta.correlation sources are numeric.
    let (_, diagnostics) =
        check("indicator(\"TA\")\nplot(ta.correlation(close, close > open, 2))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0205"]);
}

#[test]
fn ta_rising_falling_are_series_bool() {
    // The results are bool: they need a ternary to plot, and using one as
    // a number is a type error.
    let (_, diagnostics) =
        check("indicator(\"TA\")\nplot(ta.rising(close, 2) ? 1 : 0)\nplot(ta.falling(close, 2) ? 1 : 0)\n");
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    let (_, diagnostics) = check("indicator(\"TA\")\nplot(ta.rising(close, 2))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0205"]);
}

// -- loops -------------------------------------------------------------------

#[test]
fn for_loop_hir_snapshot() {
    let (dump, diagnostics) =
        check("indicator(\"L\")\nvar s = 0\ns := 0\nfor i = 1 to 10\n    s := s + i\nplot(s)\n");
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    insta::assert_snapshot!(dump);
}

#[test]
fn while_loop_hir_snapshot() {
    let (dump, diagnostics) =
        check("indicator(\"W\")\nvar i = 0\ni := 0\nwhile i < 5\n    i := i + 1\nplot(i)\n");
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    insta::assert_snapshot!(dump);
}

#[test]
fn for_loop_over_float_bounds_types_the_counter_float() {
    let (dump, diagnostics) = check(
        "indicator(\"F\")\nvar s = 0.0\ns := 0.0\nfor x = 1.0 to 2.0 by 0.5\n    s := s + x\nplot(s)\n",
    );
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    // The counter `x` is a float loop variable.
    assert!(
        dump.contains("For `x`") && dump.contains("float"),
        "dump: {dump}"
    );
}

#[test]
fn int_start_with_float_end_keeps_an_int_loop_counter() {
    let (_, diagnostics) = check("//@version=6\nindicator(\"mixed loop bounds\")\nupper = input.float(2.7)\na = array.from(5.0, 6.0, 7.0)\nsum = 0.0\nfor i = 0 to upper\n    sum += array.get(a, i) + close[i]\nplot(sum)\n");
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");

    // A float *start* produces a float counter, which is rejected as an array
    // index even though history accepts it.
    let (_, diagnostics) = check("//@version=6\nindicator(\"float start\")\na = array.from(5.0, 6.0, 7.0)\nsum = 0.0\nfor i = 0.5 to 2.5\n    sum += array.get(a, i)\nplot(sum)\n");
    assert!(has_errors(&diagnostics), "{diagnostics:?}");
}

#[test]
fn loop_counter_is_immutable() {
    // Reassigning the counter inside the body is E0217.
    let (_, diagnostics) = check("indicator(\"L\")\nfor i = 1 to 10\n    i := i + 1\n");
    assert!(error_codes(&diagnostics).contains(&"E0217"));
}

#[test]
fn step_zero_literal_compiles_for_runtime_error() {
    for step in ["0", "0.0", "-0"] {
        let source =
            format!("indicator(\"L\")\ns = 0\nfor i = 1 to 10 by {step}\n    s += 1\nplot(s)\n");
        let (_, diagnostics) = check(&source);
        assert!(error_codes(&diagnostics).is_empty(), "step: {step}");
    }
}

#[test]
fn while_condition_must_be_bool_e0208() {
    let (_, diagnostics) = check("indicator(\"W\")\nwhile close\n    plot(close)\n");
    assert!(error_codes(&diagnostics).contains(&"E0208"));
}

#[test]
fn for_bound_must_be_numeric_e0222() {
    let (_, diagnostics) = check("indicator(\"L\")\nfor i = 1 to true\n    plot(i)\n");
    assert!(error_codes(&diagnostics).contains(&"E0222"));
}

#[test]
fn break_and_continue_outside_a_loop_are_e0223() {
    let (_, diagnostics) = check("indicator(\"B\")\nbreak\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0223"]);
    let (_, diagnostics) = check("indicator(\"C\")\ncontinue\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0223"]);
    // Inside an `if` that is not inside a loop is still E0223.
    let (_, diagnostics) = check("indicator(\"B\")\nif close > open\n    break\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0223"]);
}

#[test]
fn break_and_continue_inside_a_loop_are_ok() {
    let (_, diagnostics) =
        check("indicator(\"B\")\nfor i = 1 to 10\n    if i == 5\n        break\n    continue\n");
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
}

#[test]
fn stateful_ta_in_a_loop_body_is_e0222() {
    // Engine-backed ta.ema inside a loop body.
    let (_, diagnostics) = check("indicator(\"L\")\nfor i = 1 to 10\n    x = ta.ema(close, 3)\n");
    assert!(error_codes(&diagnostics).contains(&"E0222"));
    // Slot-based ta.sma too.
    let (_, diagnostics) = check("indicator(\"L\")\nfor i = 1 to 10\n    x = ta.sma(close, 3)\n");
    assert!(error_codes(&diagnostics).contains(&"E0222"));
    // A variable-style engine builtin referenced by bare name.
    let (_, diagnostics) = check("indicator(\"L\")\nwhile close > 0\n    x = ta.obv\n");
    assert!(error_codes(&diagnostics).contains(&"E0222"));
}

#[test]
fn compound_history_in_a_loop_body_is_e0222() {
    let (_, diagnostics) = check("indicator(\"L\")\nfor i = 1 to 10\n    x = (close - open)[1]\n");
    assert!(error_codes(&diagnostics).contains(&"E0222"));
}

#[test]
fn nested_loops_are_ok_and_break_targets_the_innermost() {
    let (dump, diagnostics) = check(
        "indicator(\"N\")\nvar s = 0\ns := 0\nfor i = 1 to 3\n    for j = 1 to 3\n        s := s + 1\n        break\nplot(s)\n",
    );
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    // Two distinct counters, two nested For nodes.
    assert!(
        dump.contains("For `i`") && dump.contains("For `j`"),
        "dump: {dump}"
    );
}

#[test]
fn plain_ta_and_history_outside_a_loop_still_work() {
    // The E0222 strictness only applies inside loop bodies; the same
    // constructs are fine at global scope.
    let (_, diagnostics) =
        check("indicator(\"OK\")\nx = ta.sma(close, 3)\ny = (close - open)[1]\nplot(x)\nplot(y)\n");
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
}

// ---------------------------------------------------------------------------
// if / switch expressions
// ---------------------------------------------------------------------------

/// Collect warning codes (for the W0004 lazy-branch checks).
fn warning_codes(diagnostics: &[Diagnostic]) -> Vec<&str> {
    diagnostics
        .iter()
        .filter(|d| !d.is_error())
        .filter_map(|d| d.code.as_deref())
        .collect()
}

#[test]
fn if_expression_desugars_to_hidden_result_slot() {
    let (dump, diagnostics) =
        check("indicator(\"x\")\np = if close > open\n    close\nelse\n    open\nplot(p)\n");
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    // A synthetic result slot backs the if-expression; both branches store
    // into it and `p` reads it.
    assert!(
        dump.contains("symbol 0 `%h0`: series float"),
        "dump: {dump}"
    );
    assert!(dump.contains("If"), "dump: {dump}");
    assert!(dump.contains("ReadVar `%h0`[0]"), "dump: {dump}");
}

#[test]
fn input_controlled_value_structures_preserve_input_qualifiers() {
    for source in [
        "indicator(\"x\")\nchoose = input.bool(false)\np = if choose\n    1.0\nelse\n    2.0\nplot(p)\n",
        "indicator(\"x\")\nkind = input.string(\"A\")\np = switch kind\n    \"A\" => 1.0\n    => 2.0\nplot(p)\n",
    ] {
        let result = analyze_src(source);
        assert!(!has_errors(&result.diagnostics), "{:?}", result.diagnostics);
        let value = result
            .hir
            .symbols
            .iter()
            .find(|symbol| symbol.name == "p")
            .expect("result variable");
        assert_eq!(
            value.qualifier,
            pinewasm_hir::Qualifier::Input,
            "input-controlled selection is stable for the entire run"
        );
    }
}

#[test]
fn if_expression_na_branch_adopts_other_type() {
    let (dump, diagnostics) =
        check("indicator(\"x\")\np = if close > open\n    close\nelse\n    na\nplot(p)\n");
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    // The `na` else branch adopts the then-branch's float type.
    assert!(dump.contains("ConstNa : const float"), "dump: {dump}");
}

#[test]
fn if_expression_mismatched_branch_types_is_e0207() {
    let (_, diagnostics) =
        check("indicator(\"x\")\np = if close > open\n    true\nelse\n    1.0\nplot(p)\n");
    assert!(error_codes(&diagnostics).contains(&"E0207"));
}

#[test]
fn if_expression_non_bool_condition_is_e0208() {
    let (_, diagnostics) =
        check("indicator(\"x\")\np = if close\n    1.0\nelse\n    2.0\nplot(p)\n");
    assert!(error_codes(&diagnostics).contains(&"E0208"));
}

#[test]
fn if_expression_counted_loop_tail_returns_its_last_assignment() {
    let (_, diagnostics) = check(
        "indicator(\"x\")\np = if close > open\n    for i = 0 to 2\n        x = i\nelse\n    0.0\nplot(p)\n",
    );
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
}

#[test]
fn if_expression_assignment_tail_produces_a_value() {
    let (_, diagnostics) =
        check("indicator(\"x\")\np = if close > open\n    x = 1.0\nelse\n    0.0\nplot(p)\n");
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
}

#[test]
fn if_expression_ta_in_branch_warns_w0004() {
    // if-expressions are lazy (real blocks), so a ta call in a branch warns
    // like an if-statement branch — unlike the eager ternary.
    let (_, diagnostics) = check(
        "indicator(\"x\")\np = if close > open\n    ta.sma(close, 3)\nelse\n    0.0\nplot(p)\n",
    );
    assert!(warning_codes(&diagnostics).contains(&"W0004"));
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
}

#[test]
fn switch_subject_is_evaluated_once() {
    let (dump, diagnostics) = check(
        "indicator(\"x\")\np = switch bar_index\n    0 => 1.0\n    1 => 2.0\n    => 3.0\nplot(p)\n",
    );
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    // The subject is stored once into a hidden slot before the chain, then
    // each comparison reads that slot — a single subject evaluation.
    let subject_stores = dump.matches("ReadBuiltin bar_index[0]").count();
    assert_eq!(subject_stores, 1, "subject read more than once: {dump}");
}

#[test]
fn switch_expression_with_default_and_no_match() {
    let (dump, diagnostics) =
        check("indicator(\"x\")\np = switch bar_index\n    0 => 1.0\n    => 2.0\nplot(p)\n");
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    assert!(dump.contains("If"), "dump: {dump}");
}

#[test]
fn switch_expression_with_input_subject_does_not_warn_w0004() {
    let (_, diagnostics) = check(
        "indicator(\"x\")\nkind = input.string(\"EMA\")\np = switch kind\n    \"EMA\" => ta.ema(close, 2)\n    => ta.sma(close, 2)\nplot(p)\n",
    );
    assert!(
        !diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code.as_deref() == Some("W0004")),
        "unexpected W0004: {diagnostics:?}"
    );
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
}

#[test]
fn switch_expression_no_default_stores_na() {
    let (dump, diagnostics) =
        check("indicator(\"x\")\np = switch bar_index\n    0 => 1.0\nplot(p)\n");
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    // The unmatched (else) path stores `na` of the result type.
    assert!(dump.contains("ConstNa"), "dump: {dump}");
}

#[test]
fn subjectless_switch_arms_are_bool_conditions() {
    let (dump, diagnostics) =
        check("indicator(\"x\")\np = switch\n    close > open => 1.0\n    => 0.0\nplot(p)\n");
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    // No subject slot store; the arm condition is used directly.
    assert!(dump.contains("Binary `>`"), "dump: {dump}");
}

#[test]
fn subjectless_switch_non_bool_arm_is_e0208() {
    let (_, diagnostics) =
        check("indicator(\"x\")\np = switch\n    close => 1.0\n    => 0.0\nplot(p)\n");
    assert!(error_codes(&diagnostics).contains(&"E0208"));
}

#[test]
fn switch_label_not_comparable_to_subject_is_e0207() {
    let (_, diagnostics) =
        check("indicator(\"x\")\np = switch bar_index\n    true => 1.0\n    => 0.0\nplot(p)\n");
    assert!(error_codes(&diagnostics).contains(&"E0207"));
}

#[test]
fn switch_arms_mismatched_types_is_e0207() {
    let (_, diagnostics) =
        check("indicator(\"x\")\np = switch bar_index\n    0 => true\n    => 1.0\nplot(p)\n");
    assert!(error_codes(&diagnostics).contains(&"E0207"));
}

#[test]
fn switch_duplicate_default_is_e0225() {
    let (_, diagnostics) = check(
        "indicator(\"x\")\np = switch bar_index\n    0 => 1.0\n    => 2.0\n    => 3.0\nplot(p)\n",
    );
    assert!(error_codes(&diagnostics).contains(&"E0225"));
}

#[test]
fn switch_all_na_arms_without_expectation_is_e0220() {
    let (_, diagnostics) =
        check("indicator(\"x\")\np = switch bar_index\n    0 => na\n    => na\nplot(p)\n");
    assert!(error_codes(&diagnostics).contains(&"E0220"));
}

#[test]
fn switch_statement_desugars_to_if_chain() {
    let (dump, diagnostics) = check(
        "indicator(\"x\")\nvar float p = 0.0\nswitch bar_index\n    0 => p := 5.0\n    => p := 9.0\nplot(p)\n",
    );
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    assert!(dump.contains("If"), "dump: {dump}");
    // No result-slot read: a switch statement produces no value.
    assert!(dump.contains("Reassign `p`"), "dump: {dump}");
}

#[test]
fn nested_if_expression_in_branch() {
    let (dump, diagnostics) = check(
        "indicator(\"x\")\np = if close > open\n    if close > 11.5\n        1.0\n    else\n        2.0\nelse\n    3.0\nplot(p)\n",
    );
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    // Two result slots: the outer if and the nested inner if.
    assert!(
        dump.contains("`%h0`") && dump.contains("`%h1`"),
        "dump: {dump}"
    );
}

// ---------------------------------------------------------------------------
// User-defined functions (per-call-site instantiation)
// ---------------------------------------------------------------------------

/// Parse + analyze, returning the HIR dump and the **combined** parse and
/// analysis diagnostics. Unlike [`check`], this does not require the source to
/// parse cleanly, so it also covers parse-level errors (a nested definition).
fn check_all(source: &str) -> (String, Vec<Diagnostic>) {
    let parsed = parse(source);
    let result = analyze(&parsed.program);
    let mut diagnostics = parsed.diagnostics;
    diagnostics.extend(result.diagnostics);
    (dump_hir(&result.hir), diagnostics)
}

#[test]
fn single_line_function_inlines_at_the_call_site() {
    let (dump, diagnostics) = check("indicator(\"x\")\nadd(a, b) => a + b\nplot(add(close, 1))\n");
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    // No function or call node survives: the body is inlined as a `+` over the
    // two argument temps.
    assert!(dump.contains("Binary `+`"), "dump: {dump}");
    assert!(dump.contains("Plot slot 0"), "dump: {dump}");
}

#[test]
fn loop_tailed_function_can_be_called_for_its_effects() {
    let src = "indicator(\"x\")\nvar a = array.new_int()\nappend() =>\n    for i = 0 to 2\n        array.push(a, i)\nappend()\nplot(array.size(a))\n";
    let (_, diagnostics) = check(src);
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");

    let value_src = src.replace("append()\nplot", "x = append()\nplot");
    let (_, diagnostics) = check(&value_src);
    assert!(error_codes(&diagnostics).contains(&"E0227"));
}

#[test]
fn function_arguments_are_evaluated_once() {
    // `dbl(ta.sma(close, 2))` must allocate exactly ONE ta state: the argument
    // is evaluated once into a temp and the body reads that temp twice.
    let (dump, diagnostics) =
        check("indicator(\"x\")\ndbl(x) => x + x\nplot(dbl(ta.sma(close, 2)))\n");
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    assert_eq!(
        dump.matches("TaCall ta.sma").count(),
        1,
        "the argument's ta.sma must be evaluated exactly once: {dump}"
    );
}

#[test]
fn var_inside_a_function_gets_independent_state_per_call_site() {
    // Two `counter` call sites => two distinct `var c` slots.
    let (dump, diagnostics) = check(
        "indicator(\"x\")\ncounter(step) =>\n    var c = 0\n    c := c + step\n    c\nplot(counter(1))\nplot(counter(2))\n",
    );
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    assert_eq!(
        dump.matches("VarAssign `c`").count(),
        2,
        "each call site instantiates its own var counter: {dump}"
    );
}

#[test]
fn ta_call_inside_a_function_gets_independent_state_per_call_site() {
    let (dump, diagnostics) =
        check("indicator(\"x\")\nsm(s) => ta.sma(s, 2)\nplot(sm(close))\nplot(sm(open))\n");
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    assert_eq!(
        dump.matches("TaCall ta.sma").count(),
        2,
        "each call site instantiates its own ta.sma window: {dump}"
    );
}

#[test]
fn default_parameter_is_used_when_the_argument_is_omitted() {
    let (dump, diagnostics) = check(
        "indicator(\"x\")\nscale(x, factor = 2) => x * factor\nplot(scale(close))\nplot(scale(close, 3))\n",
    );
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
    // Both call sites expand the body; the first uses the default `2`.
    assert_eq!(dump.matches("Binary `*`").count(), 2, "dump: {dump}");
    assert!(dump.contains("ConstInt 2"), "default materialized: {dump}");
}

#[test]
fn direct_recursion_is_rejected() {
    let (_dump, diagnostics) = check("indicator(\"x\")\nf(x) => f(x) + 1\nplot(f(close))\n");
    assert_eq!(error_codes(&diagnostics), ["E0224"]);
}

#[test]
fn mutual_recursion_is_rejected() {
    let (_dump, diagnostics) =
        check("indicator(\"x\")\na(x) => b(x)\nb(x) => a(x)\nplot(a(close))\n");
    assert!(error_codes(&diagnostics).contains(&"E0224"));
}

#[test]
fn wrong_arity_too_few_is_rejected() {
    let (_dump, diagnostics) = check("indicator(\"x\")\nadd(a, b) => a + b\nplot(add(close))\n");
    assert_eq!(error_codes(&diagnostics), ["E0224"]);
}

#[test]
fn wrong_arity_too_many_is_rejected() {
    let (_dump, diagnostics) =
        check("indicator(\"x\")\nadd(a, b) => a + b\nplot(add(close, 1, 2))\n");
    assert_eq!(error_codes(&diagnostics), ["E0224"]);
}

#[test]
fn duplicate_parameter_is_rejected() {
    let (_dump, diagnostics) = check_all("indicator(\"x\")\nf(a, a) => a\n");
    assert!(error_codes(&diagnostics).contains(&"E0224"));
}

#[test]
fn nested_function_definition_is_rejected() {
    // A definition inside a block is E0224 (definitions are top-level only).
    let (_dump, diagnostics) =
        check_all("indicator(\"x\")\nif close > 0\n    g(x) => x\n    plot(g(close))\n");
    assert!(error_codes(&diagnostics).contains(&"E0224"));
}

#[test]
fn plot_inside_a_function_is_rejected() {
    let (_dump, diagnostics) =
        check("indicator(\"x\")\nf(x) =>\n    plot(x)\n    x\nplot(f(close))\n");
    assert!(error_codes(&diagnostics).contains(&"E0254"));
}

#[test]
fn deep_acyclic_call_chain_within_the_guard_compiles() {
    // A linear chain of 30 functions, each calling the next once, stays well
    // under the expansion node cap.
    let mut src = String::from("indicator(\"x\")\n");
    for i in 0..30 {
        src.push_str(&format!("f{i}(x) => f{}(x) + 1\n", i + 1));
    }
    src.push_str("f30(x) => x\nplot(f0(close))\n");
    let (_dump, diagnostics) = check(&src);
    assert!(!has_errors(&diagnostics), "diagnostics: {diagnostics:?}");
}

#[test]
fn expansion_guard_reports_a_structured_error_on_a_bomb() {
    // Each fN calls f(N+1) twice: 2^k expansion. The guard must trip with a
    // single structured E0224, not hang.
    let mut src = String::from("indicator(\"x\")\n");
    for i in 0..40 {
        src.push_str(&format!("f{i}(x) => f{}(x) + f{}(x)\n", i + 1, i + 1));
    }
    src.push_str("f40(x) => x\nplot(f0(close))\n");
    let (_dump, diagnostics) = check(&src);
    let errors = error_codes(&diagnostics);
    assert_eq!(errors, ["E0224"], "exactly one guard error: {errors:?}");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("expansion exceeded")),
        "structured guard message: {diagnostics:?}"
    );
}

#[test]
fn deep_linear_call_chain_is_a_structured_error_not_a_stack_overflow() {
    // A deep acyclic chain f0→f1→…→fN (each calls the next ONCE) inlines to
    // few statements — under the node budget — but recurses once per link
    // through the analyzer. Without a depth bound it
    // overflows the native stack; it must instead produce
    // a structured E0224.
    let mut src = String::from("indicator(\"x\")\n");
    for i in 0..300 {
        src.push_str(&format!("f{i}(x) => f{}(x)\n", i + 1));
    }
    src.push_str("f300(x) => x\nplot(f0(close))\n");
    let (_dump, diagnostics) = check(&src);
    let errors = error_codes(&diagnostics);
    assert_eq!(
        errors,
        ["E0224"],
        "exactly one structured guard error: {errors:?}"
    );
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("nesting exceeded")),
        "structured depth-guard message: {diagnostics:?}"
    );
}

#[test]
fn calling_a_stateful_function_from_a_branch_warns_w0004() {
    // The instantiation inherits the conditional context, so the ta.* site
    // inside the inlined body warns.
    let (_dump, diagnostics) = check(
        "indicator(\"x\")\nsm(s) => ta.sma(s, 2)\nx = 0.0\nif close > open\n    x := sm(close)\nplot(x)\n",
    );
    let warnings: Vec<&str> = diagnostics
        .iter()
        .filter(|d| !d.is_error())
        .filter_map(|d| d.code.as_deref())
        .collect();
    assert!(
        warnings.contains(&"W0004"),
        "expected W0004: {diagnostics:?}"
    );
}

#[test]
fn user_function_name_used_as_a_value_is_rejected() {
    let (_dump, diagnostics) = check("indicator(\"x\")\nadd(a, b) => a + b\nplot(add)\n");
    assert_eq!(error_codes(&diagnostics), ["E0216"]);
}

#[test]
fn loop_bindings_follow_the_builtin_shadowing_rule() {
    for source in [
        "indicator(\"F\")\ntotal = 0\nfor close = 1 to 1\n    total := int(close)\nplot(total)\n",
        "indicator(\"F\")\ndata = array.from(1.0)\nfor close in data\n    plot(close)\n",
    ] {
        let (_dump, diagnostics) = check(source);
        assert!(
            !error_codes(&diagnostics).contains(&"E0211"),
            "an unused built-in may be shadowed in {source:?}: {diagnostics:?}"
        );
    }
    let (_dump, diagnostics) = check(
        "indicator(\"F\")\ntotal = 0\nfor close = 1 to 1\n    total := int(close)\nplot(close)\n",
    );
    assert!(
        error_codes(&diagnostics).contains(&"E0211"),
        "{diagnostics:?}"
    );
}

#[test]
fn builtin_function_names_cannot_be_redefined() {
    let (_dump, diagnostics) = check("indicator(\"F\")\nplot() => 1\nplot(plot())\n");
    assert!(
        error_codes(&diagnostics).contains(&"E0211"),
        "expected a built-in redefinition error: {diagnostics:?}"
    );
}

#[test]
fn calling_a_stateful_function_at_top_level_does_not_warn() {
    let (_dump, diagnostics) = check("indicator(\"x\")\nsm(s) => ta.sma(s, 2)\nplot(sm(close))\n");
    assert!(
        !diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("W0004")),
        "no W0004 at top level: {diagnostics:?}"
    );
}

// -- tuples ------------------------------------------------------------------

#[test]
fn destructure_ta_tuple_declares_targets_and_emits_one_call() {
    let (dump, diagnostics) =
        check("indicator(\"T\")\n[m, s, h] = ta.macd(close, 12, 26, 9)\nplot(m)\nplot(h)\n");
    assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");
    // Three targets are declared, all series float.
    assert!(dump.contains("symbol 0 `m`: series float"));
    assert!(dump.contains("symbol 1 `s`: series float"));
    assert!(dump.contains("symbol 2 `h`: series float"));
    // A single Destructure over one tuple CallTaState (arity 3, params baked).
    assert!(dump.contains("Destructure [`m` (sym 0), `s` (sym 1), `h` (sym 2)]"));
    assert!(dump.contains("CallTaState ta.macd state0 params=[12, 26, 9]"));
}

#[test]
fn destructure_arity_too_few_targets_is_e0225() {
    // ta.bb is a 3-tuple; one target is a mismatch.
    let (_dump, diagnostics) =
        check("indicator(\"T\")\n[only] = ta.bb(close, 20, 2.0)\nplot(only)\n");
    assert_eq!(error_codes(&diagnostics), ["E0225"]);
}

#[test]
fn destructure_arity_too_many_targets_is_e0225() {
    let (_dump, diagnostics) =
        check("indicator(\"T\")\n[a, b, c, d] = ta.bb(close, 20, 2.0)\nplot(a)\n");
    assert_eq!(error_codes(&diagnostics), ["E0225"]);
}

#[test]
fn destructure_of_scalar_rhs_is_e0225() {
    // A scalar RHS is a 1-tuple: destructuring into two targets is a mismatch.
    let (_dump, diagnostics) = check("indicator(\"T\")\n[a, b] = close\nplot(a)\n");
    assert_eq!(error_codes(&diagnostics), ["E0225"]);
}

#[test]
fn tuple_ta_call_used_as_scalar_value_is_e0225() {
    let (_dump, diagnostics) = check("indicator(\"T\")\nx = ta.macd(close, 12, 26, 9)\nplot(x)\n");
    assert_eq!(error_codes(&diagnostics), ["E0225"]);
}

#[test]
fn bare_tuple_expression_outside_legal_position_is_e0225() {
    let (_dump, diagnostics) = check("indicator(\"T\")\nplot([close, open])\n");
    assert!(
        error_codes(&diagnostics).contains(&"E0225"),
        "expected E0225: {diagnostics:?}"
    );
}

#[test]
fn nested_tuple_inside_an_operand_is_e0225() {
    // The permission does not propagate into sub-expressions.
    let (_dump, diagnostics) =
        check("indicator(\"T\")\nf() => [close, open] + 1\n[a, b] = f()\nplot(a)\n");
    assert!(
        error_codes(&diagnostics).contains(&"E0225"),
        "expected E0225 for the nested tuple: {diagnostics:?}"
    );
}

#[test]
fn user_function_tuple_body_destructures_cleanly() {
    let (dump, diagnostics) = check(
        "indicator(\"T\")\n\
         pair(x) =>\n    \
             hi = x + 1.0\n    \
             lo = x - 1.0\n    \
             [hi, lo]\n\
         [a, b] = pair(close)\n\
         plot(a)\n\
         plot(b)\n",
    );
    assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");
    // The two targets are declared and bound to the two element expressions
    // (ordinary assigns, not a multi-push CallTa).
    assert!(dump.contains("`a`"));
    assert!(dump.contains("`b`"));
    assert!(
        !dump.contains("Destructure"),
        "user tuple lowers to plain assigns"
    );
}

#[test]
fn user_function_tuple_body_used_as_scalar_is_e0225() {
    let (_dump, diagnostics) = check(
        "indicator(\"T\")\n\
         pair(x) => [x + 1.0, x - 1.0]\n\
         y = pair(close)\n\
         plot(y)\n",
    );
    assert!(
        error_codes(&diagnostics).contains(&"E0225"),
        "expected E0225: {diagnostics:?}"
    );
}

#[test]
fn destructure_redeclaration_is_e0211() {
    let (_dump, diagnostics) =
        check("indicator(\"T\")\na = close\n[a, b, c] = ta.bb(close, 5, 1.0)\nplot(b)\n");
    assert!(
        error_codes(&diagnostics).contains(&"E0211"),
        "expected E0211: {diagnostics:?}"
    );
}

#[test]
fn destructure_bad_length_is_e0219() {
    // Window lengths must be int literals, even for tuple functions.
    let (_dump, diagnostics) =
        check("indicator(\"T\")\n[m, u, l] = ta.bb(close, 0, 2.0)\nplot(m)\n");
    assert!(
        error_codes(&diagnostics).contains(&"E0219"),
        "expected E0219: {diagnostics:?}"
    );
}

#[test]
fn tuple_ta_state_is_recorded_in_syntactic_order() {
    let (dump, diagnostics) = check(
        "indicator(\"T\")\n\
         [m, s, h] = ta.macd(close, 12, 26, 9)\n\
         [mid, up, lo] = ta.bb(close, 20, 2.0)\n\
         plot(m)\n\
         plot(mid)\n",
    );
    assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");
    assert!(dump.contains("ta state 0 ta.macd params=[12, 26, 9]"));
    assert!(dump.contains("ta state 1 ta.bb params=[20"));
}

// -- named-argument binding --------------------------------------------------

#[test]
fn named_argument_binds_by_name() {
    // `length = 3` binds the ta.sma length parameter by name; the resulting
    // slot carries the same const as the positional form.
    let (dump, diagnostics) = check("indicator(\"T\")\nx = ta.sma(close, length = 3)\nplot(x)\n");
    assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");
    assert!(dump.contains("slot0 len=3"), "{dump}");
}

#[test]
fn all_named_arguments_match_positional() {
    // Binding both parameters by name (in signature order) is identical to
    // the positional form.
    let (named, d1) = check("indicator(\"T\")\nx = ta.sma(source = close, length = 3)\nplot(x)\n");
    let (positional, d2) = check("indicator(\"T\")\nx = ta.sma(close, 3)\nplot(x)\n");
    assert!(d1.is_empty(), "diagnostics: {d1:?}");
    assert!(d2.is_empty(), "diagnostics: {d2:?}");
    assert_eq!(named, positional);
}

#[test]
fn named_arguments_in_any_order() {
    // A named argument may re-order relative to declaration order.
    let (reordered, d) =
        check("indicator(\"T\")\nx = ta.sma(length = 3, source = close)\nplot(x)\n");
    assert!(d.is_empty(), "diagnostics: {d:?}");
    assert!(reordered.contains("slot0 len=3"), "{reordered}");
}

#[test]
fn duplicate_binding_positional_and_named_is_e0228() {
    // `close` fills `source` positionally; `source = open` then names it again.
    let (_dump, diagnostics) =
        check("indicator(\"T\")\nx = ta.sma(close, source = open, length = 3)\nplot(x)\n");
    assert!(
        error_codes(&diagnostics).contains(&"E0228"),
        "expected E0228: {diagnostics:?}"
    );
}

#[test]
fn unknown_parameter_name_is_e0228_with_suggestion() {
    // `lenght` is a typo for `length`; the did-you-mean note names `length`.
    let (_dump, diagnostics) = check("indicator(\"T\")\nx = ta.sma(close, lenght = 3)\nplot(x)\n");
    assert!(
        error_codes(&diagnostics).contains(&"E0228"),
        "expected E0228: {diagnostics:?}"
    );
    let diag = diagnostics
        .iter()
        .find(|d| d.code.as_deref() == Some("E0228"))
        .unwrap();
    assert!(
        diag.notes.iter().any(|n| n.contains("length")),
        "expected a did-you-mean note pointing at `length`: {diag:?}"
    );
}

#[test]
fn named_argument_on_user_function() {
    // User functions bind named arguments by their parameter names; naming in
    // reverse order produces the same HIR as the matching positional call.
    let (named, d1) = check(
        "indicator(\"T\")\n\
         f(a, b) =>\n    \
             a - b\n\
         plot(f(b = close, a = open))\n",
    );
    let (positional, d2) = check(
        "indicator(\"T\")\n\
         f(a, b) =>\n    \
             a - b\n\
         plot(f(open, close))\n",
    );
    assert!(d1.is_empty(), "diagnostics: {d1:?}");
    assert!(d2.is_empty(), "diagnostics: {d2:?}");
    assert_eq!(named, positional, "named binding must match positional");
}

#[test]
fn unknown_user_function_parameter_is_e0228() {
    let (_dump, diagnostics) = check(
        "indicator(\"T\")\n\
         f(a, b) =>\n    \
             a - b\n\
         plot(f(open, c = close))\n",
    );
    assert!(
        error_codes(&diagnostics).contains(&"E0228"),
        "expected E0228: {diagnostics:?}"
    );
}

#[test]
fn indicator_title_may_be_named() {
    let (_dump, diagnostics) = check("indicator(title = \"My Script\")\nplot(close)\n");
    assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");
}

#[test]
fn named_engine_call_omitting_the_defaulted_source_matches_positional() {
    // `ta.pivothigh` has an optional *leading* `source` (defaults to `high`)
    // followed by required leftbars/rightbars. Naming the required params
    // while omitting the source must bind identically to the positional
    // `ta.pivothigh(2, 2)` form — the interior gap is filled with the default
    // `high` read, not reported as E0204.
    let (named, d1) =
        check("indicator(\"T\")\nx = ta.pivothigh(leftbars = 2, rightbars = 2)\nplot(x)\n");
    let (positional, d2) = check("indicator(\"T\")\nx = ta.pivothigh(2, 2)\nplot(x)\n");
    assert!(d1.is_empty(), "named diagnostics: {d1:?}");
    assert!(d2.is_empty(), "positional diagnostics: {d2:?}");
    assert_eq!(named, positional, "named must match positional");
}

// ---------------------------------------------------------------------------
// Strings — concatenation, comparisons, and the str.* namespace.
// ---------------------------------------------------------------------------

/// Assert the last user variable in the HIR dump has the given declared type.
fn last_symbol_type(dump: &str, expected: &str) {
    let line = dump
        .lines()
        .rfind(|l| l.trim_start().starts_with("symbol "))
        .unwrap_or_else(|| panic!("no symbol lines in dump:\n{dump}"));
    assert!(line.contains(expected), "expected `{expected}` in `{line}`");
}

#[test]
fn string_concatenation_types_as_string() {
    let (dump, diagnostics) = check("indicator(\"S\")\ns = \"a\" + \"b\"\nplot(str.length(s))\n");
    assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");
    last_symbol_type(&dump, "string");
}

#[test]
fn string_concatenation_is_series_capable() {
    // A series string (str.tostring of a series float) concatenates with a
    // literal; the result is a series string, plottable via str.length.
    let (_dump, diagnostics) =
        check("indicator(\"S\")\ns = \"px=\" + str.tostring(close)\nplot(str.length(s))\n");
    assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");
}

#[test]
fn string_plus_number_is_rejected() {
    let (_dump, diagnostics) = check("indicator(\"S\")\nx = \"a\" + 1\nplot(close)\n");
    assert!(error_codes(&diagnostics).contains(&"E0207"));
}

#[test]
fn string_ordering_comparisons_are_rejected() {
    // Strings allow only == and !=, never < <= > >=.
    for op in ["<", "<=", ">", ">="] {
        let src = format!("indicator(\"S\")\nb = \"a\" {op} \"b\"\nplot(close)\n");
        let (_dump, diagnostics) = check(&src);
        assert!(
            error_codes(&diagnostics).contains(&"E0207"),
            "operator `{op}` should be rejected on strings"
        );
    }
}

#[test]
fn string_equality_is_accepted() {
    let (_dump, diagnostics) =
        check("indicator(\"S\")\nb = (\"a\" == \"b\")\nplot(b ? 1.0 : 0.0)\n");
    assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");
}

#[test]
fn str_length_and_pos_type_as_int() {
    let (dump, diagnostics) = check("indicator(\"S\")\nn = str.length(\"abc\")\nplot(n)\n");
    assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");
    last_symbol_type(&dump, "int");

    let (dump2, d2) = check("indicator(\"S\")\np = str.pos(\"abc\", \"b\")\nplot(p)\n");
    assert!(d2.is_empty(), "diagnostics: {d2:?}");
    last_symbol_type(&dump2, "int");
}

#[test]
fn str_predicates_type_as_bool() {
    for f in ["str.contains", "str.startswith", "str.endswith"] {
        let src = format!("indicator(\"S\")\nb = {f}(\"abc\", \"a\")\nplot(b ? 1.0 : 0.0)\n");
        let (dump, diagnostics) = check(&src);
        assert!(diagnostics.is_empty(), "{f}: {diagnostics:?}");
        last_symbol_type(&dump, "bool");
    }
}

#[test]
fn line_all_types_as_array_of_line() {
    // `line.all` is a namespaced value expression (member read, not a call)
    // that types as `array<line>` (an array of the line drawing-object type).
    let (dump, diagnostics) = check("indicator(\"D\")\nla = line.all\nplot(array.size(la))\n");
    assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");
    assert!(
        dump.contains("array<line>"),
        "expected `line.all` to type as array<line>, dump:\n{dump}"
    );
}

#[test]
fn str_tonumber_types_as_float() {
    let (dump, diagnostics) = check("indicator(\"S\")\nx = str.tonumber(\"3.5\")\nplot(x)\n");
    assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");
    last_symbol_type(&dump, "float");
}

#[test]
fn str_producers_type_as_string() {
    for f in [
        "str.upper(\"ab\")",
        "str.lower(\"AB\")",
        "str.trim(\" a \")",
        "str.substring(\"abc\", 1)",
        "str.substring(\"abc\", 0, 2)",
        "str.replace(\"aa\", \"a\", \"b\")",
        "str.replace_all(\"aa\", \"a\", \"b\")",
        "str.repeat(\"a\", 3)",
        "str.repeat(\"a\", 3, \"-\")",
        "str.match(\"abc\", \"b\")",
        "str.tostring(1)",
        "str.tostring(1.5, \"#.##\")",
        "str.format(\"{0}\", 1)",
    ] {
        let src = format!("indicator(\"S\")\ns = {f}\nplot(str.length(s))\n");
        let (dump, diagnostics) = check(&src);
        assert!(diagnostics.is_empty(), "{f}: {diagnostics:?}");
        last_symbol_type(&dump, "string");
    }
}

#[test]
fn str_function_wrong_argument_type_is_rejected() {
    // str.length wants a string, not a number.
    let (_d, d1) = check("indicator(\"S\")\nn = str.length(1)\nplot(n)\n");
    assert!(error_codes(&d1).contains(&"E0205"));
    // str.substring position must be an int, not a string.
    let (_d, d2) = check("indicator(\"S\")\ns = str.substring(\"abc\", \"x\")\nplot(close)\n");
    assert!(error_codes(&d2).contains(&"E0205"));
}

#[test]
fn str_function_wrong_arity_is_rejected() {
    // str.contains needs two arguments.
    let (_d, d1) = check("indicator(\"S\")\nb = str.contains(\"abc\")\nplot(close)\n");
    assert!(error_codes(&d1).contains(&"E0204"));
    // str.tostring takes at most two.
    let (_d, d2) = check("indicator(\"S\")\ns = str.tostring(1, \"#\", \"x\")\nplot(close)\n");
    assert!(error_codes(&d2).contains(&"E0204"));
}

#[test]
fn str_tostring_accepts_a_series_format() {
    let (_d, diagnostics) =
        check("indicator(\"S\")\nf = str.tostring(close)\ns = str.tostring(1.0, f)\nplot(close)\n");
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn str_format_accepts_stringable_args() {
    // int, float, bool, and string substitution args all bind.
    let (_dump, diagnostics) = check(
        "indicator(\"S\")\ns = str.format(\"{0} {1} {2} {3}\", 1, 2.5, true, \"x\")\nplot(str.length(s))\n",
    );
    assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");
}

// ---------------------------------------------------------------------------
// Color type.
// ---------------------------------------------------------------------------

#[test]
fn color_literal_is_typed_color_const() {
    let (dump, diagnostics) = check("indicator(\"C\")\nc = #FF8800\nplot(close)\n");
    assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");
    assert!(dump.contains("ConstColor #FF8800 : const color"), "{dump}");
}

#[test]
fn color_literal_with_alpha_carries_transparency() {
    let (dump, diagnostics) = check("indicator(\"C\")\nc = #FF880080\nplot(close)\n");
    assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");
    // alpha 0x80 = 128 retains its fractional transparency exactly.
    assert!(
        dump.contains("ConstColor #FF8800 t=49.80392156862745 : const color"),
        "{dump}"
    );
}

#[test]
fn named_color_constant_is_typed_color() {
    let (dump, diagnostics) = check("indicator(\"C\")\nc = color.red\nplot(close)\n");
    assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");
    // `color.red` resolves to #F23645 and is fully opaque.
    assert!(dump.contains("ConstColor #F23645 : const color"), "{dump}");
}

#[test]
fn all_seventeen_named_colors_resolve() {
    for (name, _, _, _) in pinewasm_hir::NAMED_COLORS {
        let (_dump, diagnostics) = check(&format!("indicator(\"C\")\nc = {name}\nplot(close)\n"));
        assert!(
            diagnostics.is_empty(),
            "{name} should resolve, got {diagnostics:?}"
        );
    }
}

#[test]
fn color_equality_is_allowed_and_typed_bool() {
    let (_dump, diagnostics) =
        check("indicator(\"C\")\nb = color.red == color.blue\nplot(b ? 1.0 : 0.0)\n");
    assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");
}

#[test]
fn color_ordering_and_arithmetic_are_rejected() {
    // `<` on colors is E0207 (no ordering); `+` on colors is E0207 (no arithmetic).
    let (_d1, lt) = check("indicator(\"C\")\nb = color.red < color.blue\nplot(close)\n");
    assert!(error_codes(&lt).contains(&"E0207"), "lt: {lt:?}");
    let (_d2, add) = check("indicator(\"C\")\nc = color.red + color.blue\nplot(close)\n");
    assert!(error_codes(&add).contains(&"E0207"), "add: {add:?}");
}

#[test]
fn color_compared_to_int_is_rejected() {
    let (_d, diagnostics) = check("indicator(\"C\")\nb = color.red == 1\nplot(close)\n");
    assert!(
        error_codes(&diagnostics).contains(&"E0207"),
        "{diagnostics:?}"
    );
}

#[test]
fn plotting_a_color_is_rejected() {
    // plot's series parameter is float; a color argument is a type error.
    let (_d, diagnostics) = check("indicator(\"C\")\nplot(color.red)\n");
    assert!(has_errors(&diagnostics), "{diagnostics:?}");
}

#[test]
fn color_constructors_type_as_color() {
    for f in [
        "color.new(color.red, 50)",
        "color.rgb(255, 0, 0)",
        "color.rgb(255, 0, 0, 40)",
        "color.from_gradient(50, 0, 100, color.red, color.blue)",
    ] {
        let src = format!("indicator(\"C\")\nc = {f}\nplot(color.r(c))\n");
        let (dump, diagnostics) = check(&src);
        assert!(diagnostics.is_empty(), "{f}: {diagnostics:?}");
        last_symbol_type(&dump, "color");
    }
}

#[test]
fn color_accessors_type_as_float() {
    for f in ["color.r", "color.g", "color.b", "color.t"] {
        let src = format!("indicator(\"C\")\nx = {f}(color.red)\nplot(x)\n");
        let (dump, diagnostics) = check(&src);
        assert!(diagnostics.is_empty(), "{f}: {diagnostics:?}");
        last_symbol_type(&dump, "float");
    }
}

#[test]
fn color_function_wrong_argument_type_is_rejected() {
    // color.new wants a color first argument, not a number.
    let (_d, d1) = check("indicator(\"C\")\nc = color.new(1, 50)\nplot(close)\n");
    assert!(error_codes(&d1).contains(&"E0205"), "{d1:?}");
    // color.r wants a color, not a string.
    let (_d, d2) = check("indicator(\"C\")\nx = color.r(\"red\")\nplot(x)\n");
    assert!(error_codes(&d2).contains(&"E0205"), "{d2:?}");
    // color.rgb channels must be int, not color.
    let (_d, d3) = check("indicator(\"C\")\nc = color.rgb(color.red, 0, 0)\nplot(close)\n");
    assert!(error_codes(&d3).contains(&"E0205"), "{d3:?}");
}

#[test]
fn color_function_wrong_arity_is_rejected() {
    // color.rgb needs at least three channels.
    let (_d, d1) = check("indicator(\"C\")\nc = color.rgb(255, 0)\nplot(close)\n");
    assert!(error_codes(&d1).contains(&"E0204"), "{d1:?}");
    // color.new needs the transparency argument.
    let (_d, d2) = check("indicator(\"C\")\nc = color.new(color.red)\nplot(close)\n");
    assert!(error_codes(&d2).contains(&"E0204"), "{d2:?}");
}

// ---------------------------------------------------------------------------
// input.* interface
// ---------------------------------------------------------------------------

#[test]
fn input_declarations_hir_snapshot() {
    let (dump, diagnostics) = check(
        "//@version=6\nindicator(\"Inputs\")\nn = input.int(14, \"Length\", 1, 100)\nsrc = input.source(close, \"Source\")\nb = input.bool(true)\nplot(src + n)\n",
    );
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
    insta::assert_snapshot!(dump);
}

#[test]
fn input_int_is_input_qualified() {
    // The HIR dump labels the input read with the `input` qualifier — the
    // lattice producer this feature introduces.
    let (dump, diagnostics) = check("indicator(\"I\")\nn = input.int(3)\nplot(close + n)\n");
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
    assert!(
        dump.contains("ReadInput input0 : input int"),
        "expected an input-qualified int read, got:\n{dump}"
    );
}

#[test]
fn input_source_is_series_qualified() {
    // input.source reads a builtin series, so it is `series float`, not
    // `input` — selecting among series does not weaken it below series.
    let (dump, diagnostics) = check("indicator(\"I\")\nsrc = input.source(close)\nplot(src)\n");
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
    assert!(
        dump.contains("ReadInput input0 : series float"),
        "expected a series-qualified source read, got:\n{dump}"
    );
}

#[test]
fn input_non_const_default_is_e0230() {
    let (_d, diagnostics) = check("indicator(\"I\")\nn = input.int(close)\nplot(close)\n");
    assert!(
        error_codes(&diagnostics).contains(&"E0230"),
        "{diagnostics:?}"
    );
}

#[test]
fn input_default_out_of_range_is_e0230() {
    let (_d, diagnostics) =
        check("indicator(\"I\")\nn = input.int(500, \"n\", 1, 100)\nplot(close)\n");
    assert!(
        error_codes(&diagnostics).contains(&"E0230"),
        "{diagnostics:?}"
    );
}

#[test]
fn input_accepts_ui_metadata_params() {
    // Every `input.*` accepts the UI-metadata tail (`tooltip`, `inline`,
    // `group`, `confirm`) — dialog-shaping only, no data-path effect.
    let src = "//@version=6\nindicator(\"I\")\n\
        a = input.int(8, \"A\", minval=3, group=\"G\", inline=\"g\", tooltip=\"t\")\n\
        b = input.bool(true, \"B\", group=\"G\", inline=\"g\", tooltip=\"t\")\n\
        c = input.float(1.0, \"C\", group=\"G\", confirm=true)\n\
        d = input.source(close, \"D\", group=\"G\", tooltip=\"t\")\n\
        plot(a + (b ? c : 0.0) + d)\n";
    let (_dump, diagnostics) = check(src);
    assert!(
        diagnostics.is_empty(),
        "input UI-metadata params should compile clean: {diagnostics:?}"
    );
}

#[test]
fn persistent_const_string_initializer_can_supply_input_layout_metadata() {
    // This form compiles, and the persistent slot does not change the
    // initializer's metadata value.
    let source = "//@version=6\nindicator(\"Persistent group check\")\nvar string groupName = \"Settings\"\nlength = input.int(3, \"Length\", group=groupName, inline=groupName)\nplot(ta.sma(close, length))\n";
    let (_, diagnostics) = check(source);
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
}

#[test]
fn reassigned_persistent_string_cannot_supply_input_layout_metadata() {
    // A reassigned persistent string is rejected as `group` metadata (E0230):
    // `groupName` is a simple string after `:=`, while `group` requires a const string.
    let source = "//@version=6\nindicator(\"Reassigned group metadata case\")\nvar string groupName = \"Settings\"\ngroupName := \"Other\"\nlength = input.int(3, \"Length\", group=groupName)\nplot(ta.sma(close, length))\n";
    let (_, diagnostics) = check(source);
    assert!(
        error_codes(&diagnostics).contains(&"E0230"),
        "{diagnostics:?}"
    );
}

#[test]
fn persistent_input_int_can_supply_ta_lengths() {
    // A `var int` input can supply both TA lengths. Reinitialization happens
    // on input edits, not each bar.
    let source = "//@version=6\nindicator(\"Persistent TA length check\")\nvar int n = input.int(3, \"Length\")\nplot(ta.sma(close, n))\nplot(ta.ema(close, n))\n";
    let (_, diagnostics) = check(source);
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
}

#[test]
fn input_int_can_supply_float_ta_parameter_through_implicit_cast() {
    // Pine automatically casts input int values when a float parameter is
    // required. `ta.supertrend`'s factor is a float parameter.
    let source = "//@version=6\nindicator(\"Integer supertrend factor\")\nfactor = input.int(3, \"Factor\")\n[st, direction] = ta.supertrend(factor, 2)\nplot(st)\nplot(direction)\n";
    let (_, diagnostics) = check(source);
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");

    // Percent parameters are a separately validated float shape;
    // Pine still applies the same int-to-float parameter cast.
    let source = "//@version=6\nindicator(\"Integer percentile\")\np = input.int(50, \"Percentile\", minval=0, maxval=100)\nplot(ta.percentile_nearest_rank(close, 5, p))\n";
    let (_, diagnostics) = check(source);
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
}

#[test]
fn inferred_persistent_input_int_can_supply_literal_ta_length() {
    // A script that keeps this input alias unchanged
    // and passes it to ta.bb, whose length must be fixed at compile time.
    let source = "//@version=6\nindicator(\"Persistent Bollinger length case\")\nvar bollingerLength = input.int(20, \"Bollinger length\")\n[middle, upper, lower] = ta.bb(close, bollingerLength, 2.0)\nplot(middle)\n";
    let (_, diagnostics) = check(source);
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
}

#[test]
fn reassigned_persistent_input_int_cannot_supply_simple_ta_length() {
    // A reassigned `ta.ema` length is rejected as a non-simple int (E0219):
    // the value became `series int`, but `simple int` is required.
    let source = "//@version=6\nindicator(\"Reassigned TA length case\")\nvar int n = input.int(3, \"Length\")\nn := n + 1\nplot(ta.ema(close, n))\n";
    let (_, diagnostics) = check(source);
    assert!(
        error_codes(&diagnostics).contains(&"E0219"),
        "{diagnostics:?}"
    );
}

#[test]
fn input_and_timeframe_expression_can_supply_simple_ema_length() {
    let source = "//@version=6\nindicator(\"Simple EMA length\")\nn = input.int(3)\nlength = n + timeframe.multiplier - 1\nplot(ta.ema(close, length))\n";
    let (dump, diagnostics) = check(source);
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
    assert!(dump.contains("symbol 1 `length`: simple int"), "{dump}");
    assert!(dump.contains("TaCallDynamic ta.ema"), "{dump}");

    let source = "//@version=6\nindicator(\"Series EMA length\")\nlength = bar_index + 1\nplot(ta.ema(close, length))\n";
    let (_, diagnostics) = check(source);
    assert!(
        error_codes(&diagnostics).contains(&"E0219"),
        "{diagnostics:?}"
    );
}

fn w0010_warnings(diagnostics: &[Diagnostic]) -> Vec<&Diagnostic> {
    diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code.as_deref() == Some("W0010"))
        .collect()
}

#[test]
fn a_length_calculated_from_an_input_warns_w0010_where_it_stays_fixed() {
    let source = "//@version=6\nindicator(\"Fixed lengths\")\nn = input.int(10, \"N\")\nm = n * 2\nplot(ta.sma(close, n + 1))\nplot(ta.rsi(close, m))\nplot(ta.highest(high, n + 2))\nplot(math.sum(close, n - 1))\n";
    let (_, diagnostics) = check(source);
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
    let warnings = w0010_warnings(&diagnostics);
    assert_eq!(warnings.len(), 4, "{diagnostics:?}");
    assert_eq!(warnings[0].severity, Severity::Warning);
    assert_eq!(
        warnings[0].message,
        "`ta.sma` argument `length` is calculated from an input, so this call keeps the value the input defaults give (11)"
    );
    assert_diagnostic_source(source, warnings[0], "n + 1", 5, 20);
    assert_eq!(
        warnings[1].message,
        "`ta.rsi` argument `length` is calculated from an input, so this call keeps the value the input defaults give (20)"
    );
    assert_diagnostic_source(source, warnings[1], "m", 6, 20);
    assert!(
        warnings[2]
            .message
            .starts_with("`ta.highest` argument `length`")
            && warnings[2].message.ends_with("(12)"),
        "{diagnostics:?}"
    );
    assert_diagnostic_source(source, warnings[2], "n + 2", 7, 23);
    assert!(
        warnings[3]
            .message
            .starts_with("`math.sum` argument `length`")
            && warnings[3].message.ends_with("(9)"),
        "{diagnostics:?}"
    );
    assert_diagnostic_source(source, warnings[3], "n - 1", 8, 22);
}

#[test]
fn a_length_that_follows_its_input_or_reads_no_input_does_not_warn_w0010() {
    // A direct input and its alias are bound to the selected value, a length
    // calculated from literals has only one value, and a calculated `ta.ema`
    // length is a run-time one.
    let source = "//@version=6\nindicator(\"Live lengths\")\nn = input.int(10, \"N\")\nm = n\nk = 5\nplot(ta.sma(close, n))\nplot(ta.rsi(close, m))\nplot(ta.sma(close, k + 1))\nplot(ta.rsi(close, k * 2))\nplot(ta.ema(close, n + 1))\nplot(ta.ema(close, m * 2))\nplot(ta.ema(close, k + 1))\n";
    let (_, diagnostics) = check(source);
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
    assert!(w0010_warnings(&diagnostics).is_empty(), "{diagnostics:?}");
}

#[test]
fn a_request_keeps_a_calculated_ema_length_and_warns_w0010_once() {
    // The request lowers the declaration more than once; the chart reads the
    // same call with a run-time length.
    let source = "//@version=6\nindicator(\"Request length\")\nn = input.int(10, \"N\")\ne = ta.ema(close, n + 1)\nplot(request.security(syminfo.tickerid, \"D\", e))\nplot(e)\n";
    let (dump, diagnostics) = check(source);
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
    assert!(dump.contains("TaCallDynamic ta.ema"), "{dump}");
    let warnings = w0010_warnings(&diagnostics);
    assert_eq!(warnings.len(), 1, "{diagnostics:?}");
    assert_eq!(
        warnings[0].message,
        "`ta.ema` argument `length` is calculated from an input, so inside a `request.*()` call it keeps the value the input defaults give (11)"
    );
    assert_diagnostic_source(source, warnings[0], "n + 1", 4, 19);

    // Without a request the same call has nothing to report.
    let source = "//@version=6\nindicator(\"Chart length\")\nn = input.int(10, \"N\")\ne = ta.ema(close, n + 1)\nplot(e)\n";
    let (_, diagnostics) = check(source);
    assert!(w0010_warnings(&diagnostics).is_empty(), "{diagnostics:?}");
}

#[test]
fn a_series_helper_parameter_keeps_the_fixed_ema_length_and_warns_w0010() {
    // A `series int` parameter is not a `simple int`, so a calculated
    // argument cannot become a run-time `ta.ema` length there. The call keeps
    // compiling with the value the argument has at compile time.
    let source = "//@version=6\nindicator(\"Series parameter\")\nn = input.int(10, \"N\")\nf(series int len) => ta.ema(close, len)\nplot(f(n + 1))\nplot(f(n + 1))\nplot(f(n * 2))\n";
    let (dump, diagnostics) = check(source);
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
    assert!(!dump.contains("TaCallDynamic ta.ema"), "{dump}");
    // One warning for each value: the second `f(n + 1)` adds nothing.
    let warnings = w0010_warnings(&diagnostics);
    assert_eq!(warnings.len(), 2, "{diagnostics:?}");
    assert!(warnings[0].message.ends_with("(11)"), "{diagnostics:?}");
    assert!(warnings[1].message.ends_with("(20)"), "{diagnostics:?}");
    for warning in &warnings {
        assert!(
            warning.message.starts_with("`ta.ema` argument `length`"),
            "{diagnostics:?}"
        );
        assert_diagnostic_source(source, warning, "len", 4, 36);
    }

    // A literal or a direct input through the same parameter has one meaning.
    for call in ["f(6)", "f(n)"] {
        let source = format!(
            "//@version=6\nindicator(\"Series parameter\")\nn = input.int(10, \"N\")\nf(series int len) => ta.ema(close, len)\nplot({call})\n"
        );
        let (_, diagnostics) = check(&source);
        assert!(!has_errors(&diagnostics), "{call}: {diagnostics:?}");
        assert!(
            w0010_warnings(&diagnostics).is_empty(),
            "{call}: {diagnostics:?}"
        );
    }

    // A parameter that varies by bar still is not a length.
    let source = "//@version=6\nindicator(\"Series parameter\")\nf(series int len) => ta.ema(close, len)\nplot(f(bar_index))\n";
    let (_, diagnostics) = check(source);
    assert!(
        error_codes(&diagnostics).contains(&"E0219"),
        "{diagnostics:?}"
    );
}

#[test]
fn a_rejected_request_does_not_report_the_length_it_would_have_kept() {
    // The expression lowers as far as the `ta.ema` call, then the request is
    // rejected: no request keeps that length, so nothing is reported for it.
    for (request, code) in [
        (
            "request.security(syminfo.tickerid, \"60\", ta.ema(close, n + 1) + str.length(syminfo.ticker) * 0)",
            "E0244",
        ),
        (
            "array.size(request.security_lower_tf(syminfo.tickerid, \"1\", ta.ema(close, n + 1)))",
            "E0246",
        ),
    ] {
        let source = format!(
            "//@version=6\nindicator(\"Rejected request\")\nn = input.int(14, \"N\")\nplot({request})\n"
        );
        let (_, diagnostics) = check(&source);
        assert!(
            error_codes(&diagnostics).contains(&code),
            "{request}: {diagnostics:?}"
        );
        assert!(
            w0010_warnings(&diagnostics).is_empty(),
            "{request}: {diagnostics:?}"
        );
    }
}

#[test]
fn a_calculated_ema_length_that_the_defaults_put_out_of_range_is_e0219() {
    // The same rule as a direct input whose default is out of range, on the
    // chart and inside a request.
    for (call, reason) in [
        ("ta.ema(close, n - 20)", "must be at least 1, found -10"),
        (
            "ta.ema(close, n * 10001)",
            "must be at most 100000, found 100010",
        ),
        (
            "request.security(syminfo.tickerid, \"D\", ta.ema(close, n - 20))",
            "must be at least 1, found -10",
        ),
    ] {
        let source = format!(
            "//@version=6\nindicator(\"Out of range\")\nn = input.int(10, \"N\")\nplot({call})\n"
        );
        let (_, diagnostics) = check(&source);
        let errors: Vec<_> = diagnostics.iter().filter(|d| d.is_error()).collect();
        assert_eq!(errors.len(), 1, "{call}: {diagnostics:?}");
        assert_eq!(errors[0].code.as_deref(), Some("E0219"), "{call}");
        assert_eq!(
            errors[0].message,
            format!("invalid `ta.ema` argument `length`: {reason}"),
            "{call}"
        );
    }

    // In range for the defaults, the length is a run-time one and its range
    // is checked when the call runs.
    let source = "//@version=6\nindicator(\"In range\")\nn = input.int(10, \"N\")\nplot(ta.ema(close, n * 10000))\n";
    let (dump, diagnostics) = check(source);
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
    assert!(dump.contains("TaCallDynamic ta.ema"), "{dump}");
}

#[test]
fn a_calculated_argument_through_a_helper_warns_w0010_for_its_parameter() {
    let source = "//@version=6\nindicator(\"Pivot helper\")\nn = input.int(3, \"N\")\nf(l, r) => ta.pivothigh(high, l, r)\nplot(f(n + 1, 2))\n";
    let (_, diagnostics) = check(source);
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
    let warnings = w0010_warnings(&diagnostics);
    assert_eq!(warnings.len(), 1, "{diagnostics:?}");
    assert_eq!(
        warnings[0].message,
        "`ta.pivothigh` argument `leftbars` is calculated from an input, so this call keeps the value the input defaults give (4)"
    );
    assert_diagnostic_source(source, warnings[0], "l", 4, 31);
}

#[test]
fn input_string_accepts_options_list() {
    // `input.string(defval, options=[...])` — the dropdown form.
    let src = "//@version=6\nindicator(\"I\")\n\
        s = input.string(\"RSI\", \"Feature\", options=[\"RSI\", \"WT\", \"CCI\"], group=\"F\")\n\
        plot(close)\n";
    let (_d, diagnostics) = check(src);
    assert!(
        diagnostics.is_empty(),
        "input.string options should compile clean: {diagnostics:?}"
    );
}

#[test]
fn input_accepts_negative_literal_default() {
    // A negative literal default (`input.float(defval=-0.1)`, `input.int(-5)`)
    // is a valid const numeric literal, like its positive twin.
    let src = "//@version=6\nindicator(\"I\")\n\
        t = input.float(-0.1, \"Threshold\", minval=-10, maxval=10, step=0.1)\n\
        k = input.int(-5, \"K\", minval=-10, maxval=10)\n\
        plot(t + k)\n";
    let (_d, diagnostics) = check(src);
    assert!(
        diagnostics.is_empty(),
        "negative input defaults should compile clean: {diagnostics:?}"
    );
}

#[test]
fn input_string_rejected_as_plot_title_by_lattice() {
    // An input string is weaker than const; plot's title requires const, so
    // the qualifier check (E0206) rejects it — proving the lattice join.
    let (_d, diagnostics) = check("indicator(\"I\")\ns = input.string(\"x\")\nplot(close, s)\n");
    assert!(
        error_codes(&diagnostics).contains(&"E0206"),
        "{diagnostics:?}"
    );
}

// ---------------------------------------------------------------------------
// max_bars_back + the history ceiling literal offsets settle (E0233 past 5000)
// ---------------------------------------------------------------------------

/// Analyze `source`, asserting it parses cleanly, and return the HIR + diags.
fn analyze_ok(source: &str) -> crate::AnalysisResult {
    let parsed = parse(source);
    assert!(
        !has_errors(&parsed.diagnostics),
        "test source must parse cleanly: {:?}",
        parsed.diagnostics
    );
    analyze(&parsed.program)
}

#[test]
fn indicator_max_bars_back_named_arg_is_recorded() {
    let result = analyze_ok("indicator(\"I\", max_bars_back=1000)\nplot(close)\n");
    assert!(
        result.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        result.diagnostics
    );
    let info = result.hir.indicator.expect("indicator declared");
    assert_eq!(info.max_bars_back, Some(1000));
    assert_eq!(info.title, "I");
}

#[test]
fn indicator_without_max_bars_back_records_none() {
    let result = analyze_ok("indicator(\"I\")\nplot(close)\n");
    let info = result.hir.indicator.expect("indicator declared");
    assert_eq!(info.max_bars_back, None);
}

#[test]
fn indicator_display_params_compile_and_max_counts_const_fold() {
    // The full display surface compiles; the max_*_count caps are recorded.
    let result = analyze_ok(
        "indicator(\"I\", shorttitle=\"S\", overlay=true, format=format.percent, \
         precision=2, max_lines_count=100, max_labels_count=20, \
         max_boxes_count=30, max_polylines_count=40, timeframe=\"D\", \
         dynamic_requests=true)\nplot(close)\n",
    );
    assert!(
        result.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        result.diagnostics
    );
    let info = result.hir.indicator.expect("indicator declared");
    assert_eq!(info.max_lines_count, Some(100));
    assert_eq!(info.max_labels_count, Some(20));
    assert_eq!(info.max_boxes_count, Some(30));
    assert_eq!(info.max_polylines_count, Some(40));
}

#[test]
fn indicator_display_params_value_assert_accept_and_reject() {
    // VALUE-ASSERTING coverage for the full `indicator(...)` display surface.
    // Each display param has an observable, checkable acceptance rule: a valid
    // const value compiles with no diagnostics, and an invalid one yields the
    // exact diagnostic code (out-of-range int -> E0219; wrong type -> E0205).
    // The four `max_*_count` caps additionally fold into stored metadata,
    // asserted in `indicator_display_params_compile_and_max_counts_const_fold`.
    // One table drives every member of the cluster.
    let cases: &[(&str, &str, Option<&str>)] = &[
        // (param slot in the call, valid form, invalid form -> expected code)
        // shorttitle: const string, no range/type check beyond String.
        ("shorttitle=\"S\"", "shorttitle=\"S\"", None),
        // overlay: const bool; a string is E0205.
        ("overlay=true", "overlay=true", Some("overlay=\"yes\"")),
        // format: const string (e.g. format.percent, a string constant).
        (
            "format=format.percent",
            "format=format.percent",
            Some("format=1"),
        ),
        // precision: const int 0..=16; 17 is E0219.
        ("precision=2", "precision=2", Some("precision=17")),
        // scale: one of the registered const-string scale.* tokens.
        ("scale=scale.right", "scale=scale.right", Some("scale=2")),
        // timeframe: const string; a bool is E0205.
        ("timeframe=\"D\"", "timeframe=\"D\"", Some("timeframe=true")),
        // timeframe_gaps: const bool, only alongside a timeframe; an int is
        // E0205.
        (
            "timeframe_gaps=true",
            "timeframe=\"D\", timeframe_gaps=true",
            Some("timeframe=\"D\", timeframe_gaps=1"),
        ),
        // explicit_plot_zorder: const bool; an int is E0205.
        (
            "explicit_plot_zorder=true",
            "explicit_plot_zorder=true",
            Some("explicit_plot_zorder=1"),
        ),
        // calc_bars_count: const int; a negative value is E0219.
        (
            "calc_bars_count=100",
            "calc_bars_count=100",
            Some("calc_bars_count=-1"),
        ),
        // dynamic_requests: const bool; a string is E0205.
        (
            "dynamic_requests=true",
            "dynamic_requests=true",
            Some("dynamic_requests=\"x\""),
        ),
        // behind_chart: const bool; an int is E0205.
        (
            "behind_chart=true",
            "behind_chart=true",
            Some("behind_chart=1"),
        ),
    ];
    for (label, valid, invalid) in cases {
        let ok = analyze_ok(&format!("indicator(\"I\", {valid})\nplot(close)\n"));
        assert!(
            ok.diagnostics.is_empty(),
            "valid `{label}` should compile clean, got: {:?}",
            ok.diagnostics
        );
        if let Some(bad) = invalid {
            let err = analyze_ok(&format!("indicator(\"I\", {bad})\nplot(close)\n"));
            let codes = error_codes(&err.diagnostics);
            assert!(
                codes.contains(&"E0219") || codes.contains(&"E0205"),
                "invalid `{bad}` should be E0219/E0205, got: {:?}",
                err.diagnostics
            );
        }
    }
}

#[test]
fn indicator_scale_tokens_follow_the_overlay_contract() {
    let right = analyze_ok("indicator(\"I\", scale=scale.right)\nplot(close)\n");
    assert!(right.diagnostics.is_empty(), "{:?}", right.diagnostics);

    let none = analyze_ok("indicator(\"I\", overlay=true, scale=scale.none)\nplot(close)\n");
    assert!(none.diagnostics.is_empty(), "{:?}", none.diagnostics);

    let missing_overlay = analyze_ok("indicator(\"I\", scale=scale.none)\nplot(close)\n");
    assert!(error_codes(&missing_overlay.diagnostics).contains(&"E0219"));

    let raw_string = analyze_ok("indicator(\"I\", scale=\"scale.right\")\nplot(close)\n");
    assert!(error_codes(&raw_string.diagnostics).contains(&"E0205"));
}

#[test]
fn indicator_rejects_the_old_explicit_plot_zones_misspelling() {
    let result = analyze_ok("indicator(\"I\", explicit_plot_zones=true)\nplot(close)\n");
    let diagnostic = result
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code.as_deref() == Some("E0228"))
        .expect("misspelled parameter should be E0228");
    assert!(
        diagnostic.message.contains("explicit_plot_zones"),
        "diagnostic should identify the misspelling: {diagnostic:?}"
    );
    assert!(
        diagnostic
            .labels
            .iter()
            .any(|label| label.message == "unknown parameter name"),
        "diagnostic should mark the unknown parameter: {diagnostic:?}"
    );
}

#[test]
fn indicator_overlay_wrong_type_is_rejected() {
    // `overlay` must be a bool; a string literal is E0205.
    let result = analyze_ok("indicator(\"I\", overlay=\"yes\")\nplot(close)\n");
    assert!(error_codes(&result.diagnostics).contains(&"E0205"));
}

#[test]
fn indicator_overlay_value_is_recorded() {
    // The folded `overlay` value is carried on IndicatorInfo for hosts.
    let on = analyze_ok("indicator(\"I\", overlay=true)\nplot(close)\n");
    assert!(
        on.diagnostics.is_empty(),
        "unexpected: {:?}",
        on.diagnostics
    );
    assert!(on.hir.indicator.expect("indicator declared").overlay);

    let off = analyze_ok("indicator(\"I\", overlay=false)\nplot(close)\n");
    assert!(!off.hir.indicator.expect("indicator declared").overlay);

    // Default (omitted) is false.
    let def = analyze_ok("indicator(\"I\")\nplot(close)\n");
    assert!(!def.hir.indicator.expect("indicator declared").overlay);
}

#[test]
fn indicator_max_lines_count_out_of_range_is_e0219() {
    let result = analyze_ok("indicator(\"I\", max_lines_count=9000)\nplot(close)\n");
    assert!(error_codes(&result.diagnostics).contains(&"E0219"));
}

/// Analyze `source`, asserting it is free of diagnostics, and return the
/// effective history ceiling the program carries.
fn history_ceiling(source: &str) -> u32 {
    let result = analyze_ok(source);
    assert!(
        result.diagnostics.is_empty(),
        "unexpected diagnostics for {source:?}: {:?}",
        result.diagnostics
    );
    result.hir.max_bars_back
}

#[test]
fn offset_within_default_ceiling_leaves_it_alone() {
    // The default ceiling is 500. A literal offset inside it asks for nothing
    // more, and the offset itself is never the ceiling: a script that reads
    // one bar back keeps the whole default for its dynamic offsets.
    assert_eq!(history_ceiling("indicator(\"I\")\nplot(close)\n"), 500);
    assert_eq!(history_ceiling("indicator(\"I\")\nplot(close[1])\n"), 500);
    assert_eq!(history_ceiling("indicator(\"I\")\nplot(close[499])\n"), 500);
    assert_eq!(history_ceiling("indicator(\"I\")\nplot(close[500])\n"), 500);
}

#[test]
fn offset_beyond_default_ceiling_raises_it() {
    // A literal offset states its own requirement, so no declaration is
    // needed: the ceiling rises to the offset.
    assert_eq!(history_ceiling("indicator(\"I\")\nplot(close[501])\n"), 501);
    assert_eq!(
        history_ceiling("indicator(\"I\")\nplot(close[1000])\n"),
        1000
    );
    // Boundary: the hard limit itself is reachable.
    assert_eq!(
        history_ceiling("indicator(\"I\")\nplot(close[5000])\n"),
        5000
    );
}

#[test]
fn deepest_literal_offset_decides_the_ceiling() {
    // Every literal counts, wherever it is written: a later statement, a
    // function body analyzed at its call site, a nested history read.
    assert_eq!(
        history_ceiling("indicator(\"I\")\nplot(close[700])\nplot(open[1200])\nplot(high[900])\n"),
        1200
    );
    assert_eq!(
        history_ceiling("indicator(\"I\")\nf(src) => src[800]\nplot(f(close))\n"),
        800
    );
    assert_eq!(
        history_ceiling("indicator(\"I\")\nif close > open\n    x = close[650]\nplot(close)\n"),
        650
    );
}

#[test]
fn compound_history_offset_raises_the_ceiling_too() {
    // The offset of a computed target counts like the offset of a name.
    assert_eq!(
        history_ceiling("indicator(\"I\")\nplot((close - open)[501])\n"),
        501
    );
}

#[test]
fn literal_offset_and_declared_limit_combine_to_the_larger() {
    // A declaration is one request and a literal offset another; the larger
    // wins in either order.
    assert_eq!(
        history_ceiling("indicator(\"I\", max_bars_back=1000)\nplot(close[1000])\n"),
        1000
    );
    assert_eq!(
        history_ceiling("indicator(\"I\", max_bars_back=1000)\nplot(close[10])\n"),
        1000
    );
    assert_eq!(
        history_ceiling("indicator(\"I\", max_bars_back=100)\nplot(close[200])\n"),
        200
    );
    assert_eq!(
        history_ceiling("indicator(\"I\", max_bars_back=100)\nplot(close[50])\n"),
        100
    );
    assert_eq!(
        history_ceiling("indicator(\"I\")\nx = close[3000]\nmax_bars_back(close, 2000)\nplot(x)\n"),
        3000
    );
}

#[test]
fn strategy_history_offset_raises_the_ceiling() {
    assert_eq!(history_ceiling("strategy(\"S\")\nplot(close[750])\n"), 750);
    assert_eq!(
        history_ceiling("strategy(\"S\", max_bars_back=2000)\nplot(close[750])\n"),
        2000
    );
}

#[test]
fn offset_beyond_hard_limit_is_e0233() {
    // 5000 bars is the most any buffer holds, so no request can reach 5001.
    let result = analyze_ok("indicator(\"I\")\nplot(close[5001])\n");
    assert_eq!(error_codes(&result.diagnostics), ["E0233"]);
    // The unreachable offset asks for nothing: the ceiling stays the default.
    assert_eq!(result.hir.max_bars_back, 500);

    // Reachable offsets beside it still count.
    let mixed = analyze_ok("indicator(\"I\")\nplot(close[9000])\nplot(open[800])\n");
    assert_eq!(error_codes(&mixed.diagnostics), ["E0233"]);
    assert_eq!(mixed.hir.max_bars_back, 800);
}

#[test]
fn e0233_error_names_the_offset_and_the_limit() {
    let result = analyze_ok("indicator(\"I\")\nplot(close[7000])\n");
    let diag = result
        .diagnostics
        .iter()
        .find(|d| d.code.as_deref() == Some("E0233"))
        .expect("E0233 present");
    assert!(diag.message.contains("7000"), "{}", diag.message);
    assert!(diag.message.contains("5000"), "{}", diag.message);
}

#[test]
fn compound_history_offset_is_also_bounded() {
    // The E0233 check applies to compound-history offsets too, not just simple
    // ident history.
    let result = analyze_ok("indicator(\"I\")\nplot((close - open)[5001])\n");
    assert!(
        error_codes(&result.diagnostics).contains(&"E0233"),
        "{:?}",
        result.diagnostics
    );
}

#[test]
fn indicator_max_bars_back_raises_the_ceiling() {
    // Declaring a buffer raises the ceiling without any offset that needs it:
    // the declaration is for the offsets the compiler cannot see.
    assert_eq!(
        history_ceiling("indicator(\"I\", max_bars_back=1000)\nplot(close)\n"),
        1000
    );
    assert_eq!(
        history_ceiling("indicator(\"I\", max_bars_back=1000)\nx = close[1000]\nplot(x)\n"),
        1000
    );
}

#[test]
fn indicator_max_bars_back_is_capped_at_5000() {
    // The declared limit itself is capped at 5000 bars; asking for more is
    // E0219.
    let result = analyze_ok("indicator(\"I\", max_bars_back=6000)\nplot(close)\n");
    assert!(
        error_codes(&result.diagnostics).contains(&"E0219"),
        "{:?}",
        result.diagnostics
    );
}

#[test]
fn offset_beyond_hard_limit_is_e0233_even_when_declared() {
    // An offset past 5000 can never be satisfied, whatever is declared; E0233
    // fires and the hint mentions the hard ceiling.
    let result = analyze_ok("indicator(\"I\", max_bars_back=5000)\nplot(close[5001])\n");
    let diag = result
        .diagnostics
        .iter()
        .find(|d| d.code.as_deref() == Some("E0233"))
        .expect("E0233 present");
    assert!(
        diag.labels.iter().any(|l| l.message.contains("5000")),
        "hint should cite the hard ceiling: {:?}",
        diag.labels
    );
}

#[test]
fn indicator_max_bars_back_must_be_positive() {
    let result = analyze_ok("indicator(\"I\", max_bars_back=0)\nplot(close)\n");
    assert!(
        error_codes(&result.diagnostics).contains(&"E0219"),
        "{:?}",
        result.diagnostics
    );
}

#[test]
fn top_level_max_bars_back_function_raises_the_ceiling_regardless_of_order() {
    // The top-level `max_bars_back(source, length)` form. It may appear
    // *after* the offset that reads history, because the ceiling is settled
    // at end of program.
    let ok = analyze_ok("indicator(\"I\")\nx = close[1500]\nmax_bars_back(close, 2000)\nplot(x)\n");
    assert!(
        ok.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        ok.diagnostics
    );
    assert_eq!(ok.hir.max_bars_back, 2000);
    // The indicator's recorded metadata is unaffected (the function is a
    // separate, additive limit source).
    let info = ok.hir.indicator.expect("indicator declared");
    assert_eq!(info.max_bars_back, None);
}

#[test]
fn largest_max_bars_back_request_wins() {
    // Two declarations disagree; the larger request (2000) wins, and the
    // offset below it (1800) adds nothing.
    assert_eq!(
        history_ceiling(
            "indicator(\"I\", max_bars_back=1000)\nmax_bars_back(close, 2000)\nplot(close[1800])\n",
        ),
        2000
    );
    assert_eq!(
        history_ceiling(
            "indicator(\"I\", max_bars_back=2000)\nmax_bars_back(close, 1000)\nplot(close)\n",
        ),
        2000
    );
}

#[test]
fn deep_offset_on_broken_target_does_not_cascade_e0233() {
    // An unreachable offset on an unresolved identifier reports E0201 (unknown
    // name) but must NOT also emit E0233 — the offset is only recorded once
    // the target resolves, so no cascade.
    let result = analyze_ok("indicator(\"I\")\nplot(nope[6000])\n");
    let codes = error_codes(&result.diagnostics);
    assert!(!codes.contains(&"E0233"), "unexpected cascade: {codes:?}");
    assert!(
        codes.iter().any(|c| c.starts_with("E02")),
        "expected a resolution error: {codes:?}"
    );
    // For the same reason a reachable offset on it requests no buffer.
    let reachable = analyze_ok("indicator(\"I\")\nplot(nope[600])\n");
    assert_eq!(reachable.hir.max_bars_back, 500);
}

#[test]
fn max_bars_back_function_inside_expression_is_e0212() {
    let result = analyze_ok("indicator(\"I\")\ny = max_bars_back(close, 100)\nplot(close)\n");
    assert!(
        error_codes(&result.diagnostics).contains(&"E0212"),
        "{:?}",
        result.diagnostics
    );
}

#[test]
fn max_bars_back_function_inside_function_body_is_e0214() {
    let result = analyze_ok(
        "indicator(\"I\")\nf(x) =>\n    max_bars_back(close, 100)\n    x\nplot(f(close))\n",
    );
    assert!(
        error_codes(&result.diagnostics).contains(&"E0214"),
        "{:?}",
        result.diagnostics
    );
}

// ===========================================================================
// Collections (array core) type checking.
// ===========================================================================

#[test]
fn array_constructor_and_ops_check_clean() {
    let (_, diagnostics) = check(
        "indicator(\"C\")\na = array.new_float(0)\narray.push(a, 1.0)\nplot(array.get(a, 0))\n",
    );
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
}

#[test]
fn array_element_type_mismatch_is_e0234() {
    // Pushing a string into a float array is a strict type error.
    let (_, diagnostics) =
        check("indicator(\"C\")\na = array.new_float(0)\narray.push(a, \"x\")\nplot(close)\n");
    assert!(
        error_codes(&diagnostics).contains(&"E0234"),
        "{diagnostics:?}"
    );
}

#[test]
fn array_int_widens_to_float_element() {
    // Pushing an int into a float array is the one allowed coercion.
    let (_, diagnostics) = check(
        "indicator(\"C\")\na = array.new_float(0)\narray.push(a, 3)\nplot(array.get(a, 0))\n",
    );
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
}

#[test]
fn array_int_array_rejects_float_element() {
    // No implicit float -> int narrowing (strict).
    let (_, diagnostics) =
        check("indicator(\"C\")\na = array.new_int(0)\narray.push(a, 1.5)\nplot(close)\n");
    assert!(
        error_codes(&diagnostics).contains(&"E0234"),
        "{diagnostics:?}"
    );
}

#[test]
fn non_array_receiver_is_e0235() {
    let (_, diagnostics) = check("indicator(\"C\")\nx = 3.0\nplot(array.size(x))\n");
    assert!(
        error_codes(&diagnostics).contains(&"E0235"),
        "{diagnostics:?}"
    );
}

#[test]
fn array_of_int_is_not_array_of_float() {
    // concat requires identical element types: array<int> vs array<float>.
    let (_, diagnostics) = check(
        "indicator(\"C\")\na = array.new_int(0)\nb = array.new_float(0)\nc = array.concat(a, b)\nplot(close)\n",
    );
    assert!(
        error_codes(&diagnostics).contains(&"E0234"),
        "{diagnostics:?}"
    );
}

#[test]
fn array_from_infers_element_type() {
    let (dump, diagnostics) =
        check("indicator(\"C\")\na = array.from(1.0, 2.0, 3.0)\nplot(array.get(a, 0))\n");
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
    assert!(dump.contains("array<float>"), "dump: {dump}");
}

#[test]
fn array_from_mixed_types_is_e0234() {
    let (_, diagnostics) = check("indicator(\"C\")\na = array.from(1.0, \"x\")\nplot(close)\n");
    assert!(
        error_codes(&diagnostics).contains(&"E0234"),
        "{diagnostics:?}"
    );
}

#[test]
fn str_split_types_as_string_array() {
    let (dump, diagnostics) =
        check("indicator(\"C\")\np = str.split(\"a,b\", \",\")\nplot(array.size(p))\n");
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
    assert!(dump.contains("array<string>"), "dump: {dump}");
}

#[test]
fn str_format_time_types_as_string() {
    // 1-, 2-, and 3-argument forms all type-check and yield string.
    for src in [
        "indicator(\"C\")\ns = str.format_time(time)\nplot(str.length(s))\n",
        "indicator(\"C\")\ns = str.format_time(time, \"yyyy\")\nplot(str.length(s))\n",
        "indicator(\"C\")\ns = str.format_time(time, \"yyyy\", \"UTC\")\nplot(str.length(s))\n",
    ] {
        let (dump, diagnostics) = check(src);
        assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
        assert!(dump.contains("string"), "dump: {dump}");
    }
}

#[test]
fn str_split_non_string_arg_is_e0234() {
    let (_, diagnostics) = check("indicator(\"C\")\np = str.split(1.0, \",\")\nplot(close)\n");
    assert!(
        error_codes(&diagnostics).contains(&"E0234"),
        "{diagnostics:?}"
    );
}

#[test]
fn array_join_rejects_bool_array() {
    let (_, diagnostics) =
        check("indicator(\"C\")\na = array.new_bool(0)\nx = array.join(a, \",\")\nplot(close)\n");
    assert!(
        error_codes(&diagnostics).contains(&"E0234"),
        "{diagnostics:?}"
    );
}

#[test]
fn array_result_types_are_correct() {
    // array.size -> int; array.get on float array -> float; array.includes -> bool.
    let (dump, diagnostics) = check(
        "indicator(\"C\")\na = array.new_float(2, 0.0)\nn = array.size(a)\ng = array.get(a, 0)\nplot(g)\n",
    );
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
    let _ = dump;
}

// ---------------------------------------------------------------------------
// array.* aggregates / transforms / ordering type checking.
// ---------------------------------------------------------------------------

#[test]
fn array_aggregates_check_clean_and_return_float() {
    let (dump, diagnostics) = check(
        "indicator(\"C\")\na = array.from(1.0, 2.0, 3.0)\ns = array.sum(a)\nplot(s + array.avg(a) + array.stdev(a))\n",
    );
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
    // `s` binds a float aggregate.
    assert!(dump.contains("float"), "dump: {dump}");
}

#[test]
fn array_sum_over_int_array_is_float() {
    // A sum over an array<int> is still a float result. Assigning it to a
    // plot (float) is clean.
    let (_, diagnostics) = check("indicator(\"C\")\na = array.from(1, 2, 3)\nplot(array.sum(a))\n");
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
}

#[test]
fn array_aggregate_on_string_array_is_e0235() {
    // Aggregates require a numeric receiver; a string array is rejected.
    let (_, diagnostics) = check("indicator(\"C\")\na = array.new_string(0)\nplot(array.sum(a))\n");
    assert!(
        error_codes(&diagnostics).contains(&"E0235"),
        "{diagnostics:?}"
    );
}

#[test]
fn array_abs_returns_float_array() {
    // array.abs on an int array yields array<float>; get is float.
    let (dump, diagnostics) = check(
        "indicator(\"C\")\na = array.from(-1, 2, -3)\nb = array.abs(a)\nplot(array.get(b, 0))\n",
    );
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
    assert!(dump.contains("array<float>"), "dump: {dump}");
}

#[test]
fn array_sort_indices_returns_int_array() {
    let (dump, diagnostics) = check(
        "indicator(\"C\")\na = array.from(3.0, 1.0, 2.0)\nidx = array.sort_indices(a)\nplot(array.get(idx, 0))\n",
    );
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
    assert!(dump.contains("array<int>"), "dump: {dump}");
}

#[test]
fn array_sort_with_order_constant_checks_clean() {
    let (_, diagnostics) = check(
        "indicator(\"C\")\na = array.from(3.0, 1.0, 2.0)\narray.sort(a, order.descending)\nplot(array.get(a, 0))\n",
    );
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
}

#[test]
fn order_constants_are_int() {
    let (_, diagnostics) =
        check("indicator(\"C\")\nx = order.ascending\ny = order.descending\nplot(x + y)\n");
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
}

// -----------------------------------------------------------------------
// array.* remainder type-checking.
// -----------------------------------------------------------------------

#[test]
fn array_binary_search_returns_int() {
    let (dump, diagnostics) = check(
        "indicator(\"C\")\na = array.from(1.0, 2.0, 3.0)\ni = array.binary_search(a, 2.0)\nplot(i)\n",
    );
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
    // `i` is int; plotting it coerces to float, but the binding types as int.
    assert!(dump.contains("`i`: series int"), "dump: {dump}");
}

#[test]
fn array_percentrank_and_percentile_return_float() {
    let (_, d1) =
        check("indicator(\"C\")\na = array.from(1.0, 2.0, 3.0)\nplot(array.percentrank(a, 1))\n");
    assert!(d1.is_empty(), "percentrank: {d1:?}");
    let (_, d2) = check(
        "indicator(\"C\")\na = array.from(1.0, 2.0, 3.0)\nplot(array.percentile_nearest_rank(a, 50.0))\n",
    );
    assert!(d2.is_empty(), "nearest_rank: {d2:?}");
    let (_, d3) = check(
        "indicator(\"C\")\na = array.from(1.0, 2.0, 3.0)\nplot(array.percentile_linear_interpolation(a, 50.0))\n",
    );
    assert!(d3.is_empty(), "linear interp: {d3:?}");
}

#[test]
fn array_percentile_accepts_int_percentage_via_coercion() {
    // The percentage argument coerces int -> float, so a literal int is fine.
    let (_, diagnostics) = check(
        "indicator(\"C\")\na = array.from(1.0, 2.0, 3.0)\nplot(array.percentile_nearest_rank(a, 50))\n",
    );
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
}

#[test]
fn array_covariance_biased_defaults_and_checks_clean() {
    // Two-arg form (biased defaults to true) and three-arg form both check.
    let (_, d1) = check(
        "indicator(\"C\")\na = array.from(1.0, 2.0, 3.0)\nb = array.from(2.0, 4.0, 6.0)\nplot(array.covariance(a, b))\n",
    );
    assert!(d1.is_empty(), "two-arg: {d1:?}");
    let (_, d2) = check(
        "indicator(\"C\")\na = array.from(1.0, 2.0, 3.0)\nb = array.from(2.0, 4.0, 6.0)\nplot(array.covariance(a, b, false))\n",
    );
    assert!(d2.is_empty(), "three-arg: {d2:?}");
}

#[test]
fn array_covariance_on_string_array_is_rejected() {
    // A non-numeric receiver is E0235 (require_numeric_array).
    let (_, diagnostics) = check(
        "indicator(\"C\")\na = array.from(\"x\", \"y\")\nb = array.from(\"p\", \"q\")\nplot(array.covariance(a, b))\n",
    );
    assert!(
        diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0235")),
        "{diagnostics:?}"
    );
}

#[test]
fn array_binary_search_on_bool_array_is_rejected() {
    // Binary search requires a numeric receiver.
    let (_, diagnostics) = check(
        "indicator(\"C\")\na = array.from(true, false)\nplot(array.binary_search(a, true))\n",
    );
    assert!(
        diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0235")),
        "{diagnostics:?}"
    );
}

#[test]
fn array_every_and_some_type_check_as_bool_on_bool_and_numeric_arrays() {
    // Bool receiver: array.every / array.some type as bool (method + namespace).
    for src in [
        "indicator(\"C\")\na = array.from(true, false)\nplot(array.every(a) ? 1.0 : 0.0)\n",
        "indicator(\"C\")\na = array.from(1.0, 2.0)\nplot(a.some() ? 1.0 : 0.0)\n",
        "indicator(\"C\")\na = array.from(1, 0)\nplot(array.every(a) ? 1.0 : 0.0)\n",
    ] {
        let (_, diagnostics) = check(src);
        assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
    }
}

#[test]
fn array_every_on_string_array_is_rejected() {
    // Boolean reductions require a bool or numeric receiver, not string.
    let (_, diagnostics) =
        check("indicator(\"C\")\na = array.from(\"x\", \"y\")\nplot(array.every(a) ? 1.0 : 0.0)\n");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0235")),
        "{diagnostics:?}"
    );
}

#[test]
fn array_every_takes_no_callback_argument() {
    // `array.every(id)` is unary; a second argument is an arity error.
    let (_, diagnostics) = check(
        "indicator(\"C\")\na = array.from(true, false)\nplot(array.every(a, true) ? 1.0 : 0.0)\n",
    );
    assert!(
        !diagnostics.is_empty(),
        "expected an arity diagnostic for the extra argument"
    );
}

#[test]
fn array_binary_search_method_form_equals_namespace_form() {
    // Method syntax works for these functions too.
    let (_, diagnostics) =
        check("indicator(\"C\")\na = array.from(1.0, 2.0, 3.0)\nplot(a.binary_search(2.0))\n");
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
}

// -----------------------------------------------------------------------
// map.*
// -----------------------------------------------------------------------

#[test]
fn map_constructor_and_ops_check_clean() {
    // `map.put` returns the previous value; bind it so it is not discarded.
    let (_, diagnostics) = check(
        "indicator(\"C\")\nm = map.new_string_float()\np = map.put(m, \"a\", 1.0)\nplot(map.get(m, \"a\"))\n",
    );
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
}

#[test]
fn map_put_result_discarded_warns_w0002() {
    // `map.put` is value-producing (returns the previous value); using it as a
    // bare statement discards that value and warns W0002.
    let (_, diagnostics) = check(
        "indicator(\"C\")\nm = map.new_string_float()\nmap.put(m, \"a\", 1.0)\nplot(map.get(m, \"a\"))\n",
    );
    assert!(
        diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("W0002")),
        "{diagnostics:?}"
    );
}

#[test]
fn map_new_infers_map_type() {
    let (dump, diagnostics) = check("indicator(\"C\")\nm = map.new_string_float()\nplot(close)\n");
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
    assert!(dump.contains("map<string, float>"), "dump: {dump}");
}

#[test]
fn map_get_returns_value_type() {
    let (dump, diagnostics) = check(
        "indicator(\"C\")\nm = map.new_int_float()\np = map.put(m, 1, 2.0)\nv = map.get(m, 1)\nplot(v)\n",
    );
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
    // v is a float (the value type).
    assert!(dump.contains("float"), "dump: {dump}");
}

#[test]
fn map_keys_returns_array_of_key_type() {
    let (dump, diagnostics) = check(
        "indicator(\"C\")\nm = map.new_string_float()\nks = map.keys(m)\nplot(array.size(ks))\n",
    );
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
    assert!(dump.contains("array<string>"), "dump: {dump}");
}

#[test]
fn map_values_returns_array_of_value_type() {
    let (dump, diagnostics) = check(
        "indicator(\"C\")\nm = map.new_string_int()\nvs = map.values(m)\nplot(array.size(vs))\n",
    );
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
    assert!(dump.contains("array<int>"), "dump: {dump}");
}

#[test]
fn map_key_type_mismatch_is_e0234() {
    // Passing an int key to a map<string, float> is a type error.
    let (_, diagnostics) =
        check("indicator(\"C\")\nm = map.new_string_float()\nplot(map.get(m, 3))\n");
    assert!(
        error_codes(&diagnostics).contains(&"E0234"),
        "{diagnostics:?}"
    );
}

#[test]
fn map_value_type_mismatch_is_e0234() {
    // Putting a string value into a map<string, float>.
    let (_, diagnostics) = check(
        "indicator(\"C\")\nm = map.new_string_float()\nx = map.put(m, \"a\", \"nope\")\nplot(close)\n",
    );
    assert!(
        error_codes(&diagnostics).contains(&"E0234"),
        "{diagnostics:?}"
    );
}

#[test]
fn map_int_key_value_widens_to_float() {
    // A map<int, float> accepts an int value argument (int -> float widening).
    let (_, diagnostics) = check(
        "indicator(\"C\")\nm = map.new_int_float()\nx = map.put(m, 1, 5)\nplot(map.get(m, 1))\n",
    );
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
}

#[test]
fn non_map_receiver_is_e0235() {
    let (_, diagnostics) = check("indicator(\"C\")\nx = 3.0\nplot(map.size(x))\n");
    assert!(
        error_codes(&diagnostics).contains(&"E0235"),
        "{diagnostics:?}"
    );
}

#[test]
fn map_put_all_mismatched_types_is_e0234() {
    let (_, diagnostics) = check(
        "indicator(\"C\")\na = map.new_string_float()\nb = map.new_string_int()\nmap.put_all(a, b)\nplot(close)\n",
    );
    assert!(
        error_codes(&diagnostics).contains(&"E0234"),
        "{diagnostics:?}"
    );
}

#[test]
fn map_contains_returns_bool() {
    let (dump, diagnostics) = check(
        "indicator(\"C\")\nm = map.new_string_float()\nb = map.contains(m, \"a\")\nplot(b ? 1.0 : 0.0)\n",
    );
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
    assert!(dump.contains("bool"), "dump: {dump}");
}

#[test]
fn map_copy_returns_same_map_type() {
    let (dump, diagnostics) =
        check("indicator(\"C\")\nm = map.new_int_color()\nc = map.copy(m)\nplot(map.size(c))\n");
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
    assert!(dump.contains("map<int, color>"), "dump: {dump}");
}

#[test]
fn map_constructor_rejects_arguments() {
    let (_, diagnostics) =
        check("indicator(\"C\")\nm = map.new_string_float(5)\nplot(map.size(m))\n");
    assert!(
        error_codes(&diagnostics).contains(&"E0204"),
        "{diagnostics:?}"
    );
}

// ---------------------------------------------------------------------------
// matrix.* — checker / typing tests.
// ---------------------------------------------------------------------------

#[test]
fn matrix_new_infers_matrix_type() {
    let (dump, diagnostics) =
        check("indicator(\"M\")\nm = matrix.new_float(2, 2, 0.0)\nplot(close)\n");
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
    assert!(dump.contains("matrix<float>"), "dump: {dump}");
}

#[test]
fn matrix_new_without_initial_ok() {
    let (dump, diagnostics) = check("indicator(\"M\")\nm = matrix.new_int(2, 3)\nplot(close)\n");
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
    assert!(dump.contains("matrix<int>"), "dump: {dump}");
}

#[test]
fn matrix_get_returns_element_type() {
    let (dump, diagnostics) = check(
        "indicator(\"M\")\nm = matrix.new_float(1, 1, 0.0)\nv = matrix.get(m, 0, 0)\nplot(v)\n",
    );
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
    assert!(dump.contains("float"), "dump: {dump}");
}

#[test]
fn matrix_rows_returns_int() {
    let (dump, diagnostics) =
        check("indicator(\"M\")\nm = matrix.new_float(2, 3, 0.0)\nr = matrix.rows(m)\nplot(r)\n");
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
    assert!(dump.contains("int"), "dump: {dump}");
}

#[test]
fn matrix_line_results_preserve_receiver_element_type_in_both_call_forms() {
    let cases = [
        (
            "matrix.new<int>(1, 1, 7)",
            "matrix.row(m, 0)",
            "array<int>",
            "plot(array.get(result, 0))",
        ),
        (
            "matrix.new<string>(1, 1, \"kept\")",
            "m.col(0)",
            "array<string>",
            "plot(result.get(0) == \"kept\" ? 1.0 : 0.0)",
        ),
    ];
    for (constructor, operation, expected_type, observation) in cases {
        let source =
            format!("indicator(\"M\")\nm = {constructor}\nresult = {operation}\n{observation}\n");
        let (dump, diagnostics) = check(&source);
        assert!(diagnostics.is_empty(), "{operation}: {diagnostics:?}");
        assert!(dump.contains(expected_type), "{operation}: {dump}");
    }
}

#[test]
fn matrix_removals_return_typed_arrays_in_both_call_forms() {
    let cases = [
        (
            "matrix.new<int>(2, 1, 7)",
            "m.remove_row(0)",
            "array<int>",
            "plot(removed.get(0))",
        ),
        (
            "matrix.new<string>(1, 2, \"kept\")",
            "matrix.remove_col(m, 0)",
            "array<string>",
            "plot(array.get(removed, 0) == \"kept\" ? 1.0 : 0.0)",
        ),
    ];
    for (constructor, operation, expected_type, observation) in cases {
        let source =
            format!("indicator(\"M\")\nm = {constructor}\nremoved = {operation}\n{observation}\n");
        let (dump, diagnostics) = check(&source);
        assert!(diagnostics.is_empty(), "{operation}: {diagnostics:?}");
        assert!(dump.contains(expected_type), "{operation}: {dump}");
    }
}

#[test]
fn discarded_matrix_removal_results_do_not_warn() {
    // Removal is a mutation-first operation whose returned line is optional to
    // consume, just like array.pop/shift/remove.
    let (_, diagnostics) = check(
        "indicator(\"M\")\nm = matrix.new<int>(2, 2, 1)\nm.remove_row(0)\nmatrix.remove_col(m, 0)\nplot(matrix.get(m, 0, 0))\n",
    );
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
}

#[test]
fn matrix_structural_line_indices_must_be_int() {
    for operation in [
        "matrix.row(m, 0.5)",
        "matrix.col(m, 0.5)",
        "matrix.remove_row(m, 0.5)",
        "matrix.remove_col(m, 0.5)",
    ] {
        let source = format!(
            "indicator(\"M\")\nm = matrix.new<string>(1, 1, \"x\")\nv = {operation}\nplot(close)\n"
        );
        let (_, diagnostics) = check(&source);
        assert!(
            error_codes(&diagnostics).contains(&"E0234"),
            "{operation}: {diagnostics:?}"
        );
    }
}

#[test]
fn matrix_removal_index_remains_required() {
    for operation in ["matrix.remove_row(m)", "m.remove_col()"] {
        let source = format!(
            "indicator(\"M\")\nm = matrix.new<int>(1, 1, 0)\nv = {operation}\nplot(close)\n"
        );
        let (_, diagnostics) = check(&source);
        assert!(
            error_codes(&diagnostics).contains(&"E0204"),
            "{operation}: {diagnostics:?}"
        );
    }
}

#[test]
fn matrix_transpose_returns_matrix() {
    let (dump, diagnostics) = check(
        "indicator(\"M\")\nm = matrix.new_float(2, 3, 0.0)\nt = matrix.transpose(m)\nplot(matrix.rows(t))\n",
    );
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
    assert!(dump.contains("matrix<float>"), "dump: {dump}");
}

#[test]
fn float_valued_matrix_operations_widen_int_receivers() {
    for expression in [
        "matrix.sum(m, m)",
        "matrix.diff(m, m)",
        "matrix.mult(m, m)",
        "matrix.kron(m, m)",
        "matrix.pow(m, 2)",
        "matrix.inv(m)",
        "matrix.eigenvectors(m)",
        "matrix.pinv(m)",
    ] {
        let source = format!(
            "indicator(\"M\")\nm = matrix.new_int(2, 2, 0)\nmatrix.set(m, 0, 0, 1)\nmatrix.set(m, 1, 1, 1)\nr = {expression}\nplot(matrix.get(r, 0, 0))\n"
        );
        let (dump, diagnostics) = check(&source);
        assert!(diagnostics.is_empty(), "{expression}: {diagnostics:?}");
        assert!(dump.contains("matrix<float>"), "{expression}: {dump}");
    }
}

#[test]
fn matrix_sort_is_a_void_mutator() {
    let (dump, diagnostics) =
        check("indicator(\"M\")\nm = matrix.new<int>(2, 1, 0)\nr = matrix.sort(m)\nplot(1)\n");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0212")),
        "{diagnostics:?}"
    );
    assert!(
        !dump.contains("r ="),
        "void sort must not bind a result: {dump}"
    );

    let (dump, diagnostics) =
        check("indicator(\"M\")\nm = matrix.new<string>(2, 1, \"\")\nmatrix.sort(m)\nplot(1)\n");
    assert!(
        diagnostics.is_empty(),
        "string matrix sort should be accepted: {diagnostics:?}"
    );
    assert!(dump.contains("matrix.sort"), "{dump}");
}

#[test]
fn matrix_sort_udt_overload_binds_const_field_indices_and_names() {
    let accepted = [
        "matrix.sort(m, 0, order.ascending, 0)",
        "matrix.sort(m, sort_field = \"score\")",
        "matrix.sort(m, column = 0, order = order.descending, sort_field = fieldName)",
    ];
    for call in accepted {
        let source = format!(
            "indicator(\"M\")\ntype Item\n    float score\n    string name\nconst string fieldName = \"name\"\nm = matrix.new<Item>(1, 1, Item.new(1.0, \"a\"))\n{call}\nplot(m.get(0, 0).score)\n"
        );
        let (_, diagnostics) = check(&source);
        assert!(diagnostics.is_empty(), "{call}: {diagnostics:?}");
    }
}

#[test]
fn matrix_sort_udt_rejects_nonconst_missing_and_nonscalar_fields() {
    for (declaration, call) in [
        (
            "fieldIndex = input.int(0)",
            "matrix.sort(m, 0, order.ascending, fieldIndex)",
        ),
        ("", "matrix.sort(m, 0, order.ascending, \"missing\")"),
        ("", "matrix.sort(m, 0, order.ascending, 1)"),
    ] {
        let source = format!(
            "indicator(\"M\")\ntype Item\n    float score\n    Item child\n{declaration}\nm = matrix.new<Item>(1, 1, Item.new(1.0, na))\n{call}\nplot(1)\n"
        );
        let (_, diagnostics) = check(&source);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.severity == Severity::Error),
            "{call}: {diagnostics:?}"
        );
    }
}

#[test]
fn matrix_reverse_is_void_and_binding_it_is_e0212() {
    // reverse mutates in place and the VM returns na. Typing it as a matrix
    // let the binding compile, and any use of the bound name then aborted the
    // run with na_method_receiver. Worse, because a non-void result is subject
    // to the W0002 discard warning, the correct bare call warned and the
    // broken binding did not, steering callers into the abort.
    let (_, diagnostics) = check(
        "indicator(\"M\")\nm = matrix.new_float(2, 2, 1.0)\nr = matrix.reverse(m)\nplot(matrix.get(r, 0, 0))\n",
    );
    assert!(
        error_codes(&diagnostics).contains(&"E0212"),
        "{diagnostics:?}"
    );
}

#[test]
fn matrix_reverse_bare_call_does_not_warn() {
    // The bare call is the correct usage, so it must be silent.
    let (_, diagnostics) = check(
        "indicator(\"M\")\nm = matrix.new_float(2, 2, 1.0)\nmatrix.reverse(m)\nplot(matrix.get(m, 0, 0))\n",
    );
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
}

#[test]
fn matrix_receiver_type_error_is_e0235() {
    // Passing a non-matrix (an int) as the receiver.
    let (_, diagnostics) = check("indicator(\"M\")\nplot(matrix.rows(close))\n");
    assert!(
        error_codes(&diagnostics).contains(&"E0235"),
        "{diagnostics:?}"
    );
}

#[test]
fn matrix_set_element_type_mismatch_is_e0234() {
    // Setting a string cell in a float matrix.
    let (_, diagnostics) = check(
        "indicator(\"M\")\nm = matrix.new_float(1, 1, 0.0)\nmatrix.set(m, 0, 0, \"nope\")\nplot(close)\n",
    );
    assert!(
        error_codes(&diagnostics).contains(&"E0234"),
        "{diagnostics:?}"
    );
}

#[test]
fn matrix_det_on_non_numeric_is_e0234() {
    // Linear-algebra ops require a numeric matrix; a string matrix is rejected.
    let (_, diagnostics) =
        check("indicator(\"M\")\nm = matrix.new_string(2, 2, \"x\")\nplot(matrix.det(m))\n");
    assert!(
        error_codes(&diagnostics).contains(&"E0234"),
        "{diagnostics:?}"
    );
}

#[test]
fn matrix_sum_mismatched_element_type_is_e0234() {
    // matrix.sum with a matrix of a different element type.
    let (_, diagnostics) = check(
        "indicator(\"M\")\na = matrix.new_float(1, 1, 0.0)\nb = matrix.new_int(1, 1, 0)\nc = matrix.sum(a, b)\nplot(close)\n",
    );
    assert!(
        error_codes(&diagnostics).contains(&"E0234"),
        "{diagnostics:?}"
    );
}

#[test]
fn matrix_sum_accepts_scalar() {
    let (dump, diagnostics) = check(
        "indicator(\"M\")\na = matrix.new_float(1, 1, 0.0)\nc = matrix.sum(a, 5.0)\nplot(matrix.get(c, 0, 0))\n",
    );
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
    assert!(dump.contains("matrix<float>"), "dump: {dump}");
}

#[test]
fn matrix_new_dimensions_are_optional() {
    for call in ["matrix.new_float()", "matrix.new_float(2)"] {
        let (dump, diagnostics) = check(&format!("indicator(\"M\")\nm = {call}\nplot(close)\n"));
        assert!(diagnostics.is_empty(), "{call}: {diagnostics:?}");
        assert!(dump.contains("matrix<float>"), "{call}: {dump}");
    }
}

#[test]
fn matrix_new_named_arguments_match_positional_hir() {
    // `matrix.new<T>` binds the parameter names `rows`, `columns`, and
    // `initial_value` in any order. Each named spelling types as `matrix<T>`
    // with no diagnostics and lowers to the same HIR as its positional call.
    let cases = [
        (
            "matrix<float>",
            "matrix.new<float>(2, 3, 1.5)",
            "matrix.new<float>(columns = 3, rows = 2, initial_value = 1.5)",
        ),
        (
            "matrix<float>",
            "matrix.new<float>(2, 3, 1.5)",
            "matrix.new<float>(2, initial_value = 1.5, columns = 3)",
        ),
        (
            "matrix<int>",
            "matrix.new<int>(2, 3, 7)",
            "matrix.new<int>(initial_value = 7, rows = 2, columns = 3)",
        ),
        (
            "matrix<int>",
            "matrix.new<int>(2, 3)",
            "matrix.new<int>(columns = 3, rows = 2)",
        ),
        (
            "matrix<string>",
            "matrix.new_string(1, 2, \"x\")",
            "matrix.new_string(1, 2, initial_value = \"x\")",
        ),
        (
            "matrix<bool>",
            "matrix.new<bool>(2, 1, true)",
            "matrix.new<bool>(initial_value = true, columns = 1, rows = 2)",
        ),
        (
            "matrix<color>",
            "matrix.new<color>(1, 2, color.red)",
            "matrix.new<color>(columns = 2, rows = 1, initial_value = color.red)",
        ),
    ];
    for (ty, positional_call, named_call) in cases {
        let source = |call: &str| format!("indicator(\"M\")\nm = {call}\nplot(close)\n");
        let (positional, d1) = check(&source(positional_call));
        let (named, d2) = check(&source(named_call));
        assert!(d1.is_empty(), "{positional_call}: {d1:?}");
        assert!(d2.is_empty(), "{named_call}: {d2:?}");
        assert!(named.contains(ty), "{named_call}: {named}");
        assert_eq!(
            named, positional,
            "{named_call} must lower like {positional_call}"
        );
    }
}

#[test]
fn matrix_new_unknown_parameter_name_is_e0228_with_suggestion() {
    // `colums` is a typo for `columns`; the did-you-mean note names it.
    let (_, diagnostics) =
        check("indicator(\"M\")\nm = matrix.new<float>(rows = 2, colums = 3)\nplot(close)\n");
    let diag = diagnostic_with_code(&diagnostics, "E0228");
    assert!(
        diag.message.contains("`colums`"),
        "unexpected message: {diag:?}"
    );
    assert!(
        diag.notes.iter().any(|note| note.contains("`columns`")),
        "expected a did-you-mean note pointing at `columns`: {diag:?}"
    );
    // A name matching no parameter is rejected instead of silently binding by
    // position. The call reports only that binding error: the unbound values
    // are not analyzed on their own, so a named `na` adds no `E0220`.
    for call in [
        "matrix.new<float>(2, 3, bogus = 1.0)",
        "matrix.new<float>(2, 3, bogus = na)",
    ] {
        let (_, diagnostics) = check(&format!("indicator(\"M\")\nm = {call}\nplot(close)\n"));
        assert_eq!(
            error_codes(&diagnostics),
            ["E0228"],
            "{call}: {diagnostics:?}"
        );
    }
    // The spelling `initial` is too far from `initial_value` for the
    // edit-distance suggestion, so the constructor points at the rename.
    let (_, diagnostics) =
        check("indicator(\"M\")\nm = matrix.new<float>(2, 3, initial = 1.0)\nplot(close)\n");
    assert_eq!(error_codes(&diagnostics), ["E0228"], "{diagnostics:?}");
    let diag = diagnostic_with_code(&diagnostics, "E0228");
    assert!(
        diag.message.contains("has no parameter named `initial`"),
        "unexpected message: {diag:?}"
    );
    assert!(
        diag.notes
            .iter()
            .any(|note| note.contains("did you mean `initial_value`?")),
        "expected a note pointing at `initial_value`: {diag:?}"
    );
}

#[test]
fn matrix_new_non_int_dimension_names_the_parameter() {
    // `rows`/`columns` are dimensions, not cells: a non-int one is `E0234`
    // naming that parameter and pointing at its value wherever it is written.
    for (call, param, value) in [
        ("matrix.new<float>(rows = 2.5, columns = 2)", "rows", "2.5"),
        (
            "matrix.new<float>(columns = \"x\", rows = 2)",
            "columns",
            "\"x\"",
        ),
        ("matrix.new<string>(1, 2.0, \"s\")", "columns", "2.0"),
    ] {
        let source = format!("indicator(\"M\")\nm = {call}\nplot(close)\n");
        let (_, diagnostics) = check(&source);
        assert_eq!(
            error_codes(&diagnostics),
            ["E0234"],
            "{call}: {diagnostics:?}"
        );
        let diag = diagnostic_with_code(&diagnostics, "E0234");
        assert!(
            diag.message
                .contains(&format!("`matrix.new` {param} must be int")),
            "{call}: unexpected message: {diag:?}"
        );
        let span = diag.primary_span().expect("E0234 has a primary span");
        assert_eq!(
            &source[span.start as usize..span.end as usize],
            value,
            "{call}"
        );
    }
}

#[test]
fn matrix_new_duplicate_rows_binding_is_e0228() {
    // `2` fills `rows` positionally; `rows = 4` then names it again.
    let (_, diagnostics) =
        check("indicator(\"M\")\nm = matrix.new<float>(2, 3, rows = 4)\nplot(close)\n");
    let diag = diagnostic_with_code(&diagnostics, "E0228");
    assert!(
        diag.message.contains("`rows` is bound more than once"),
        "unexpected message: {diag:?}"
    );
}

#[test]
fn matrix_new_named_initial_value_can_omit_dimensions() {
    // Rows and columns are optional. A named initial value must bind to
    // its own parameter without shifting into either dimension slot.
    for call in [
        "matrix.new<float>(2, initial_value = 7.0)",
        "matrix.new<float>(rows = 2, initial_value = 7)",
        "matrix.new<float>(rows = 2, initial_value = na)",
    ] {
        let (_, diagnostics) = check(&format!("indicator(\"M\")\nm = {call}\nplot(close)\n"));
        assert!(diagnostics.is_empty(), "{call}: {diagnostics:?}");
    }
}

#[test]
fn matrix_is_square_returns_bool() {
    let (dump, diagnostics) = check(
        "indicator(\"M\")\nm = matrix.new_float(2, 2, 0.0)\nb = matrix.is_square(m)\nplot(b ? 1.0 : 0.0)\n",
    );
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
    assert!(dump.contains("bool"), "dump: {dump}");
}

#[test]
fn matrix_add_row_requires_matching_array() {
    // add_row of an int array into a float matrix is a type error.
    let (_, diagnostics) = check(
        "indicator(\"M\")\nm = matrix.new_float(1, 2, 0.0)\na = array.new_int(2, 0)\nmatrix.add_row(m, 1, a)\nplot(close)\n",
    );
    assert!(
        error_codes(&diagnostics).contains(&"E0234"),
        "{diagnostics:?}"
    );
}

// ---- collection syntax sugar (semantic checks) --------------------------

#[test]
fn method_call_desugars_and_types_like_namespace_form() {
    // `a.size()` types as int, exactly like `array.size(a)`.
    let (dump, diagnostics) =
        check("indicator(\"M\")\na = array.new_float(0)\na.push(1.0)\nn = a.size()\nplot(n)\n");
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
    // The desugared call lowers to the same CallCollection as the namespace
    // form: the HIR dump names `array.push` / `array.size`.
    assert!(dump.contains("array.push"), "dump: {dump}");
    assert!(dump.contains("array.size"), "dump: {dump}");
}

#[test]
fn unknown_method_on_collection_is_e0235() {
    // `a.frobnicate()` — the receiver is an array but `frobnicate` is not a
    // method of `array`.
    let (_, diagnostics) =
        check("indicator(\"M\")\na = array.new_float(0)\nplot(a.frobnicate())\n");
    assert!(
        error_codes(&diagnostics).contains(&"E0235"),
        "{diagnostics:?}"
    );
}

#[test]
fn method_call_on_non_collection_is_not_a_method() {
    // A method-style call on a non-collection value is not a method call; it
    // falls through to the unknown-function path (`x.push` is unknown).
    let (_, diagnostics) = check("indicator(\"M\")\nx = 3.0\nplot(x.push(1.0))\n");
    assert!(
        !error_codes(&diagnostics).is_empty(),
        "expected an error for a method on a non-collection: {diagnostics:?}"
    );
}

#[test]
fn for_in_binds_element_and_desugars_to_array_get() {
    let (dump, diagnostics) = check(
        "indicator(\"F\")\ndata = array.from(1.0, 2.0)\nt = 0.0\nfor x in data\n\tt := t + x\nplot(t)\n",
    );
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
    // The desugar reads elements through `array.get` and bounds via
    // `array.size`.
    assert!(dump.contains("array.get"), "dump: {dump}");
    assert!(dump.contains("array.size"), "dump: {dump}");
}

#[test]
fn for_in_over_non_array_is_e0235() {
    let (_, diagnostics) = check("indicator(\"F\")\nx = 3.0\nfor e in x\n\tplot(e)\n");
    assert!(
        error_codes(&diagnostics).contains(&"E0235"),
        "{diagnostics:?}"
    );
}

#[test]
fn for_in_over_matrix_binds_rows() {
    // A matrix is iterated row by row: `for row in m` and `for [i, row] in m`
    // bind each row as an `array<T>`.
    for source in [
        "//@version=6\nindicator(\"F\")\nm = matrix.new<float>(2, 2, 0.0)\n\
         s = 0.0\nfor row in m\n    s += row.sum()\nplot(s)\n",
        "//@version=6\nindicator(\"F\")\nm = matrix.new<float>(2, 2, 0.0)\n\
         s = 0\nfor [i, row] in m\n    s += i + row.size()\nplot(s)\n",
    ] {
        let (dump, diagnostics) = check(source);
        assert!(diagnostics.is_empty(), "{source:?}: {diagnostics:?}");
        // Rows come from `matrix.row`, bounded by `matrix.rows`.
        assert!(dump.contains("matrix.row"), "dump: {dump}");
        assert!(dump.contains("matrix.rows"), "dump: {dump}");
    }
}

#[test]
fn for_in_over_map_binds_keys_and_values() {
    // A map is iterated as `for [key, value] in m`.
    let (dump, diagnostics) = check(
        "//@version=6\nindicator(\"F\")\nm = map.new<string, float>()\n\
         s = \"\"\nt = 0.0\nfor [key, value] in m\n    s += key\n    t += value\nplot(t)\n",
    );
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    // The keys are snapshotted once with `map.keys`; each value is read
    // with `map.get`, with no second snapshot of the values.
    assert!(dump.contains("map.keys"), "dump: {dump}");
    assert!(dump.contains("map.get"), "dump: {dump}");
    assert!(!dump.contains("map.values"), "dump: {dump}");
    // A body that may write a map snapshots the values too.
    let (dump, diagnostics) = check(
        "//@version=6\nindicator(\"F\")\nm = map.new<string, float>()\n\
         t = 0.0\nfor [key, value] in m\n    t += value\n    m.put(key, 1.0)\nplot(t)\n",
    );
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert!(dump.contains("map.values"), "dump: {dump}");
}

#[test]
fn for_in_over_map_without_key_binding_is_e0235() {
    // A map has no single-binding `for … in` form.
    let (_dump, diagnostics) = check(
        "//@version=6\nindicator(\"F\")\nm = map.new<string, float>()\n\
         for value in m\n    x = 1\nplot(close)\n",
    );
    assert_eq!(error_codes(&diagnostics), vec!["E0235"], "{diagnostics:?}");
}

#[test]
fn method_call_receiver_shadowing_namespace_does_not_panic() {
    // A variable named like a namespace root: `array.push` resolves as the
    // namespace builtin (arity error), not a method call. Must not panic.
    let (_, diagnostics) = check("indicator(\"M\")\narr = array.new_float(0)\nplot(arr.size())\n");
    // `arr.size()` is a valid method call → no error.
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
}

// -- enums -------------------------------------------------------------------

#[test]
fn enum_member_types_as_enum_const() {
    let (dump, diagnostics) = check(
        "indicator(\"E\")\nenum Dir\n    up\n    down\n\
         d = Dir.down\nplot(d == Dir.up ? 1.0 : 0.0)\n",
    );
    assert!(
        !has_errors(&diagnostics),
        "unexpected diagnostics: {diagnostics:?}"
    );
    // The second member lowers to variant index 1, typed as the enum.
    assert!(dump.contains("ConstInt 1 : const Dir"), "dump:\n{dump}");
    // The first member is index 0.
    assert!(dump.contains("ConstInt 0 : const Dir"), "dump:\n{dump}");
    // The variable's declared type is the enum.
    assert!(dump.contains("`d`: const Dir"), "dump:\n{dump}");
}

#[test]
fn enum_equality_is_allowed() {
    let (_dump, diagnostics) = check(
        "indicator(\"E\")\nenum Dir\n    up\n    down\n\
         d = Dir.up\nplot(d == Dir.up ? 1.0 : 2.0)\n",
    );
    assert!(
        !has_errors(&diagnostics),
        "enum equality should be allowed: {diagnostics:?}"
    );
}

#[test]
fn enum_switch_is_allowed() {
    let (_dump, diagnostics) = check(
        "indicator(\"E\")\nenum Osc\n    rsi\n    mfi\n\
         o = Osc.rsi\n\
         v = switch o\n    Osc.rsi => 1.0\n    Osc.mfi => 2.0\nplot(v)\n",
    );
    assert!(
        !has_errors(&diagnostics),
        "enum switch should be allowed: {diagnostics:?}"
    );
}

#[test]
fn duplicate_enum_name_is_e0239() {
    let (_dump, diagnostics) =
        check("indicator(\"E\")\nenum Dir\n    up\nenum Dir\n    down\nplot(close)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0239"]);
}

#[test]
fn duplicate_enum_member_is_e0239() {
    let (_dump, diagnostics) = check("indicator(\"E\")\nenum Dir\n    up\n    up\nplot(close)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0239"]);
}

#[test]
fn unknown_enum_member_is_e0239() {
    let (_dump, diagnostics) = check(
        "indicator(\"E\")\nenum Dir\n    up\n    down\n\
         d = Dir.sideways\nplot(close)\n",
    );
    assert_eq!(error_codes(&diagnostics), vec!["E0239"]);
}

#[test]
fn comparing_enum_to_int_is_e0207() {
    let (_dump, diagnostics) = check(
        "indicator(\"E\")\nenum Dir\n    up\n    down\n\
         d = Dir.up\nplot(d == 0 ? 1.0 : 2.0)\n",
    );
    assert_eq!(error_codes(&diagnostics), vec!["E0207"]);
}

#[test]
fn distinct_enums_do_not_compare() {
    // Nominal typing: two enums with the same members are still distinct.
    let (_dump, diagnostics) = check(
        "indicator(\"E\")\nenum A\n    x\nenum B\n    x\n\
         a = A.x\nb = B.x\nplot(a == b ? 1.0 : 2.0)\n",
    );
    assert_eq!(error_codes(&diagnostics), vec!["E0207"]);
}

// -----------------------------------------------------------------------
// User-defined types (`type Name`, fields, `.new()`/`.copy()`, field
// access, reference semantics, E0236/E0237).
// -----------------------------------------------------------------------

#[test]
fn type_new_and_field_read_type_as_field_type() {
    let (dump, diagnostics) = check(
        "indicator(\"U\")\ntype Point\n    float x\n    float y\n\
         p = Point.new(1.0, 2.0)\nplot(p.x)\n",
    );
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
    assert!(dump.contains("NewObject Point"));
    assert!(dump.contains("GetField .0 : series float"));
}

#[test]
fn duplicate_type_name_is_e0236() {
    let (_dump, diagnostics) =
        check("indicator(\"U\")\ntype Point\n    float x\ntype Point\n    float y\nplot(close)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0236"]);
}

#[test]
fn duplicate_field_is_e0236() {
    let (_dump, diagnostics) =
        check("indicator(\"U\")\ntype Point\n    float x\n    float x\nplot(close)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0236"]);
}

#[test]
fn unknown_field_type_is_e0236() {
    let (_dump, diagnostics) = check("indicator(\"U\")\ntype Point\n    Widget w\nplot(close)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0236"]);
}

#[test]
fn type_name_colliding_with_enum_is_e0236() {
    let (_dump, diagnostics) =
        check("indicator(\"U\")\nenum Dir\n    up\ntype Dir\n    float x\nplot(close)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0236"]);
}

#[test]
fn field_type_mismatch_on_new_is_e0237() {
    // A string argument for a float field.
    let (_dump, diagnostics) = check(
        "indicator(\"U\")\ntype Point\n    float x\n\
         p = Point.new(\"hi\")\nplot(close)\n",
    );
    assert_eq!(error_codes(&diagnostics), vec!["E0237"]);
}

#[test]
fn field_type_mismatch_on_write_is_e0237() {
    let (_dump, diagnostics) = check(
        "indicator(\"U\")\ntype Point\n    float x\n\
         p = Point.new(1.0)\np.x := \"hi\"\nplot(close)\n",
    );
    assert_eq!(error_codes(&diagnostics), vec!["E0237"]);
}

#[test]
fn unknown_field_read_is_e0237() {
    let (_dump, diagnostics) = check(
        "indicator(\"U\")\ntype Point\n    float x\n\
         p = Point.new(1.0)\nplot(p.z)\n",
    );
    assert_eq!(error_codes(&diagnostics), vec!["E0237"]);
}

#[test]
fn int_field_default_widens_to_float() {
    // An int literal default for a float field is coerced (no error).
    let (_dump, diagnostics) =
        check("indicator(\"U\")\ntype Point\n    float x = 3\np = Point.new()\nplot(p.x)\n");
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
}

#[test]
fn self_referential_field_type_is_allowed() {
    // A field typed as the owning type (linked-list node) resolves.
    let (_dump, diagnostics) = check(
        "indicator(\"U\")\ntype Node\n    int val\n    Node next\n\
         n = Node.new(1)\nplot(n.val)\n",
    );
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
}

// -----------------------------------------------------------------------
// Drawing-style constant namespaces (xloc/yloc/extend/line.style_*/
// hline.style_*/label.style_*/size/text.align_*/text.wrap_*).
// Registered as opaque string tokens (like format.*): each resolves to a
// const string carrying its registered string value. Other namespaces keep
// their existing token representation.
// -----------------------------------------------------------------------

#[test]
fn d1_drawing_style_constants_are_const_string() {
    // A representative member from each of these namespaces resolves to a
    // `const string` (not an int/float), typed distinctly from the numeric
    // constants. The dump shows a ConstString for each.
    for (member, token) in [
        ("xloc.bar_index", "bi"),
        ("yloc.abovebar", "ab"),
        ("extend.both", "b"),
        ("line.style_dashed", "dsh"),
        ("hline.style_dotted", "hline.style_dotted"),
        ("label.style_label_up", "lup"),
        ("size.large", "large"),
        ("text.align_center", "center"),
        ("text.wrap_auto", "auto"),
    ] {
        let src = format!("indicator(\"D\")\ns = {member}\nplot(str.length(s))\n");
        let (dump, diagnostics) = check(&src);
        assert!(
            diagnostics.is_empty(),
            "{member}: unexpected diagnostics: {diagnostics:?}"
        );
        assert!(
            dump.contains(&format!("ConstString {token:?} : const string")),
            "{member}: dump did not carry the const string token:\n{dump}"
        );
    }
}

#[test]
fn d1_drawing_style_constant_has_no_history() {
    // Like other namespaced constants, a `[1]` history reference is E0209.
    let (_dump, diagnostics) =
        check("indicator(\"D\")\ns = line.style_solid[1]\nplot(str.length(s))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0209"]);
}

// -----------------------------------------------------------------------
// Drawing-style constant namespaces (position.*/alert.freq_*).
// The enum-like constants that the `table.*` positioning and `alert()`
// frequency features consume. Same opaque-string-token model as above:
// drawing constants carry their registered string values; alert values keep
// their existing representation.
// -----------------------------------------------------------------------

#[test]
fn d2_position_and_alert_freq_constants_are_const_string() {
    // A representative member from each of these namespaces resolves to a
    // `const string`, distinct across members, exactly like the tokens above.
    for (member, token) in [
        ("position.top_left", "top_left"),
        ("position.middle_center", "middle_center"),
        ("position.bottom_right", "bottom_right"),
        ("alert.freq_all", "alert.freq_all"),
        ("alert.freq_once_per_bar", "alert.freq_once_per_bar"),
        (
            "alert.freq_once_per_bar_close",
            "alert.freq_once_per_bar_close",
        ),
    ] {
        let src = format!("indicator(\"D\")\ns = {member}\nplot(str.length(s))\n");
        let (dump, diagnostics) = check(&src);
        assert!(
            diagnostics.is_empty(),
            "{member}: unexpected diagnostics: {diagnostics:?}"
        );
        assert!(
            dump.contains(&format!("ConstString {token:?} : const string")),
            "{member}: dump did not carry the const string token:\n{dump}"
        );
    }
}

#[test]
fn d2_full_position_and_alert_freq_member_lists_are_registered() {
    // The registered member lists (9 position anchors, 3 alert
    // frequencies) each register as a distinct const string.
    let registry = crate::BuiltinRegistry::standard();
    for member in [
        "position.top_left",
        "position.top_center",
        "position.top_right",
        "position.middle_left",
        "position.middle_center",
        "position.middle_right",
        "position.bottom_left",
        "position.bottom_center",
        "position.bottom_right",
        "alert.freq_all",
        "alert.freq_once_per_bar",
        "alert.freq_once_per_bar_close",
    ] {
        let c = registry
            .lookup_constant(member)
            .unwrap_or_else(|| panic!("{member} not registered"));
        assert_eq!(
            c.ty,
            pinewasm_hir::Type::String,
            "{member} must be a const string"
        );
        assert_eq!(
            c.string_value,
            Some(match member {
                "font.family_default" => "default",
                "font.family_monospace" => "monospace",
                _ => member.strip_prefix("position.").unwrap_or(member),
            }),
            "{member} must carry the expected Pine value"
        );
    }
}

// -----------------------------------------------------------------------
// `barmerge.*` merge-behavior constants for `request.security`.
// Opaque string tokens (gap and lookahead modes), same model as the
// enum-like constant namespaces above.
// -----------------------------------------------------------------------

#[test]
fn fa1_barmerge_constants_are_const_string() {
    for member in [
        "barmerge.gaps_on",
        "barmerge.gaps_off",
        "barmerge.lookahead_on",
        "barmerge.lookahead_off",
    ] {
        let src = format!("indicator(\"D\")\ns = {member}\nplot(str.length(s))\n");
        let (dump, diagnostics) = check(&src);
        assert!(
            diagnostics.is_empty(),
            "{member}: unexpected diagnostics: {diagnostics:?}"
        );
        assert!(
            dump.contains(&format!("ConstString {member:?} : const string")),
            "{member}: dump did not carry the const string token:\n{dump}"
        );
        let registry = crate::BuiltinRegistry::standard();
        let c = registry
            .lookup_constant(member)
            .unwrap_or_else(|| panic!("{member} not registered"));
        assert_eq!(c.ty, pinewasm_hir::Type::String);
        assert_eq!(c.string_value, Some(member));
    }
}

// -----------------------------------------------------------------------
// Display, style, and selector constant namespaces.
// Opaque string tokens (plot styles, display locations, price scales,
// marker shapes, fonts, request selectors) plus the `chart.is_*` const
// bools. Same const-registration model as the constant blocks above; the
// on-chart rendering effect the tokens select is not modeled by the VM.
// -----------------------------------------------------------------------

#[test]
fn display_style_constants_are_const_strings() {
    // A representative from each namespace preserves its declared type and
    // value. Formatting constants are nominal, unlike the string selectors.
    for member in [
        "plot.style_area",
        "plot.style_columns",
        "display.none",
        "display.price_scale",
        "scale.left",
        "location.top",
        "location.absolute",
        "shape.triangleup",
        "shape.labeldown",
        "font.family_monospace",
        "text.format_bold",
        "adjustment.splits",
        "backadjustment.on",
        "settlement_as_close.inherit",
        "dividends.gross",
        "earnings.actual",
        "splits.numerator",
    ] {
        let formatting = member.starts_with("text.format_");
        // `display.*` values are nominal `plot_display` sets, not strings.
        let display = member.starts_with("display.");
        let src = if formatting {
            format!("indicator(\"C\")\ns = {member}\nlabel.new(0,0,text_formatting=s)\nplot(1)\n")
        } else if display {
            format!("indicator(\"C\")\ns = {member}\nplot(1, display=s)\n")
        } else {
            format!("indicator(\"C\")\ns = {member}\nplot(str.length(s))\n")
        };
        let (dump, diagnostics) = check(&src);
        assert!(
            diagnostics.is_empty(),
            "{member}: unexpected diagnostics: {diagnostics:?}"
        );
        assert!(
            dump.contains(&format!(
                "ConstString {:?} : const {}",
                match member {
                    "font.family_default" => "default",
                    "font.family_monospace" => "monospace",
                    _ => member.strip_prefix("position.").unwrap_or(member),
                },
                if formatting {
                    "text_format"
                } else if matches!(member, "display.all" | "display.none") {
                    "plot_simple_display"
                } else if display {
                    "plot_display"
                } else {
                    "string"
                }
            )),
            "{member}: dump did not carry the const string token:\n{dump}"
        );
        let registry = crate::BuiltinRegistry::standard();
        let c = registry
            .lookup_constant(member)
            .unwrap_or_else(|| panic!("{member} not registered"));
        assert_eq!(
            c.ty,
            if formatting {
                pinewasm_hir::Type::TextFormat
            } else if matches!(member, "display.all" | "display.none") {
                pinewasm_hir::Type::PlotSimpleDisplay
            } else if display {
                pinewasm_hir::Type::PlotDisplay
            } else {
                pinewasm_hir::Type::String
            }
        );
        assert_eq!(
            c.string_value,
            Some(match member {
                "font.family_default" => "default",
                "font.family_monospace" => "monospace",
                _ => member.strip_prefix("position.").unwrap_or(member),
            })
        );
    }
}

#[test]
fn display_style_member_lists_are_registered() {
    // The member lists preserve string selectors and the distinct
    // nominal formatting type.
    let registry = crate::BuiltinRegistry::standard();
    for member in [
        "plot.style_line",
        "plot.style_linebr",
        "plot.style_stepline",
        "plot.style_steplinebr",
        "plot.style_stepline_diamond",
        "plot.style_histogram",
        "plot.style_cross",
        "plot.style_area",
        "plot.style_areabr",
        "plot.style_columns",
        "plot.style_circles",
        "plot.linestyle_solid",
        "plot.linestyle_dotted",
        "plot.linestyle_dashed",
        "text.format_none",
        "text.format_bold",
        "text.format_italic",
        "display.none",
        "display.pane",
        "display.data_window",
        "display.price_scale",
        "display.status_line",
        "display.pine_screener",
        "display.all",
        "scale.left",
        "scale.right",
        "scale.none",
        "location.abovebar",
        "location.belowbar",
        "location.top",
        "location.bottom",
        "location.absolute",
        "shape.xcross",
        "shape.cross",
        "shape.circle",
        "shape.triangleup",
        "shape.triangledown",
        "shape.flag",
        "shape.arrowup",
        "shape.arrowdown",
        "shape.square",
        "shape.diamond",
        "shape.labelup",
        "shape.labeldown",
        "font.family_default",
        "font.family_monospace",
        "adjustment.dividends",
        "adjustment.none",
        "adjustment.splits",
        "backadjustment.inherit",
        "backadjustment.on",
        "backadjustment.off",
        "settlement_as_close.inherit",
        "settlement_as_close.on",
        "settlement_as_close.off",
        "dividends.gross",
        "dividends.net",
        "earnings.actual",
        "earnings.estimate",
        "earnings.standardized",
        "splits.denominator",
        "splits.numerator",
    ] {
        let c = registry
            .lookup_constant(member)
            .unwrap_or_else(|| panic!("{member} not registered"));
        assert_eq!(
            c.ty,
            if member.starts_with("text.format_") {
                pinewasm_hir::Type::TextFormat
            } else if matches!(member, "display.all" | "display.none") {
                pinewasm_hir::Type::PlotSimpleDisplay
            } else if member.starts_with("display.") {
                pinewasm_hir::Type::PlotDisplay
            } else {
                pinewasm_hir::Type::String
            },
            "{member} must retain its declared type"
        );
        assert_eq!(
            c.string_value,
            Some(match member {
                "font.family_default" => "default",
                "font.family_monospace" => "monospace",
                _ => member.strip_prefix("position.").unwrap_or(member),
            }),
            "{member} must carry the expected Pine value"
        );
    }
}

#[test]
fn chart_type_predicates_are_simple_bools() {
    // `chart.is_standard` folds to `true`; every other
    // chart-type predicate folds to `false`. Each remains a `simple bool`.
    let registry = crate::BuiltinRegistry::standard();
    for (member, expected) in [
        ("chart.is_standard", true),
        ("chart.is_heikinashi", false),
        ("chart.is_kagi", false),
        ("chart.is_linebreak", false),
        ("chart.is_pnf", false),
        ("chart.is_range", false),
        ("chart.is_renko", false),
    ] {
        let c = registry
            .lookup_constant(member)
            .unwrap_or_else(|| panic!("{member} not registered"));
        assert_eq!(c.ty, pinewasm_hir::Type::Bool, "{member} must be bool");
        assert_eq!(
            c.bool_value,
            Some(expected),
            "{member} must fold to {expected}"
        );
        // It compiles and the checker folds the constant into the dump.
        let src = format!("indicator(\"C\")\nb = {member}\nplot(b ? 1.0 : 0.0)\n");
        let (dump, diagnostics) = check(&src);
        assert!(
            diagnostics.is_empty(),
            "{member}: unexpected diagnostics: {diagnostics:?}"
        );
        assert!(
            dump.contains(&format!("ConstBool {expected} : simple bool")),
            "{member}: dump did not carry the folded const bool:\n{dump}"
        );
    }
}

// -----------------------------------------------------------------------
// Methods and collections-of-UDTs.
// -----------------------------------------------------------------------

#[test]
fn method_call_resolves_and_inlines() {
    // A pure method: `p.length()` resolves to the `Point` method and inlines,
    // reading the receiver's fields.
    let (dump, diagnostics) = check(
        "indicator(\"U\")\ntype Point\n    float x = 0.0\n    float y = 0.0\n\
         method length(Point this) =>\n    this.x + this.y\n\
         p = Point.new(3.0, 4.0)\nplot(p.length())\n",
    );
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
    // The method inlined a field read on the receiver.
    assert!(dump.contains("GetField"), "{dump}");
}

#[test]
fn mutating_method_emits_set_field() {
    // `p.move(...)` writes fields via `:=`, lowering to SetField.
    let (dump, diagnostics) = check(
        "indicator(\"U\")\ntype Point\n    float x = 0.0\n\
         method move(Point this, dx) =>\n    this.x := this.x + dx\n    this\n\
         p = Point.new(1.0)\np.move(2.0)\nplot(p.x)\n",
    );
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
    assert!(dump.contains("SetField"), "{dump}");
}

#[test]
fn unknown_method_on_object_is_e0237() {
    let (_dump, diagnostics) = check(
        "indicator(\"U\")\ntype Point\n    float x = 0.0\n\
         method length(Point this) =>\n    this.x\n\
         p = Point.new(1.0)\nplot(p.lenght())\n",
    );
    assert_eq!(error_codes(&diagnostics), vec!["E0237"]);
}

#[test]
fn method_on_unknown_receiver_type_is_e0237() {
    // The receiver type names a type that does not exist.
    let (_dump, diagnostics) =
        check("indicator(\"U\")\nmethod f(Widget this) =>\n    this\nplot(close)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0237"]);
}

#[test]
fn duplicate_method_for_type_is_e0224() {
    let (_dump, diagnostics) = check(
        "indicator(\"U\")\ntype Point\n    float x = 0.0\n\
         method f(Point this) =>\n    this.x\n\
         method f(Point this) =>\n    this.x\nplot(close)\n",
    );
    assert_eq!(error_codes(&diagnostics), vec!["E0224"]);
}

#[test]
fn same_method_name_on_two_types_does_not_collide() {
    // Two different types may each define a method of the same name.
    let (_dump, diagnostics) = check(
        "indicator(\"U\")\ntype A\n    float x = 0.0\ntype B\n    float y = 0.0\n\
         method v(A this) =>\n    this.x\n\
         method v(B this) =>\n    this.y\n\
         a = A.new(1.0)\nb = B.new(2.0)\nplot(a.v() + b.v())\n",
    );
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
}

#[test]
fn array_of_udt_constructs_and_reads_field() {
    let (dump, diagnostics) = check(
        "indicator(\"U\")\ntype Point\n    float x = 0.0\n\
         objs = array.new<Point>()\narray.push(objs, Point.new(3.0))\n\
         p = array.get(objs, 0)\nplot(p.x)\n",
    );
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
    // The element type flows through as an object array.
    assert!(dump.contains("array<Point>"), "{dump}");
}

#[test]
fn map_string_to_udt_constructs() {
    let (dump, diagnostics) = check(
        "indicator(\"U\")\ntype Point\n    float x = 0.0\n\
         m = map.new<string, Point>()\nmap.put(m, \"a\", Point.new(1.0))\n\
         p = map.get(m, \"a\")\nplot(p.x)\n",
    );
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
    assert!(dump.contains("map<string, Point>"), "{dump}");
}

#[test]
fn drawing_typed_declaration_types_the_variable() {
    // `line l = na` types the variable as a line; the na initializer adopts the
    // annotation (no E0220), and a `:=` from line.new() matches.
    let (_dump, diagnostics) =
        check("indicator(\"D\")\nline l = na\nl := line.new(0, 1.0, 1, 2.0)\nplot(close)\n");
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
}

#[test]
fn drawing_typed_declaration_wrong_constructor_is_e0226() {
    // `line l = box.new(...)` — a box value into a line-typed declaration is a
    // declared-type/initializer mismatch (E0226).
    let (_dump, diagnostics) =
        check("indicator(\"D\")\nline l = box.new(1, 2.0, 3, 4.0)\nplot(close)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0226"]);
}

#[test]
fn drawing_typed_reassign_wrong_type_is_e0218() {
    // A line-typed variable reassigned from a box.new() mismatches (E0218).
    let (_dump, diagnostics) =
        check("indicator(\"D\")\nline l = na\nl := box.new(1, 2.0, 3, 4.0)\nplot(close)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0218"]);
}

#[test]
fn all_drawing_type_names_are_valid_annotations() {
    // Each drawing type name is accepted as a `na`-init variable annotation and
    // fixes the variable's declared type: assigning a `line` value (from
    // line.new()) into a non-`line`-typed variable is a type mismatch (E0218),
    // which can only fire if the annotation actually took effect.
    for ty in ["label", "box", "table", "linefill", "polyline"] {
        let ok = format!("indicator(\"D\")\n{ty} v = na\nplot(close)\n");
        let (_dump, diagnostics) = check(&ok);
        assert!(!has_errors(&diagnostics), "{ty} na-init: {diagnostics:?}");

        let mismatch =
            format!("indicator(\"D\")\n{ty} v = na\nv := line.new(0, 1.0, 1, 2.0)\nplot(close)\n");
        let (_dump, diagnostics) = check(&mismatch);
        assert_eq!(error_codes(&diagnostics), vec!["E0218"], "{ty} mismatch");
    }
}

#[test]
fn chart_point_typed_declaration_types_the_variable() {
    // The dotted `chart.point` type name is accepted as an annotation.
    let (_dump, diagnostics) =
        check("indicator(\"D\")\nchart.point p = chart.point.from_index(0, 1.0)\nplot(close)\n");
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
}

#[test]
fn array_new_line_is_an_array_of_line() {
    // The named shorthand `array.new_line()` yields an `array<line>`.
    let (dump, diagnostics) =
        check("indicator(\"D\")\na = array.new_line()\nplot(array.size(a))\n");
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
    assert!(dump.contains("array<line>"), "{dump}");
}

#[test]
fn array_new_table_is_an_array_of_table() {
    let (dump, diagnostics) =
        check("indicator(\"D\")\na = array.new_table()\nplot(array.size(a))\n");
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
    assert!(dump.contains("array<table>"), "{dump}");
}

#[test]
fn array_new_box_is_an_array_of_box() {
    let (dump, diagnostics) = check("indicator(\"D\")\na = array.new_box()\nplot(array.size(a))\n");
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
    assert!(dump.contains("array<box>"), "{dump}");
}

#[test]
fn array_new_linefill_is_an_array_of_linefill() {
    let (dump, diagnostics) =
        check("indicator(\"D\")\na = array.new_linefill()\nplot(array.size(a))\n");
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
    assert!(dump.contains("array<linefill>"), "{dump}");
}

#[test]
fn array_of_unknown_udt_is_e0234() {
    let (_dump, diagnostics) =
        check("indicator(\"U\")\na = array.new<Widget>()\nplot(array.size(a))\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0234"]);
}

#[test]
fn object_elem_tag_is_above_scalar_range() {
    // The object element tag encoding (mirrored from
    // `pinewasm_mir::heap::ElemType`) must sit above the scalar tag range
    // (0..=4) so a decoder can distinguish object from scalar element types.
    for id in [0u32, 1, 7, 42] {
        assert!(super::object_elem_tag(id) >= 1000);
    }
}

// ---- drawing objects ------------------------------------------------------

#[test]
fn drawing_new_setter_getter_delete_analyze_ok() {
    let result = analyze_ok(
        "indicator(\"D\")\n\
         ln = line.new(0, 1.0, 1, 2.0, color = color.red, width = 2)\n\
         line.set_x2(ln, 5)\n\
         w = line.get_x2(ln)\n\
         line.delete(ln)\n\
         p = chart.point.from_index(0, 3.0)\n\
         plot(w + p.price)\n",
    );
    assert!(
        !pinewasm_span::has_errors(&result.diagnostics),
        "unexpected errors: {:?}",
        result.diagnostics
    );
}

#[test]
fn drawing_delete_accepts_named_id() {
    let result = analyze_ok(
        "indicator(\"D\")\nln = line.new(0, 1.0, 1, 2.0)\nline.delete(id = ln)\nplot(close)\n",
    );
    assert!(!pinewasm_span::has_errors(&result.diagnostics));
}

#[test]
fn drawing_delete_rejects_unknown_named_argument() {
    let (_dump, diagnostics) = check(
        "indicator(\"D\")\nln = line.new(0, 1.0, 1, 2.0)\nline.delete(object = ln)\nplot(close)\n",
    );
    assert_eq!(error_codes(&diagnostics), vec!["E0228"]);
}

#[test]
fn unknown_drawing_member_is_e0238() {
    let (_dump, diagnostics) = check(
        "indicator(\"D\")\nln = line.new(0, 1.0, 1, 2.0)\nline.frobnicate(ln)\nplot(close)\n",
    );
    assert_eq!(error_codes(&diagnostics), vec!["E0238"]);
}

#[test]
fn unknown_drawing_field_setter_is_e0238() {
    let (_dump, diagnostics) = check(
        "indicator(\"D\")\nln = line.new(0, 1.0, 1, 2.0)\nline.set_nope(ln, 1)\nplot(close)\n",
    );
    assert_eq!(error_codes(&diagnostics), vec!["E0238"]);
}

#[test]
fn drawing_setter_wrong_receiver_type_is_e0238() {
    let (_dump, diagnostics) = check("indicator(\"D\")\nline.set_x1(3.0, 1)\nplot(close)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0238"]);
}

#[test]
fn table_polyline_linefill_new_typecheck_clean() {
    // The three drawing namespaces construct and type-check cleanly.
    let (dump, diagnostics) = check(
        "indicator(\"D\")\n\
         t = table.new(position.top_left, 1, 1)\n\
         table.set_frame_width(t, 2)\n\
         pts = array.new<chart.point>()\n\
         array.push(pts, chart.point.from_index(0, 1.0))\n\
         pl = polyline.new(pts, closed = true)\n\
         a = line.new(0, 1.0, 1, 2.0)\n\
         b = line.new(0, 3.0, 1, 4.0)\n\
         lf = linefill.new(a, b, color.red)\n\
         linefill.set_color(lf, color.blue)\n\
         plot(close)\n",
    );
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
    // The linefill value is an object of type `linefill`.
    assert!(dump.contains("linefill"), "{dump}");
}

#[test]
fn linefill_new_wrong_arg_type_is_e0238() {
    // linefill.line1 expects a `line` object; passing a float is rejected.
    let (_dump, diagnostics) =
        check("indicator(\"D\")\nlinefill.new(1.0, 2.0, color.red)\nplot(close)\n");
    assert!(!diagnostics.is_empty(), "expected a diagnostic");
}

#[test]
fn unknown_table_member_is_e0238() {
    let (_dump, diagnostics) = check(
        "indicator(\"D\")\nt = table.new(position.top_left, 1, 1)\ntable.frobnicate(t)\nplot(close)\n",
    );
    assert_eq!(error_codes(&diagnostics), vec!["E0238"]);
}

#[test]
fn table_cell_and_mutators_typecheck_clean() {
    // Table cells: `table.cell` with named attributes, a `cell_set_*` mutator,
    // `table.clear`, and `table.merge_cells` all type-check and lower to the
    // cell-op HIR nodes.
    let (dump, diagnostics) = check(
        "indicator(\"D\")\n\
         t = table.new(position.top_left, 2, 2)\n\
         table.cell(t, 0, 0, \"hi\", bgcolor = color.red, text_color = color.blue, text_formatting = text.format_bold)\n\
         table.cell_set_text(t, 0, 0, \"bye\")\n\
         table.cell_set_text_formatting(t, 0, 0, text.format_italic)\n\
         table.merge_cells(t, 0, 0, 1, 0)\n\
         table.clear(t, 1, 1)\n\
         plot(close)\n",
    );
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
    assert!(dump.contains("SetCell"), "{dump}");
    assert!(dump.contains("MergeCells"), "{dump}");
    assert!(dump.contains("ClearCells"), "{dump}");
}

#[test]
fn drawing_method_dispatch_keeps_constructors_out_of_method_surface() {
    let (dump, diagnostics) = check(
        "indicator(\"D\")\n\
         ln = line.new(0, 1.0, 1, 2.0)\n\
         ln.set_xy1(2, 3.0)\n\
         plot(ln.get_x1())\n",
    );
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
    assert!(dump.contains("SetFields"), "{dump}");

    let (_, diagnostics) = check(
        "indicator(\"D\")\nln = line.new(0, 1.0, 1, 2.0)\nln.new(0, 1.0, 1, 2.0)\nplot(close)\n",
    );
    assert!(error_codes(&diagnostics).contains(&"E0237"));
}

#[test]
fn coordinate_setters_accept_mixed_named_arguments() {
    let (dump, diagnostics) = check(
        "indicator(\"D\")\nln = line.new(0, 1.0, 1, 2.0)\nline.set_xy1(ln, x = 3, y = 4.5)\nline.set_xy2(y = 5.5, id = ln, x = 6)\nlb = label.new(0, 1.0, \"probe\")\nlabel.set_xy(lb, x = 7, y = 8.5)\nplot(line.get_x1(ln))\n",
    );
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
    assert_eq!(dump.matches("SetFields").count(), 3, "{dump}");

    let (_, diagnostics) = check(
        "indicator(\"D\")\nln = line.new(0, 1.0, 1, 2.0)\nline.set_xy1(ln, x = 3, price = 4.5)\nplot(close)\n",
    );
    assert!(has_errors(&diagnostics));
}

#[test]
fn valuewhen_occurrence_folds_literal_bound_helper_arithmetic() {
    let (dump, diagnostics) = check(
        "indicator(\"V\")\nf(iter) =>\n    ta.valuewhen(bar_index % 3 == 0, bar_index, iter - 1)\nplot(f(1))\nplot(f(2))\n",
    );
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
    assert!(dump.contains("params=[0]"), "{dump}");
    assert!(dump.contains("params=[1]"), "{dump}");

    let (_, diagnostics) =
        check("indicator(\"V\")\npick = bar_index % 2\nplot(ta.valuewhen(true, close, pick))\n");
    assert!(error_codes(&diagnostics).contains(&"E0219"));
}

#[test]
fn literal_bound_helper_skips_unreachable_negative_valuewhen_occurrence() {
    let (dump, diagnostics) = check(
        "indicator(\"V\")\nf(iter) =>\n    iter == 0 ? bar_index : ta.valuewhen(bar_index % 3 == 0, bar_index, iter - 1)\nplot(f(0))\nplot(f(1))\nplot(f(2))\n",
    );
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
    assert_eq!(
        dump.matches("CallTaState ta.valuewhen").count(),
        2,
        "{dump}"
    );

    let (_, diagnostics) = check("indicator(\"V\")\nplot(ta.valuewhen(true, close, -1))\n");
    assert!(error_codes(&diagnostics).contains(&"E0219"));
}

#[test]
fn table_cell_binds_named_receiver_and_coordinates() {
    let (dump, diagnostics) = check(
        "indicator(\"D\")\n\
         t = table.new(position.top_left, 2, 2)\n\
         table.cell(table_id = t, column = 0, row = 1, text = \"named\")\n\
         table.cell(t, 1, row = 0, text = \"mixed\")\n\
         plot(close)\n",
    );
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
    assert_eq!(dump.matches("SetCell").count(), 2, "{dump}");

    let (_, diagnostics) = check(
        "indicator(\"D\")\nt = table.new(position.top_left, 1, 1)\ntable.cell(t, 0, 0, table_id = t)\nplot(close)\n",
    );
    assert!(error_codes(&diagnostics).contains(&"E0238"));
}

#[test]
fn table_cell_wrong_attribute_type_is_e0238() {
    // `bgcolor` is a color attribute; passing an int is rejected.
    let (_dump, diagnostics) = check(
        "indicator(\"D\")\n\
         t = table.new(position.top_left, 1, 1)\n\
         table.cell(t, 0, 0, bgcolor = 5)\n\
         plot(close)\n",
    );
    assert!(!diagnostics.is_empty(), "expected a diagnostic");
}

#[test]
fn table_cell_unknown_attribute_is_e0238() {
    let (_dump, diagnostics) = check(
        "indicator(\"D\")\n\
         t = table.new(position.top_left, 1, 1)\n\
         table.cell(t, 0, 0, frobnicate = \"x\")\n\
         plot(close)\n",
    );
    assert_eq!(error_codes(&diagnostics), vec!["E0238"]);
}

#[test]
fn table_cell_set_on_non_table_is_e0238() {
    // A `cell_set_*` mutator receiver must be a table object.
    let (_dump, diagnostics) = check(
        "indicator(\"D\")\n\
         l = line.new(0, 1.0, 1, 2.0)\n\
         table.cell_set_text(l, 0, 0, \"x\")\n\
         plot(close)\n",
    );
    assert!(!diagnostics.is_empty(), "expected a diagnostic");
}

// ---- event streams: log.* + alert() --------------------------------------

#[test]
fn log_calls_analyze_as_log_statements() {
    let (dump, diagnostics) = check(
        "indicator(\"L\")\n\
         log.info(\"x={0}\", close)\n\
         log.warning(\"w\")\n\
         log.error(\"e {0}\", bar_index)\n\
         plot(close)\n",
    );
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
    assert!(dump.contains("Log info"));
    assert!(dump.contains("Log warning"));
    assert!(dump.contains("Log error"));
}

#[test]
fn alert_and_alertcondition_analyze() {
    let (dump, diagnostics) = check(
        "indicator(\"A\")\n\
         alert(\"m\")\n\
         alert(\"m2\", alert.freq_all)\n\
         alertcondition(close > 1.0, \"t\", \"msg\")\n\
         plot(close)\n",
    );
    assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
    assert!(dump.contains("Alert"));
    assert!(dump.contains("AlertCondition"));
}

#[test]
fn log_requires_a_format_argument() {
    let (_dump, diagnostics) = check("indicator(\"L\")\nlog.info()\nplot(close)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0204"]);
}

#[test]
fn alertcondition_condition_must_be_bool() {
    let (_dump, diagnostics) =
        check("indicator(\"A\")\nalertcondition(close, \"t\", \"m\")\nplot(close)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0205"]);
}

#[test]
fn log_used_in_expression_is_e0212() {
    // Effect calls produce no value; using log.* inside an expression is E0212.
    let (_dump, diagnostics) = check("indicator(\"L\")\nx = log.info(\"m\") + 1\nplot(x)\n");
    assert!(error_codes(&diagnostics).contains(&"E0212"));
}

// -- strategy() declaration --------------------------------------------------

fn analyze_src(source: &str) -> crate::AnalysisResult {
    let parsed = parse(source);
    assert!(
        !has_errors(&parsed.diagnostics),
        "test source must parse cleanly: {:?}",
        parsed.diagnostics
    );
    analyze(&parsed.program)
}

#[test]
fn strategy_declaration_defaults() {
    let result = analyze_src("strategy(\"S\")\nplot(close)\n");
    assert!(!has_errors(&result.diagnostics), "{:?}", result.diagnostics);
    let s = result.hir.strategy.expect("strategy recorded");
    assert_eq!(s.title, "S");
    assert_eq!(s.initial_capital, 1_000_000.0);
    assert_eq!(s.default_qty_type, "fixed");
    assert_eq!(s.default_qty_value, 1.0);
    assert_eq!(s.commission_type, "percent");
    assert_eq!(s.commission_value, 0.0);
    assert_eq!(s.slippage, 0);
    assert_eq!(s.pyramiding, 1);
    assert!(!s.process_orders_on_close);
    assert_eq!(s.currency, "NONE");
    assert_eq!(s.shorttitle, "S");
    assert!(!s.overlay);
    assert_eq!(s.format, "format.inherit");
    assert_eq!(s.precision, None);
    assert_eq!(s.scale, None);
    assert_eq!(s.max_bars_back, None);
    assert_eq!(s.backtest_fill_limits_assumption, 0);
    assert_eq!(s.close_entries_rule, "FIFO");
    assert_eq!(s.margin_long, 100.0);
    assert_eq!(s.margin_short, 100.0);
    assert_eq!(s.max_lines_count, Some(50));
    assert_eq!(s.max_labels_count, Some(50));
    assert_eq!(s.max_boxes_count, Some(50));
    assert_eq!(s.max_polylines_count, Some(50));
    assert_eq!(s.risk_free_rate, 2.0);
    assert!(s.dynamic_requests);
    assert!(s.behind_chart);
    assert!(!s.calc_on_every_history_tick);
    // A strategy script does not warrant the "no declaration" W0001 warning.
    assert!(result
        .diagnostics
        .iter()
        .all(|d| d.code.as_deref() != Some("W0001")));
    // And it is not also an indicator.
    assert!(result.hir.indicator.is_none());
}

#[test]
fn strategy_full_param_surface_const_folds() {
    let result = analyze_src(
        "strategy(\"S\", shorttitle=\"Short\", overlay=true, format=format.price, \
         precision=4, scale=scale.right, max_bars_back=100, \
         backtest_fill_limits_assumption=2, initial_capital=5000, default_qty_type=\"cash\", \
         default_qty_value=250, commission_type=\"cash_per_order\", commission_value=1.5, \
         slippage=2, pyramiding=3, process_orders_on_close=true, close_entries_rule=\"ANY\", \
         calc_on_every_tick=true, calc_on_order_fills=true, currency=\"USD\", \
         margin_long=50, margin_short=25, explicit_plot_zorder=true, max_lines_count=101, \
         max_labels_count=102, max_boxes_count=103, calc_bars_count=12345, \
         risk_free_rate=3.5, use_bar_magnifier=true, fill_orders_on_standard_ohlc=true, \
         max_polylines_count=99, dynamic_requests=false, behind_chart=false)\nplot(close)\n",
    );
    assert!(!has_errors(&result.diagnostics), "{:?}", result.diagnostics);
    let s = result.hir.strategy.expect("strategy recorded");
    assert_eq!(s.initial_capital, 5000.0);
    assert_eq!(s.default_qty_type, "cash");
    assert_eq!(s.default_qty_value, 250.0);
    assert_eq!(s.commission_type, "cash_per_order");
    assert_eq!(s.commission_value, 1.5);
    assert_eq!(s.slippage, 2);
    assert_eq!(s.pyramiding, 3);
    assert!(s.process_orders_on_close);
    assert_eq!(s.currency, "USD");
    assert_eq!(s.margin_long, 50.0);
    assert_eq!(s.margin_short, 25.0);
    assert_eq!(s.shorttitle, "Short");
    assert!(s.overlay);
    assert_eq!(s.format, "format.price");
    assert_eq!(s.precision, Some(4));
    assert_eq!(s.scale.as_deref(), Some("scale.right"));
    assert_eq!(s.max_bars_back, Some(100));
    assert_eq!(s.backtest_fill_limits_assumption, 2);
    assert_eq!(s.close_entries_rule, "ANY");
    assert!(s.calc_on_every_tick);
    assert!(s.calc_on_order_fills);
    assert!(s.explicit_plot_zorder);
    assert_eq!(s.max_lines_count, Some(101));
    assert_eq!(s.max_labels_count, Some(102));
    assert_eq!(s.max_boxes_count, Some(103));
    assert_eq!(s.calc_bars_count, 12345);
    assert_eq!(s.risk_free_rate, 3.5);
    assert!(s.use_bar_magnifier);
    assert!(s.fill_orders_on_standard_ohlc);
    assert_eq!(s.max_polylines_count, Some(99));
    assert!(!s.dynamic_requests);
    assert!(!s.behind_chart);
    assert!(!s.calc_on_every_history_tick);
}

#[test]
fn strategy_full_param_surface_accepts_const_expressions_and_aliases() {
    let result = analyze_src(
        "TITLE_PREFIX = \"Const \"\n\
         TITLE = TITLE_PREFIX + \"Strategy\"\n\
         SHORT = TITLE + \" short\"\n\
         YES = 2 > 1 and not false\n\
         NO = not YES\n\
         TWO = 1 + 1\n\
         FOUR = TWO * TWO\n\
         FIFTY = int(100 / TWO)\n\
         FORMAT_VALUE = format.percent\n\
         SCALE_VALUE = scale.right\n\
         QTY_VALUE = strategy.percent_of_equity\n\
         CURRENCY_VALUE = currency.USD\n\
         COMMISSION_VALUE = strategy.commission.percent\n\
         CLOSE_RULE = \"A\" + \"NY\"\n\
         strategy(TITLE, shorttitle=SHORT, overlay=YES, format=FORMAT_VALUE, \
         precision=FOUR, scale=SCALE_VALUE, pyramiding=TWO, \
         calc_on_order_fills=YES, calc_on_every_tick=YES, \
         max_bars_back=FIFTY * TWO, backtest_fill_limits_assumption=TWO, \
         default_qty_type=QTY_VALUE, default_qty_value=FIFTY / TWO, \
         initial_capital=FIFTY * 100, currency=CURRENCY_VALUE, slippage=TWO, \
         commission_type=COMMISSION_VALUE, commission_value=1.0 / TWO, \
         process_orders_on_close=YES, close_entries_rule=CLOSE_RULE, \
         margin_long=FIFTY, margin_short=FIFTY / TWO, explicit_plot_zorder=YES, \
         max_lines_count=FIFTY + 1, max_labels_count=FIFTY + TWO, \
         max_boxes_count=FIFTY + 3, calc_bars_count=FIFTY * 10, \
         risk_free_rate=FOUR - 1.0, use_bar_magnifier=YES, \
         fill_orders_on_standard_ohlc=YES, max_polylines_count=FIFTY, \
         dynamic_requests=NO, behind_chart=NO)\n\
         plot(close)\n",
    );
    assert!(!has_errors(&result.diagnostics), "{:?}", result.diagnostics);
    let s = result.hir.strategy.expect("strategy recorded");
    assert_eq!(s.title, "Const Strategy");
    assert_eq!(s.shorttitle, "Const Strategy short");
    assert!(s.overlay);
    assert_eq!(s.format, "format.percent");
    assert_eq!(s.precision, Some(4));
    assert_eq!(s.scale.as_deref(), Some("scale.right"));
    assert_eq!(s.pyramiding, 2);
    assert!(s.calc_on_order_fills);
    assert!(s.calc_on_every_tick);
    assert_eq!(s.max_bars_back, Some(100));
    assert_eq!(s.backtest_fill_limits_assumption, 2);
    assert_eq!(s.default_qty_type, "percent_of_equity");
    assert_eq!(s.default_qty_value, 25.0);
    assert_eq!(s.initial_capital, 5_000.0);
    assert_eq!(s.currency, "USD");
    assert_eq!(s.slippage, 2);
    assert_eq!(s.commission_type, "percent");
    assert_eq!(s.commission_value, 0.5);
    assert!(s.process_orders_on_close);
    assert_eq!(s.close_entries_rule, "ANY");
    assert_eq!(s.margin_long, 50.0);
    assert_eq!(s.margin_short, 25.0);
    assert!(s.explicit_plot_zorder);
    assert_eq!(s.max_lines_count, Some(51));
    assert_eq!(s.max_labels_count, Some(52));
    assert_eq!(s.max_boxes_count, Some(53));
    assert_eq!(s.calc_bars_count, 500);
    assert_eq!(s.risk_free_rate, 3.0);
    assert!(s.use_bar_magnifier);
    assert!(s.fill_orders_on_standard_ohlc);
    assert_eq!(s.max_polylines_count, Some(50));
    assert!(!s.dynamic_requests);
    assert!(!s.behind_chart);
    assert!(!s.calc_on_every_history_tick);
}

#[test]
fn strategy_const_parameters_reject_input_qualified_aliases() {
    for source in [
        "TITLE = input.string(\"S\")\nstrategy(TITLE)\nplot(close)\n",
        "COUNT = input.int(2)\nstrategy(\"S\", pyramiding=COUNT)\nplot(close)\n",
        "FLAG = input.bool(true)\nstrategy(\"S\", overlay=FLAG)\nplot(close)\n",
    ] {
        // A right-typed value whose qualifier is stronger than `const` is a
        // qualifier error, `E0206` (codes.rs).
        let (_dump, diagnostics) = check(source);
        assert_eq!(
            error_codes(&diagnostics),
            vec!["E0206"],
            "input-qualified strategy argument: {diagnostics:?}"
        );
    }
}

#[test]
fn strategy_history_tick_is_a_declaration_parameter() {
    // `calc_on_every_history_tick` is accepted as a `strategy()` declaration
    // parameter. A host run override still wins over whatever the declaration
    // seeds.
    let source = "strategy(\"S\", calc_on_every_history_tick=true)\nplot(close)\n";
    let (_dump, diagnostics) = check(source);
    assert_eq!(error_codes(&diagnostics), Vec::<&str>::new());
}

#[test]
fn strategy_dynamic_requests_false_rejects_local_and_conditional_calls() {
    for body in [
        "if close > open\n    x = request.security(syminfo.tickerid, \"1W\", close)",
        "for i = 0 to 1\n    x = request.security(syminfo.tickerid, \"1W\", close)",
        "x = close > open ? request.security(syminfo.tickerid, \"1W\", close) : close",
        "x = close > open and request.security(syminfo.tickerid, \"1W\", close) > 0",
    ] {
        let source = format!("strategy(\"S\", dynamic_requests=false)\n{body}\nplot(close)\n");
        let result = analyze_src(&source);
        let diagnostic = diagnostic_with_code(&result.diagnostics, "E0242");
        assert!(
            diagnostic.message.contains("dynamic_requests = false"),
            "{diagnostic:?}"
        );
    }

    let wrapped = analyze_src(
        "strategy(\"S\", dynamic_requests=false)\nfetch() => request.security(syminfo.tickerid, \"1W\", close)\nif close > open\n    x = fetch()\nplot(close)\n",
    );
    let diagnostic = diagnostic_with_code(&wrapped.diagnostics, "E0242");
    assert!(diagnostic.message.contains("dynamic_requests = false"));
}

#[test]
fn migrated_v5_timeframe_period_helper_resolves_only_exact_chart_equivalence() {
    let helper = concat!(
        "__v6_timeframePeriodV5() => ",
        "timeframe.multiplier != 1 ? timeframe.period : ",
        "timeframe.isdaily ? \"D\" : timeframe.isweekly ? \"W\" : ",
        "timeframe.ismonthly ? \"M\" : timeframe.isseconds ? \"S\" : timeframe.period\n",
    );
    let accepted = analyze_src(&format!(
        "indicator(\"S\")\n{helper}plot(request.security(syminfo.tickerid, __v6_timeframePeriodV5(), close))\n"
    ));
    assert!(
        !has_errors(&accepted.diagnostics),
        "exact timeframe helper diagnostics: {:?}",
        accepted.diagnostics
    );

    for altered in [
        helper.replace("multiplier != 1", "multiplier != 2"),
        helper.replace("timeframe.isdaily ? \"D\"", "timeframe.isdaily ? \"2D\""),
    ] {
        let result = analyze_src(&format!(
            "indicator(\"S\")\n{altered}plot(request.security(syminfo.tickerid, __v6_timeframePeriodV5(), close))\n"
        ));
        assert!(
            error_codes(&result.diagnostics).contains(&"E0242"),
            "non-equivalent helper must not be treated as the chart timeframe: {:?}",
            result.diagnostics
        );
    }
}

#[test]
fn strategy_dynamic_requests_default_allows_local_calls_and_false_allows_global_calls() {
    for declaration in ["strategy(\"S\")", "strategy(\"S\", dynamic_requests=true)"] {
        let source = format!(
            "{declaration}\nif close > open\n    x = request.security(syminfo.tickerid, \"1W\", close)\nplot(close)\n"
        );
        let result = analyze_src(&source);
        assert!(!has_errors(&result.diagnostics), "{:?}", result.diagnostics);
    }

    let global = analyze_src(
        "strategy(\"S\", dynamic_requests=false)\nx = request.security(syminfo.tickerid, \"1W\", close)\nplot(x)\n",
    );
    assert!(!has_errors(&global.diagnostics), "{:?}", global.diagnostics);

    // A request in an inlined function called only from global scope is still
    // non-dynamic.
    let global_wrapper = analyze_src(
        "strategy(\"S\", dynamic_requests=false)\nfetch() => request.security(syminfo.tickerid, \"1D\", close)\nplot(fetch())\n",
    );
    assert!(
        !has_errors(&global_wrapper.diagnostics),
        "{:?}",
        global_wrapper.diagnostics
    );

    let nested_global_wrapper = analyze_src(
        "strategy(\"S\", dynamic_requests=false)\nfetch() => request.security(syminfo.tickerid, \"1D\", close)\nwrapped() => fetch()\nplot(wrapped())\n",
    );
    assert!(
        !has_errors(&nested_global_wrapper.diagnostics),
        "{:?}",
        nested_global_wrapper.diagnostics
    );
}

#[test]
fn strategy_order_commands_preserve_complete_v6_hir_layouts() {
    let result = analyze_src(
        "strategy(\"S\")\n\
         strategy.entry(\"E\", strategy.long, qty=1, limit=2, stop=3, \
             oca_name=\"entry-group\", oca_type=strategy.oca.cancel, comment=\"entry\", \
             alert_message=\"entry-alert\", disable_alert=true)\n\
         strategy.order(\"O\", strategy.short, qty=4, limit=5, stop=6, \
             oca_name=\"order-group\", oca_type=strategy.oca.reduce, comment=\"order\", \
             alert_message=\"order-alert\", disable_alert=true)\n\
         strategy.exit(\"X\", \"E\", qty=7, qty_percent=8, profit=9, limit=10, \
             loss=11, stop=12, trail_price=13.0, trail_points=14, trail_offset=15, \
             oca_name=\"exit-group\", comment=\"exit\", comment_profit=\"profit\", \
             comment_loss=\"loss\", comment_trailing=\"trail\", alert_message=\"exit-alert\", \
             alert_profit=\"profit-alert\", alert_loss=\"loss-alert\", \
             alert_trailing=\"trail-alert\", disable_alert=true)\n\
         strategy.close(\"E\", comment=\"close\", qty=16, qty_percent=17, \
             alert_message=\"close-alert\", immediately=true, disable_alert=true)\n\
         strategy.close_all(comment=\"all\", alert_message=\"all-alert\", \
             immediately=true, disable_alert=true)\n",
    );
    assert!(!has_errors(&result.diagnostics), "{:?}", result.diagnostics);

    let orders: Vec<(OrderKind, &[pinewasm_hir::HirExpr])> = result
        .hir
        .body
        .iter()
        .filter_map(|stmt| match stmt {
            HirStmt::Order { kind, operands, .. } => Some((*kind, operands.as_slice())),
            _ => None,
        })
        .collect();
    let expected = [
        (OrderKind::Entry, 10usize),
        (OrderKind::Order, 10),
        (OrderKind::Exit, 21),
        (OrderKind::Close, 7),
        (OrderKind::CloseAll, 4),
    ];
    assert_eq!(orders.len(), expected.len());
    for ((kind, operands), (expected_kind, expected_count)) in orders.iter().zip(expected) {
        assert_eq!(*kind, expected_kind);
        assert_eq!(operands.len(), expected_count);
        assert_eq!(usize::from(kind.operand_count()), expected_count);
        assert_eq!(kind.param_names().len(), expected_count);
    }

    assert_eq!(
        orders[0]
            .1
            .iter()
            .map(|operand| operand.ty.clone())
            .collect::<Vec<_>>(),
        vec![
            Type::String,
            Type::Enum(pinewasm_hir::STRATEGY_DIRECTION_TYPE.to_string()),
            Type::Float,
            Type::Float,
            Type::Float,
            Type::String,
            Type::String,
            Type::String,
            Type::String,
            Type::Bool,
        ]
    );
    assert!(matches!(orders[2].1[8].kind, HirExprKind::ConstFloat(13.0)));
    assert!(matches!(orders[2].1[20].kind, HirExprKind::ConstBool(true)));
    assert!(matches!(orders[3].1[5].kind, HirExprKind::ConstBool(true)));
}

#[test]
fn strategy_order_commands_are_valid_inside_user_functions() {
    let result = analyze_src(
        "strategy(\"S\")\nplace() =>\n    strategy.entry(\"E\", strategy.long, comment=\"from function\")\n    1\nplace()\nplot(strategy.position_size)\n",
    );
    assert!(!has_errors(&result.diagnostics), "{:?}", result.diagnostics);
    let entries = result
        .hir
        .body
        .iter()
        .filter(|stmt| {
            matches!(
                stmt,
                HirStmt::Order {
                    kind: OrderKind::Entry,
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        entries, 1,
        "the written function call must carry one order effect"
    );
}

#[test]
fn strategy_order_commands_materialize_documented_optional_defaults() {
    let result = analyze_src(
        "strategy(\"S\")\n\
         strategy.entry(\"E\", strategy.long)\n\
         strategy.exit(\"X\", stop=100)\n\
         strategy.close(\"E\")\n\
         strategy.close_all()\n",
    );
    assert!(!has_errors(&result.diagnostics), "{:?}", result.diagnostics);
    let orders: Vec<&[pinewasm_hir::HirExpr]> = result
        .hir
        .body
        .iter()
        .filter_map(|stmt| match stmt {
            HirStmt::Order { operands, .. } => Some(operands.as_slice()),
            _ => None,
        })
        .collect();

    assert!(matches!(orders[0][2].kind, HirExprKind::ConstNa));
    assert!(matches!(
        &orders[0][5].kind,
        HirExprKind::ConstString(value) if value.is_empty()
    ));
    assert!(matches!(
        &orders[0][6].kind,
        HirExprKind::ConstString(value) if value == "strategy.oca.none"
    ));
    assert!(matches!(
        &orders[0][8].kind,
        HirExprKind::ConstString(value) if value.is_empty()
    ));
    assert!(matches!(orders[0][9].kind, HirExprKind::ConstBool(false)));

    assert!(matches!(
        &orders[1][1].kind,
        HirExprKind::ConstString(value) if value.is_empty()
    ));
    assert!(matches!(orders[1][3].kind, HirExprKind::ConstFloat(100.0)));
    assert!(matches!(orders[1][20].kind, HirExprKind::ConstBool(false)));
    assert!(matches!(orders[2][3].kind, HirExprKind::ConstFloat(100.0)));
    assert!(matches!(orders[2][5].kind, HirExprKind::ConstBool(false)));
    assert!(matches!(orders[2][6].kind, HirExprKind::ConstBool(false)));
    assert!(matches!(
        &orders[3][1].kind,
        HirExprKind::ConstString(value) if value.is_empty()
    ));
    assert!(matches!(orders[3][2].kind, HirExprKind::ConstBool(false)));
    assert!(matches!(orders[3][3].kind, HirExprKind::ConstBool(false)));
}

#[test]
fn strategy_order_new_parameters_enforce_documented_types_and_oca_qualifier() {
    let cases = [
        (
            "strategy.entry(\"E\", strategy.long, oca_name=true)",
            "E0205",
        ),
        (
            "strategy.entry(\"E\", strategy.long, disable_alert=1)",
            "E0205",
        ),
        (
            "strategy.exit(\"X\", stop=1, trail_offset=\"bad\")",
            "E0205",
        ),
        ("strategy.close(\"E\", immediately=\"yes\")", "E0205"),
    ];
    for (call, code) in cases {
        let (_dump, diagnostics) = check(&format!("strategy(\"S\")\n{call}\n"));
        assert!(
            error_codes(&diagnostics).contains(&code),
            "{call}: {diagnostics:?}"
        );
    }

    let (_dump, diagnostics) = check(
        "strategy(\"S\")\n\
         dynamic_oca = close > open ? strategy.oca.cancel : strategy.oca.none\n\
         strategy.order(\"O\", strategy.long, oca_type=dynamic_oca)\n",
    );
    assert!(
        error_codes(&diagnostics).contains(&"E0206"),
        "{diagnostics:?}"
    );

    let result = analyze_src(
        "strategy(\"S\")\n\
         direction = close > open ? strategy.long : strategy.short\n\
         strategy.entry(\"E\", direction)\n",
    );
    assert!(!has_errors(&result.diagnostics), "{:?}", result.diagnostics);
    let direction = result
        .hir
        .body
        .iter()
        .find_map(|stmt| match stmt {
            HirStmt::Order { operands, .. } => Some(&operands[1]),
            _ => None,
        })
        .expect("entry direction operand");
    assert_eq!(
        direction.ty,
        Type::Enum(pinewasm_hir::STRATEGY_DIRECTION_TYPE.to_string())
    );
    assert_eq!(direction.qualifier, pinewasm_hir::Qualifier::Series);

    let (_dump, diagnostics) = check("strategy(\"S\")\nstrategy.entry(\"E\", \"strategy.long\")\n");
    assert!(
        error_codes(&diagnostics).contains(&"E0205"),
        "arbitrary strings are not strategy_direction values: {diagnostics:?}"
    );
}

#[test]
fn strategy_v6_rejects_na_for_direction_and_bool_command_parameters() {
    for call in [
        "strategy.entry(\"E\", na)",
        "strategy.entry(\"E\", strategy.long, disable_alert=na)",
        "strategy.close_all(immediately=na)",
    ] {
        let (_dump, diagnostics) = check(&format!("strategy(\"S\")\n{call}\n"));
        assert!(
            error_codes(&diagnostics).contains(&if call == "strategy.entry(\"E\", na)" {
                "E0205"
            } else {
                "E0220"
            }),
            "{call} must reject na: {diagnostics:?}"
        );
    }
}

#[test]
fn strategy_value_functions_are_always_series_qualified() {
    let (dump, diagnostics) = check(
        "strategy(\"S\")\n\
         closed_profit = strategy.closedtrades.profit(0)\n\
         account_value = strategy.convert_to_account(1)\n\
         entry_qty = strategy.default_entry_qty(100)\n\
         plot(closed_profit + account_value + entry_qty)\n",
    );
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
    for name in ["closed_profit", "account_value", "entry_qty"] {
        assert!(
            dump.contains(&format!("`{name}`: series float")),
            "{name} must keep the series qualifier:\n{dump}"
        );
    }
}

#[test]
fn strategy_risk_parameters_require_simple_or_weaker_values() {
    let series_cases = [
        "strategy.risk.max_position_size(close)",
        "strategy.risk.max_drawdown(close, strategy.cash)",
        "strategy.risk.max_intraday_loss(close, strategy.cash)",
        "strategy.risk.max_intraday_filled_orders(bar_index)",
        "strategy.risk.max_cons_loss_days(bar_index)",
        "strategy.risk.max_drawdown(10, strategy.cash, str.tostring(close))",
        "strategy.risk.max_position_size(strategy.closedtrades.profit(0))",
    ];
    for call in series_cases {
        let (_dump, diagnostics) = check(&format!("strategy(\"S\")\n{call}\n"));
        assert!(
            error_codes(&diagnostics).contains(&"E0206"),
            "{call} must reject a series risk argument: {diagnostics:?}"
        );
    }

    let result = analyze_src(
        "strategy(\"S\")\n\
         cap = input.float(3)\n\
         days = input.int(2)\n\
         strategy.risk.max_position_size(cap)\n\
         strategy.risk.allow_entry_in(strategy.direction.long)\n\
         strategy.risk.max_drawdown(25, strategy.percent_of_equity, \"drawdown\")\n\
         strategy.risk.max_intraday_loss(100, strategy.cash, \"intraday\")\n\
         strategy.risk.max_intraday_filled_orders(4, \"fills\")\n\
         strategy.risk.max_cons_loss_days(days, \"days\")\n",
    );
    assert!(
        !has_errors(&result.diagnostics),
        "input/const/simple risk arguments must compile: {:?}",
        result.diagnostics
    );
}

#[test]
fn strategy_risk_accepts_input_derived_simple_string_tokens() {
    let result = analyze_src(
        "strategy(\"S\")\n\
         long_only = input.bool(true)\n\
         allowed = long_only ? strategy.direction.long : strategy.direction.all\n\
         cash_mode = input.bool(true)\n\
         unit = cash_mode ? strategy.cash : strategy.percent_of_equity\n\
         strategy.risk.allow_entry_in(allowed)\n\
         strategy.risk.max_drawdown(10, unit)\n\
         strategy.risk.max_intraday_loss(10, unit)\n",
    );
    assert!(
        !has_errors(&result.diagnostics),
        "input-derived simple string tokens must compile: {:?}",
        result.diagnostics
    );
}

#[test]
fn strategy_risk_count_parameters_require_ints() {
    for call in [
        "strategy.risk.max_intraday_filled_orders(2.5)",
        "strategy.risk.max_cons_loss_days(2.5)",
    ] {
        let (_dump, diagnostics) = check(&format!("strategy(\"S\")\n{call}\n"));
        assert_eq!(
            error_codes(&diagnostics),
            vec!["E0205"],
            "{call}: {diagnostics:?}"
        );
    }

    let result = analyze_src(
        "strategy(\"S\")\n\
         fills = input.int(2)\n\
         days = input.int(3)\n\
         strategy.risk.max_intraday_filled_orders(fills)\n\
         strategy.risk.max_cons_loss_days(days)\n",
    );
    assert!(!has_errors(&result.diagnostics), "{:?}", result.diagnostics);
    for operands in result.hir.body.iter().filter_map(|stmt| match stmt {
        HirStmt::Order {
            kind:
                pinewasm_hir::OrderKind::RiskMaxIntradayFilledOrders
                | pinewasm_hir::OrderKind::RiskMaxConsLossDays,
            operands,
            ..
        } => Some(operands),
        _ => None,
    }) {
        assert_eq!(operands[0].ty, Type::Int);
    }
}

#[test]
fn strategy_risk_loss_threshold_literals_obey_documented_ranges() {
    for call in [
        "strategy.risk.max_drawdown(-1, strategy.cash)",
        "strategy.risk.max_intraday_loss(-1, strategy.cash)",
        "strategy.risk.max_drawdown(101, strategy.percent_of_equity)",
        "strategy.risk.max_intraday_loss(100.1, strategy.percent_of_equity)",
    ] {
        let (_dump, diagnostics) = check(&format!("strategy(\"S\")\n{call}\n"));
        assert_eq!(
            error_codes(&diagnostics),
            vec!["E0205"],
            "{call}: {diagnostics:?}"
        );
    }

    let (_dump, diagnostics) = check(
        "strategy(\"S\")\n\
         strategy.risk.max_drawdown(0, strategy.cash)\n\
         strategy.risk.max_drawdown(100, strategy.percent_of_equity)\n\
         strategy.risk.max_intraday_loss(0, strategy.percent_of_equity)\n\
         strategy.risk.max_intraday_loss(101, strategy.cash)\n",
    );
    assert!(
        !has_errors(&diagnostics),
        "inclusive zero/100 percent and unbounded non-negative cash must compile: {diagnostics:?}"
    );
}

#[test]
fn indicator_rejects_calc_on_every_tick() {
    for value in ["true", "false", "3"] {
        let (_, diagnostics) = check(&format!(
            "indicator(\"I\", calc_on_every_tick={value})\nplot(close)\n"
        ));
        assert_eq!(error_codes(&diagnostics), vec!["E0228"]);
    }
    let result = analyze_src("indicator(\"I\")\nplot(close)\n");
    assert!(!result.hir.indicator.unwrap().calc_on_every_tick);
}

#[test]
fn strategy_and_indicator_are_mutually_exclusive() {
    // strategy() then indicator() => E0240.
    let (_dump, diagnostics) = check("strategy(\"S\")\nindicator(\"I\")\nplot(close)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0240"]);
    // indicator() then strategy() => E0240 as well.
    let (_dump, diagnostics) = check("indicator(\"I\")\nstrategy(\"S\")\nplot(close)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0240"]);
}

#[test]
fn duplicate_strategy_is_e0213() {
    let (_dump, diagnostics) = check("strategy(\"A\")\nstrategy(\"B\")\nplot(close)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0213"]);
}

#[test]
fn strategy_non_string_title_is_rejected() {
    let (_dump, diagnostics) = check("strategy(123)\nplot(close)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0205"]);
}

#[test]
fn strategy_missing_title_is_e0204() {
    let (_dump, diagnostics) = check("strategy()\nplot(close)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0204"]);
}

#[test]
fn strategy_exit_requires_a_documented_effective_price_pair() {
    for call in [
        "strategy.exit(\"X\")",
        "strategy.exit(\"X\", trail_offset=1)",
        "strategy.exit(\"X\", trail_price=100)",
    ] {
        let (_dump, diagnostics) = check(&format!("strategy(\"S\")\n{call}\n"));
        assert!(
            error_codes(&diagnostics).contains(&"E0219"),
            "{call}: {diagnostics:?}"
        );
    }

    for call in [
        "strategy.exit(\"X\", stop=90, trail_price=100)",
        "strategy.exit(\"X\", trail_price=100, trail_offset=1)",
        "strategy.exit(\"X\", trail_points=10, trail_offset=1)",
    ] {
        let (_dump, diagnostics) = check(&format!("strategy(\"S\")\n{call}\n"));
        assert!(!has_errors(&diagnostics), "{call}: {diagnostics:?}");
    }
}

#[test]
fn strategy_bad_qty_type_token_is_rejected() {
    let (_dump, diagnostics) = check("strategy(\"S\", default_qty_type=\"bogus\")\nplot(close)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0219"]);
}

#[test]
fn strategy_risk_allow_entry_in_requires_direction_token() {
    let (_dump, diagnostics) =
        check("strategy(\"S\")\nstrategy.risk.allow_entry_in(strategy.long)\nplot(close)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0205"]);
}

#[test]
fn strategy_risk_max_drawdown_requires_type_and_accepts_percent_and_cash_units() {
    let (_dump, diagnostics) = check(
        "strategy(\"S\")\n\
         strategy.risk.max_drawdown(10, strategy.percent_of_equity)\n\
         strategy.risk.max_drawdown(500, strategy.cash, \"halt\")\n\
         plot(close)\n",
    );
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );

    let (_dump, diagnostics) =
        check("strategy(\"S\")\nstrategy.risk.max_drawdown(10)\nplot(close)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0204"]);
}

#[test]
fn strategy_risk_max_drawdown_rejects_direction_tokens_as_units() {
    let (_dump, diagnostics) =
        check("strategy(\"S\")\nstrategy.risk.max_drawdown(10, strategy.long)\nplot(close)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0205"]);
}

#[test]
fn strategy_risk_intraday_and_loss_day_rules_typecheck() {
    let (_dump, diagnostics) = check(
        "strategy(\"S\")\n\
         strategy.risk.max_cons_loss_days(2, \"halt\")\n\
         strategy.risk.max_intraday_filled_orders(3)\n\
         strategy.risk.max_intraday_loss(10, strategy.percent_of_equity, \"halt\")\n\
         strategy.risk.max_intraday_loss(500, strategy.cash)\n\
         plot(close)\n",
    );
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );

    let (_dump, diagnostics) =
        check("strategy(\"S\")\nstrategy.risk.max_intraday_loss(10, strategy.long)\nplot(close)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0205"]);

    let (_dump, diagnostics) =
        check("strategy(\"S\")\nstrategy.risk.max_intraday_loss(10)\nplot(close)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0204"]);
}

#[test]
fn strategy_negative_initial_capital_is_rejected() {
    let (_dump, diagnostics) = check("strategy(\"S\", initial_capital=-1)\nplot(close)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0219"]);
}

// -----------------------------------------------------------------------
// Context accessors: the `session.*` type constants plus the
// `timeframe.*`/`syminfo.*` variable and function registry entries. Runtime
// values are asserted in the VM tests; here we pin the registry shape.
// -----------------------------------------------------------------------

#[test]
fn context_accessors_registry() {
    use pinewasm_hir::Type;
    let registry = crate::BuiltinRegistry::standard();
    // session.regular / session.extended are const-string session tokens.
    for member in ["session.regular", "session.extended"] {
        let c = registry
            .lookup_constant(member)
            .unwrap_or_else(|| panic!("{member} not registered"));
        assert_eq!(c.ty, Type::String, "{member} must be a const string");
        assert_eq!(c.string_value, Some(member));
    }
    // The timeframe.*/syminfo.*/session.* variables register as builtin series
    // with the expected scalar type.
    let series_ty = |name: &str| {
        let s = registry
            .lookup_series(name)
            .unwrap_or_else(|| panic!("{name} not a registered series"));
        if s.is_bool() {
            Type::Bool
        } else if s.is_int() {
            Type::Int
        } else if s.is_string() {
            Type::String
        } else {
            Type::Float
        }
    };
    assert_eq!(series_ty("timeframe.period"), Type::String);
    assert_eq!(series_ty("timeframe.multiplier"), Type::Int);
    assert_eq!(series_ty("timeframe.isdaily"), Type::Bool);
    assert_eq!(series_ty("syminfo.ticker"), Type::String);
    assert_eq!(series_ty("syminfo.tickerid"), Type::String);
    assert!(registry.lookup_constant("syminfo.ticker").is_none());
    assert!(registry.lookup_constant("syminfo.tickerid").is_none());
    assert_eq!(series_ty("syminfo.currency"), Type::String);
    assert_eq!(series_ty("syminfo.mintick"), Type::Float);
    assert_eq!(series_ty("syminfo.pricescale"), Type::Int);
    assert_eq!(series_ty("session.isfirstbar"), Type::Bool);
    assert_eq!(series_ty("time_tradingday"), Type::Int);
    // Fundamental base types: the float-valued estimate count and the
    // integer-valued total shares outstanding.
    assert_eq!(series_ty("syminfo.employees"), Type::Int);
    assert_eq!(series_ty("syminfo.recommendations_total"), Type::Int);
    assert_eq!(series_ty("syminfo.recommendations_date"), Type::Int);
    assert_eq!(series_ty("syminfo.target_price_estimates"), Type::Float);
    assert_eq!(series_ty("syminfo.shares_outstanding_total"), Type::Int);
    assert_eq!(series_ty("syminfo.shares_outstanding_float"), Type::Float);
    assert_eq!(series_ty("syminfo.target_price_average"), Type::Float);
    // The two timeframe.* functions register as pure builtins.
    for name in ["timeframe.in_seconds", "timeframe.from_seconds"] {
        assert!(
            registry.lookup_function(name).is_some(),
            "{name} not registered as a function"
        );
    }
}

#[test]
fn timeframe_values_are_simple_until_history_is_read() {
    let source = "indicator(\"Timeframe qualifiers\")\ntf = timeframe.period\nmain = timeframe.main_period\nmult = timeframe.multiplier\nflags = timeframe.isdaily or timeframe.isweekly or timeframe.ismonthly or timeframe.isseconds\nprior = timeframe.period[1]\nplot(mult)\n";
    let (dump, diagnostics) = check(source);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    for expected in [
        "symbol 0 `tf`: simple string",
        "symbol 1 `main`: simple string",
        "symbol 2 `mult`: simple int",
        "symbol 3 `flags`: simple bool",
        "symbol 4 `prior`: series string",
    ] {
        assert!(dump.contains(expected), "missing {expected} in {dump}");
    }
}

#[test]
fn dividends_earnings_future_series_resolve_as_typed_builtin_series() {
    use pinewasm_hir::Type;
    let registry = crate::BuiltinRegistry::standard();
    let series_ty = |name: &str| {
        let s = registry
            .lookup_series(name)
            .unwrap_or_else(|| panic!("{name} not a registered series"));
        if s.is_int() {
            Type::Int
        } else {
            Type::Float
        }
    };
    // `dividends.future_*` / `earnings.future_*` are built-in series, NOT
    // `request.dividends()`/`request.earnings()` field selectors. Amounts / EPS / revenue are `series float`; the ex/pay
    // dates and period-end / announcement times are epoch-ms `series int`.
    assert_eq!(series_ty("dividends.future_amount"), Type::Float);
    assert_eq!(series_ty("dividends.future_ex_date"), Type::Int);
    assert_eq!(series_ty("dividends.future_pay_date"), Type::Int);
    assert_eq!(series_ty("earnings.future_eps"), Type::Float);
    assert_eq!(series_ty("earnings.future_period_end_time"), Type::Int);
    assert_eq!(series_ty("earnings.future_revenue"), Type::Float);
    assert_eq!(series_ty("earnings.future_time"), Type::Int);
    // They must not be registered as const-string tokens (a field-selector
    // regression):
    // the valid `field` selectors remain only `dividends.gross/net` and
    // `earnings.actual/estimate/standardized`.
    for name in [
        "dividends.future_amount",
        "dividends.future_ex_date",
        "dividends.future_pay_date",
        "earnings.future_eps",
        "earnings.future_period_end_time",
        "earnings.future_revenue",
        "earnings.future_time",
    ] {
        assert!(
            registry.lookup_constant(name).is_none(),
            "{name} must not be a const string; it is a builtin series"
        );
    }
    // Numeric use compiles: as const strings, `plot(dividends.future_amount)`
    // and `syminfo.mintick * earnings.future_eps` would be type errors.
    let (_dump, diagnostics) = check(
        "indicator(\"D\")\n\
         plot(dividends.future_amount)\n\
         plot(earnings.future_time)\n\
         plot(syminfo.mintick * earnings.future_eps)\n",
    );
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
}

// ---- timestamp() + ticker.* -----------------------------------------------

#[test]
fn timestamp_both_overloads_analyze_and_type_as_int() {
    // UTC overload: 3..=6 int fields, result int.
    let (hir, diags) = check("indicator(\"T\")\nplot(timestamp(2021, 1, 1))\n");
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
    assert!(hir.contains("CallPure"));
    // Timezone overload: leading string then int fields.
    let (_, diags) = check("indicator(\"T\")\nplot(timestamp(\"UTC-5\", 2021, 1, 1, 0, 0))\n");
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
}

#[test]
fn timestamp_wrong_field_type_is_e0205() {
    // A float year is not an int field.
    let (_, diags) = check("indicator(\"T\")\nplot(timestamp(2021.5, 1, 1))\n");
    assert!(error_codes(&diags).contains(&"E0205"));
}

#[test]
fn timestamp_year_first_strings_are_const_input_time_values() {
    let source = r#"//@version=6
indicator("Timestamp forms")
start = input.time(timestamp("2019-11-01T00:00:00"), "Start")
end = input.time(timestamp("2023-12-12T00:00:00"), "End")
older = input.time(timestamp("2015 03 03"), "Older")
plot(start == 1572566400000 and end == 1702339200000 and older == 1425340800000 ? 1 : 0)
"#;
    let (hir, diags) = check(source);
    assert!(
        !diags.iter().any(Diagnostic::is_error),
        "unexpected diagnostics: {diags:?}"
    );
    assert!(hir.contains("input 0 input.time title=\"Start\" defval=1572566400000"));
    assert!(hir.contains("input 1 input.time title=\"End\" defval=1702339200000"));
    assert!(hir.contains("input 2 input.time title=\"Older\" defval=1425340800000"));
}

#[test]
fn timestamp_year_first_strings_reject_invalid_calendar_dates() {
    for value in ["2019-02-29T00:00:00", "2015 13 03", "2023-12-12T24:00:00"] {
        let source = format!("indicator(\"T\")\nplot(timestamp(\"{value}\"))\n");
        let (_, diags) = check(&source);
        assert!(error_codes(&diags).contains(&"E0204"), "{value}: {diags:?}");
    }
}

#[test]
fn ticker_functions_register_and_analyze_as_strings() {
    let registry = crate::BuiltinRegistry::standard();
    for name in [
        "ticker.new",
        "ticker.modify",
        "ticker.heikinashi",
        "ticker.kagi",
        "ticker.linebreak",
        "ticker.pointfigure",
        "ticker.renko",
        "ticker.standard",
        "ticker.inherit",
    ] {
        assert!(
            registry.lookup_function(name).is_some(),
            "{name} not registered as a function"
        );
    }
    // A ticker id concatenated with a string proves it types as `string`.
    let (_, diags) = check(
        "indicator(\"T\")\ns = ticker.heikinashi(\"AAPL\") + \"!\"\nplot(na(s) ? 0.0 : 1.0)\n",
    );
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
}

// ---- runtime.error() + the display plot family ----------------------------

#[test]
fn display_plot_family_compiles_cleanly() {
    // Each display function type-checks and compiles with representative args.
    let (_, diags) = check(
        "//@version=6\nindicator(\"D\")\n\
         plotshape(close > open)\n\
         plotchar(close > open)\n\
         plotarrow(close - open)\n\
         plotcandle(open, high, low, close)\n\
         plotbar(open, high, low, close)\n\
         hline(50)\n\
         bgcolor(color.red)\n\
         barcolor(color.green)\n\
         plot(close)\n",
    );
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
}

#[test]
fn display_style_metadata_and_named_color_channels_are_retained() {
    let parsed = parse(
        "//@version=6\nindicator(\"Styled\")\n\
         plot(close, title = \"Histogram\", color = close > open ? color.new(color.green, 25) : color.red, linewidth = 3, style = plot.style_histogram, histbase = -2.5, offset = 2, show_last = 7, display = display.pane, precision = 4, force_overlay = true, linestyle = plot.linestyle_dashed)\n\
         hline(50, title = \"Reference\", color = color.blue, linestyle = hline.style_dotted, linewidth = 2, display = display.none)\n\
         plotshape(close > open, title = \"Signal\", style = shape.square, location = location.abovebar, color = color.yellow, text = \"S\", size = size.large)\n\
         plotcandle(open, high, low, close, title = \"Synthetic\", color = color.green, wickcolor = color.blue, bordercolor = color.red)\n\
         bgcolor(color.new(color.orange, 88), title = \"Regime\")\n\
         barcolor(close > open ? color.green : color.red, title = \"Bars\")\n",
    );
    assert!(
        !has_errors(&parsed.diagnostics),
        "parse: {:?}",
        parsed.diagnostics
    );
    let result = analyze(&parsed.program);
    assert!(
        !has_errors(&result.diagnostics),
        "analysis: {:?}",
        result.diagnostics
    );

    let histogram = &result.hir.plots[0];
    assert_eq!(histogram.title.as_deref(), Some("Histogram"));
    assert_eq!(histogram.style.kind, OutputDisplayKind::Plot);
    assert_eq!(histogram.style.plot_style, PlotStyleKind::Histogram);
    assert_eq!(histogram.style.line_style, PlotLineStyle::Dashed);
    assert_eq!(histogram.style.line_width, 3);
    assert_eq!(histogram.style.histogram_base, -2.5);
    assert_eq!(histogram.style.offset, 2);
    assert_eq!(histogram.style.show_last, Some(7));
    assert_eq!(histogram.style.display, PlotDisplay::Pane);
    assert_eq!(histogram.style.precision, Some(4));
    assert!(histogram.style.force_overlay);
    assert_eq!(histogram.color_roles, vec![PlotColorRole::Color]);

    let hline = &result.hir.plots[1];
    assert_eq!(hline.style.kind, OutputDisplayKind::Hline);
    assert_eq!(hline.style.line_style, PlotLineStyle::Dotted);
    assert_eq!(hline.style.line_width, 2);
    assert_eq!(hline.style.display, PlotDisplay::None);

    let shape = &result.hir.plots[2];
    assert_eq!(shape.style.marker_shape, Some(MarkerShape::Square));
    assert_eq!(shape.style.marker_location, Some(MarkerLocation::AboveBar));

    let candle = &result.hir.plots[3..7];
    assert!(candle
        .iter()
        .all(|plot| plot.style.kind == OutputDisplayKind::PlotCandle));
    assert_eq!(candle[0].style.group, Some(3));
    assert_eq!(candle[0].style.ohlc_field, Some(OhlcField::Open));
    assert_eq!(candle[3].style.ohlc_field, Some(OhlcField::Close));
    assert_eq!(
        candle[0].color_roles,
        vec![
            PlotColorRole::Color,
            PlotColorRole::Wick,
            PlotColorRole::Border
        ]
    );

    assert_eq!(result.hir.visuals.len(), 2);
    assert_eq!(result.hir.visuals[0].style.kind, OutputDisplayKind::Bgcolor);
    assert_eq!(
        result.hir.visuals[1].style.kind,
        OutputDisplayKind::Barcolor
    );
    assert_eq!(
        result.hir.visuals[1].color_roles,
        vec![PlotColorRole::Color]
    );
    assert!(result
        .hir
        .plots
        .iter()
        .all(|plot| plot.style.unsupported.is_empty()));
    assert!(result
        .hir
        .visuals
        .iter()
        .all(|visual| visual.style.unsupported.is_empty()));
}

#[test]
fn portable_display_metadata_is_not_tied_to_one_renderer() {
    let parsed = parse(
        "//@version=6\nindicator(\"Portable\")\n\
         plot(close, style = plot.style_cross, editable = false, display = display.data_window, format = format.mintick)\n\
         plotshape(close > open, style = shape.xcross, location = location.top, color = color.green, textcolor = color.white, editable = false)\n\
         plotarrow(close - open, minheight = 2, maxheight = 8)\n\
         bgcolor(color.red)\n",
    );
    assert!(
        !has_errors(&parsed.diagnostics),
        "parse: {:?}",
        parsed.diagnostics
    );
    let result = analyze(&parsed.program);
    assert!(
        !has_errors(&result.diagnostics),
        "analysis: {:?}",
        result.diagnostics
    );

    let plot = &result.hir.plots[0];
    assert_eq!(plot.style.plot_style, PlotStyleKind::Cross);
    assert_eq!(plot.style.display, PlotDisplay::DataWindow);
    assert_eq!(plot.style.format.as_deref(), Some("format.mintick"));
    assert!(!plot.style.editable);

    let shape = &result.hir.plots[1];
    assert_eq!(shape.style.marker_shape, Some(MarkerShape::XCross));
    assert_eq!(shape.style.marker_location, Some(MarkerLocation::Top));
    assert_eq!(
        shape.color_roles,
        vec![PlotColorRole::Color, PlotColorRole::Text]
    );

    let arrow = &result.hir.plots[2];
    assert_eq!(arrow.style.min_height, Some(2));
    assert_eq!(arrow.style.max_height, Some(8));

    assert!(result
        .hir
        .plots
        .iter()
        .all(|plot| plot.style.unsupported.is_empty()));
    assert!(result
        .hir
        .visuals
        .iter()
        .all(|visual| visual.style.unsupported.is_empty()));
}

#[test]
fn input_derived_linewidth_is_captured_by_the_engine() {
    // The portable display contract reports a limitation only for a style
    // scalar that changes bar to bar. Pine caps `linewidth` at `input`, so an
    // expression over inputs holds one value for the whole run. The engine
    // captures that value and claims no renderer limitation.
    let source = r"
//@version=6
indicator('bound', overlay = true)
speed = input.string('Slow', 'Speed', options = ['Slow', 'Fast'])
width = input.int(2, 'Width')
emphasized = speed == 'Slow' ? width : 1
plot(close, 'Bound', linewidth = math.max(1, emphasized))
";
    let parsed = parse(source);
    let result = analyze(&parsed.program);
    assert!(!has_errors(&result.diagnostics), "{:?}", result.diagnostics);

    let style = &result.hir.plots[0].style;
    assert!(style.bound_scalars.is_empty());
    assert_eq!(style.runtime_scalars.len(), 1);
    assert_eq!(style.runtime_scalars[0].field, "linewidth");
    assert_eq!(
        style.runtime_scalars[0].qualifier,
        pinewasm_hir::Qualifier::Input
    );
    assert_eq!(style.line_width, 1);
    assert!(style.unsupported.is_empty());
}

#[test]
fn a_series_qualified_pure_call_is_never_bound() {
    // Serializability is not the invariant, the qualifier is. `math.random` is
    // a pure call over constants, so it converts cleanly, but it is
    // series-qualified and changes every bar. Binding it would trade an honest
    // report for a silently wrong width because style scalars are carried once
    // per output.
    let source = r"
//@version=6
indicator('random', overlay = true)
plot(close, 'Random', linewidth = int(math.random(1, 4)))
";
    let parsed = parse(source);
    let result = analyze(&parsed.program);
    assert!(has_errors(&result.diagnostics));
    assert_eq!(error_codes(&result.diagnostics), vec!["E0206"]);
    assert!(result.hir.plots.is_empty());
}

#[test]
fn nz_cannot_supply_input_qualified_parameters() {
    for (body, code) in [
        (
            "base = input.float(7.5)\nplot(close, histbase = nz(na, base))",
            "E0206",
        ),
        (
            "viaNz = input.bool(true, active = nz(na, 0) > 1)\nplot(close)",
            "E0230",
        ),
    ] {
        let (_, diagnostics) = check(&format!("//@version=6\nindicator(\"N\")\n{body}\n"));
        assert_eq!(error_codes(&diagnostics), vec![code]);
    }
}

#[test]
fn a_literal_style_scalar_binds_nothing() {
    // Guards the common path: a folded literal must not start carrying a
    // binding, which would make every host do resolution work for nothing.
    let source = r"
//@version=6
indicator('literal', overlay = true)
plot(close, 'Literal', linewidth = 3, offset = 2)
";
    let parsed = parse(source);
    let result = analyze(&parsed.program);
    assert!(!has_errors(&result.diagnostics), "{:?}", result.diagnostics);
    let plot = &result.hir.plots[0];
    assert_eq!(plot.style.line_width, 3);
    assert_eq!(plot.style.offset, 2);
    assert!(plot.style.bound_scalars.is_empty());
    assert!(plot.style.unsupported.is_empty());
}

#[test]
fn na_inside_an_input_derived_style_scalar_stays_bindable() {
    // Without an explicit `na` node, this common conditional-style expression
    // is unrepresentable and falls back to being reported as a renderer
    // limitation it is not.
    let source = r"
//@version=6
indicator('na', overlay = true)
useMirror = input.bool(false, 'Use Mirror')
width = input.int(2, 'Width')
mirrorWidth = useMirror ? width : na
plot(close, 'Mirror', offset = math.max(1, mirrorWidth))
";
    let parsed = parse(source);
    let result = analyze(&parsed.program);
    assert!(!has_errors(&result.diagnostics), "{:?}", result.diagnostics);
    let plot = &result.hir.plots[0];
    assert!(plot.style.unsupported.is_empty());
    assert_eq!(plot.style.bound_scalars.len(), 1);
    assert_eq!(plot.style.bound_scalars[0].field, "offset");
    let InputActiveExpr::Call { args, .. } = &plot.style.bound_scalars[0].expression else {
        panic!("expected math.max: {:?}", plot.style.bound_scalars[0]);
    };
    let InputActiveExpr::Ternary { when_false, .. } = &args[1] else {
        panic!("expected the mirror ternary: {args:?}");
    };
    assert!(matches!(**when_false, InputActiveExpr::Na));
}

#[test]
fn input_derived_switch_style_scalars_bind_instead_of_reporting_a_limitation() {
    // An input-dependent marker offset can use a `switch`. It lowers to a
    // hidden slot filled by an if/else chain, so the slot read must carry the
    // same host-resolvable expression a ternary would. Otherwise the host
    // loses the requested offset when rendering the marker.
    let source = r"
//@version=6
indicator('switch offset', overlay = true)
alignCurrent = input.bool(false, 'Align Signal with Current Bar')
signalOffset = switch
    alignCurrent => 0
    => -1
mode = input.string('Near', 'Mode', options = ['Near', 'Far'])
modeOffset = switch mode
    'Near' => 1
    'Far' => 3
plotshape(close > open, style = shape.triangleup, offset = signalOffset)
plotshape(close < open, style = shape.triangledown, offset = modeOffset)
";
    let parsed = parse(source);
    let result = analyze(&parsed.program);
    assert!(!has_errors(&result.diagnostics), "{:?}", result.diagnostics);
    assert_eq!(result.hir.plots.len(), 2);
    for visual in &result.hir.plots {
        assert!(
            visual.style.unsupported.is_empty(),
            "{:?}",
            visual.style.unsupported
        );
        assert_eq!(
            visual
                .style
                .bound_scalars
                .iter()
                .map(|entry| entry.field.as_str())
                .collect::<Vec<_>>(),
            vec!["offset"],
        );
    }
    // Subjectless: `alignCurrent ? 0 : -1`.
    let InputActiveExpr::Ternary {
        condition,
        when_true,
        when_false,
    } = &result.hir.plots[0].style.bound_scalars[0].expression
    else {
        panic!("expected a ternary: {:?}", result.hir.plots[0].style);
    };
    assert!(matches!(**condition, InputActiveExpr::Input { .. }));
    assert!(matches!(
        **when_true,
        InputActiveExpr::Value {
            value: InputValue::Int(0)
        }
    ));
    assert!(matches!(
        **when_false,
        InputActiveExpr::Unary { .. } | InputActiveExpr::Value { .. }
    ));
    // Subject switch without a default: the unmatched path is `na`.
    let InputActiveExpr::Ternary {
        condition,
        when_false,
        ..
    } = &result.hir.plots[1].style.bound_scalars[0].expression
    else {
        panic!("expected a ternary: {:?}", result.hir.plots[1].style);
    };
    assert!(matches!(
        **condition,
        InputActiveExpr::Binary {
            op: InputActiveBinaryOp::Eq,
            ..
        }
    ));
    let InputActiveExpr::Ternary {
        when_false: tail, ..
    } = &**when_false
    else {
        panic!("expected the second arm: {when_false:?}");
    };
    assert!(matches!(**tail, InputActiveExpr::Na));
}

#[test]
fn input_fixed_style_switch_and_display_ternary_bind_as_run_scalars() {
    // Both style and display depend on inputs but
    // remain fixed for one run. They must travel as host-resolvable style
    // bindings instead of falling back to line/all or being marked unsupported.
    let source = r#"
//@version=6
indicator("Input style contract")
enabled = input.bool(false, "Enabled")
width = input.int(7, "Width", minval = 1)
kind = input.string("Histogram", "Style", options = ["Line", "Histogram"])
chosenStyle = switch kind
    "Histogram" => plot.style_histogram
    => plot.style_line
plot(close, "Style", linewidth = width, style = chosenStyle, display = enabled ? display.all : display.none)
"#;
    let parsed = parse(source);
    let result = analyze(&parsed.program);
    assert!(!has_errors(&result.diagnostics), "{:?}", result.diagnostics);
    assert_eq!(result.hir.plots.len(), 1);

    let style = &result.hir.plots[0].style;
    assert_eq!(
        style
            .bound_scalars
            .iter()
            .map(|entry| entry.field.as_str())
            .collect::<Vec<_>>(),
        vec!["style", "display"],
    );
    assert!(style.unsupported.is_empty(), "{:?}", style.unsupported);
    // Raw declarations retain their neutral defaults. The engine supplies
    // linewidth while the host resolves style and display bindings.
    assert_eq!(style.line_width, 1);
    assert_eq!(style.plot_style, PlotStyleKind::Line);
    assert_eq!(style.display, PlotDisplay::All);
    assert_eq!(style.runtime_scalars.len(), 1);
    assert_eq!(style.runtime_scalars[0].field, "linewidth");
    assert_eq!(
        style.runtime_scalars[0].qualifier,
        pinewasm_hir::Qualifier::Input
    );
    assert!(matches!(
        style.bound_scalars[0].expression,
        InputActiveExpr::Ternary { .. }
    ));
    assert!(matches!(
        style.bound_scalars[1].expression,
        InputActiveExpr::Ternary { .. }
    ));
}

#[test]
fn a_series_switch_style_scalar_still_reports_a_limitation() {
    // The qualifier stays the gate: a `switch` over a series condition is not
    // bound, whatever shape its arms have.
    let source = r"
//@version=6
indicator('series switch', overlay = true)
o = switch
    close > open => 0
    => -1
plot(close, 'Series', linewidth = 1, offset = 0)
plotshape(true, offset = o)
";
    let parsed = parse(source);
    let result = analyze(&parsed.program);
    // `offset` is `simple int`, so a series switch is rejected outright.
    assert!(has_errors(&result.diagnostics));
}

#[test]
fn display_plot_family_type_checks_each_member() {
    // VALUE-ASSERTING: every plot-family member accepts its correctly-typed
    // argument (clean compile) and rejects a wrong-typed one with E0205. This
    // pins the observable acceptance contract of the surface, not merely that a
    // representative script compiles. One table drives the whole cluster.
    let cases: &[(&str, &str)] = &[
        // (accepted call, wrong-typed positional call -> E0205). The data arg
        // (shape/char: bool OR number; arrow/candle/bar: number) or the color
        // arg (barcolor) is given a mismatched type. A `plotchar` bool passes;
        // a color (neither bool nor number) is the mismatch.
        ("plotchar(close > open)", "plotchar(color.red)"),
        ("plotarrow(close - open)", "plotarrow(close > open)"),
        (
            "plotcandle(open, high, low, close)",
            "plotcandle(close > open, high, low, close)",
        ),
        (
            "plotbar(open, high, low, close)",
            "plotbar(close > open, high, low, close)",
        ),
        ("barcolor(color.green)", "barcolor(close)"),
    ];
    for (ok_call, bad_call) in cases {
        let (_, ok) = check(&format!(
            "//@version=6\nindicator(\"D\")\n{ok_call}\nplot(close)\n"
        ));
        assert!(
            ok.is_empty(),
            "`{ok_call}` should compile clean, got: {ok:?}"
        );
        let (_, bad) = check(&format!(
            "//@version=6\nindicator(\"D\")\n{bad_call}\nplot(close)\n"
        ));
        assert!(
            error_codes(&bad).contains(&"E0205"),
            "`{bad_call}` should be E0205, got: {bad:?}"
        );
    }
}

#[test]
fn fill_and_bgcolor_compile_and_surface_no_data_plot() {
    // `fill(hline1, hline2, color, ...)` and `bgcolor(color)` are color/id-only:
    // they type-check and evaluate but surface no data plot of their own. Assert
    // the observable contract — a clean compile and exactly three output plots
    // (the two hline endpoints and `plot(close)`).
    let (dump, diags) = check(
        "//@version=6\nindicator(\"F\")\n\
         upper = hline(1)\n\
         lower = hline(2)\n\
         fill(upper, lower, color.new(color.blue, 90))\n\
         bgcolor(color.new(color.red, 90))\n\
         plot(close)\n",
    );
    assert!(
        diags.is_empty(),
        "valid fill/bgcolor should compile clean: {diags:?}"
    );
    // The two hline endpoints and explicit plot(close) are surfaced. fill and
    // bgcolor are color/id-only and contribute no additional plot slot.
    let plot_count = dump.matches("Plot slot").count();
    assert_eq!(
        plot_count, 3,
        "expected two hlines and plot(close), no fill/bgcolor plot:\n{dump}"
    );
}

#[test]
fn runtime_error_compiles_and_accepts_a_string() {
    let (_, diags) =
        check("//@version=6\nindicator(\"R\")\nruntime.error(\"boom\")\nplot(close)\n");
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
}

#[test]
fn plotshape_accepts_bool_or_number_rejects_other_types() {
    // `plotshape`/`plotchar` accept a `series bool` or a `series int/float`
    // (a shape is plotted where the value is not `na` and not `0`) — the
    // ubiquitous `cond ? price : na` idiom. A non-bool, non-numeric series
    // (here a string) is still E0205.
    let (_, bool_diags) = check("indicator(\"X\")\nplotshape(close > open)\n");
    assert!(
        bool_diags.is_empty(),
        "bool condition should compile: {bool_diags:?}"
    );
    let (_, num_diags) = check("indicator(\"X\")\nplotshape(close)\n");
    assert!(
        num_diags.is_empty(),
        "numeric series should compile (plotted where not na/0): {num_diags:?}"
    );
    let (_, idiom_diags) =
        check("indicator(\"X\")\nplotshape(close > open ? high : na)\nplot(close)\n");
    assert!(
        idiom_diags.is_empty(),
        "the `cond ? price : na` idiom should compile: {idiom_diags:?}"
    );
    let (_, str_diags) = check("indicator(\"X\")\nplotshape(syminfo.ticker)\n");
    assert_eq!(error_codes(&str_diags), vec!["E0205"]);
}

#[test]
fn runtime_error_of_non_string_is_rejected() {
    let (_, diags) = check("indicator(\"X\")\nruntime.error(42)\n");
    assert_eq!(error_codes(&diags), vec!["E0205"]);
}

#[test]
fn display_family_inside_function_body_is_rejected() {
    let (_, diags) = check("indicator(\"X\")\nf() =>\n    plotshape(true)\n    1\nplot(f())\n");
    assert_eq!(error_codes(&diags), vec!["E0254"]);
}

#[test]
fn effect_only_display_call_in_expression_position_is_rejected() {
    let (_, diags) = check("indicator(\"X\")\nx = bgcolor(color.red)\nplot(close)\n");
    assert_eq!(error_codes(&diags), vec!["E0212"]);
}

// ---- order-flow footprint / volume_row interfaces -------------------------

#[test]
fn footprint_full_surface_analyzes_cleanly() {
    // Every footprint.* and volume_row.* accessor type-checks against the
    // seeded object types; `request.footprint()` yields a `footprint` object.
    let (_, diags) = check(
        "//@version=6\nindicator(\"FP\")\n\
         fp = request.footprint(\"XYZ\")\n\
         rows = footprint.rows(fp)\n\
         vr = array.get(rows, 0)\n\
         w = footprint.get_row_by_price(fp, 100.0)\n\
         t = footprint.buy_volume(fp) + footprint.sell_volume(fp) + footprint.total_volume(fp) \
             + footprint.delta(fp) + footprint.poc(fp) + footprint.vah(fp) + footprint.val(fp)\n\
         u = volume_row.buy_volume(vr) + volume_row.sell_volume(vr) + volume_row.total_volume(vr) \
             + volume_row.delta(vr) + volume_row.up_price(vr) + volume_row.down_price(vr)\n\
         plot(t + u + array.size(rows) + (na(w) ? 0.0 : 1.0) \
              + (volume_row.has_buy_imbalance(vr) or volume_row.has_sell_imbalance(vr) ? 1.0 : 0.0))\n",
    );
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
}

#[test]
fn footprint_accessor_on_wrong_receiver_type_is_e0245() {
    // A footprint accessor applied to a non-footprint receiver is E0245.
    let (_, diags) =
        check("//@version=6\nindicator(\"FP\")\nplot(footprint.total_volume(close))\n");
    assert_eq!(error_codes(&diags), vec!["E0245"]);
}

#[test]
fn volume_row_accessor_on_wrong_receiver_type_is_e0245() {
    let (_, diags) = check("//@version=6\nindicator(\"FP\")\nplot(volume_row.up_price(close))\n");
    assert_eq!(error_codes(&diags), vec!["E0245"]);
}

// ---- host-or-default surface (bid/ask, input.price/time,
// request.security_lower_tf/seed, chart colors, format.price) ----------------

#[test]
fn misc_host_or_default_surface_type_checks_cleanly() {
    // Every interface used below compiles without diagnostics: the language surface
    // is real and testable; the runtime value is host-or-default.
    let (_, diags) = check(
        "//@version=6\nindicator(\"M\")\n\
         p = input.price(10.0)\n\
         t = input.time(1000)\n\
         a = request.security_lower_tf(syminfo.tickerid, \"1\", close)\n\
         s = request.seed(\"src\", \"ACME\", close)\n\
         c = chart.bg_color\n\
         f = format.price\n\
         plot(p + t + array.size(a) + (na(s) ? 0.0 : s) + close)\n\
         plot(bid + ask)\n\
         plot(syminfo.expiration_date)\n\
         plot(chart.left_visible_bar_time + chart.right_visible_bar_time)\n",
    );
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
}

#[test]
fn scalar_request_interfaces_bind_the_canonical_named_parameters() {
    let (_, diags) = check(
        "//@version=6\nindicator(\"R\")\n\
         a = request.dividends(ticker = \"MARKET:ACME\", field = dividends.net, gaps = barmerge.gaps_off, lookahead = barmerge.lookahead_off, ignore_invalid_symbol = true, currency = currency.USD)\n\
         b = request.splits(ticker = \"MARKET:ACME\", field = splits.numerator, ignore_invalid_symbol = false)\n\
         c = request.earnings(ticker = \"MARKET:ACME\", field = earnings.actual, currency = currency.USD)\n\
         d = request.financial(symbol = \"MARKET:ACME\", financial_id = \"TOTAL_REVENUE\", period = \"FQ\", gaps = barmerge.gaps_off, ignore_invalid_symbol = true, currency = currency.USD)\n\
         e = request.economic(country_code = \"US\", field = \"GDP\", gaps = barmerge.gaps_off, ignore_invalid_symbol = true)\n\
         f = request.currency_rate(from = currency.EUR, to = currency.USD, ignore_invalid_currency = true)\n\
         g = request.seed(source = \"seed_src\", symbol = \"ACME\", expression = close, ignore_invalid_symbol = true, calc_bars_count = 100)\n\
         h = request.quandl(ticker = \"DATASET/SERIES\", gaps = barmerge.gaps_off, index = 0, ignore_invalid_symbol = true)\n\
         plot(a + b + c + d + e + f + g + h)\n",
    );
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
}

#[test]
fn scalar_request_interfaces_enforce_max_arity_and_optional_types() {
    let (_, too_many) = check(
        "//@version=6\nindicator(\"R\")\nplot(request.dividends(\"A\", \"F\", \"g\", \"l\", true, \"USD\", 7))\n",
    );
    assert!(
        error_codes(&too_many).contains(&"E0204"),
        "got {too_many:?}"
    );

    let (_, wrong_type) = check(
        "//@version=6\nindicator(\"R\")\nplot(request.economic(\"US\", \"GDP\", ignore_invalid_symbol = \"yes\"))\n",
    );
    assert!(
        error_codes(&wrong_type).contains(&"E0205"),
        "got {wrong_type:?}"
    );

    let (_, unknown_name) = check(
        "//@version=6\nindicator(\"R\")\nplot(request.currency_rate(from = \"EUR\", to = \"USD\", ignore_invalid_symbol = true))\n",
    );
    assert!(
        error_codes(&unknown_name).contains(&"E0228"),
        "got {unknown_name:?}"
    );
}

#[test]
fn request_seed_requires_its_expression() {
    let (_, diags) = check(
        "//@version=6\nindicator(\"R\")\nplot(request.seed(source = \"seed_src\", symbol = \"AAPL\"))\n",
    );
    assert!(error_codes(&diags).contains(&"E0204"), "got {diags:?}");
}

#[test]
fn security_lower_tf_current_feed_alias_is_accepted() {
    // A plain alias retains the same-feed provenance even though the runtime
    // string is the real host symbol rather than an opaque member-name token.
    let (_, diags) = check(
        "//@version=6\nindicator(\"L\")\ns = syminfo.ticker\n\
         plot(array.size(request.security_lower_tf(s, \"1\", close)))\n",
    );
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
}

#[test]
fn security_lower_tf_binds_all_v6_named_parameters() {
    let (_, diags) = check(
        "//@version=6\nindicator(\"L\")\n\
         xs = request.security_lower_tf(symbol = syminfo.tickerid, timeframe = \"1\", expression = close, ignore_invalid_symbol = true, currency = currency.NONE, ignore_invalid_timeframe = true, calc_bars_count = 100)\n\
         plot(array.size(xs))\n",
    );
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
}

#[test]
fn security_lower_tf_enforces_max_arity_and_tail_types() {
    let (_, too_many) = check(
        "//@version=6\nindicator(\"L\")\nplot(array.size(request.security_lower_tf(syminfo.tickerid, \"1\", close, true, \"NONE\", false, 100, 8)))\n",
    );
    assert!(
        error_codes(&too_many).contains(&"E0204"),
        "got {too_many:?}"
    );

    let (_, wrong_type) = check(
        "//@version=6\nindicator(\"L\")\nplot(array.size(request.security_lower_tf(syminfo.tickerid, \"1\", close, calc_bars_count = \"all\")))\n",
    );
    assert!(
        error_codes(&wrong_type).contains(&"E0205"),
        "got {wrong_type:?}"
    );
}

#[test]
fn security_lower_tf_runtime_symbol_expression_is_e0246() {
    let (_, diags) = check(
        "//@version=6\nindicator(\"L\")\ns = str.tostring(close)\n\
         plot(array.size(request.security_lower_tf(s, \"1\", close)))\n",
    );
    assert!(error_codes(&diags).contains(&"E0246"), "got {diags:?}");
}

#[test]
fn security_lower_tf_non_constant_timeframe_is_e0246() {
    let (_, diags) = check(
        "//@version=6\nindicator(\"L\")\ntf = str.tostring(close)\n\
         plot(array.size(request.security_lower_tf(syminfo.tickerid, tf, close)))\n",
    );
    assert!(error_codes(&diags).contains(&"E0246"), "got {diags:?}");
}

#[test]
fn security_tuple_checks_arity_and_rejects_unsupported_contexts() {
    for (body, code) in [
        ("[a, b] = [1.0, 2.0]", "E0225"),
        ("[a] = request.security(syminfo.tickerid, \"1W\", [open, close])", "E0225"),
        ("plot(request.security(syminfo.tickerid, \"1W\", [open, close]))", "E0225"),
        ("[a, b] = request.security(syminfo.tickerid, \"1W\", [close, syminfo.mintick])", "E0244"),
        ("[a, b] = request.security(syminfo.tickerid, \"1W\", [close, 1])", "E0244"),
        ("[a, b, c] = request.security(syminfo.tickerid, \"1W\", ta.bb(close, 3, 2))", "E0244"),
        ("f() =>\n    label.new(bar_index, close)\n    [open, close]\n[a, b] = request.security(syminfo.tickerid, \"1W\", f())", "E0244"),
        ("f() =>\n    var avg = ta.sma(close, 3)\n    [open, avg]\n[a, b] = request.security(syminfo.tickerid, \"1W\", f())", "E0244"),
    ] {
        let (_, diags) = check(&format!("//@version=6\nindicator(\"S\")\n{body}\n"));
        assert!(error_codes(&diags).contains(&code), "{body}: {diags:?}");
    }
}

#[test]
fn security_external_plain_source_lowers_to_host_selector() {
    let (dump, diags) = check(
        "//@version=6\nindicator(\"S\")\n\
         plot(request.security(\"MARKET:ACME\", \"1W\", close))\n",
    );
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
    assert!(
        dump.contains("ReadSecurity security/MARKET:ACME/1W/close close[604800s, months=None]"),
        "expected host-backed external security selector in HIR dump:\n{dump}"
    );
}

#[test]
fn security_external_compound_expression_lowers_to_host_selector() {
    let (dump, diags) = check(
        "//@version=6\nindicator(\"S\")\n\
         plot(request.security(\"MARKET:ACME\", \"1W\", ta.sma(close, 2)))\n",
    );
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
    assert!(
        dump.contains(
            "ReadRequest security_expression[security_expression/MARKET:ACME/1W/gaps_off/lookahead_off/t6:ta.sma:2(s5:close)]"
        ),
        "expected canonical host-backed compound selector in HIR dump:\n{dump}"
    );
}

#[test]
fn security_rejects_non_price_float_sources() {
    // The secondary higher-timeframe context only resamples OHLCV/derived price
    // series. Non-price `series float` builtins — `dividends.future_*` and
    // `earnings.future_*`, `bid`/`ask`, and the
    // host-or-default `syminfo.*` fundamentals — must be REJECTED, not silently
    // resampled to `na`. `request.security` → E0244; `request.security_lower_tf`
    // → E0246.
    for source in [
        "dividends.future_amount",
        "earnings.future_revenue",
        "bid",
        "syminfo.mintick",
    ] {
        let (_, diags) = check(&format!(
            "//@version=6\nindicator(\"S\")\n\
             plot(request.security(syminfo.tickerid, \"1W\", {source}))\n"
        ));
        assert!(
            error_codes(&diags).contains(&"E0244"),
            "{source}: expected E0244, got {diags:?}"
        );

        let (_, diags) = check(&format!(
            "//@version=6\nindicator(\"L\")\n\
             plot(array.size(request.security_lower_tf(syminfo.tickerid, \"1\", {source})))\n"
        ));
        assert!(
            error_codes(&diags).contains(&"E0246"),
            "{source}: expected E0246, got {diags:?}"
        );
    }

    // A genuine price source still lowers cleanly (guard is not over-tightened).
    let (_, diags) = check(
        "//@version=6\nindicator(\"S\")\n\
         plot(request.security(syminfo.tickerid, \"1W\", hl2))\n",
    );
    assert!(
        !error_codes(&diags).contains(&"E0244"),
        "hl2 must remain a valid price source: {diags:?}"
    );
}

#[test]
fn security_compound_error_lists_only_supported_ta_functions() {
    let (_, diagnostics) = check(
        "//@version=6\nindicator(\"S\")\n\
         plot(request.security(syminfo.tickerid, \"1W\", ta.rma(close, 2)))\n",
    );
    let diagnostic = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code.as_deref() == Some("E0244"))
        .expect("unsupported HTF function reports E0244");
    let help = diagnostic
        .labels
        .iter()
        .map(|label| label.message.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        help.contains("`ta.sma`/`ta.ema`/`ta.change`"),
        "labels: {help}"
    );
    assert!(!help.contains("ta.rma"), "labels: {help}");
}

#[test]
fn security_external_gaps_on_is_e0242() {
    // A host-backed external read has no HTF bucketing on our side for `gaps`
    // to affect, so reject rather than silently ignore it.
    let (_, diags) = check(
        "//@version=6\nindicator(\"S\")\n\
         plot(request.security(\"MARKET:ACME\", \"1W\", close, barmerge.gaps_on))\n",
    );
    assert!(error_codes(&diags).contains(&"E0242"), "got {diags:?}");
}

#[test]
fn security_external_lookahead_on_is_e0242() {
    let (_, diags) = check(
        "//@version=6\nindicator(\"S\")\n\
         plot(request.security(\"MARKET:ACME\", \"1W\", close, lookahead=barmerge.lookahead_on))\n",
    );
    assert!(error_codes(&diags).contains(&"E0242"), "got {diags:?}");
}

#[test]
fn security_external_explicit_off_flags_are_accepted() {
    // Explicitly passing the `_off` defaults is not a divergence from the
    // default behavior, so it must not be rejected.
    let (dump, diags) = check(
        "//@version=6\nindicator(\"S\")\n\
         plot(request.security(\"MARKET:ACME\", \"1W\", close, barmerge.gaps_off, barmerge.lookahead_off))\n",
    );
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
    assert!(
        dump.contains("ReadSecurity security/MARKET:ACME/1W/close close[604800s, months=None]"),
        "expected host-backed external security selector in HIR dump:\n{dump}"
    );
}

// ta.pivot_point_levels -----------------------------------------------------

#[test]
fn pivot_point_levels_types_as_array_of_float() {
    // `ta.pivot_point_levels("Traditional", anchor)` type-checks cleanly and
    // produces an `array<float>` that flows through the collection accessors.
    let (dump, diags) = check(
        "//@version=6\nindicator(\"P\")\n\
         anchor = bar_index == 2\n\
         p = ta.pivot_point_levels(\"Traditional\", anchor)\n\
         plot(array.size(p))\n",
    );
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
    assert!(
        dump.contains("CallPivotPointLevels argc=3"),
        "expected a runtime pivot call node: {dump}"
    );
    assert!(
        dump.contains("array<float>"),
        "expected array<float> typing: {dump}"
    );
}

#[test]
fn pivot_point_levels_developing_defaults_to_false() {
    // The required anchor is a series bool; only developing is optional.
    let (dump, diags) = check(
        "//@version=6\nindicator(\"P\")\n\
         p = ta.pivot_point_levels(\"Fibonacci\", bar_index == 3)\n\
         plot(array.size(p))\n",
    );
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
    assert!(
        dump.contains("ConstBool false"),
        "expected developing=false in the lowered call: {dump}"
    );
}

#[test]
fn pivot_point_levels_series_type_is_accepted() {
    let (_, diags) = check(
        "//@version=6\nindicator(\"P\")\nt = input.string(\"Traditional\")\n\
         plot(array.size(ta.pivot_point_levels(t, bar_index == 4)))\n",
    );
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
}

#[test]
fn pivot_point_levels_series_anchor_is_accepted() {
    let (_, diags) = check(
        "//@version=6\nindicator(\"P\")\na = close > open\n\
         plot(array.size(ta.pivot_point_levels(\"Traditional\", a)))\n",
    );
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
}

#[test]
fn pivot_point_levels_named_developing_and_required_anchor_bind() {
    let (_, diags) = check(
        "//@version=6\nindicator(\"P\")\n\
         levels = ta.pivot_point_levels(type=\"Traditional\", anchor=close > open, developing=true)\n\
         plot(array.size(levels))\n",
    );
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");

    let (_, missing_anchor) = check(
        "//@version=6\nindicator(\"P\")\n\
         plot(array.size(ta.pivot_point_levels(\"Traditional\")))\n",
    );
    assert!(
        error_codes(&missing_anchor).contains(&"E0204"),
        "got {missing_anchor:?}"
    );
}

#[test]
fn pivot_point_levels_rejects_wrong_series_argument_types() {
    let (_, bad_anchor) = check(
        "//@version=6\nindicator(\"P\")\n\
         plot(array.size(ta.pivot_point_levels(\"Traditional\", \"1D\")))\n",
    );
    assert!(
        error_codes(&bad_anchor).contains(&"E0205"),
        "got {bad_anchor:?}"
    );

    let (_, bad_developing) = check(
        "//@version=6\nindicator(\"P\")\n\
         plot(array.size(ta.pivot_point_levels(\"Traditional\", true, developing=1)))\n",
    );
    assert!(
        error_codes(&bad_developing).contains(&"E0205"),
        "got {bad_developing:?}"
    );
}

#[test]
fn pivot_point_levels_all_documented_systems_are_accepted() {
    let (_, diags) = check(
        "//@version=6\nindicator(\"P\")\n\
         plot(array.size(ta.pivot_point_levels(\"Woodie\", timeframe.change(\"1W\"))))\n",
    );
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
}

#[test]
fn chart_bg_and_fg_colors_are_input_colors() {
    // `chart.bg_color`/`chart.fg_color` are usable `input color` values
    // (white background, black foreground). The channel
    // accessors consume them cleanly.
    let (_, diags) = check(
        "//@version=6\nindicator(\"C\")\n\
         plot(color.r(chart.bg_color) + color.r(chart.fg_color))\n",
    );
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
}

// ---- Strategy quantity, commission, OCA, direction, and currency constants ----

#[test]
fn strategy_named_qty_and_commission_constants_resolve() {
    // The named `strategy.*` constants are usable in the matching strategy()
    // params and fold to the same bare tokens the raw strings match.
    let result = analyze_src(
        "strategy(\"S\", default_qty_type=strategy.percent_of_equity, \
         default_qty_value=10, commission_type=strategy.commission.cash_per_contract, \
         commission_value=2.0)\nplot(close)\n",
    );
    assert!(!has_errors(&result.diagnostics), "{:?}", result.diagnostics);
    let s = result.hir.strategy.expect("strategy recorded");
    assert_eq!(s.default_qty_type, "percent_of_equity");
    assert_eq!(s.commission_type, "cash_per_contract");
    // `strategy.fixed` resolves to the same token as the raw `"fixed"`.
    let result = analyze_src("strategy(\"S\", default_qty_type=strategy.fixed)\nplot(close)\n");
    assert!(!has_errors(&result.diagnostics), "{:?}", result.diagnostics);
    assert_eq!(result.hir.strategy.unwrap().default_qty_type, "fixed");
}

#[test]
fn strategy_direction_constant_in_allow_entry_in() {
    // A `strategy.direction.*` named constant is accepted by
    // `strategy.risk.allow_entry_in`.
    let (_dump, diagnostics) = check(
        "strategy(\"S\")\nstrategy.risk.allow_entry_in(strategy.direction.long)\nplot(close)\n",
    );
    assert!(
        !has_errors(&diagnostics),
        "unexpected errors: {diagnostics:?}"
    );
}

#[test]
fn strategy_oca_constants_are_distinct_const_strings() {
    let registry = crate::BuiltinRegistry::standard();
    for member in [
        "strategy.oca.cancel",
        "strategy.oca.none",
        "strategy.oca.reduce",
        "strategy.commission.percent",
        "strategy.commission.cash_per_order",
    ] {
        let c = registry
            .lookup_constant(member)
            .unwrap_or_else(|| panic!("{member} not registered"));
        assert_eq!(c.ty, pinewasm_hir::Type::String);
        assert_eq!(c.string_value, Some(member));
    }
}

#[test]
fn currency_constants_fold_to_their_iso_codes() {
    // A currency constant is a const string whose value is the bare ISO code
    // (`currency.USD == "USD"`), distinct from the display-token model.
    let registry = crate::BuiltinRegistry::standard();
    for (member, code) in [
        ("currency.USD", "USD"),
        ("currency.EUR", "EUR"),
        ("currency.JPY", "JPY"),
        ("currency.BTC", "BTC"),
        ("currency.NONE", "NONE"),
    ] {
        let c = registry
            .lookup_constant(member)
            .unwrap_or_else(|| panic!("{member} not registered"));
        assert_eq!(c.ty, pinewasm_hir::Type::String);
        assert_eq!(c.string_value, Some(code));
    }
    // And it is usable as the strategy `currency` argument, folding to the code.
    let result = analyze_src("strategy(\"S\", currency=currency.EUR)\nplot(close)\n");
    assert!(!has_errors(&result.diagnostics), "{:?}", result.diagnostics);
    assert_eq!(result.hir.strategy.unwrap().currency, "EUR");
}

// ---- Type-cast type checks --------------------------------------------------

#[test]
fn casts_type_check_to_their_result_types() {
    for (expr, node, ty) in [
        ("int(3.7)", "int", "int"),
        ("float(5)", "float", "float"),
        ("bool(1)", "bool", "bool"),
        ("string(42)", "str.tostring", "string"),
        ("color(#FF0000)", "color", "color"),
    ] {
        let condition = if ty == "bool" { "x" } else { "na(x)" };
        let src = format!("indicator(\"C\")\nx = {expr}\nplot({condition} ? 0.0 : 1.0)\n");
        let (dump, diagnostics) = check(&src);
        assert!(
            diagnostics.is_empty(),
            "{expr}: unexpected diagnostics: {diagnostics:?}"
        );
        // The cast lowers to `CallPure <node> argc=1 : const <ty>`.
        assert!(
            dump.contains(&format!("CallPure {node} argc=1 : const {ty}")),
            "{expr}: dump did not show `CallPure {node} argc=1 : const {ty}`:\n{dump}"
        );
    }
}

#[test]
fn cast_of_wrong_type_is_e0205() {
    // int() of a string is rejected.
    let (_dump, diagnostics) = check("indicator(\"C\")\nplot(int(\"hi\") == 0 ? 0.0 : 1.0)\n");
    assert!(diagnostics
        .iter()
        .any(|d| d.code.as_deref() == Some("E0205")));
    // color() of an int is rejected.
    let (_dump, diagnostics) = check("indicator(\"C\")\nc = color(5)\nplot(na(c) ? 0.0 : 1.0)\n");
    assert!(diagnostics
        .iter()
        .any(|d| d.code.as_deref() == Some("E0205")));
}

#[test]
fn drawing_cast_wrong_type_is_e0205() {
    // line() of a number is rejected.
    let (_dump, diagnostics) = check("indicator(\"C\")\nl = line(5)\nplot(na(l) ? 0.0 : 1.0)\n");
    assert!(diagnostics
        .iter()
        .any(|d| d.code.as_deref() == Some("E0205")));
}

// ---- Unary plus -------------------------------------------------------------

#[test]
fn unary_plus_on_numeric_is_accepted() {
    let (_dump, diagnostics) = check("indicator(\"P\")\nplot(+5)\nplot(+close)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
}

#[test]
fn unary_plus_on_string_is_e0207() {
    let (_dump, diagnostics) = check("indicator(\"P\")\ns = +\"x\"\nplot(str.length(s))\n");
    assert!(diagnostics
        .iter()
        .any(|d| d.code.as_deref() == Some("E0207")));
}

// ---- Documentation annotations compile -------------------------------------

#[test]
fn doc_comment_annotations_compile() {
    // `//@description` and the other doc annotations are recognized as comments
    // (parsed + skipped), not errors — the script compiles cleanly.
    let src = "//@version=6\n\
               //@description A tool that outputs a value.\n\
               //@function Outputs a value.\n\
               //@param labelText the text\n\
               //@returns the value\n\
               //@type a type\n\
               //@field a field\n\
               indicator(\"Doc\")\n\
               plot(close)\n";
    let (_dump, diagnostics) = check(src);
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
}

// ---- Deprecated request.quandl stub ----------------------------------------

#[test]
fn request_quandl_compiles_as_deprecated_stub() {
    let (_dump, diagnostics) = check(
        "indicator(\"Q\")\nq = request.quandl(\"CFTC/SB_FO_ALL\", barmerge.gaps_off, 0)\nplot(na(q) ? 0.0 : q)\n",
    );
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
}

// ---- weekofyear variable and function typing -------------------------------

#[test]
fn weekofyear_variable_and_function_type_check() {
    // The bare variable is a series int; the function form too.
    let (dump, diagnostics) = check("indicator(\"W\")\nplot(weekofyear)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert!(dump.contains("weekofyear"), "dump: {dump}");
    let (_dump, diagnostics) = check("indicator(\"W\")\nplot(weekofyear(time))\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    let (_dump, diagnostics) = check("indicator(\"W\")\nplot(weekofyear(time, \"UTC\"))\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
}

#[test]
fn calendar_variable_and_function_forms_type_check() {
    // Each of year/month/dayofmonth/dayofweek/hour/minute/second
    // exists as BOTH a bare `series int` variable and a `(time[, timezone])`
    // function form. Both must type-check to `series int` with no diagnostics.
    for name in [
        "year",
        "month",
        "dayofmonth",
        "dayofweek",
        "hour",
        "minute",
        "second",
    ] {
        // Bare variable.
        let (dump, diagnostics) = check(&format!("indicator(\"C\")\nplot({name})\n"));
        assert!(
            diagnostics.is_empty(),
            "variable {name}: unexpected diagnostics: {diagnostics:?}"
        );
        assert!(
            dump.contains(name),
            "variable {name} missing from dump: {dump}"
        );
        // Function form, one and two arguments.
        let (_dump, diagnostics) = check(&format!("indicator(\"C\")\nplot({name}(time))\n"));
        assert!(
            diagnostics.is_empty(),
            "function {name}(time): unexpected diagnostics: {diagnostics:?}"
        );
        let (_dump, diagnostics) =
            check(&format!("indicator(\"C\")\nplot({name}(time, \"UTC\"))\n"));
        assert!(
            diagnostics.is_empty(),
            "function {name}(time, tz): unexpected diagnostics: {diagnostics:?}"
        );
    }
}

#[test]
fn calendar_functions_accept_input_and_series_timezones() {
    let source = concat!(
        "indicator(\"Calendar timezone\")\n",
        "chosen = input.string(\"America/New_York\")\n",
        "changing = close > open ? \"America/New_York\" : \"UTC\"\n",
        "plot(hour(time, chosen))\n",
        "plot(weekofyear(time, changing))\n",
        "plot(year(time, changing))\n",
        "plot(month(time, changing))\n",
        "plot(dayofmonth(time, changing))\n",
        "plot(dayofweek(time, changing))\n",
        "plot(hour(time, changing))\n",
        "plot(minute(time, changing))\n",
        "plot(second(time, changing))\n",
    );
    let (_dump, diagnostics) = check(source);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");

    let (_dump, diagnostics) = check("indicator(\"C\")\nplot(hour(time, 2))\n");
    assert!(error_codes(&diagnostics).contains(&"E0205"));
}

// ---- Bare ta.tr variable typing --------------------------------------------

#[test]
fn bare_ta_tr_variable_type_checks() {
    let (_dump, diagnostics) = check("indicator(\"TR\")\nplot(ta.tr)\n");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
}

// ---- Value assertions for remaining partial coverage rows ------------------

#[test]
fn version_directive_below_6_is_rejected_with_e0249() {
    // Versions 5 and earlier give some identical-looking code a different
    // meaning (`5 / 2` truncates, `and`/`or` evaluate both sides, `bool` has
    // `na`), so compiling them as version 6 would silently change results.
    // They are rejected with E0249.
    for version in [1u8, 4, 5] {
        let (_dump, diagnostics) = check(&format!(
            "//@version={version}\nindicator(\"V\")\nx = close - close[1]\nplot(x)\n"
        ));
        assert!(
            error_codes(&diagnostics).contains(&"E0249"),
            "v{version}: expected E0249, got {diagnostics:?}"
        );
        assert!(
            !warning_codes(&diagnostics).contains(&"W0003"),
            "v{version}: {diagnostics:?}"
        );
    }
}

#[test]
fn version_directive_above_6_warns_w0003_and_compiles_as_v6() {
    // A version newer than 6 is a divergence: instead of switching language
    // versions we warn (`W0003`) and compile the body as version 6.
    let (dump, diagnostics) =
        check("//@version=7\nindicator(\"V\")\nx = close - close[1]\nplot(x)\n");
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
    assert!(
        warning_codes(&diagnostics).contains(&"W0003"),
        "expected W0003, got {diagnostics:?}"
    );
    assert!(dump.contains("series float"), "dump: {dump}");
    // The version 6 directive raises neither.
    let (_dump, diagnostics) =
        check("//@version=6\nindicator(\"V\")\nx = close - close[1]\nplot(x)\n");
    assert!(
        !warning_codes(&diagnostics).contains(&"W0003") && !has_errors(&diagnostics),
        "the version 6 directive must compile cleanly: {diagnostics:?}"
    );
}

#[test]
fn udt_field_reads_are_series_qualified() {
    // A field can be reassigned on any bar, so a UDT field read (`p.x`) is
    // `series` even when the object was built from constants.
    let (dump, diagnostics) = check(
        "indicator(\"U\")\ntype Point\n    float x\n    float y\n\
         p = Point.new(1.0, 2.0)\nq = p.x\nplot(q)\n",
    );
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
    assert!(dump.contains("GetField .0 : series float"), "dump: {dump}");
    // The variable bound to the field read carries the `series` qualifier.
    assert!(dump.contains("symbol 1 `q`: series float"), "dump: {dump}");
}

#[test]
fn field_history_is_rejected_only_when_written_on_the_field() {
    // Drawing getters and user functions or methods returning a field lower
    // to the same field read as `p.x`, but `[]` applies to the call result,
    // which is materialized like any compound history. `[]` written directly
    // on a field, with a literal or a dynamic offset, stays `E0209`.
    let prelude = "indicator(\"F\")\ntype P\n    float x\n\
                   getX(P p) => p.x\nmethod mx(P self) => self.x\n\
                   p = P.new(close)\npts = array.new<P>()\npts.push(p)\n\
                   ln = line.new(0, close, 1, close)\nn = bar_index > 1 ? 1 : 0\n";
    for target in [
        "line.get_x1(ln)",
        "ln.get_y1()",
        "getX(p)",
        "p.mx()",
        "pts.get(0).mx()",
    ] {
        for offset in ["1", "n"] {
            let (dump, diagnostics) = check(&format!("{prelude}plot({target}[{offset}])\n"));
            assert!(
                !has_errors(&diagnostics),
                "{target}[{offset}]: {diagnostics:?}"
            );
            assert!(
                dump.contains("MaterializeHistory"),
                "{target}[{offset}]: {dump}"
            );
        }
    }
    for target in ["p.x", "(p.x)", "pts.get(0).x", "(pts.get(0).x)"] {
        for offset in ["1", "n"] {
            let (_, diagnostics) = check(&format!("{prelude}plot({target}[{offset}])\n"));
            assert!(
                error_codes(&diagnostics).contains(&"E0209"),
                "{target}[{offset}]: {diagnostics:?}"
            );
            // The field is `series`, so the message names the field rule
            // rather than asking for a series value.
            let diagnostic = diagnostic_with_code(&diagnostics, "E0209");
            assert_eq!(
                diagnostic.message, "`[]` cannot be applied directly to an object field",
                "{target}[{offset}]"
            );
            assert!(
                diagnostic.labels[0]
                    .message
                    .contains("assign it to a variable"),
                "{target}[{offset}]: {diagnostic:?}"
            );
        }
    }
}

#[test]
fn const_qualifier_flows_from_literal_to_binding() {
    // A binding to a literal expression is `const`; a binding derived from a
    // series builtin is `series`. This pins both ends of the lattice that the
    // analyzer infers.
    let (dump, diagnostics) = check("indicator(\"C\")\nk = 1 + 2 * 3\nplot(k)\n");
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
    assert!(dump.contains("symbol 0 `k`: const int"), "dump: {dump}");

    let (dump, diagnostics) = check("indicator(\"C\")\ns = close + 1.0\nplot(s)\n");
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
    assert!(dump.contains("symbol 0 `s`: series float"), "dump: {dump}");
}

#[test]
fn input_qualifier_is_produced_by_input_builtins() {
    // `input.*` builtins produce the `input` qualifier; combining an input with
    // a const literal joins upward to `input` (weakest guarantee wins).
    let (dump, diagnostics) = check("indicator(\"I\")\nn = input.int(2)\nplot(close + n)\n");
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
    assert!(dump.contains("symbol 0 `n`: input int"), "dump: {dump}");
}

#[test]
fn qualifier_join_takes_the_upward_maximum() {
    // Binary expressions join qualifiers upward: const `join` series = series;
    // const `join` input = input. The join rule is `max` over the lattice
    // `Const < Input < Series` (Simple sits between Input and Series).
    use pinewasm_hir::Qualifier;
    assert_eq!(Qualifier::Const.join(Qualifier::Series), Qualifier::Series);
    assert_eq!(Qualifier::Const.join(Qualifier::Input), Qualifier::Input);
    assert_eq!(Qualifier::Input.join(Qualifier::Simple), Qualifier::Simple);
    assert_eq!(Qualifier::Simple.join(Qualifier::Series), Qualifier::Series);
    assert_eq!(Qualifier::Series.join(Qualifier::Const), Qualifier::Series);
    // A concrete binary expression: const literal + series builtin = series.
    let (dump, diagnostics) = check("indicator(\"J\")\nz = 2.0 + close\nplot(z)\n");
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
    assert!(dump.contains("symbol 0 `z`: series float"), "dump: {dump}");
    // const literal + input = input.
    let (dump, diagnostics) =
        check("indicator(\"J\")\nn = input.int(3)\nw = 2 + n\nplot(close + w)\n");
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
    assert!(dump.contains("symbol 1 `w`: input int"), "dump: {dump}");
}

#[test]
fn v6_parameter_names_are_accepted_as_named_arguments() {
    // Named-argument regression: every parameter below is spelled the way
    // PineWASM's registry declares it. A rename or dropped parameter
    // shows up here as `E0228`.
    let source = "indicator(\"P\")\n\
                  a = math.abs(number = -1.0)\n\
                  b = math.sin(angle = 1.0)\n\
                  c = math.todegrees(radians = 1.0)\n\
                  d = math.toradians(degrees = 1.0)\n\
                  e = math.round(number = 1.5, precision = 2)\n\
                  f = na(x = close) ? 0.0 : 1.0\n\
                  g = str.length(str.match(source = \"ab\", regex = \"a\"))\n\
                  h = ta.cmo(series = close, length = 9)\n\
                  i = ta.mfi(series = close, length = 9)\n\
                  [bbm, bbu, bbl] = ta.bb(series = close, length = 20, mult = 2.0)\n\
                  [kcm, kcu, kcl] = ta.kc(series = close, length = 20, mult = 2.0, useTrueRange = true)\n\
                  [dip, dim, adx] = ta.dmi(diLength = 14, adxSmoothing = 14)\n\
                  [ml, sl, hl] = ta.macd(source = close, fastlen = 12, slowlen = 26, siglen = 9)\n\
                  [st, dir] = ta.supertrend(factor = 3.0, atrPeriod = 10)\n\
                  j = ta.alma(series = close, length = 9, offset = 0.85, sigma = 6, floor = true)\n\
                  k = str.length(ticker.new(prefix = \"TEST\", ticker = \"ACME\", backadjustment = backadjustment.on))\n\
                  l = str.length(ticker.renko(symbol = \"ACME\", style = \"ATR\", param = 14, request_wicks = true))\n\
                  m = request.security(syminfo.tickerid, \"D\", close, currency = currency.NONE, calc_bars_count = 0)\n\
                  mx = matrix.rows(matrix.new<float>(rows = 1, columns = 1, initial_value = 0.0))\n\
                  plot(a + b + c + d + e + f + g + h + i + bbm + kcm + dip + ml + st + j + k + l + m + mx)\n";
    let (_dump, diagnostics) = check(source);
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
}

#[test]
fn v6_display_and_drawing_parameter_names_are_accepted() {
    // `plotchar`'s third parameter is `char`; `plotbar` has no
    // wick/border color; the drawing constructors take `force_overlay` and the
    // `chart.point` overloads.
    let source = "indicator(\"D\", overlay = true)\n\
                  plotchar(close, char = \"X\", show_last = 10)\n\
                  plotshape(close > open, style = shape.circle, show_last = 10)\n\
                  plotcandle(open, high, low, close, format = format.price, precision = 2)\n\
                  plotbar(open, high, low, close, format = format.price, precision = 2)\n\
                  p1 = chart.point.now(close)\n\
                  ln = line.new(first_point = p1, second_point = p1, force_overlay = true)\n\
                  lb = label.new(point = p1, text = \"x\", force_overlay = true)\n\
                  bx = box.new(top_left = p1, bottom_right = p1, force_overlay = true)\n\
                  tb = table.new(position.top_right, 1, 1, force_overlay = true)\n\
                  pl = polyline.new(array.new<chart.point>(0), force_overlay = true)\n\
                  line.set_xloc(ln, 1, 2, xloc.bar_index)\n\
                  label.set_xloc(lb, 1, xloc.bar_index)\n\
                  table.cell_set_width(tb, 0, 0, 12.5)\n\
                  plot(line.get_x1(ln) + label.get_x(lb) + box.get_left(bx))\n";
    let (_dump, diagnostics) = check(source);
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
}

#[test]
fn force_overlay_has_no_setter_or_getter() {
    // `force_overlay` is accepted only at construction, so deriving accessors
    // from the field table would invent members the registry does not declare.
    for source in [
        "indicator(\"D\")\nln = line.new(0, 1.0, 1, 2.0)\nline.set_force_overlay(ln, true)\nplot(close)\n",
        "indicator(\"D\")\nln = line.new(0, 1.0, 1, 2.0)\nplot(line.get_force_overlay(ln) ? 1.0 : 0.0)\n",
    ] {
        let (_dump, diagnostics) = check(source);
        assert!(
            error_codes(&diagnostics).contains(&"E0238"),
            "force_overlay accessor unexpectedly accepted: {diagnostics:?}"
        );
    }
}

/// A parameter without a default after one with a default passes the count
/// check when omitted (`binder::plan_def_args` classifies this as
/// `ArityErr::OmittedWithoutDefault`). It reports `E0224` naming the missing
/// parameter, on the call.
#[test]
fn user_function_required_after_default_omitted_is_e0224() {
    for call in ["f(5.0)", "f(a = 5.0)"] {
        let source =
            format!("//@version=6\nindicator(\"t\")\nf(a = 1.0, b) => a + b\nplot({call})\n");
        let (_, diagnostics) = check(&source);
        assert_eq!(
            error_codes(&diagnostics),
            ["E0224"],
            "{call}: {diagnostics:?}"
        );
        assert_eq!(
            diagnostics[0].message, "`f` is missing required argument `b`",
            "{call}: {diagnostics:?}"
        );
        assert_diagnostic_source(&source, &diagnostics[0], call, 4, 6);
    }
}

#[test]
fn user_function_omitted_required_reports_the_first_one() {
    // Count 2 is within `2 to 5`; `c` and `e` are omitted without defaults.
    let (_, diagnostics) = check(
        "//@version=6\nindicator(\"t\")\nf(a = 0.0, b = 0.0, c, d = 0.0, e) => a + b + c + d + e\nplot(f(1.0, 2.0))\n",
    );
    assert_eq!(error_codes(&diagnostics), ["E0224"], "{diagnostics:?}");
    assert_eq!(
        diagnostics[0].message,
        "`f` is missing required argument `c`"
    );
}

#[test]
fn user_function_omitted_required_still_checks_arguments() {
    // Like the count error, the omitted-default error still analyzes the
    // arguments, so their own errors are reported too.
    let (_, diagnostics) =
        check("//@version=6\nindicator(\"t\")\nf(a = 1.0, b) => a + b\nplot(f(qqqqqqqq))\n");
    assert_eq!(
        error_codes(&diagnostics),
        ["E0224", "E0201"],
        "{diagnostics:?}"
    );
}

#[test]
fn user_function_omitted_required_does_not_stop_analysis() {
    // A later, well-formed call to the same function still inlines, and a
    // later error elsewhere is still reported.
    let (_, diagnostics) = check(
        "//@version=6\nindicator(\"t\")\nf(a = 1.0, b) => a + b\nx = f(5.0)\ny = f(5.0, 2.0)\nplot(y + qqqqqqqq)\n",
    );
    assert_eq!(
        error_codes(&diagnostics),
        ["E0224", "E0201"],
        "{diagnostics:?}"
    );
}

#[test]
fn method_omitted_required_is_e0224() {
    // The receiver is prepended as argument 0, so `p.m(2.0)` binds `this`
    // and `a`, and leaves `b` out.
    let (_, diagnostics) = check(
        "indicator(\"U\")\ntype Point\n    float x = 0.0\n\
         method m(Point this, a = 1.0, b) =>\n    this.x + a + b\n\
         p = Point.new(3.0)\nplot(p.m(2.0))\n",
    );
    assert_eq!(error_codes(&diagnostics), ["E0224"], "{diagnostics:?}");
    assert_eq!(
        diagnostics[0].message,
        "`m` is missing required argument `b`"
    );
}

#[test]
fn user_function_required_after_default_binds_when_supplied() {
    let (_, diagnostics) =
        check("//@version=6\nindicator(\"t\")\nf(a = 1.0, b) => a + b\nplot(f(5.0, 2.0))\n");
    assert!(error_codes(&diagnostics).is_empty(), "{diagnostics:?}");
}

#[test]
fn user_function_arity_count_errors_are_e0224() {
    for call in ["f()", "f(1.0, 2.0, 3.0)"] {
        let source =
            format!("//@version=6\nindicator(\"t\")\nf(a, b = 1.0) => a + b\nplot({call})\n");
        let (_, diagnostics) = check(&source);
        assert_eq!(
            error_codes(&diagnostics),
            ["E0224"],
            "{call}: {diagnostics:?}"
        );
        assert!(
            diagnostics[0]
                .message
                .contains("expects 1 to 2 argument(s)"),
            "{call}: {diagnostics:?}"
        );
    }
}

#[test]
fn user_function_named_interior_default_is_filled() {
    let (_, diagnostics) = check(
        "//@version=6\nindicator(\"t\")\nf(a, b = 1.0, c) => a + b + c\nplot(f(1.0, c = 2.0))\n",
    );
    assert!(error_codes(&diagnostics).is_empty(), "{diagnostics:?}");
}

// -- syntax the front end accepts -----------------------------------------

#[test]
fn field_read_off_a_call_result_reads_the_field() {
    let (dump, diagnostics) = check(
        "indicator(\"U\")\ntype Point\n    float x\n    int bar\n\
         var pts = array.new<Point>()\npts.push(Point.new(1.5, 2))\n\
         plot(pts.get(0).x)\nplot(pts.first().bar)\n",
    );
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
    assert!(dump.contains("GetField .0 : series float"), "{dump}");
    assert!(dump.contains("GetField .1 : series int"), "{dump}");
}

#[test]
fn field_read_off_a_call_result_checks_the_field() {
    let (_dump, diagnostics) = check(
        "indicator(\"U\")\ntype Point\n    float x\n\
         var pts = array.new<Point>()\nplot(pts.get(0).z)\n",
    );
    assert_eq!(error_codes(&diagnostics), vec!["E0237"]);
    // A field read on a non-object value is E0237 too.
    let (_dump, diagnostics) =
        check("indicator(\"U\")\nvar xs = array.new<float>()\nplot(xs.get(0).x)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0237"]);
}

#[test]
fn matrix_and_map_annotations_resolve_to_collection_types() {
    let (dump, diagnostics) = check(
        "indicator(\"U\")\n\
         matrix<float> m = matrix.new<float>(2, 2, 0.0)\n\
         var map<string, float> w = map.new<string, float>()\n\
         w.put(\"a\", m.get(0, 0))\nplot(w.get(\"a\"))\n",
    );
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
    assert!(dump.contains("matrix<float>"), "{dump}");
    assert!(dump.contains("map<string, float>"), "{dump}");
}

#[test]
fn matrix_and_map_types_in_fields_and_parameters() {
    let (_dump, diagnostics) = check(
        "indicator(\"U\")\ntype Book\n    map<string, float> prices\n\
         total(matrix<float> m) => m.rows()\n\
         b = Book.new(map.new<string, float>())\nprices = b.prices\n\
         plot(total(matrix.new<float>(1, 1, 2.0)) + prices.size())\n",
    );
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
}

#[test]
fn mismatched_matrix_annotation_is_an_error() {
    let (_dump, diagnostics) =
        check("indicator(\"U\")\nmatrix<int> m = matrix.new<float>(1, 1, 0.0)\nplot(close)\n");
    assert!(has_errors(&diagnostics), "{diagnostics:?}");
}

#[test]
fn unknown_map_value_type_is_e0236() {
    let (_dump, diagnostics) = check("indicator(\"U\")\nmap<string, Nope> m = na\nplot(close)\n");
    assert_eq!(error_codes(&diagnostics), vec!["E0236"]);
}

#[test]
fn comma_separated_statements_analyze_in_order() {
    let (_dump, diagnostics) =
        check("indicator(\"U\")\na = 1.0, b = a + 1\nb := b * 2, a := b\nplot(a), plot(b)\n");
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
    // The second statement on the line sees the first one's declaration;
    // reading a later one is still undefined.
    let (_dump, diagnostics) = check("indicator(\"U\")\na = b, b = 1.0\nplot(a)\n");
    assert!(has_errors(&diagnostics), "{diagnostics:?}");
}

#[test]
fn inline_function_body_with_commas_returns_the_last_value() {
    let (_dump, diagnostics) =
        check("indicator(\"U\")\nf(float x) => y = x * 2, y + 1\nplot(f(close))\n");
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
}

#[test]
fn type_keyword_names_are_ordinary_variables() {
    let (_dump, diagnostics) = check(
        "indicator(\"U\")\nstring string = \"a\"\nbool = close > open\n\
         f(string) => string + \"!\"\nint = str.length(f(string))\nplot(bool ? int : 0)\n",
    );
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
}

#[test]
fn method_receiver_qualifier_and_default_are_accepted() {
    let (_dump, diagnostics) = check(
        "indicator(\"U\")\nmethod twice(series float x = 0) => x * 2\n\
         method tag(simple string s) => s + \"!\"\n\
         v = close\ns = \"a\"\nt = s.tag()\nplot(v.twice())\n",
    );
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
}

#[test]
fn conditional_tuple_checks_component_types_arity_and_scalar_contexts() {
    for (body, code) in [
        ("[a, b] = if true\n    [1, 2]\nelse\n    [3]", "E0225"),
        ("[a, b] = if true\n    [1, 2]\nelse\n    3", "E0225"),
        (
            "[a, b] = if true\n    [1, 2]\nelse\n    [3, false]",
            "E0207",
        ),
        ("[a, b] = switch 1\n    1 => [1, 2]\n    => [3]", "E0225"),
        (
            "[a, b] = switch 1\n    1 => [1, 2]\n    => [3, false]",
            "E0207",
        ),
        ("x = if true\n    [1, 2]\nelse\n    [3, 4]", "E0212"),
        ("[a, b] = true ? [1, 2] : [3, 4]", "E0225"),
        (
            "[a, b] = if true\n    [[1, 2], 3]\nelse\n    [4, 5]",
            "E0225",
        ),
    ] {
        let (_, diags) = check(&format!("//@version=6\nindicator(\"S\")\n{body}\n"));
        assert!(error_codes(&diags).contains(&code), "{body}: {diags:?}");
    }
}

#[test]
fn conditional_tuple_components_share_the_joined_qualifier() {
    let (_, diags) = check("//@version=6\nindicator(\"S\")\n[a, n] = if close > open\n    [close, 2]\nelse\n    [open, 3]\nplot(ta.ema(a, n))\n");
    assert!(error_codes(&diags).contains(&"E0219"), "{diags:?}");
}

#[test]
fn helper_constant_metadata_preserves_typed_values_without_freezing_persistent_arguments() {
    let (_, diags) = check("//@version=6\nindicator(\"S\")\nf(title, group) => input.float(1.0, title=title, group=group)\ng(title, group) => f(title, group)\nplot(g(\"Price\", \"Settings\"))\n");
    assert!(error_codes(&diags).is_empty(), "{diags:?}");
    let (_, diags) = check("//@version=6\nindicator(\"S\")\nvar title = \"Price\"\nf(t) => input.float(1.0, title=t)\nplot(f(title))\ntitle := \"Changed\"\n");
    assert!(
        !error_codes(&diags).is_empty(),
        "persistent metadata was frozen"
    );
}

#[test]
fn conditional_tuple_missing_components_infer_from_corresponding_siblings() {
    for body in [
        "[a, b] = switch bar_index % 3\n    0 => [na, na]\n    1 => [close, 3]\n    => [open, 5]",
        "[a, b] = switch bar_index % 3\n    0 => [close, 3]\n    1 => [(na), (na)]\n    => [open, 5]",
        "[a, b] = if close > open\n    [na, 3]\nelse\n    [close, na]",
        "[a, b] = if close > open\n    [na, \"x\"]\nelse\n    [close, na]",
    ] {
        let (_, diagnostics) = check(&format!("//@version=6\nindicator(\"S\")\n{body}\n"));
        assert!(error_codes(&diagnostics).is_empty(), "{body}: {diagnostics:?}");
    }
    let (dump, diagnostics) = check("//@version=6\nindicator(\"S\")\n[a,b] = if close > open\n    [na,3]\nelse\n    [close,na]\n");
    assert!(error_codes(&diagnostics).is_empty(), "{diagnostics:?}");
    assert!(dump.contains("`b`: series int"), "{dump}");
}

#[test]
fn conditional_tuple_missing_inference_does_not_leak_or_invent_a_type() {
    for body in [
        "[a, b] = [na, 3]",
        "[a, b] = if true\n    [na, 3]\nelse\n    [na, 5]",
        "[a, b] = switch 1\n    1 => [na, 3]\n    => [na, 5]",
        "[a, b] = if true\n    [x, y] = [na, 3]\n    [close, y]\nelse\n    [open, 5]",
        "[a, b] = if true\n    [close, 3]\nelse\n    [open, 5]\n[c, d] = [na, 3]",
        "[a, b] = if true\n    [na + na, 3]\nelse\n    [close, 5]",
    ] {
        let (_, diagnostics) = check(&format!("//@version=6\nindicator(\"S\")\n{body}\n"));
        assert!(
            error_codes(&diagnostics).contains(&"E0220"),
            "{body}: {diagnostics:?}"
        );
    }
}

#[test]
fn conditional_tuple_explicit_na_does_not_unify_with_v6_bool() {
    for body in [
        "[a,b] = if close > open\n    [na,3]\nelse\n    [true,5]",
        "[a,b] = if close > open\n    [true,3]\nelse\n    [na,5]",
        "[a,b] = switch bar_index\n    0 => [na,3]\n    => [false,5]",
    ] {
        let (_, diagnostics) = check(&format!("//@version=6\nindicator(\"S\")\n{body}\n"));
        assert!(
            error_codes(&diagnostics).contains(&"E0207"),
            "{body}: {diagnostics:?}"
        );
    }
}

#[test]
fn plot_display_combinations_retain_destinations() {
    let parsed = parse("//@version=6\nindicator(\"Display\")\nplot(close, display=display.status_line + display.data_window)\nplot(close, display=display.all - display.pane)\nplot(close, display=display.pane + display.price_scale)\n");
    let result = analyze(&parsed.program);
    assert!(!has_errors(&result.diagnostics), "{:?}", result.diagnostics);
    let styles: Vec<_> = result
        .hir
        .plots
        .iter()
        .map(|plot| (plot.style.display, plot.style.unsupported.len()))
        .collect();
    assert_eq!(
        styles,
        vec![
            (PlotDisplay::StatusLineDataWindow, 0),
            (PlotDisplay::PriceScaleStatusLineDataWindowPineScreener, 0),
            (PlotDisplay::PanePriceScale, 0)
        ]
    );
}

#[test]
fn v6_plot_and_hline_reject_nonpositive_constant_widths() {
    for call in ["plot(close", "hline(25"] {
        for expression in ["0", "-1", "zeroWidth", "math.max(-2, -1)"] {
            let source = format!(
                "//@version=6\nindicator(\"Width\")\nconst int zeroWidth = 0\n{call}, linewidth = {expression})\n"
            );
            let (_, diagnostics) = check(&source);
            assert_eq!(error_codes(&diagnostics), vec!["E0219"], "{source}");
            let diagnostic = diagnostic_with_code(&diagnostics, "E0219");
            assert!(diagnostic.message.contains("linewidth"), "{diagnostic:?}");
            let span = diagnostic.labels[0].span;
            assert_eq!(&source[span.start as usize..span.end as usize], expression);
        }
        for expression in ["1", "4294967295", "na", "input.int(0)"] {
            let source =
                format!("//@version=6\nindicator(\"Width\")\n{call}, linewidth = {expression})\n");
            let (_, diagnostics) = check(&source);
            assert!(!has_errors(&diagnostics), "{source}: {diagnostics:?}");
        }
    }
}

#[test]
fn plot_and_hline_glow_widths_are_preserved() {
    let parsed = parse("//@version=6\nindicator(\"Glow\")\nplot(close, linewidth=7)\nplot(close, linewidth=10, force_overlay=true)\nhline(20, linewidth=7)\nhline(10, linewidth=300)\n");
    let result = analyze(&parsed.program);
    assert!(!has_errors(&result.diagnostics), "{:?}", result.diagnostics);
    assert_eq!(
        result
            .hir
            .plots
            .iter()
            .map(|plot| (plot.style.line_width, plot.style.unsupported.len()))
            .collect::<Vec<_>>(),
        vec![(7, 0), (10, 0), (7, 0), (300, 0)]
    );
}

#[test]
fn non_scalar_map_key_annotation_is_e0234() {
    for source in [
        "indicator(\"U\")\ntype Point\n    float x\nmap<Point, float> m = na\nplot(close)\n",
        "indicator(\"U\")\nvar map<array<int>, float> m = na\nplot(close)\n",
        "indicator(\"U\")\ntype Book\n    map<Book, float> prices\nplot(close)\n",
    ] {
        let (_dump, diagnostics) = check(source);
        assert!(
            error_codes(&diagnostics).contains(&"E0234"),
            "{source:?}: {diagnostics:?}"
        );
    }
}

// Enum types are accepted as array, matrix or map element types, and as a
// map key type.

#[test]
fn matrix_new_accepts_enum_element_type() {
    let (dump, diagnostics) = check(
        "//@version=6\nindicator(\"U\")\nenum Side\n    buy\n    sell\n\
         m = matrix.new<Side>(1, 1, Side.buy)\nplot(m.get(0, 0) == Side.buy ? 1 : 0)\n",
    );
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert!(dump.contains("matrix<Side>"), "dump: {dump}");
}

#[test]
fn map_new_accepts_enum_key_type() {
    let (dump, diagnostics) = check(
        "//@version=6\nindicator(\"U\")\nenum Side\n    buy\n    sell\n\
         m = map.new<Side, float>()\nprevious = m.put(Side.sell, 1.0)\nplot(m.get(Side.sell))\n",
    );
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert!(dump.contains("map<Side, float>"), "dump: {dump}");
}

#[test]
fn map_new_accepts_enum_value_type() {
    let (dump, diagnostics) = check(
        "//@version=6\nindicator(\"U\")\nenum Side\n    buy\n    sell\n\
         m = map.new<string, Side>()\nprevious = m.put(\"a\", Side.sell)\n\
         plot(m.get(\"a\") == Side.sell ? 1 : 0)\n",
    );
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert!(dump.contains("map<string, Side>"), "dump: {dump}");
}

#[test]
fn map_enum_key_annotation_is_accepted() {
    // In a variable declaration and in a field of a user-defined type.
    for source in [
        "//@version=6\nindicator(\"U\")\nenum Side\n    buy\n    sell\n\
         map<Side, float> m = na\nplot(close)\n",
        "//@version=6\nindicator(\"U\")\nenum Side\n    buy\n    sell\n\
         type Book\n    map<Side, float> m = na\nplot(close)\n",
    ] {
        let (_dump, diagnostics) = check(source);
        assert!(diagnostics.is_empty(), "{source:?}: {diagnostics:?}");
    }
}

#[test]
fn array_new_accepts_enum_element_type() {
    let (dump, diagnostics) = check(
        "//@version=6\nindicator(\"U\")\nenum Side\n    buy\n    sell\n\
         a = array.new<Side>(1, Side.sell)\na.push(Side.buy)\n\
         plot(a.get(1) == Side.buy ? 1 : 0)\n",
    );
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert!(dump.contains("array<Side>"), "dump: {dump}");
}

#[test]
fn enum_collection_element_of_another_type_is_e0234() {
    // The nominal enum type still governs the elements: an int initial value,
    // another enum's member, or a mismatched key is rejected.
    for source in [
        "//@version=6\nindicator(\"U\")\nenum Side\n    buy\n    sell\n\
         a = array.new<Side>(1, 3)\nplot(close)\n",
        "//@version=6\nindicator(\"U\")\nenum Side\n    buy\n    sell\n\
         enum Mode\n    fast\n    slow\n\
         m = matrix.new<Side>(1, 1, Mode.fast)\nplot(close)\n",
        "//@version=6\nindicator(\"U\")\nenum Side\n    buy\n    sell\n\
         m = map.new<Side, float>()\nm.put(1, 1.0)\nplot(close)\n",
    ] {
        let (_dump, diagnostics) = check(source);
        assert!(
            error_codes(&diagnostics).contains(&"E0234"),
            "{source:?}: {diagnostics:?}"
        );
    }
}

#[test]
fn enum_collections_reject_another_enums_member_in_every_element_operation() {
    // An enum collection stores its members in the int lane, so every
    // element operand must be checked against the nominal enum, not the
    // lane: another enum's member is E0234 in each element-taking call, and
    // an annotation of another enum's collection is E0226.
    let header = "//@version=6\nindicator(\"U\")\nenum Side\n    buy\n    sell\n\
                  enum Mode\n    fast\n    slow\n\
                  a = array.new<Side>(1)\nm = matrix.new<Side>(1, 1)\n\
                  byKey = map.new<Side, float>()\nbyValue = map.new<string, Side>()\n";
    for (body, code) in [
        ("a.push(Mode.fast)", "E0234"),
        ("a.set(0, Mode.fast)", "E0234"),
        ("a.fill(Mode.fast)", "E0234"),
        ("a.concat(array.new<Mode>(1))", "E0234"),
        ("b = array.from(Side.buy, Mode.fast)", "E0234"),
        ("b = a.includes(Mode.fast)", "E0234"),
        ("m.set(0, 0, Mode.fast)", "E0234"),
        ("m.fill(Mode.fast)", "E0234"),
        ("byKey.put(Mode.fast, 1.0)", "E0234"),
        ("b = byKey.get(Mode.fast)", "E0234"),
        ("byValue.put(\"a\", Mode.fast)", "E0234"),
        ("array<Side> b = array.new<Mode>()", "E0226"),
        ("map<Side, float> b = map.new<Mode, float>()", "E0226"),
    ] {
        let source = format!("{header}{body}\nplot(close)\n");
        let (_dump, diagnostics) = check(&source);
        assert!(
            error_codes(&diagnostics).contains(&code),
            "{body}: {diagnostics:?}"
        );
    }
}

#[test]
fn typed_and_object_array_constructors_reject_unknown_named_arguments() {
    // The typed and object `array.new` forms bind `size` and
    // `initial_value` by name; an unknown name is the did-you-mean binding
    // error, not silently consumed in written order.
    let header = "//@version=6\nindicator(\"U\")\nenum Side\n    buy\n    sell\n\
                  type P\n    float x\n";
    for body in [
        "a = array.new<Side>(2, initial = Side.sell)",
        "a = array.new<P>(2, initial = P.new(1.0))",
        "a = array.new_line(count = 2)",
    ] {
        let source = format!("{header}{body}\nplot(close)\n");
        let (_dump, diagnostics) = check(&source);
        assert!(
            error_codes(&diagnostics).contains(&"E0228"),
            "{body}: {diagnostics:?}"
        );
    }
}

#[test]
fn string_overload_result_qualifiers_match_reference_families() {
    use pinewasm_hir::Qualifier::{Const, Input, Series, Simple};
    let calls = [
        ("str.tonumber(s)", [Const, Input, Simple, Series]),
        ("str.trim(s)", [Const, Input, Simple, Series]),
        ("str.repeat(s, 2)", [Const, Input, Simple, Series]),
        (
            "str.replace_all(s, \"a\", \"b\")",
            [Simple, Simple, Simple, Series],
        ),
        ("str.match(s, \"a\")", [Simple, Simple, Simple, Series]),
        ("str.contains(s, \"a\")", [Const, Simple, Simple, Series]),
        ("str.substring(s, 0, 1)", [Const, Simple, Simple, Series]),
        (
            "str.replace(s, \"a\", \"b\")",
            [Const, Simple, Simple, Series],
        ),
        ("str.lower(s)", [Const, Simple, Simple, Series]),
        ("str.upper(s)", [Const, Simple, Simple, Series]),
        ("str.startswith(s, \"a\")", [Const, Simple, Simple, Series]),
        ("str.endswith(s, \"a\")", [Const, Simple, Simple, Series]),
        ("str.pos(s, \"a\")", [Const, Simple, Simple, Series]),
        ("str.length(s)", [Const, Simple, Simple, Series]),
    ];
    for (call, qualifiers) in calls {
        for (source, expected) in [
            "\"123\"",
            "input.string(\"123\")",
            "syminfo.ticker",
            "close > open ? \"123\" : \"456\"",
        ]
        .into_iter()
        .zip(qualifiers)
        {
            let result = analyze_src(&format!(
                "indicator(\"String qualifiers\")\ns = {source}\np = {call}\nplot(close)\n"
            ));
            assert!(
                !has_errors(&result.diagnostics),
                "{call}: {:?}",
                result.diagnostics
            );
            let value = result
                .hir
                .symbols
                .iter()
                .find(|symbol| symbol.name == "p")
                .unwrap();
            assert_eq!(value.qualifier, expected, "{call} with {source}");
        }
    }
}

#[test]
fn na_return_qualifiers_follow_argument_type() {
    use pinewasm_hir::Qualifier::{Series, Simple};
    for (expression, qualifier) in [
        ("na(1)", Simple),
        ("na(1.0)", Simple),
        ("na(input.int(1))", Simple),
        ("na(close)", Series),
        ("na(\"\")", Series),
        ("na(color.red)", Series),
        ("na(syminfo.isin)", Series),
        ("na(array.new<float>())", Series),
    ] {
        let result = analyze_src(&format!(
            "indicator(\"na qualifiers\")\np = {expression}\nplot(close)\n"
        ));
        assert!(
            !has_errors(&result.diagnostics),
            "{expression}: {:?}",
            result.diagnostics
        );
        let value = result.hir.symbols.iter().find(|s| s.name == "p").unwrap();
        assert_eq!(value.qualifier, qualifier, "{expression}");
    }
    for expression in ["na(1)", "na(input.int(1))", "na(\"\")"] {
        let result = analyze_src(&format!(
            "indicator(\"invalid active\")\nn = input.int(1, active={expression})\nplot(n)\n"
        ));
        assert!(
            has_errors(&result.diagnostics),
            "{expression} cannot be input-qualified"
        );
    }
}

#[test]
fn na_rejects_boolean_operands_at_every_qualifier() {
    for value in [
        "true",
        "input.bool(true)",
        "chart.is_standard",
        "close > open",
    ] {
        for call in [format!("na({value})"), format!("na(x={value})")] {
            let result = analyze_src(&format!(
                "indicator(\"Invalid boolean na\")\np = {call}\nplot(close)\n"
            ));
            assert!(has_errors(&result.diagnostics), "must reject {call}");
        }
    }
}

#[test]
fn na_literal_cannot_adopt_a_boolean_context_in_v6() {
    for body in [
        "bool x = na",
        "var bool x = na",
        "varip bool x = na",
        "bool x = false\nx := na",
        "x = not na",
        "x = true and na",
        "x = na or false",
        "x = na ? 1.0 : 2.0",
        "if na\n    x = close",
        "x = input.bool(na)",
        "f(bool x) => x\ny = f(na)",
        "x = close > open ? true : na",
    ] {
        let result = analyze_src(&format!(
            "//@version=6\nindicator(\"Boolean na\")\n{body}\nplot(close)\n"
        ));
        assert!(has_errors(&result.diagnostics), "must reject {body}");
        assert!(
            error_codes(&result.diagnostics).contains(&"E0220"),
            "expected bool-na diagnostic for {body}: {:?}",
            result.diagnostics
        );
    }
}

#[test]
fn nz_v6_color_and_literal_inference() {
    for (call, ty) in [
        ("nz(na, 7)", "int"),
        ("nz(7, (na))", "int"),
        ("nz(na, color.red)", "color"),
        ("nz(color.red, na)", "color"),
        ("nz(source = na, replacement = color.red)", "color"),
        ("nz(replacement = 7, source = na)", "int"),
        ("nz(color.red)", "color"),
        ("nz(na)", "float"),
    ] {
        let (dump, diagnostics) = check(&format!(
            "//@version=6\nindicator(\"N\")\nx = {call}\nplot(close)\n"
        ));
        assert!(diagnostics.is_empty(), "{call}: {diagnostics:?}");
        assert!(
            dump.contains(&format!("`x`: simple {ty}")),
            "{call}: {dump}"
        );
    }
}

#[test]
fn nz_v6_overload_qualifier_matrix() {
    let cases = [
        (
            "int",
            ["1", "input.int(1)", "timeframe.multiplier", "bar_index"],
        ),
        (
            "float",
            ["1.5", "input.float(1.5)", "syminfo.mintick", "close"],
        ),
        (
            "color",
            [
                "color.red",
                "input.color(color.red)",
                "chart.is_standard ? color.red : color.blue",
                "close > open ? color.red : color.blue",
            ],
        ),
    ];
    for (ty, values) in cases {
        for (i, source) in values.iter().enumerate() {
            for (j, replacement) in values.iter().enumerate() {
                for call in [
                    format!("nz({source}, {replacement})"),
                    format!("nz(replacement={replacement}, source={source})"),
                ] {
                    let (dump, diagnostics) =
                        check(&format!("indicator(\"N\")\nx = {call}\nplot(close)\n"));
                    assert!(diagnostics.is_empty(), "{call}: {diagnostics:?}");
                    let qualifier = if i == 3 || j == 3 { "series" } else { "simple" };
                    assert!(
                        dump.contains(&format!("`x`: {qualifier} {ty}")),
                        "{call}: {dump}"
                    );
                }
            }
        }
    }
    for value in [
        "true",
        "input.bool(true)",
        "chart.is_standard",
        "close > open",
    ] {
        for call in [
            format!("nz({value})"),
            format!("nz(source={value})"),
            format!("nz(1, {value})"),
            format!("nz(source=1, replacement={value})"),
        ] {
            let (_, diagnostics) = check(&format!("indicator(\"N\")\nx = {call}\nplot(close)\n"));
            assert_eq!(error_codes(&diagnostics), vec!["E0205"], "{call}");
        }
    }
}

#[test]
fn an_input_active_condition_compares_two_ints_exactly_and_mixed_numbers_by_value() {
    // The folder of `active = ...` compared every numeric pair through f64,
    // so two ints that differ only past 2^53 compared equal.
    let default = |condition: &str| {
        let parsed = parse(&format!(
            "//@version=6\nindicator(\"A\")\nx = input.bool(true, \"X\", active = {condition})\nplot(close)\n"
        ));
        let result = analyze(&parsed.program);
        assert!(!has_errors(&result.diagnostics), "{:?}", result.diagnostics);
        result.hir.inputs[0].active.default
    };
    assert!(!default("9007199254740993 == 9007199254740992"));
    assert!(default("9007199254740993 != 9007199254740992"));
    assert!(default("9007199254740993 == 9007199254740993"));
    // A statically-int expression that holds a float compares by value.
    assert!(default("(7 / 2) * 2 == 7"));
    assert!(!default("(7 / 2) * 2 != 7"));
    assert!(!default("7 / 2 == 3"));
}

// The compiler integration tests cover real sources. These two tests protect
// scanner AST boundaries
// that must not be mistaken for an ordinary, shallow plain helper call.

#[test]
fn structured_generic_guard_nodes_do_not_borrow_plain_helper_facts() {
    use pinewasm_syntax::ast;
    let source = "//@version=6\nindicator(\"Generic guard boundary\")\nf() => close > open\nvar int shift = 1\nplot(close, offset=shift)\nif f()\n    shift := 2\n";
    let mut parsed = parse(source);
    assert!(parsed.diagnostics.iter().all(|d| !d.is_error()));
    let registry = crate::BuiltinRegistry::standard_ref();
    let ordinary = crate::persistent_writes::summarize(&parsed.program, registry);
    assert_eq!(ordinary.known_carried_reads.len(), 1);
    assert_eq!(
        ordinary
            .floors
            .values()
            .next()
            .expect("shift floor")
            .declaration,
        Some(pinewasm_hir::Qualifier::Series)
    );

    let branch = parsed
        .program
        .items
        .iter_mut()
        .find_map(|item| match item {
            ast::Item::Stmt(ast::Stmt::If(branch)) => Some(branch),
            _ => None,
        })
        .expect("if branch");
    let ast::Expr::Call {
        callee, type_args, ..
    } = &mut branch.cond
    else {
        panic!("plain helper call");
    };
    // The parser emits structured type arguments for generic constructors,
    // not f<...>. Exercise the scanner's AST gate without inventing syntax.
    type_args.push(ast::Ident {
        name: "GuardType".to_string(),
        span: callee.span,
    });
    let generic = crate::persistent_writes::summarize(&parsed.program, registry);
    assert!(generic.known_carried_reads.is_empty());
    assert_eq!(generic.floors.len(), 1);
    assert!(generic
        .floors
        .values()
        .next()
        .expect("shift floor")
        .declaration
        .is_none());
}

#[test]
fn helper_body_proofs_share_the_existing_nesting_bound() {
    use pinewasm_syntax::ast;
    let source = "//@version=6\nindicator(\"Helper depth boundary\")\nf() => close > open\nvar int shift = 1\nplot(close, offset=shift)\nif f()\n    shift := 2\n";
    let mut parsed = parse(source);
    assert!(parsed.diagnostics.iter().all(|d| !d.is_error()));
    let definition = parsed
        .program
        .items
        .iter_mut()
        .find_map(|item| match item {
            ast::Item::FuncDef(definition) => Some(definition),
            _ => None,
        })
        .expect("helper definition");
    let [ast::Stmt::Expr { expr, .. }] = definition.body.as_mut_slice() else {
        panic!("single expression body");
    };
    for _ in 0..crate::MAX_ANALYSIS_NESTING {
        let span = expr.span();
        let inner = std::mem::replace(expr, ast::Expr::Bool { value: false, span });
        *expr = ast::Expr::Group {
            inner: Box::new(inner),
            span,
        };
    }
    let summary = crate::persistent_writes::summarize(
        &parsed.program,
        crate::BuiltinRegistry::standard_ref(),
    );
    assert!(summary.known_carried_reads.is_empty());
    assert_eq!(summary.floors.len(), 1);
    assert!(summary
        .floors
        .values()
        .next()
        .expect("shift floor")
        .declaration
        .is_none());
}

#[test]
fn display_signatures_expose_the_accepted_destination_type() {
    use pinewasm_hir::Type;

    let registry = crate::BuiltinRegistry::standard();
    for (name, expected) in [
        ("hline", Type::PlotSimpleDisplay),
        ("fill", Type::PlotSimpleDisplay),
        ("bgcolor", Type::PlotSimpleDisplay),
        ("barcolor", Type::PlotSimpleDisplay),
        ("plot", Type::PlotDisplay),
        ("plotcandle", Type::PlotDisplay),
    ] {
        let function = registry.lookup_function(name).unwrap();
        let display = function
            .params
            .iter()
            .find(|parameter| parameter.name == "display")
            .unwrap();
        assert_eq!(display.ty, expected, "{name} display signature");
    }
}

// ---------------------------------------------------------------------------
// "did you mean" suggestions and the bound on searching for them
// ---------------------------------------------------------------------------

/// The name a diagnostic's "did you mean" note offers.
fn suggested(diagnostic: &Diagnostic) -> Option<&str> {
    diagnostic.notes.iter().find_map(|note| {
        note.strip_prefix("did you mean `")
            .and_then(|rest| rest.strip_suffix("`?"))
    })
}

#[test]
fn every_kind_of_unknown_name_is_offered_the_nearest_known_one() {
    const POINT: &str = "type Point\n    float x\n";
    for (what, body, code, expected) in [
        ("identifier", "plot(clos)".to_string(), "E0201", "close"),
        ("function", "plott(close)".to_string(), "E0202", "plot"),
        (
            "namespaced function",
            "plot(ta.smaa(close, 3))".to_string(),
            "E0202",
            "ta.sma",
        ),
        (
            "collection method",
            "a = array.new_float(0)\na.pusj(1.0)\nplot(close)".to_string(),
            "E0235",
            "array.push",
        ),
        (
            "method of a user-defined type",
            format!("{POINT}method shift(Point p) => p.x\np = Point.new(1.0)\nplot(p.shfit())"),
            "E0237",
            "shift",
        ),
        (
            "named argument",
            "plot(close, titel = \"x\")".to_string(),
            "E0228",
            "title",
        ),
        (
            "element type",
            format!("{POINT}a = array.new<Poin>()\nplot(close)"),
            "E0234",
            "Point",
        ),
        (
            "receiver type",
            format!("{POINT}method m(Poin p) => 1\nplot(close)"),
            "E0237",
            "Point",
        ),
    ] {
        let result = analyze_ok(&format!("indicator(\"I\")\n{body}\n"));
        let offered: Vec<&str> = result
            .diagnostics
            .iter()
            .filter(|d| d.code.as_deref() == Some(code))
            .filter_map(suggested)
            .collect();
        assert_eq!(offered, [expected], "{what}: {:?}", result.diagnostics);
    }
}

#[test]
fn an_unknown_name_with_nothing_close_gets_no_suggestion() {
    let result = analyze_ok("indicator(\"I\")\nplot(qqqqqqqq)\n");
    let unknown = diagnostic_with_code(&result.diagnostics, "E0201");
    assert_eq!(suggested(unknown), None);
}

/// A script of `count` statements, each one the unknown name `name(index)`,
/// and the `E0201` diagnostics it gets, in source order.
fn unknown_identifier_diagnostics(count: usize, name: impl Fn(usize) -> String) -> Vec<Diagnostic> {
    let mut source = String::from("indicator(\"I\")\n");
    for index in 0..count {
        source.push_str(&name(index));
        source.push('\n');
    }
    let parsed = parse(&source);
    assert!(!has_errors(&parsed.diagnostics), "{:?}", parsed.diagnostics);
    // These scripts have more unknown names than an analysis reports. The
    // list here has room for every one of them, so the tests see what the
    // suggestion search did for each name, not where the report was cut.
    let mut unknown: Vec<Diagnostic> = crate::analyze_into(
        &parsed.program,
        crate::BuiltinRegistry::standard_ref(),
        crate::ImportedLibraries::default(),
        pinewasm_span::Diagnostics::with_limits(count, pinewasm_span::MAX_WARNINGS),
    )
    .diagnostics
    .into_iter()
    .filter(|d| d.code.as_deref() == Some("E0201"))
    .collect();
    unknown.sort_by_key(|d| d.labels[0].span.start);
    unknown
}

#[test]
fn one_unknown_name_used_many_times_is_suggested_every_time() {
    // 2,000 uses of one misspelling cost one search of the built-in names, so
    // every one of them carries the note, far past what the budget would pay
    // for if each were searched again.
    let table = crate::BuiltinRegistry::standard_ref().suggestion_candidates();
    assert!(2_000 * table > crate::suggestions::SUGGESTION_BUDGET);
    let unknown = unknown_identifier_diagnostics(2_000, |_| "clos".to_string());
    assert_eq!(unknown.len(), 2_000);
    assert!(unknown.iter().all(|d| suggested(d) == Some("close")));
}

#[test]
fn many_distinct_unknown_names_stop_being_searched_for_at_the_budget() {
    // 2,000 different names, each a built-in series name with two more
    // characters, so each is within reach of a suggestion while searching
    // lasts. A search of the built-in names is charged the size of the table,
    // and the search that crosses the budget still runs: exactly that many
    // names get a note, the first ones in source order, and every later one
    // is reported without.
    const TAIL: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_";
    let name = |index: usize| {
        let base = ["close", "volume"][index % 2];
        let tail = index / 2;
        format!(
            "{base}{}{}",
            TAIL[tail / TAIL.len()] as char,
            TAIL[tail % TAIL.len()] as char
        )
    };
    let unknown = unknown_identifier_diagnostics(2_000, name);
    assert_eq!(unknown.len(), 2_000);

    let table = crate::BuiltinRegistry::standard_ref().suggestion_candidates();
    let searched = crate::suggestions::SUGGESTION_BUDGET.div_ceil(table);
    assert!(searched > 100 && searched < 1_900, "{searched}");
    for (index, diagnostic) in unknown.iter().enumerate() {
        assert_eq!(
            suggested(diagnostic).is_some(),
            index < searched,
            "name {index} of {searched} searched: {:?}",
            diagnostic.notes
        );
    }
}
