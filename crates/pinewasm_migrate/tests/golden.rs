//! Golden tests: each v5 input migrates to exactly the
//! expected v6 text. Where PineWASM supports every construct in the output,
//! the output must also compile cleanly, which checks that the rewrite is
//! valid v6 and not merely different.

use pinewasm_compiler::{compile, CompileOptions};
use pinewasm_migrate::{migrate, MigrationOptions, Status};
use pretty_assertions::assert_eq;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Compile {
    Yes,
    No,
}

fn golden(input: &str, expected: &str, compiles: Compile) {
    let result = migrate(input, &MigrationOptions::default());
    assert_eq!(result.status, Status::Migrated, "{:?}", result.parse_errors);
    assert!(
        !result.findings.iter().any(|f| f.rule == "internal"),
        "{:?}",
        result.findings
    );
    assert_eq!(result.output, expected);
    // Applying the reported edit reproduces the output.
    let mut applied = input.to_string();
    for edit in result.edits.iter().rev() {
        applied.replace_range(edit.start as usize..edit.end as usize, &edit.new_text);
    }
    assert_eq!(applied, result.output);
    // Migrating the output again changes nothing: it is already v6.
    assert_eq!(
        migrate(&result.output, &MigrationOptions::default()).status,
        Status::AlreadyV6
    );
    if compiles == Compile::Yes {
        let compiled = compile(&result.output, CompileOptions::default());
        let errors: Vec<_> = compiled
            .diagnostics
            .iter()
            .filter(|d| d.severity == pinewasm_span::Severity::Error)
            .collect();
        assert!(errors.is_empty(), "{errors:?}\n{}", result.output);
    }
}

#[test]
fn every_existing_literal_alpha_survives_legacy_transp_removal() {
    for alpha in 0..=255 {
        let literal = format!("#123456{alpha:02X}");
        let input = format!(
            "//@version=5\nindicator(\"Alpha\")\nplot(close, color = {literal}, transp = 50)\n"
        );
        let color = if alpha == 255 {
            format!("color.new({literal}, 50)")
        } else {
            literal
        };
        let expected =
            format!("//@version=6\nindicator(\"Alpha\")\nplot(close, color = {color})\n");
        golden(&input, &expected, Compile::Yes);
    }
}

#[test]
fn version_directive_becomes_6() {
    golden(
        r#"//@version=5
indicator("t")
plot(close)
"#,
        r#"//@version=6
indicator("t")
plot(close)
"#,
        Compile::Yes,
    );
}

#[test]
fn version_bump_preserves_line_wrapped_string_values() {
    golden(
        "//@version=5\nindicator(\"Wrapped strings\")\nmessage = \"first line\n  continued line\"\nplot(close)\n",
        "//@version=6\nindicator(\"Wrapped strings\")\nmessage = \"first line\n  continued line\"\nplot(close)\n",
        Compile::Yes,
    );
}

#[test]
fn version_bump_preserves_unsupported_string_escape_characters() {
    golden(
        r#"//@version=5
indicator("Literal escape characters")
pattern = "\s+\S*"
plot(1)
"#,
        r#"//@version=6
indicator("Literal escape characters")
pattern = "\s+\S*"
plot(1)
"#,
        Compile::Yes,
    );
}

#[test]
fn version_bump_preserves_hyphenated_import_publisher() {
    golden(
        r#"//@version=5
library("Text wrapper")
import Example-Author/TextKit/6 as textLib
export join(a, b) => textLib.concat(a, b, " ")
"#,
        r#"//@version=6
library("Text wrapper")
import Example-Author/TextKit/6 as textLib
export join(a, b) => textLib.concat(a, b, " ")
"#,
        Compile::No,
    );
}

#[test]
fn loop_result_expression_survives_v5_conversion() {
    golden(
        "//@version=5\nindicator(\"Loop result\")\nvalues = array.from(1, 2)\nresult = for value in values\n    value * 2\nplot(result)\n",
        "//@version=6\nindicator(\"Loop result\")\nvalues = array.from(1, 2)\nresult = for value in values\n    value * 2\nplot(result)\n",
        Compile::No,
    );
}

#[test]
fn loop_result_expression_applies_v5_integer_division_conversion_in_its_body() {
    golden(
        "//@version=5\nindicator(\"Loop result division\")\nresult = for index = 0 to 1\n    5 / 2\nplot(result)\n",
        "//@version=6\nindicator(\"Loop result division\")\nresult = for index = 0 to 1\n    int(5 / 2)\nplot(result)\n",
        Compile::No,
    );
}

#[test]
fn generic_matrix_and_map_function_parameters_survive_v5_conversion() {
    golden(
        "//@version=5\nindicator(\"Generic collection parameters\")\nmetric(matrix<float> values, map<string, float> totals) =>\n    matrix.rows(values) + map.size(totals)\nm = matrix.new<float>(1, 1, 0.0)\ntotals = map.new<string, float>()\nplot(metric(m, totals))\n",
        "//@version=6\nindicator(\"Generic collection parameters\")\nmetric(matrix<float> values, map<string, float> totals) =>\n    matrix.rows(values) + map.size(totals)\nm = matrix.new<float>(1, 1, 0.0)\ntotals = map.new<string, float>()\nplot(metric(m, totals))\n",
        Compile::Yes,
    );
}

#[test]
fn field_reads_after_array_get_survive_v5_conversion() {
    golden(
        "//@version=5\nindicator(\"Object field after call\")\ntype point\n    float x\n    float y\npoints = array.new<point>()\narray.push(points, point.new(2.0, 3.0))\nplot(array.get(points, 0).x + array.get(points, 0).y)\n",
        "//@version=6\nindicator(\"Object field after call\")\ntype point\n    float x\n    float y\npoints = array.new<point>()\narray.push(points, point.new(2.0, 3.0))\nplot(array.get(points, 0).x + array.get(points, 0).y)\n",
        Compile::Yes,
    );
}

#[test]
fn final_assignments_in_function_value_branches_get_explicit_reads() {
    golden(
        concat!(
            "//@version=5\nindicator(\"Branch tails\")\n",
            "f(x) =>\n",
            "    if x > 0\n        result = x + 1\n",
            "    else\n        result = x - 1\n",
            "g(x) =>\n",
            "    float result = 0.0\n",
            "    if x > 0\n        result := x + 1\n",
            "    else\n        result := x - 1\n",
            "plot(f(close))\nplot(g(close))\n",
        ),
        concat!(
            "//@version=6\nindicator(\"Branch tails\")\n",
            "f(x) =>\n",
            "    if x > 0\n        result = x + 1\n        result\n",
            "    else\n        result = x - 1\n        result\n",
            "g(x) =>\n",
            "    float result = 0.0\n",
            "    if x > 0\n        result := x + 1\n        result\n",
            "    else\n        result := x - 1\n        result\n",
            "plot(f(close))\nplot(g(close))\n",
        ),
        Compile::Yes,
    );
}

/// A body indented by a width that is not a multiple of four columns is a
/// wrapped continuation of the `=>` line, not a block. Appending a read at
/// that indentation would continue the same line again and break the
/// parse, so the single-line body is left as written, like `=> x = ...`.
#[test]
fn wrapped_single_line_function_bodies_get_no_tail_read() {
    golden(
        concat!(
            "//@version=5\nindicator(\"Wrapped bodies\")\n",
            "grow(base, rate) =>\n   total = base * (1 + rate)\n",
            "shrink(base, rate) =>\n  total = base * (1 - rate)\n",
            "plot(grow(close, 0.5) + shrink(close, 0.5))\n",
        ),
        concat!(
            "//@version=6\nindicator(\"Wrapped bodies\")\n",
            "grow(base, rate) =>\n   total = base * (1 + rate)\n",
            "shrink(base, rate) =>\n  total = base * (1 - rate)\n",
            "plot(grow(close, 0.5) + shrink(close, 0.5))\n",
        ),
        Compile::Yes,
    );
}

#[test]
fn side_effect_if_assignments_do_not_get_extra_reads() {
    golden(
        "//@version=5\nindicator(\"Side effects\")\nfloat result = 0.0\nif close > open\n    result := close\nplot(result)\n",
        "//@version=6\nindicator(\"Side effects\")\nfloat result = 0.0\nif close > open\n    result := close\nplot(result)\n",
        Compile::Yes,
    );
}

#[test]
fn zero_strength_pivots_keep_the_version_only_conversion() {
    golden(
        "//@version=5\nindicator(\"Pivot zero check\")\nplot(ta.pivothigh(high, 0, 2), \"PH left zero\")\nplot(ta.pivotlow(low, 0, 0), \"PL both zero\")\n",
        "//@version=6\nindicator(\"Pivot zero check\")\nplot(ta.pivothigh(high, 0, 2), \"PH left zero\")\nplot(ta.pivotlow(low, 0, 0), \"PL both zero\")\n",
        Compile::Yes,
    );
}

#[test]
fn valuewhen_integer_source_keeps_the_version_only_conversion() {
    golden(
        "//@version=5\nindicator(\"valuewhen int type check\")\nint n = ta.valuewhen(bar_index > 0, bar_index, 0)\nplot(n, \"Captured index\")\nplot(close[bar_index - n], \"History by captured index\")\n",
        "//@version=6\nindicator(\"valuewhen int type check\")\nint n = ta.valuewhen(bar_index > 0, bar_index, 0)\nplot(n, \"Captured index\")\nplot(close[bar_index - n], \"History by captured index\")\n",
        Compile::Yes,
    );
}

#[test]
fn builtin_named_parameter_keeps_the_version_only_conversion() {
    golden(
        "//@version=5\nindicator(\"volume parameter shadow check\")\npine_vwma(x, y, volume) =>\n    ta.sma(x * volume, y) / ta.sma(volume, y)\nplot(pine_vwma(hl2, 3, volume), \"VWMA\")\n",
        "//@version=6\nindicator(\"volume parameter shadow check\")\npine_vwma(x, y, volume) =>\n    ta.sma(x * volume, y) / ta.sma(volume, y)\nplot(pine_vwma(hl2, 3, volume), \"VWMA\")\n",
        Compile::Yes,
    );
}

#[test]
fn when_moves_into_if_and_evaluates_stateful_arguments_first() {
    golden(
        r#"//@version=5
strategy("t")
c = close > open
strategy.entry("L", strategy.long, qty = ta.sma(volume, 3), when = c)
"#,
        r#"//@version=6
strategy("t", margin_long = 0, margin_short = 0)
c = close > open
__v6_1 = ta.sma(volume, 3)
if c
    strategy.entry("L", strategy.long, qty = __v6_1)
"#,
        Compile::Yes,
    );
}

#[test]
fn positional_when_on_strategy_close() {
    golden(
        r#"//@version=5
strategy("t")
strategy.close("L", close < open, "bye")
"#,
        r#"//@version=6
strategy("t", margin_long = 0, margin_short = 0)
if close < open
    strategy.close("L", comment = "bye")
"#,
        Compile::Yes,
    );
}

#[test]
fn when_true_is_dropped() {
    golden(
        r#"//@version=5
strategy("t")
strategy.entry("L", strategy.long, when = true)
"#,
        r#"//@version=6
strategy("t", margin_long = 0, margin_short = 0)
strategy.entry("L", strategy.long)
"#,
        Compile::Yes,
    );
}

#[test]
fn when_in_single_line_function_body() {
    // PineWASM does not yet support a function body ending in an `if` statement with a void call, so this
    // output is checked for the rewrite only.
    golden(
        r#"//@version=5
strategy("t")
enter(bool c) => strategy.entry("L", strategy.long, when = c)
enter(close > open)
"#,
        r#"//@version=6
strategy("t", margin_long = 0, margin_short = 0)
enter(bool c) =>
    if c
        strategy.entry("L", strategy.long)
enter(close > open)
"#,
        Compile::No,
    );
}

#[test]
fn exit_pair_keeps_v5_precedence() {
    golden(
        r#"//@version=5
strategy("t")
strategy.exit("X", "L", profit = 100, limit = high * 1.01)
"#,
        r#"//@version=6
strategy("t", margin_long = 0, margin_short = 0)
strategy.exit("X", "L", profit = na(high * 1.01) ? 100 : na, limit = high * 1.01)
"#,
        Compile::Yes,
    );
}

#[test]
fn trailing_exit_pairs_keep_v5_absolute_precedence() {
    // The migration keeps these trailing pairs unchanged.
    // PineWASM applies v5-style absolute-first precedence here.
    golden(
        r#"//@version=5
strategy("Trail precedence mechanics")
strategy.exit("const", "L", trail_price = 100, trail_points = 10, trail_offset = 4)
strategy.exit("dynamic", "L", trail_points = 10, trail_price = close + 5, trail_offset = 4)
strategy.exit("na absolute", "L", trail_points = 10, trail_price = na, trail_offset = 4)
"#,
        r#"//@version=6
strategy("Trail precedence mechanics", margin_long = 0, margin_short = 0)
strategy.exit("const", "L", trail_price = 100, trail_offset = 4)
strategy.exit("dynamic", "L", trail_points = na(close + 5) ? 10 : na, trail_price = close + 5, trail_offset = 4)
strategy.exit("na absolute", "L", trail_points = 10, trail_offset = 4)
"#,
        Compile::Yes,
    );
}

#[test]
fn exit_pair_with_constant_absolute_drops_relative() {
    golden(
        r#"//@version=5
strategy("t")
strategy.exit("X", "L", loss = 50, stop = 90.5)
"#,
        r#"//@version=6
strategy("t", margin_long = 0, margin_short = 0)
strategy.exit("X", "L", stop = 90.5)
"#,
        Compile::Yes,
    );
}

#[test]
fn exit_pair_hoists_stateful_arguments_in_source_order() {
    golden(
        r#"//@version=5
strategy("Exit stateful", margin_long = 0, margin_short = 0)
strategy.exit("X", "L", profit = ta.atr(3), limit = ta.sma(close, 3))
"#,
        r#"//@version=6
strategy("Exit stateful", margin_long = 0, margin_short = 0)
__v6_1 = ta.atr(3)
__v6_2 = ta.sma(close, 3)
strategy.exit("X", "L", profit = na(__v6_2) ? __v6_1 : na, limit = __v6_2)
"#,
        Compile::Yes,
    );
    golden(
        r#"//@version=5
strategy("Exit stateful", margin_long = 0, margin_short = 0)
strategy.exit("X", "L", limit = ta.sma(close, 3), profit = ta.atr(3))
"#,
        r#"//@version=6
strategy("Exit stateful", margin_long = 0, margin_short = 0)
__v6_1 = ta.sma(close, 3)
__v6_2 = ta.atr(3)
strategy.exit("X", "L", limit = __v6_1, profit = na(__v6_1) ? __v6_2 : na)
"#,
        Compile::Yes,
    );
}

#[test]
fn strategy_margins_default_to_zero() {
    golden(
        r#"//@version=5
strategy("t", overlay = true, margin_long = 50)
plot(close)
"#,
        r#"//@version=6
strategy("t", overlay = true, margin_long = 50, margin_short = 0)
plot(close)
"#,
        Compile::Yes,
    );
}

#[test]
fn duplicate_argument_keeps_the_first() {
    golden(
        r#"//@version=5
indicator("t")
plot(close, color = color.blue, color = color.orange)
"#,
        r#"//@version=6
indicator("t")
plot(close, color = color.blue)
"#,
        Compile::Yes,
    );
}

#[test]
fn input_source_confirm_is_named() {
    golden(
        r#"//@version=5
indicator("t")
src = input.source(close, "Src", "tip", "row", "grp", display.all, true)
plot(src)
"#,
        r#"//@version=6
indicator("t")
src = input.source(close, "Src", "tip", "row", "grp", display.all, confirm = true)
plot(src)
"#,
        Compile::Yes,
    );
}

#[test]
fn time_bars_back_is_named() {
    golden(
        r#"//@version=5
indicator("t")
t = time("D", 1)
plot(t)
"#,
        r#"//@version=6
indicator("t")
t = time("D", bars_back = 1)
plot(t)
"#,
        Compile::Yes,
    );
}

#[test]
fn numeric_condition_is_cast_to_bool() {
    golden(
        r#"//@version=5
indicator("t")
if volume
    log.info("v")
x = not bar_index ? 1 : 2
plot(x)
"#,
        r#"//@version=6
indicator("t")
if bool(volume)
    log.info("v")
x = not bool(bar_index) ? 1 : 2
plot(x)
"#,
        Compile::Yes,
    );
}

#[test]
fn numeric_operand_of_boolean_equality_is_cast_to_bool() {
    golden(
        "//@version=5\nindicator(\"Bool numeric comparisons\")\nb = close > open\nn = input.int(2, \"Number\")\nplot(b == 0 ? 1 : 0)\nplot(1 == b ? 1 : 0)\nplot(b != n ? 1 : 0)\nplot(b == close ? 1 : 0)\n",
        "//@version=6\nindicator(\"Bool numeric comparisons\")\nb = close > open\nn = input.int(2, \"Number\")\nplot(b == bool(0) ? 1 : 0)\nplot(bool(1) == b ? 1 : 0)\nplot(b != bool(n) ? 1 : 0)\nplot(b == bool(close) ? 1 : 0)\n",
        Compile::Yes,
    );
}

#[test]
fn udt_input_length_vwma_keeps_version_only_conversion() {
    golden(
        "//@version=5\nindicator(\"VWMA UDT input length check\")\ntype Settings\n    float source\n    int bars\nSettings settings = Settings.new(input.source(close, \"Source\"), input.int(3, \"Bars\"))\nplot(ta.vwma(settings.source, settings.bars), \"VWMA\")\n",
        "//@version=6\nindicator(\"VWMA UDT input length check\")\ntype Settings\n    float source\n    int bars\nSettings settings = Settings.new(input.source(close, \"Source\"), input.int(3, \"Bars\"))\nplot(ta.vwma(settings.source, settings.bars), \"VWMA\")\n",
        Compile::Yes,
    );
}

#[test]
fn bool_na_becomes_false() {
    golden(
        r#"//@version=5
indicator("t")
bool b = na
var bool v = na
b := na
f(bool p = na) => p
type T
    bool flag = na
x = close > open ? true : na
plotshape(b or v or f() or x)
"#,
        r#"//@version=6
indicator("t")
bool b = false
var bool v = false
b := false
f(bool p = false) => p
type T
    bool flag = false
x = close > open ? true : false
plotshape(b or v or f() or x)
"#,
        Compile::Yes,
    );
}

#[test]
fn nz_of_bool_is_removed() {
    golden(
        r#"//@version=5
indicator("t")
b = close > open
plotshape(nz(b[1]))
"#,
        r#"//@version=6
indicator("t")
b = close > open
plotshape(b[1])
"#,
        Compile::Yes,
    );
}

#[test]
fn na_of_comparison_tests_operands() {
    golden(
        r#"//@version=5
indicator("t")
x = na(close > ta.sma(close, 5))
plotshape(x)
"#,
        r#"//@version=6
indicator("t")
__v6_1 = na(ta.sma(close, 5))
x = (na(close) or __v6_1)
plotshape(x)
"#,
        Compile::Yes,
    );
}

#[test]
fn comparison_with_na_literal_is_false() {
    golden(
        r#"//@version=5
indicator("t")
x = close == na
plotshape(x)
"#,
        r#"//@version=6
indicator("t")
x = false
plotshape(x)
"#,
        Compile::Yes,
    );
}

#[test]
fn na_for_special_constant_arguments() {
    // Keep the condition input-qualified: a plot style cannot vary per bar.
    // This fixture checks the na rewrite and requires valid migrated output.
    golden(
        r#"//@version=5
indicator("t")
plot(close, style = na, display = na)
s = input.bool(true) ? plot.style_line : na
plot(open, style = s)
"#,
        r#"//@version=6
indicator("t")
plot(close)
s = input.bool(true) ? plot.style_line : plot.style_line
plot(open, style = s)
"#,
        Compile::Yes,
    );
}

#[test]
fn missing_unique_style_branch_uses_v5_plot_default() {
    // The unselected branch uses plot.style_line.
    // An incomplete if leaves no style in that branch, so the migration
    // supplies that default.
    golden(
        r#"//@version=5
indicator("Unique style fallback")
useCross = input.bool(false)
chosenStyle = if useCross
    plot.style_cross
plot(close, style = chosenStyle)
"#,
        r#"//@version=6
indicator("Unique style fallback")
useCross = input.bool(false)
chosenStyle = if useCross
    plot.style_cross
else
    plot.style_line
plot(close, style = chosenStyle)
"#,
        Compile::Yes,
    );
}

#[test]
fn const_int_division_is_truncated() {
    golden(
        r#"//@version=5
indicator("t")
a = 5 / 2
b = 7
b := 8
c = b / 2
var d = 0
d += 1
e = d / 2
plot(a + c + e + bar_index / 2)
"#,
        r#"//@version=6
indicator("t")
a = int(5 / 2)
b = 7
b := 8
c = int(b / 2)
var d = 0
d := d + 1
e = d / 2
plot(a + c + e + bar_index / 2)
"#,
        Compile::Yes,
    );
}

#[test]
fn reassigned_length_uses_first_bar_value() {
    golden(
        r#"//@version=5
indicator("t")
var len = 0
len += 1
plot(ta.ema(close, len))
"#,
        r#"//@version=6
indicator("t")
var len = 0
len := len + 1
plot(ta.ema(close, 1))
"#,
        Compile::Yes,
    );
}

#[test]
fn non_persistent_global_length_keeps_weak_qualifier() {
    // A global length reassigned from an input is migrated unchanged. A
    // bar-varying reassignment or `var` is rejected by the PineWASM checker,
    // which still requires literal EMA lengths.
    golden(
        r#"//@version=5
indicator("Reassigned input length check")
lenInput = input.int(3, "Length")
len = 1
len := lenInput
plot(ta.ema(close, len))
"#,
        r#"//@version=6
indicator("Reassigned input length check")
lenInput = input.int(3, "Length")
len = 1
len := lenInput
plot(ta.ema(close, len))
"#,
        Compile::No,
    );
    golden(
        r#"//@version=5
indicator("Constant reassignment")
len = 1
len := 3
plot(ta.ema(close, len))
"#,
        r#"//@version=6
indicator("Constant reassignment")
len = 1
len := 3
plot(ta.ema(close, len))
"#,
        Compile::No,
    );
}

#[test]
fn linewidth_below_one_is_clamped() {
    golden(
        r#"//@version=5
indicator("t")
w = input.int(2)
plot(close, linewidth = 0)
plot(open, linewidth = w)
plot(high, linewidth = -3)
"#,
        r#"//@version=6
indicator("t")
w = input.int(2)
plot(close, linewidth = math.max(1, 0))
plot(open, linewidth = math.max(1, w))
plot(high, linewidth = math.max(1, -3))
"#,
        Compile::Yes,
    );
}

#[test]
fn stateful_and_operand_is_evaluated_first() {
    golden(
        r#"//@version=5
indicator("t")
c = close > open and ta.rsi(close, 14) > 50
plotshape(c)
"#,
        r#"//@version=6
indicator("t")
__v6_1 = ta.rsi(close, 14) > 50
c = close > open and __v6_1
plotshape(c)
"#,
        Compile::Yes,
    );
}

#[test]
fn stateful_or_operand_inside_ternary_branch() {
    golden(
        r#"//@version=5
indicator("t")
x = close > open ? (volume > 0 or ta.sma(close, 5) > close) : false
plotshape(x)
"#,
        r#"//@version=6
indicator("t")
bool __v6_1 = if close > open
    __v6_2 = ta.sma(close, 5) > close
    (volume > 0 or __v6_2)
else
    false
x = __v6_1
plotshape(x)
"#,
        Compile::Yes,
    );
}

#[test]
fn stateful_operand_in_else_if() {
    golden(
        r#"//@version=5
indicator("t")
x = 0
if close > open
    x := 1
else if volume > 0 and ta.rsi(close, 14) > 50
    x := 2
plot(x)
"#,
        r#"//@version=6
indicator("t")
x = 0
if close > open
    x := 1
else
    __v6_1 = ta.rsi(close, 14) > 50
    if volume > 0 and __v6_1
        x := 2
plot(x)
"#,
        Compile::Yes,
    );
}

#[test]
fn stateful_operand_in_while_condition() {
    // PineWASM does not yet support stateful calls inside loop bodies, so this
    // output is checked for the rewrite only.
    golden(
        r#"//@version=5
indicator("t")
i = 0
while i < 10 and ta.sma(close, 3) > 0
    i += 1
plot(i)
"#,
        r#"//@version=6
indicator("t")
i = 0
while true
    __v6_1 = ta.sma(close, 3) > 0
    if not (i < 10 and __v6_1)
        break
    i := i + 1
    i
plot(i)
"#,
        Compile::No,
    );
}

#[test]
fn stateful_operand_in_request_expression() {
    // PineWASM does not yet support user functions inside `request.security` expressions, so this
    // output is checked for the rewrite only.
    golden(
        r#"//@version=5
indicator("t")
s = request.security(syminfo.tickerid, "D", close > open and ta.rsi(close, 14) > 50)
plotshape(s)
"#,
        r#"//@version=6
indicator("t")
__v6_1() =>
    __v6_2 = ta.rsi(close, 14) > 50
    close > open and __v6_2
s = request.security(syminfo.tickerid, "D", __v6_1())
plotshape(s)
"#,
        Compile::No,
    );
}

#[test]
fn stateful_operand_in_switch_label() {
    golden(
        r#"//@version=5
indicator("t")
y = switch
    close > open => 1
    volume > 0 and ta.rsi(close, 14) > 50 => 2
    => 3
plot(y)
"#,
        r#"//@version=6
indicator("t")
y = if close > open
    1
else
    __v6_1 = ta.rsi(close, 14) > 50
    if volume > 0 and __v6_1
        2
    else
        3
plot(y)
"#,
        Compile::Yes,
    );
}

#[test]
fn pure_operands_are_left_alone() {
    golden(
        r#"//@version=5
indicator("t")
c = close > open and volume > 0
plotshape(c)
"#,
        r#"//@version=6
indicator("t")
c = close > open and volume > 0
plotshape(c)
"#,
        Compile::Yes,
    );
}

#[test]
fn loop_end_that_changes_is_fixed_at_entry() {
    golden(
        r#"//@version=5
indicator("t")
a = array.new_float(3)
for i = 0 to array.size(a) - 1
    array.push(a, i)
plot(array.size(a))
"#,
        r#"//@version=6
indicator("t")
a = array.new_float(3)
__v6_1 = array.size(a) - 1
for i = 0 to __v6_1 by 1
    array.push(a, i)
plot(array.size(a))
"#,
        Compile::Yes,
    );
}

#[test]
fn immutable_arithmetic_loop_bound_migrates_exactly() {
    golden(
        r#"//@version=5
indicator("Loop bound repro")
length = input.int(3)
f(x) => x + 1
sum = 0
for i = 1 to length - 1
    sum += f(i)
plot(sum)
"#,
        r#"//@version=6
indicator("Loop bound repro")
length = input.int(3)
f(x) => x + 1
sum = 0
for i = 1 to length - 1 by 1
    sum := sum + f(i)
    sum
plot(sum)
"#,
        Compile::Yes,
    );
}

#[test]
fn implicit_loop_step_is_made_explicit() {
    golden(
        "//@version=5\nindicator(\"Loop step\")\ncount = 0\nfor i = 3 to 0 // descending\n    count += i\nfor j = 0 to 3 by 2\n    count += j\nplot(count)\n",
        "//@version=6\nindicator(\"Loop step\")\ncount = 0\nfor i = 3 to 0 by 1 // descending\n    count := count + i\n    count\nfor j = 0 to 3 by 2\n    count := count + j\n    count\nplot(count)\n",
        Compile::Yes,
    );
}

#[test]
fn compound_assignments_expand_without_changing_rhs_grouping() {
    golden(
        r#"//@version=5
indicator("Compound reassignment")
x = 24.0
a = 2.0
b = 3.0
x += a * b
x -= a - b
x *= a + b
x /= a / b
x %= a + b
plot(x)
"#,
        r#"//@version=6
indicator("Compound reassignment")
x = 24.0
a = 2.0
b = 3.0
x := x + a * b
x := x - (a - b)
x := x * (a + b)
x := x / (a / b)
x := x % (a + b)
plot(x)
"#,
        Compile::Yes,
    );
}

#[test]
fn compound_assignment_with_switch_rhs_keeps_the_block_expression_unwrapped() {
    golden(
        r#"//@version=5
indicator("Switch compound")
string text = ""
text += switch close > open
    true => "up"
    false => "down"
plot(text == "up" ? 1 : 0)
"#,
        r#"//@version=6
indicator("Switch compound")
string text = ""
text := text + switch close > open
    true => "up"
    false => "down"
plot(text == "up" ? 1 : 0)
"#,
        Compile::Yes,
    );
}

#[test]
fn reassigned_arithmetic_loop_bound_is_saved_once() {
    golden(
        r#"//@version=5
indicator("Mutable loop bound")
length = 3
for i = 1 to length - 1
    length += 1
plot(length)
"#,
        r#"//@version=6
indicator("Mutable loop bound")
length = 3
__v6_1 = length - 1
for i = 1 to __v6_1 by 1
    length := length + 1
    length
plot(length)
"#,
        Compile::Yes,
    );
}

#[test]
fn loop_result_expression_end_that_changes_is_fixed_at_entry() {
    golden(
        r#"//@version=5
indicator("Loop result bound")
a = array.from(1.0, 2.0, 3.0)
total = for i = 0 to array.size(a) - 1
    array.pop(a)
plot(total)
"#,
        r#"//@version=6
indicator("Loop result bound")
a = array.from(1.0, 2.0, 3.0)
__v6_1 = array.size(a) - 1
total = for i = 0 to __v6_1
    array.pop(a)
plot(total)
"#,
        Compile::No,
    );
}

#[test]
fn loop_end_that_cannot_change_is_left_alone() {
    golden(
        r#"//@version=5
indicator("t")
a = array.new_float(3)
s = 0.0
for i = 0 to array.size(a) - 1
    s += array.get(a, i)
plot(s)
"#,
        r#"//@version=6
indicator("t")
a = array.new_float(3)
s = 0.0
for i = 0 to array.size(a) - 1 by 1
    s := s + array.get(a, i)
    s
plot(s)
"#,
        Compile::Yes,
    );
}

#[test]
fn negative_loop_step_retains_v5_runtime_error() {
    golden(
        r#"//@version=5
indicator("t")
s = 0
for i = 10 to 0 by -2
    s += i
plot(s)
"#,
        r#"//@version=6
indicator("t")
s = 0
for i = 10 to 0 by -2
    s := s + i
    s
plot(s)
"#,
        Compile::Yes,
    );
}

#[test]
fn negative_input_loop_step_is_not_silently_made_positive() {
    golden(
        r#"//@version=5
indicator("t")
step = input.int(-2)
s = 0
for i = 0 to 6 by step
    s += 1
plot(s)
"#,
        r#"//@version=6
indicator("t")
step = input.int(-2)
s = 0
for i = 0 to 6 by step
    s := s + 1
    s
plot(s)
"#,
        Compile::Yes,
    );
}

#[test]
fn history_of_a_literal_keeps_leading_na() {
    golden(
        r#"//@version=5
indicator("t")
plot(6[1])
"#,
        r#"//@version=6
indicator("t")
__v6_1 = 6
plot(__v6_1[1])
"#,
        Compile::Yes,
    );
}

#[test]
fn history_of_a_fresh_udt_field() {
    golden(
        r#"//@version=5
indicator("t")
type P
    float v
p = P.new(close)
plot(p.v[1])
"#,
        r#"//@version=6
indicator("t")
type P
    float v
p = P.new(close)
__v6_1 = p[1]
plot(__v6_1.v)
"#,
        Compile::Yes,
    );
}

#[test]
fn timeframe_period_comparison_and_display() {
    // A chart-period request selector is resolved by the runtime against its feed.
    golden(
        r#"//@version=5
indicator("t")
d = timeframe.period == "D"
label.new(bar_index, high, timeframe.period)
x = request.security(syminfo.tickerid, timeframe.period, close)
plotshape(d)
plot(x)
"#,
        r#"//@version=6
indicator("t")
// v5 spelled a multiplier of 1 without the number ("D", not "1D").
__v6_timeframePeriodV5() =>
    timeframe.multiplier != 1 ? timeframe.period : timeframe.isdaily ? "D" : timeframe.isweekly ? "W" : timeframe.ismonthly ? "M" : timeframe.isseconds ? "S" : timeframe.period
d = timeframe.period == "1D"
label.new(bar_index, high, __v6_timeframePeriodV5(), textcolor = color.black)
x = request.security(syminfo.tickerid, timeframe.period, close)
plotshape(d)
plot(x)
"#,
        Compile::Yes,
    );
}

#[test]
fn integer_input_can_build_a_request_timeframe() {
    golden(
        r#"//@version=5
indicator("Input-derived timeframe")
daysBack = input.int(2, minval = 1, maxval = 10)
daysTf = str.tostring(daysBack) + "D"
plot(request.security(syminfo.tickerid, daysTf, close))
"#,
        r#"//@version=6
indicator("Input-derived timeframe")
daysBack = input.int(2, minval = 1, maxval = 10)
daysTf = str.tostring(daysBack) + "D"
plot(request.security(syminfo.tickerid, daysTf, close))
"#,
        Compile::Yes,
    );
}

#[test]
fn security_requested_barstate_islast_keeps_boolean_result() {
    golden(
        r#"//@version=5
indicator("Requested last bar")
lastDay = request.security(syminfo.tickerid, "1D", barstate.islast, lookahead = barmerge.lookahead_on)
plotshape(lastDay)
"#,
        r#"//@version=6
indicator("Requested last bar")
lastDay = request.security(syminfo.tickerid, "1D", barstate.islast, lookahead = barmerge.lookahead_on)
plotshape(lastDay)
"#,
        Compile::Yes,
    );
}

#[test]
fn integer_input_timeframe_and_requested_last_bar_convert_together() {
    golden(
        r#"//@version=5
indicator("Input-selected requested last bar")
daysBack = input.int(2, minval = 1, maxval = 2)
tf = str.tostring(daysBack) + "D"
lastDay = request.security(syminfo.tickerid, tf, barstate.islast, lookahead = barmerge.lookahead_on)
plotshape(lastDay)
"#,
        r#"//@version=6
indicator("Input-selected requested last bar")
daysBack = input.int(2, minval = 1, maxval = 2)
tf = str.tostring(daysBack) + "D"
lastDay = request.security(syminfo.tickerid, tf, barstate.islast, lookahead = barmerge.lookahead_on)
plotshape(lastDay)
"#,
        Compile::Yes,
    );
}

#[test]
fn recolored_constants_keep_v5_values() {
    golden(
        r#"//@version=5
indicator("t")
plot(close, color = color.red)
plot(open, color = color.teal)
plot(high, color = color.yellow)
plot(low, color = color.green)
"#,
        r#"//@version=6
indicator("t")
plot(close, color = #FF5252)
plot(open, color = #00897B)
plot(high, color = #FFEB3B)
plot(low, color = color.green)
"#,
        Compile::Yes,
    );
}

#[test]
fn transp_folds_into_the_color() {
    golden(
        r#"//@version=5
indicator("t")
plot(close, color = color.orange, transp = 40)
bgcolor(#FF000080, transp = 50)
plotshape(close > open, transp = 70)
"#,
        r#"//@version=6
indicator("t")
plot(close, color = color.new(color.orange, 40))
bgcolor(#FF000080)
plotshape(close > open, color = color.new(color.blue, 70))
"#,
        Compile::Yes,
    );
}

#[test]
fn conditional_color_alias_dag_is_classified_without_expansion() {
    let mut input = String::from("//@version=5\nindicator(\"t\")\nc0 = color.lime\n");
    for i in 1..=40 {
        input.push_str(&format!("c{i} = close > open ? c{} : c{}\n", i - 1, i - 1));
    }
    input.push_str("plot(close, color=c40, transp=70)\n");
    let expected = input
        .replace("//@version=5", "//@version=6")
        .replace("color=c40, transp=70", "color=color.new(c40, 70)");
    golden(&input, &expected, Compile::Yes);
}

#[test]
fn conditional_plain_colors_preserve_legacy_transparency() {
    golden(
        "//@version=5\nindicator(\"t\")\nplot(1, color=close > 0 ? color.lime : color.orange, transp=70)\n",
        "//@version=6\nindicator(\"t\")\nplot(1, color=color.new(close > 0 ? color.lime : color.orange, 70))\n",
        Compile::Yes,
    );
}

#[test]
fn mixed_conditional_colors_preserve_each_branch_opacity() {
    golden(
        "//@version=5\nindicator(\"t\")\nplot(1, color=close > 0 ? color.lime : color.new(color.orange, 40), transp=70)\nplot(2, color=false ? color.lime : color.new(color.orange, 0), transp=70)\n",
        "//@version=6\nindicator(\"t\")\nplot(1, color=close > 0 ? color.new(color.lime, 70) : color.new(color.orange, 40))\nplot(2, color=false ? color.new(color.lime, 70) : color.new(color.orange, 0))\n",
        Compile::Yes,
    );
}

#[test]
fn dynamic_mixed_conditional_colors_preserve_v5_transparency_per_branch() {
    golden(
        "//@version=5\nindicator(\"t\")\nup = close > open\nplot(1, color=up ? color.lime : color.new(color.orange, 40), transp=70)\nplot(2, color=up ? color.new(color.orange, 40) : color.lime, transp=70)\nplot(3, color=up ? color.lime : color.new(color.orange, 0), transp=70)\nplot(4, color=up ? color.new(color.orange, 0) : color.lime, transp=70)\n",
        "//@version=6\nindicator(\"t\")\nup = close > open\nplot(1, color=up ? color.new(color.lime, 70) : color.new(color.orange, 40))\nplot(2, color=up ? color.new(color.orange, 40) : color.new(color.lime, 70))\nplot(3, color=up ? color.new(color.lime, 70) : color.new(color.orange, 0))\nplot(4, color=up ? color.new(color.orange, 0) : color.new(color.lime, 70))\n",
        Compile::Yes,
    );
}

#[test]
fn named_input_color_defaults_preserve_legacy_transparency() {
    golden(
        "//@version=5\nindicator(\"t\")\na = input.color(title=\"Named\", defval=color.lime)\nb = input(title=\"Generic\", defval=color.lime)\nplot(1, color=a, transp=70)\nplot(2, color=b, transp=70)\n",
        "//@version=6\nindicator(\"t\")\na = input.color(title=\"Named\", defval=color.lime)\nb = input(title=\"Generic\", defval=color.lime)\nplot(1, color=color.new(a, 70))\nplot(2, color=color.new(b, 70))\n",
        Compile::Yes,
    );
}

#[test]
fn color_constructors_keep_their_own_opacity_even_at_zero_transparency() {
    golden(
        "//@version=5\nindicator(\"t\")\nplot(1, color=color.lime, transp=70)\nplot(2, color=color.new(color.lime, 0), transp=70)\nplot(3, color=color.new(transp=0, color=color.lime), transp=70)\nplot(4, color=color.rgb(0, 230, 118), transp=70)\nplot(5, color=color.rgb(0, 230, 118, 0), transp=70)\nplot(6, color=color.rgb(transp=0, blue=118, green=230, red=0), transp=70)\n",
        "//@version=6\nindicator(\"t\")\nplot(1, color=color.new(color.lime, 70))\nplot(2, color=color.new(color.lime, 0))\nplot(3, color=color.new(transp=0, color=color.lime))\nplot(4, color=color.rgb(0, 230, 118))\nplot(5, color=color.rgb(0, 230, 118, 0))\nplot(6, color=color.rgb(transp=0, blue=118, green=230, red=0))\n",
        Compile::Yes,
    );
}

#[test]
fn aliased_arrow_constructor_colors_keep_their_own_opacity() {
    golden(
        "//@version=5\nindicator(\"t\")\nup = color.new(color.lime, 0)\ndown = color.rgb(255, 152, 0)\nplotarrow(close - open, colorup=up, colordown=down, transp=70)\n",
        "//@version=6\nindicator(\"t\")\nup = color.new(color.lime, 0)\ndown = color.rgb(255, 152, 0)\nplotarrow(close - open, colorup=up, colordown=down)\n",
        Compile::Yes,
    );
}

#[test]
fn plotarrow_transparency_preserves_both_explicit_colors() {
    golden(
        "//@version=5\nindicator(\"t\")\nplotarrow(close - open, colorup=color.lime, colordown=color.orange, transp=70)\n",
        "//@version=6\nindicator(\"t\")\nplotarrow(close - open, colorup=color.new(color.lime, 70), colordown=color.new(color.orange, 70))\n",
        Compile::Yes,
    );
}

#[test]
fn plotarrow_transparency_composes_with_v5_color_preservation() {
    golden(
        "//@version=5\nindicator(\"t\")\nplotarrow(close - open, colorup=color.yellow, colordown=color.red, transp=70)\n",
        "//@version=6\nindicator(\"t\")\nplotarrow(close - open, colorup=color.new(#FFEB3B, 70), colordown=color.new(#FF5252, 70))\n",
        Compile::Yes,
    );
}

#[test]
fn plotarrow_transparency_keeps_existing_alpha_in_either_direction() {
    golden(
        "//@version=5\nindicator(\"t\")\nplotarrow(close - open, colorup=#00FF0080, colordown=color.orange, transp=70)\nplotarrow(open - close, colorup=color.lime, colordown=#FF980080, transp=70)\n",
        "//@version=6\nindicator(\"t\")\nplotarrow(close - open, colorup=#00FF0080, colordown=color.new(color.orange, 70))\nplotarrow(open - close, colorup=color.new(color.lime, 70), colordown=#FF980080)\n",
        Compile::Yes,
    );
}

#[test]
fn label_text_color_keeps_v5_default() {
    golden(
        r#"//@version=5
indicator("t")
label.new(bar_index, high, "a")
label.new(bar_index, low, "b", textcolor = color.white)
"#,
        r#"//@version=6
indicator("t")
label.new(bar_index, high, "a", textcolor = color.black)
label.new(bar_index, low, "b", textcolor = color.white)
"#,
        Compile::Yes,
    );
}

#[test]
fn ordinary_global_requests_keep_the_converter_default() {
    golden(
        r#"//@version=5
indicator("t")
x = request.security(syminfo.tickerid, "D", close)
plot(x)
"#,
        r#"//@version=6
indicator("t")
x = request.security(syminfo.tickerid, "D", close)
plot(x)
"#,
        Compile::Yes,
    );
}

#[test]
fn input_selected_request_currency_preserves_source_and_exposes_runtime_gap() {
    // The migration changes only the version directive for this
    // input-selected currency request. PineWASM retains the expression and
    // reports its separate runtime currency-conversion gap.
    let source = "//@version=5\nindicator(\"Input currency request check\")\ncur = input.string(\"USD\", options = [\"USD\", \"EUR\"])\nv = request.security(\"NASDAQ:AAPL\", \"D\", close, currency = cur)\nplot(v, \"Converted value\")\n";
    let expected = "//@version=6\nindicator(\"Input currency request check\")\ncur = input.string(\"USD\", options = [\"USD\", \"EUR\"])\nv = request.security(\"NASDAQ:AAPL\", \"D\", close, currency = cur)\nplot(v, \"Converted value\")\n";
    golden(source, expected, Compile::No);
    let diagnostics = compile(expected, CompileOptions::default()).diagnostics;
    assert!(diagnostics
        .iter()
        .any(|d| d.code.as_deref() == Some("E0242") && d.message.contains("currency")));
}

#[test]
fn local_request_wrapper_keeps_dynamic_requests_enabled() {
    // A request inside a function called from a local block is migrated
    // with dynamic requests enabled, so the request mode is not pinned to false.
    golden(
        r#"//@version=5
indicator("Wrapped local request")
f() => request.security(syminfo.tickerid, "D", close)
float x = na
if bar_index % 2 == 0
    x := f()
plot(x)
"#,
        r#"//@version=6
indicator("Wrapped local request")
f() => request.security(syminfo.tickerid, "D", close)
float x = na
if bar_index % 2 == 0
    x := f()
plot(x)
"#,
        Compile::Yes,
    );
}

#[test]
fn globally_seeded_nested_request_pins_v5_context_and_reads_branch_value() {
    // The migrator pins dynamic_requests to false here so the nested request
    // keeps its v5 evaluation on every closed bar.
    // The migrated form also adds this branch's final value read.
    // PineWASM's checker still rejects this nested expression with E0244.
    golden(
        r#"//@version=5
indicator("Cross-context nested request check")
inner = request.security("", "", close)
f() => request.security("NASDAQ:AAPL", "1D", inner)
v = f()
if barstate.islast
    local = f()
plot(v, "Cross-context value")
"#,
        r#"//@version=6
indicator("Cross-context nested request check", dynamic_requests = false)
inner = request.security("", "", close)
f() => request.security("NASDAQ:AAPL", "1D", inner)
v = f()
if barstate.islast
    local = f()
    local
plot(v, "Cross-context value")
"#,
        Compile::No,
    );
}

#[test]
fn later_global_wrapper_call_does_not_establish_the_local_request() {
    golden(
        r#"//@version=5
indicator("Cross-context request with late global call")
inner = request.security("", "", close)
f() => request.security("NASDAQ:AAPL", "1D", inner)
if barstate.islast
    local = f()
v = f()
plot(v)
"#,
        r#"//@version=6
indicator("Cross-context request with late global call")
inner = request.security("", "", close)
f() => request.security("NASDAQ:AAPL", "1D", inner)
if barstate.islast
    local = f()
    local
v = f()
plot(v)
"#,
        Compile::No,
    );
}

#[test]
fn persistent_global_wrapper_call_does_not_establish_every_bar_request() {
    golden(
        r#"//@version=5
indicator("Persistent cross-context request")
inner = request.security("", "", close)
f() => request.security("NASDAQ:AAPL", "1D", inner)
var v = f()
if barstate.islast
    local = f()
plot(v)
"#,
        r#"//@version=6
indicator("Persistent cross-context request")
inner = request.security("", "", close)
f() => request.security("NASDAQ:AAPL", "1D", inner)
var v = f()
if barstate.islast
    local = f()
    local
plot(v)
"#,
        Compile::No,
    );
}

#[test]
fn conditional_expression_request_wrapper_keeps_dynamic_requests_enabled() {
    // The v5 source and its v6 form are both covered by this golden.
    // Dynamic requests stay enabled for this function, which executes in a
    // ternary branch.
    golden(
        r#"//@version=5
indicator("Conditional request wrapper check")
fetch() => request.security(syminfo.tickerid, "1D", close)
plot(close > open ? fetch() : na)
"#,
        r#"//@version=6
indicator("Conditional request wrapper check")
fetch() => request.security(syminfo.tickerid, "1D", close)
plot(close > open ? fetch() : na)
"#,
        Compile::Yes,
    );
}

#[test]
fn direct_request_in_function_ternary_keeps_dynamic_requests_enabled() {
    // This request shape takes a timeframe parameter. The request mode stays
    // unspecified here, even when the function is called globally.
    golden(
        r#"//@version=5
indicator("Request inside function ternary")
fetch() => close > open ? close : request.security(syminfo.tickerid, "1D", close)
plot(fetch())
"#,
        r#"//@version=6
indicator("Request inside function ternary")
fetch() => close > open ? close : request.security(syminfo.tickerid, "1D", close)
plot(fetch())
"#,
        Compile::Yes,
    );
}

#[test]
fn eager_hoisted_request_wrapper_can_keep_non_dynamic_mode() {
    // The eager-evaluation phase moves the v5 Boolean operand to global
    // scope before request-mode analysis, so the v5 request mode is kept.
    golden(
        r#"//@version=5
indicator("Lazy request wrapper check")
fetch() => request.security(syminfo.tickerid, "1D", close) > 0
plot(close > open and fetch() ? close : na)
"#,
        r#"//@version=6
indicator("Lazy request wrapper check")
fetch() => request.security(syminfo.tickerid, "1D", close) > 0
__v6_1 = fetch()
plot(close > open and __v6_1 ? close : na)
"#,
        Compile::Yes,
    );
}

#[test]
fn comments_and_layout_survive() {
    golden(
        r#"//@version=5
indicator("t")
// header comment
x = 5 / 2  // trailing comment


// spaced block
plot(x)
"#,
        r#"//@version=6
indicator("t")
// header comment
x = int(5 / 2)  // trailing comment


// spaced block
plot(x)
"#,
        Compile::Yes,
    );
}

#[test]
fn numeric_branch_of_boolean_conditional_is_cast() {
    // v5 types `c ? close : false` as `bool`, casting the number; the
    // migration writes the cast explicitly.
    golden(
        r#"//@version=5
indicator("t")
c = close > open
plotshape(c ? close : false)
y = if c
    1
else
    false
plotshape(y)
"#,
        r#"//@version=6
indicator("t")
c = close > open
plotshape(c ? bool(close) : false)
y = if c
    bool(1)
else
    false
plotshape(y)
"#,
        Compile::Yes,
    );
}

#[test]
fn hline_style_switch_gains_a_default_arm() {
    golden(
        r#"//@version=5
indicator("t")
s = input.string("Solid", options = ["Solid", "Dashed"])
style = switch s
    "Solid" => hline.style_solid
    "Dashed" => hline.style_dashed
hline(0, linestyle = style)
"#,
        r#"//@version=6
indicator("t")
s = input.string("Solid", options = ["Solid", "Dashed"])
style = switch s
    "Solid" => hline.style_solid
    "Dashed" => hline.style_dashed
    => hline.style_solid
hline(0, linestyle = style)
"#,
        Compile::Yes,
    );
}

#[test]
fn function_returning_a_multiline_style_switch_keeps_its_default_arm() {
    golden(
        "//@version=5\nindicator(\"t\")\nset_style(s) =>\n    result = switch s\n        \"Solid\" => hline.style_solid\n        \"Dashed\" => hline.style_dashed\nhline(0, linestyle = set_style(input.string(\"Solid\")))\n",
        "//@version=6\nindicator(\"t\")\nset_style(s) =>\n    result = switch s\n        \"Solid\" => hline.style_solid\n        \"Dashed\" => hline.style_dashed\n        => hline.style_solid\nhline(0, linestyle = set_style(input.string(\"Solid\")))\n",
        Compile::Yes,
    );
}

#[test]
fn contextual_type_and_bool_names_are_kept() {
    golden(
        "//@version=5\nindicator(\"Contextual names\")\ntype = input.string(\"SMA\")\nbool = close > open\nplot(type == \"SMA\" and bool ? close : open)\n",
        "//@version=6\nindicator(\"Contextual names\")\ntype = input.string(\"SMA\")\nbool = close > open\nplot(type == \"SMA\" and bool ? close : open)\n",
        Compile::Yes,
    );
}
