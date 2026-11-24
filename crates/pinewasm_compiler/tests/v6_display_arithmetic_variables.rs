//! `display.*` values are destination sets of Pine's `plot_display` type, and
//! `+` / `-` between them are set union and set difference (the `plot()`
//! `display` parameter: `display.all - display.status_line` shows the plot
//! everywhere except the status line). The expected sets below are
//! computed by hand from that rule, not from the compiler.
//!
//! The same set must reach the host however the script spells it: written
//! inline, held in a variable, chosen by a ternary or `switch`, or assigned and
//! then reassigned under an input-controlled `if`. The host evaluator here
//! accepts only Pine's seven display names joined by `+` / `-`, so a
//! concatenated string such as `"display.panedisplay.price_scale"` fails it.
//!
//! A string is not a display value, so the compiler rejects `display = "display.pane"`,
//! an `input.string` and a string variable.

use pinewasm_compiler::{
    compile, compile_and_run, run_program, CompileOptions, InputValue, MarketData, PlotDisplay,
    RunOptions,
};
use pinewasm_hir::{DisplayKind, DisplayStyle, InputActiveBinaryOp, InputActiveExpr};
use std::collections::HashMap;

const PANE: u8 = 1;
const PRICE_SCALE: u8 = 2;
const STATUS_LINE: u8 = 4;
const DATA_WINDOW: u8 = 8;
const PINE_SCREENER: u8 = 16;
const ALL: u8 = 31;

/// `display.all - display.status_line`
const DIFFERENCE: (&str, u8) = (
    "display.all - display.status_line",
    PANE | PRICE_SCALE | DATA_WINDOW | PINE_SCREENER,
);
/// `display.pane + display.price_scale`
const SUM: (&str, u8) = ("display.pane + display.price_scale", PANE | PRICE_SCALE);
/// `display.all - display.pane - display.data_window + display.pane`
const CHAIN: (&str, u8) = (
    "display.all - display.pane - display.data_window + display.pane",
    PANE | PRICE_SCALE | STATUS_LINE | PINE_SCREENER,
);
const NONE: (&str, u8) = ("display.none", 0);

const HEADER: &str =
    "//@version=6\nindicator(\"Display arithmetic\")\nsel = input.bool(true, \"Select\")\n";

fn market() -> MarketData {
    MarketData {
        open: vec![10.0; 4],
        high: vec![12.0; 4],
        low: vec![9.0; 4],
        close: vec![11.0; 4],
        volume: vec![100.0; 4],
        timeframe: "1D".into(),
        ..MarketData::default()
    }
}

fn plot_mask(display: PlotDisplay) -> u8 {
    match display {
        PlotDisplay::None => 0,
        PlotDisplay::Pane => PANE,
        PlotDisplay::PriceScale => PRICE_SCALE,
        PlotDisplay::StatusLine => STATUS_LINE,
        PlotDisplay::DataWindow => DATA_WINDOW,
        PlotDisplay::All => ALL,
        PlotDisplay::PanePriceScale => PANE | PRICE_SCALE,
        PlotDisplay::PaneStatusLine => PANE | STATUS_LINE,
        PlotDisplay::PriceScaleStatusLine => PRICE_SCALE | STATUS_LINE,
        PlotDisplay::PanePriceScaleStatusLine => PANE | PRICE_SCALE | STATUS_LINE,
        PlotDisplay::PaneDataWindow => PANE | DATA_WINDOW,
        PlotDisplay::PriceScaleDataWindow => PRICE_SCALE | DATA_WINDOW,
        PlotDisplay::PanePriceScaleDataWindow => PANE | PRICE_SCALE | DATA_WINDOW,
        PlotDisplay::StatusLineDataWindow => STATUS_LINE | DATA_WINDOW,
        PlotDisplay::PaneStatusLineDataWindow => PANE | STATUS_LINE | DATA_WINDOW,
        PlotDisplay::PriceScaleStatusLineDataWindow => PRICE_SCALE | STATUS_LINE | DATA_WINDOW,
        PlotDisplay::PanePriceScaleStatusLineDataWindow => {
            PANE | PRICE_SCALE | STATUS_LINE | DATA_WINDOW
        }
        PlotDisplay::PineScreener => PINE_SCREENER,
        PlotDisplay::PanePineScreener => PANE | PINE_SCREENER,
        PlotDisplay::PriceScalePineScreener => PRICE_SCALE | PINE_SCREENER,
        PlotDisplay::PanePriceScalePineScreener => PANE | PRICE_SCALE | PINE_SCREENER,
        PlotDisplay::StatusLinePineScreener => STATUS_LINE | PINE_SCREENER,
        PlotDisplay::PaneStatusLinePineScreener => PANE | STATUS_LINE | PINE_SCREENER,
        PlotDisplay::PriceScaleStatusLinePineScreener => PRICE_SCALE | STATUS_LINE | PINE_SCREENER,
        PlotDisplay::PanePriceScaleStatusLinePineScreener => {
            PANE | PRICE_SCALE | STATUS_LINE | PINE_SCREENER
        }
        PlotDisplay::DataWindowPineScreener => DATA_WINDOW | PINE_SCREENER,
        PlotDisplay::PaneDataWindowPineScreener => PANE | DATA_WINDOW | PINE_SCREENER,
        PlotDisplay::PriceScaleDataWindowPineScreener => PRICE_SCALE | DATA_WINDOW | PINE_SCREENER,
        PlotDisplay::PanePriceScaleDataWindowPineScreener => {
            PANE | PRICE_SCALE | DATA_WINDOW | PINE_SCREENER
        }
        PlotDisplay::StatusLineDataWindowPineScreener => STATUS_LINE | DATA_WINDOW | PINE_SCREENER,
        PlotDisplay::PaneStatusLineDataWindowPineScreener => {
            PANE | STATUS_LINE | DATA_WINDOW | PINE_SCREENER
        }
        PlotDisplay::PriceScaleStatusLineDataWindowPineScreener => {
            PRICE_SCALE | STATUS_LINE | DATA_WINDOW | PINE_SCREENER
        }
    }
}

/// A strict host: Pine display names, `+` / `-` as set operations, and
/// ternaries over input values. Anything else is a contract fault.
fn host_mask(expr: &InputActiveExpr, inputs: &HashMap<u32, InputValue>) -> Result<u8, String> {
    let scalar = |expr: &InputActiveExpr| -> Result<InputValue, String> {
        match expr {
            InputActiveExpr::Value { value } => Ok(value.clone()),
            InputActiveExpr::Input { input } => inputs
                .get(input)
                .cloned()
                .ok_or_else(|| format!("unknown input {input}")),
            other => Err(format!("unsupported scalar {other:?}")),
        }
    };
    match expr {
        InputActiveExpr::Value {
            value: InputValue::String(name),
        } => Ok(match name.as_str() {
            "display.none" => 0,
            "display.pane" => PANE,
            "display.price_scale" => PRICE_SCALE,
            "display.status_line" => STATUS_LINE,
            "display.data_window" => DATA_WINDOW,
            "display.all" => ALL,
            "display.pine_screener" => PINE_SCREENER,
            other => return Err(format!("not a Pine display name: {other:?}")),
        }),
        InputActiveExpr::Binary { op, lhs, rhs } => {
            let (left, right) = (host_mask(lhs, inputs)?, host_mask(rhs, inputs)?);
            match op {
                InputActiveBinaryOp::Add => Ok(left | right),
                InputActiveBinaryOp::Sub => Ok(left & !right),
                other => Err(format!("unsupported display operator {other:?}")),
            }
        }
        InputActiveExpr::Ternary {
            condition,
            when_true,
            when_false,
        } => {
            let selected = match condition.as_ref() {
                InputActiveExpr::Binary {
                    op: InputActiveBinaryOp::Eq,
                    lhs,
                    rhs,
                } => scalar(lhs)? == scalar(rhs)?,
                other => match scalar(other)? {
                    InputValue::Bool(value) => value,
                    value => return Err(format!("non-bool condition {value:?}")),
                },
            };
            host_mask(if selected { when_true } else { when_false }, inputs)
        }
        other => Err(format!("unsupported display binding {other:?}")),
    }
}

/// The destination set the host draws for the last output of `field` after a
/// run with `overrides`.
fn drawn(source: &str, field: &str, overrides: &[(u32, InputValue)]) -> Result<u8, String> {
    let options = RunOptions {
        inputs: overrides.iter().cloned().collect(),
        ..RunOptions::default()
    };
    let result = compile_and_run(source, &market(), CompileOptions::default(), &options);
    let errors: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|d| d.is_error())
        .map(|d| format!("{:?} {}", d.code, d.message))
        .collect();
    if !errors.is_empty() {
        return Err(format!("rejected: {errors:?}"));
    }
    if !result.runtime_errors.is_empty() {
        return Err(format!("runtime: {:?}", result.runtime_errors));
    }
    let program = result.compile.as_ref().unwrap().program.as_ref().unwrap();
    let mut inputs: HashMap<u32, InputValue> = program
        .inputs
        .iter()
        .enumerate()
        .map(|(id, decl)| (id as u32, decl.defval.clone()))
        .collect();
    inputs.extend(overrides.iter().cloned());
    let style: &DisplayStyle = match field {
        "plot" => &result.plots.last().unwrap().style,
        "visual" => &result.visuals.last().unwrap().style,
        _ => unreachable!(),
    };
    if let Some(option) = style.unsupported.iter().find(|o| o.field == "display") {
        return Err(format!("unsupported display: {}", option.reason));
    }
    match style.bound_scalars.iter().find(|b| b.field == "display") {
        Some(binding) => host_mask(&binding.expression, &inputs),
        None => Ok(plot_mask(style.display)),
    }
}

fn spellings(v: &str, w: &str) -> Vec<(&'static str, String)> {
    vec![
        ("inline", v.to_string()),
        ("alias", format!("K = {v}\n@K")),
        ("const_ternary", format!("true ? {v} : {w}")),
        ("input_ternary", format!("sel ? {v} : {w}")),
        ("input_ternary_var", format!("X = sel ? {v} : {w}\n@X")),
        (
            "input_switch",
            format!("s = input.string(\"a\", \"Pick\", options = [\"a\", \"b\"])\nX = switch s\n    \"a\" => {v}\n    => {w}\n@X"),
        ),
        // A display variable reassigned under an input-controlled `if` must
        // still give the host one display value.
        ("input_if", format!("X = {v} + display.none\nif not sel\n    X := {w}\n@X")),
    ]
}

/// Places one display argument in a display call.
type Call = dyn Fn(&str) -> String;

/// Split a spelling into its prelude and the argument expression, then place
/// the argument in `call`.
fn script(spelling: &str, call: &Call) -> String {
    match spelling.rsplit_once('@') {
        Some((prelude, argument)) => format!("{HEADER}{prelude}{}\n", call(argument)),
        None => format!("{HEADER}{}\n", call(spelling)),
    }
}

#[test]
fn display_sets_reach_the_host_however_they_are_held() {
    let plot = |x: &str| format!("plot(close, display = {x})");
    let plotshape = |x: &str| format!("plotshape(close > open, display = {x})");
    let plotcandle = |x: &str| format!("plotcandle(open, high, low, close, display = {x})");
    let calls: [(&str, &Call); 3] = [
        ("plot", &plot),
        ("plotshape", &plotshape),
        ("plotcandle", &plotcandle),
    ];
    let mut failures = Vec::new();
    let mut checked = 0;
    for (name, call) in calls {
        for (v, w) in [
            (DIFFERENCE, NONE),
            (SUM, NONE),
            (CHAIN, NONE),
            (NONE, DIFFERENCE),
        ] {
            for (form, spelling) in spellings(v.0, w.0) {
                let source = script(&spelling, call);
                // Default inputs select `v` in every spelling.
                match drawn(&source, "plot", &[]) {
                    Ok(mask) if mask == v.1 => {}
                    other => failures.push(format!("{name} {form} `{}`: {other:?}", v.0)),
                }
                checked += 1;
                // The input-controlled spellings select `w` when flipped.
                let flip = match form {
                    "input_ternary" | "input_ternary_var" | "input_if" => {
                        Some((0, InputValue::Bool(false)))
                    }
                    "input_switch" => Some((1, InputValue::String("b".into()))),
                    _ => None,
                };
                if let Some(flip) = flip {
                    match drawn(&source, "plot", &[flip]) {
                        Ok(mask) if mask == w.1 => {}
                        other => {
                            failures.push(format!("{name} {form} flipped `{}`: {other:?}", w.0));
                        }
                    }
                    checked += 1;
                }
            }
        }
    }
    assert_eq!(checked, 3 * 4 * 11);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn simple_display_parameters_take_display_values_held_in_variables() {
    // The two-state call accepts only values inferred as plot_simple_display.
    let bgcolor = |x: &str| format!("bgcolor(color.new(color.blue, 80), display = {x})");
    for (v, w) in [
        (("display.none", 0), ("display.all", ALL)),
        (("display.all", ALL), ("display.none", 0)),
    ] {
        for (form, spelling) in spellings(v.0, w.0) {
            let spelling = spelling.replace(" + display.none\nif not sel", "\nif not sel");
            let source = script(&spelling, &bgcolor);
            assert_eq!(drawn(&source, "visual", &[]), Ok(v.1), "{form} `{}`", v.0);
            let flip = match form {
                "input_ternary" | "input_ternary_var" | "input_if" => {
                    Some((0, InputValue::Bool(false)))
                }
                "input_switch" => Some((1, InputValue::String("b".into()))),
                _ => None,
            };
            if let Some(flip) = flip {
                assert_eq!(
                    drawn(&source, "visual", &[flip]),
                    Ok(w.1),
                    "{form} flipped `{}`",
                    w.0
                );
            }
        }
    }
}

fn rejected(body: &str) -> Vec<(Option<String>, String, String)> {
    let source = format!("{HEADER}{body}\n");
    let result = compile(&source, CompileOptions::default());
    assert!(!result.success, "accepted:\n{source}");
    result
        .diagnostics
        .iter()
        .filter(|d| d.is_error())
        .map(|d| {
            let label = d
                .labels
                .first()
                .map(|label| source[label.span.start as usize..label.span.end as usize].to_string())
                .unwrap_or_default();
            (d.code.clone(), d.message.clone(), label)
        })
        .collect()
}

#[test]
fn strings_are_not_display_values() {
    let type_error = |body: &str, argument: &str| {
        let errors = rejected(body);
        assert!(
            errors
                .iter()
                .any(|(code, message, label)| code.as_deref() == Some("E0205")
                    && (message.contains("must be plot_display, found string")
                        || message.contains("must be plot_simple_display, found string"))
                    && label == argument),
            "{body}: {errors:?}"
        );
    };
    // A literal that looks like a concatenated display sum.
    type_error(
        "plot(close, display = \"display.panedisplay.price_scale\")",
        "\"display.panedisplay.price_scale\"",
    );
    // A literal spelling of a real display name is still a string.
    type_error(
        "plotshape(close > open, display = \"display.pane\")",
        "\"display.pane\"",
    );
    type_error(
        "s = input.string(\"display.pane\", \"Display\")\nplot(close, display = s)",
        "s",
    );
    type_error(
        "X = \"display.panedisplay.price_scale\"\nif not sel\n    X := \"display.none\"\nplot(close, display = X)",
        "X",
    );
    type_error(
        "bgcolor(color.blue, display = sel ? \"display.all\" : \"display.none\")",
        "sel ? \"display.all\" : \"display.none\"",
    );
}

#[test]
fn display_values_do_not_mix_with_strings() {
    let errors = rejected("plot(close, display = display.pane + \"display.price_scale\")");
    assert!(
        errors
            .iter()
            .any(|(code, message, _)| code.as_deref() == Some("E0207")
                && message.contains("cannot be applied to plot_display and string")),
        "{errors:?}"
    );
    // A display value cannot be reassigned with a string, and a string
    // variable cannot be reassigned with a display value.
    let reassign_error = |body: &str, found: &str, argument: &str| {
        let errors = rejected(body);
        assert!(
            errors
                .iter()
                .any(|(code, message, label)| code.as_deref() == Some("E0218")
                    && message.contains(found)
                    && label == argument),
            "{body}: {errors:?}"
        );
    };
    reassign_error(
        "X = display.pane\nif not sel\n    X := \"display.none\"\nplot(close, display = X)",
        "it has type plot_display, but the value is string",
        "\"display.none\"",
    );
    reassign_error(
        "X = \"a\"\nX := display.none\nplot(close)",
        "it has type string, but the value is plot_simple_display",
        "display.none",
    );
    // An input display argument held in a string variable is not a display
    // constant either.
    let errors =
        rejected("d = \"display.none\"\nlen = input.int(5, \"Length\", display = d)\nplot(close)");
    assert!(
        errors
            .iter()
            .any(|(code, _, label)| code.as_deref() == Some("E0230") && label == "d"),
        "{errors:?}"
    );
}

#[test]
fn na_is_not_a_display_value() {
    // `plot_display` is a nominal type whose values are the display.* sets;
    // like text_format, it has no `na`. Hiding an output is display.none.
    for body in [
        "plot(close, display = na)",
        "plotshape(close > open, display = na)",
        "bgcolor(color.blue, display = na)",
        "plot(close, display = sel ? display.pane : na)",
        "plot(close, display = sel ? na : display.pane)",
        "X = display.pane\nif not sel\n    X := na\nplot(close, display = X)",
    ] {
        let errors = rejected(body);
        assert!(
            errors
                .iter()
                .any(|(code, message, _)| code.as_deref() == Some("E0220")
                    && message.contains("plot_display")),
            "{body}: {errors:?}"
        );
    }
}

#[test]
fn display_values_are_not_array_elements() {
    // Same guard as text_format: a display set has no array element type.
    let errors = rejected("a = array.from(display.pane)\nplot(close)");
    assert!(
        errors
            .iter()
            .any(|(code, message, label)| code.as_deref() == Some("E0234")
                && message.contains("plot_display")
                && label == "display.pane"),
        "{errors:?}"
    );
}

/// Display arithmetic whose operands are only known at run time.
///
/// The plot-family `display` parameter is typed `input plot_display`, so a
/// display value chosen by an input is a valid argument, and `+` / `-` on it
/// are the same set union and difference as on constants. Each case lists the
/// input overrides to run with and the destination set expected, computed by
/// hand from that rule.
/// `HEADER` declares `sel` as input 0.
struct RuntimeCase {
    name: &'static str,
    body: &'static str,
    /// (input overrides, expected destination set)
    runs: &'static [(&'static [(u32, RuntimeInput)], u8)],
}

#[derive(Clone, Copy, Debug)]
enum RuntimeInput {
    Bool(bool),
    Str(&'static str),
}

impl RuntimeInput {
    fn value(self) -> InputValue {
        match self {
            RuntimeInput::Bool(value) => InputValue::Bool(value),
            RuntimeInput::Str(value) => InputValue::String(value.into()),
        }
    }
}

const OFF: RuntimeInput = RuntimeInput::Bool(false);

const RUNTIME_CASES: &[RuntimeCase] = &[
    RuntimeCase {
        name: "input_ternary_sum",
        body: "X = input.bool(true) ? display.pane : display.none\nplot(close, display = X + display.price_scale)",
        runs: &[(&[], PANE | PRICE_SCALE), (&[(1, OFF)], PRICE_SCALE)],
    },
    RuntimeCase {
        name: "input_string_switch_sum",
        body: "s = input.string(\"Pane\", \"Where\", options = [\"Pane\", \"Status\", \"Hidden\"])\nX = switch s\n    \"Pane\" => display.pane\n    \"Status\" => display.status_line\n    => display.none\nplot(close, display = X + display.data_window)",
        runs: &[
            (&[], PANE | DATA_WINDOW),
            (
                &[(1, RuntimeInput::Str("Status"))],
                STATUS_LINE | DATA_WINDOW,
            ),
            (&[(1, RuntimeInput::Str("Hidden"))], DATA_WINDOW),
        ],
    },
    RuntimeCase {
        name: "input_switch_screener_difference",
        body: "X = sel ? display.pine_screener : display.none\nplot(close, display = display.all - X)",
        runs: &[(&[], ALL & !PINE_SCREENER), (&[(0, OFF)], ALL)],
    },
    RuntimeCase {
        name: "input_switch_screener_union",
        body: "X = sel ? display.pine_screener : display.none\nplot(close, display = display.data_window + X)",
        runs: &[(&[], DATA_WINDOW | PINE_SCREENER), (&[(0, OFF)], DATA_WINDOW)],
    },
    RuntimeCase {
        name: "difference_from_constant",
        body: "X = sel ? display.pane : display.none\nplot(close, display = display.all - X)",
        runs: &[
            (&[], PRICE_SCALE | STATUS_LINE | DATA_WINDOW | PINE_SCREENER),
            (&[(0, OFF)], ALL),
        ],
    },
    RuntimeCase {
        name: "difference_of_runtime_operand",
        body: "X = sel ? display.all : display.pane + display.status_line\nplot(close, display = X - display.pane)",
        runs: &[
            (&[], PRICE_SCALE | STATUS_LINE | DATA_WINDOW | PINE_SCREENER),
            (&[(0, OFF)], STATUS_LINE),
        ],
    },
    RuntimeCase {
        name: "nested_sum_of_two_runtime_operands",
        body: "X = sel ? display.pane : display.none\nY = sel ? display.none : display.status_line\nZ = X + Y + display.price_scale\nplot(close, display = Z - (Y - display.status_line) + (X + Y))",
        runs: &[
            (&[], PANE | PRICE_SCALE),
            (&[(0, OFF)], PRICE_SCALE | STATUS_LINE),
        ],
    },
    RuntimeCase {
        name: "if_expression_sum",
        body: "X = if sel\n    display.pane\nelse\n    display.status_line\nplot(close, display = X + display.price_scale)",
        runs: &[
            (&[], PANE | PRICE_SCALE),
            (&[(0, OFF)], PRICE_SCALE | STATUS_LINE),
        ],
    },
    // Reassignment under an input-controlled `if` is resolved by the engine
    // after the run instead of binding an input expression.
    RuntimeCase {
        name: "reassigned_operand_sum",
        body: "X = display.pane\nif not sel\n    X := display.none\nplot(close, display = X + display.price_scale)",
        runs: &[(&[], PANE | PRICE_SCALE), (&[(0, OFF)], PRICE_SCALE)],
    },
    RuntimeCase {
        name: "plotshape_held_sum",
        body: "X = sel ? display.status_line : display.none\nD = display.pane + X\nplotshape(close > open, display = D)",
        runs: &[(&[], PANE | STATUS_LINE), (&[(0, OFF)], PANE)],
    },
];

fn overrides_of(overrides: &[(u32, RuntimeInput)]) -> Vec<(u32, InputValue)> {
    overrides
        .iter()
        .map(|(id, value)| (*id, value.value()))
        .collect()
}

#[test]
fn display_arithmetic_on_runtime_operands_draws_the_set_pine_defines() {
    let mut failures = Vec::new();
    for case in RUNTIME_CASES {
        let source = format!("{HEADER}{}\n", case.body);
        for (overrides, expected) in case.runs {
            match drawn(&source, "plot", &overrides_of(overrides)) {
                Ok(mask) if mask == *expected => {}
                other => failures.push(format!(
                    "{} {overrides:?}: expected {expected}, got {other:?}",
                    case.name
                )),
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Every node a display binding may contain: one of Pine's seven display names
/// as a value, a `+` union of member names, and ternaries over input values.
/// Never our joined wire token, never a `-` the host would have to evaluate,
/// never `na`.
fn assert_member_union_shape(expr: &InputActiveExpr, context: &str) {
    const NAMES: [&str; 7] = [
        "display.none",
        "display.pane",
        "display.price_scale",
        "display.status_line",
        "display.data_window",
        "display.pine_screener",
        "display.all",
    ];
    fn member_union(expr: &InputActiveExpr) -> bool {
        match expr {
            InputActiveExpr::Value {
                value: InputValue::String(name),
            } => NAMES[1..6].contains(&name.as_str()),
            InputActiveExpr::Binary {
                op: InputActiveBinaryOp::Add,
                lhs,
                rhs,
            } => member_union(lhs) && member_union(rhs),
            _ => false,
        }
    }
    match expr {
        InputActiveExpr::Ternary {
            when_true,
            when_false,
            ..
        } => {
            assert_member_union_shape(when_true, context);
            assert_member_union_shape(when_false, context);
        }
        InputActiveExpr::Value {
            value: InputValue::String(name),
        } => assert!(NAMES.contains(&name.as_str()), "{context}: {expr:?}"),
        other => assert!(member_union(other), "{context}: {other:?}"),
    }
}

fn last_style<'a>(
    plots: impl Iterator<Item = &'a DisplayStyle>,
    visuals: impl Iterator<Item = &'a DisplayStyle>,
) -> &'a DisplayStyle {
    plots.chain(visuals).last().expect("an output")
}

#[test]
fn runtime_display_arithmetic_reaches_hosts_as_a_member_union_marked_non_static() {
    for case in RUNTIME_CASES {
        let source = format!("{HEADER}{}\n", case.body);
        // Compile-time metadata: the display is never presented as a fixed
        // value. It is either an input-bound expression the host evaluates or
        // a runtime binding the engine resolves after the run.
        let result = compile(&source, CompileOptions::default());
        assert!(result.success, "{}: {:?}", case.name, result.diagnostics);
        let program = result.program.as_ref().unwrap();
        let style = last_style(
            program.plots.iter().map(|plot| &plot.style),
            program.visuals.iter().map(|visual| &visual.style),
        );
        assert!(
            !style.unsupported.iter().any(|o| o.field == "display"),
            "{}: {:?}",
            case.name,
            style.unsupported
        );
        let bound = style.bound_scalars.iter().find(|b| b.field == "display");
        let runtime = style.runtime_scalars.iter().any(|b| b.field == "display");
        assert!(
            bound.is_some() != runtime,
            "{}: the display must be bound or resolved at run time, exactly one",
            case.name
        );
        if let Some(binding) = bound {
            assert_member_union_shape(&binding.expression, case.name);
        }
        // After a run, every resolved binding is the member union of the set
        // drawn, exactly the shape a compile-time sum binds as.
        for (overrides, expected) in case.runs {
            let options = RunOptions {
                inputs: overrides_of(overrides).into_iter().collect(),
                ..RunOptions::default()
            };
            let run = compile_and_run(&source, &market(), CompileOptions::default(), &options);
            let style = last_style(
                run.plots.iter().map(|plot| &plot.style),
                run.visuals.iter().map(|visual| &visual.style),
            );
            assert!(style.runtime_scalars.is_empty(), "{}", case.name);
            let binding = style
                .bound_scalars
                .iter()
                .find(|b| b.field == "display")
                .unwrap_or_else(|| panic!("{}: no display binding", case.name));
            assert_member_union_shape(&binding.expression, case.name);
            if runtime {
                assert_eq!(
                    binding.expression,
                    pinewasm_hir::display_set::binding_expr(*expected).unwrap(),
                    "{}",
                    case.name
                );
            }
        }
    }
}

/// Pine source for a destination set, written with constants only.
fn pine_set(mask: u8) -> String {
    const NAMES: [&str; 5] = [
        "display.pane",
        "display.price_scale",
        "display.status_line",
        "display.data_window",
        "display.pine_screener",
    ];
    let members: Vec<&str> = (0..NAMES.len())
        .filter(|bit| mask & (1 << bit) != 0)
        .map(|bit| NAMES[bit])
        .collect();
    if members.is_empty() {
        "display.none".into()
    } else {
        members.join(" + ")
    }
}

/// A series script that runs `+` and `-` over every pair of the 16
/// destination sets, one pair per bar, and plots each result as its set
/// number (pane 1, price scale 2, status line 4, Data Window 8).
fn every_pair_source() -> String {
    let pick = |var: &str, index: &str| {
        let mut out = format!("{var} = switch {index}\n");
        for mask in 0..15u8 {
            out.push_str(&format!("    {mask} => {}\n", pine_set(mask)));
        }
        out.push_str(&format!("    => {}\n", pine_set(15)));
        out
    };
    let decode = |expr: &str| {
        let mut out = String::new();
        for mask in 0..15u8 {
            out.push_str(&format!("{expr} == {} ? {mask} : ", pine_set(mask)));
        }
        out.push_str("15");
        out
    };
    format!(
        "//@version=6\nindicator(\"Display pairs\")\ni = bar_index % 16\nj = math.floor(bar_index / 16) % 16\n{}{}U = X + Y\nD = X - Y\nplot({})\nplot({})\n",
        pick("X", "i"),
        pick("Y", "j"),
        decode("U"),
        decode("D"),
    )
}

#[test]
fn runtime_union_and_difference_match_set_algebra_for_every_pair() {
    let bars = 256;
    let data = MarketData {
        open: vec![10.0; bars],
        high: vec![12.0; bars],
        low: vec![9.0; bars],
        close: vec![11.0; bars],
        volume: vec![100.0; bars],
        timeframe: "1D".into(),
        ..MarketData::default()
    };
    let result = compile_and_run(
        &every_pair_source(),
        &data,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    let errors: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|d| d.is_error())
        .map(|d| format!("{:?} {}", d.code, d.message))
        .collect();
    assert!(errors.is_empty(), "{errors:?}");
    assert!(
        result.runtime_errors.is_empty(),
        "{:?}",
        result.runtime_errors
    );
    for bar in 0..bars {
        let (x, y) = ((bar % 16) as u8, (bar / 16) as u8);
        let value = |plot: usize| match &result.plots[plot].values[bar] {
            pinewasm_mir::Value::Float(v) => *v,
            pinewasm_mir::Value::Int(v) => *v as f64,
            other => panic!("bar {bar}: {other:?}"),
        };
        assert_eq!(value(0), f64::from(x | y), "{x} + {y}");
        assert_eq!(value(1), f64::from(x & !y), "{x} - {y}");
    }
}

#[test]
fn runtime_display_arithmetic_still_rejects_strings_and_series() {
    // A string never becomes a display value, even next to a run-time one.
    let errors = rejected(
        "X = sel ? display.pane : display.none\nplot(close, display = X + \"display.price_scale\")",
    );
    assert!(
        errors
            .iter()
            .any(|(code, message, _)| code.as_deref() == Some("E0207")
                && message.contains("cannot be applied to plot_display and string")),
        "{errors:?}"
    );
    // A display argument is `input plot_display`: a set that changes per bar
    // is a series and is rejected.
    let errors = rejected(
        "X = close > open ? display.pane : display.none\nplot(close, display = X + display.price_scale)",
    );
    assert!(
        errors
            .iter()
            .any(|(code, message, _)| code.as_deref() == Some("E0206")
                && message.contains("display")),
        "{errors:?}"
    );
}

/// Compile-time display binding and post-run display binding of the first
/// plot, and the engine's own value of `D` (plotted as its set number by the
/// second plot) on the last bar.
fn bindings_and_engine_set(
    source: &str,
    overrides: &HashMap<u32, InputValue>,
) -> (Option<InputActiveExpr>, bool, InputActiveExpr, u8) {
    let compiled = compile(source, CompileOptions::default());
    assert!(compiled.success, "{source}: {:?}", compiled.diagnostics);
    let style = &compiled.program.as_ref().unwrap().plots[0].style;
    let bound = style
        .bound_scalars
        .iter()
        .find(|b| b.field == "display")
        .map(|b| b.expression.clone());
    let runtime = style.runtime_scalars.iter().any(|b| b.field == "display");
    let options = RunOptions {
        inputs: overrides.clone(),
        ..RunOptions::default()
    };
    let run = compile_and_run(source, &market(), CompileOptions::default(), &options);
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    let resolved = run.plots[0]
        .style
        .bound_scalars
        .iter()
        .find(|b| b.field == "display")
        .map_or_else(
            || panic!("{source}: no display binding after the run"),
            |b| b.expression.clone(),
        );
    let engine = match run.plots[1].values.last() {
        Some(pinewasm_mir::Value::Float(v)) => *v as u8,
        Some(pinewasm_mir::Value::Int(v)) => *v as u8,
        other => panic!("{source}: {other:?}"),
    };
    (bound, runtime, resolved, engine)
}

/// `plot(...)` that draws the engine's value of `D` as its set number.
fn decode_d() -> String {
    let mut out = String::from("plot(");
    for mask in 0..ALL {
        out.push_str(&format!("D == ({}) ? {mask} : ", pine_set(mask)));
    }
    out.push_str(&format!("{ALL})\n"));
    out
}

#[test]
fn display_arithmetic_past_the_binding_cap_is_resolved_after_the_run() {
    // Seven independent input choices give 2^7 = 128 sets, past the
    // 64-choice cap: the compiler must not bind them, and must not present a
    // fixed value either. After the run the host gets the member union of
    // the set drawn.
    const MEMBERS: [u8; 5] = [PANE, PRICE_SCALE, STATUS_LINE, DATA_WINDOW, PINE_SCREENER];
    let mut source = String::from("//@version=6\nindicator(\"Wide display sum\")\n");
    for k in 0..7 {
        source.push_str(&format!(
            "b{k} = input.bool(false)\nX{k} = b{k} ? {} : display.none\n",
            pine_set(MEMBERS[k % MEMBERS.len()])
        ));
    }
    source.push_str("D = X0 + X1 + X2 + X3 + X4 + X5 + X6\nplot(close, display = D)\n");
    source.push_str(&decode_d());
    for on in [0u32, 0b1, 0b100, 0b1000, 0b10000, 0b100_0010, 0b111_1111] {
        let overrides: HashMap<u32, InputValue> = (0..7)
            .map(|k| (k, InputValue::Bool(on & (1 << k) != 0)))
            .collect();
        let expected = (0..7)
            .filter(|k| on & (1 << k) != 0)
            .fold(0, |set, k| set | MEMBERS[k % MEMBERS.len()]);
        let (bound, runtime, resolved, engine) = bindings_and_engine_set(&source, &overrides);
        assert!(bound.is_none() && runtime, "on={on:b}: {bound:?}");
        assert_eq!(engine, expected, "on={on:b}");
        assert_eq!(
            resolved,
            pinewasm_hir::display_set::binding_expr(expected).unwrap(),
            "on={on:b}"
        );
    }
}

#[test]
fn display_arithmetic_within_the_choice_cap_but_past_the_binding_size_limit_still_compiles() {
    // Three `input.string` switches of four sets each choose between
    // 4 * 4 * 4 = 64 sets, inside the 64-choice cap. Written out as ternaries
    // over member unions, that binding has far more than the 256 nodes the
    // binding protocol accepts (`MAX_INPUT_ACTIVE_EXPRESSION_NODES`), so it
    // must not be bound: the engine resolves the set after the run instead.
    let sets: [[u8; 4]; 3] = [
        [PANE, PANE | PRICE_SCALE, STATUS_LINE | DATA_WINDOW, 0],
        [
            PRICE_SCALE,
            PANE | STATUS_LINE,
            DATA_WINDOW,
            PANE | DATA_WINDOW,
        ],
        [
            STATUS_LINE,
            PRICE_SCALE | DATA_WINDOW,
            0,
            PANE | PRICE_SCALE,
        ],
    ];
    const NAMES: [&str; 4] = ["A", "B", "C", "D"];
    let mut source = String::from("//@version=6\nindicator(\"Three switches\")\n");
    for (k, choices) in sets.iter().enumerate() {
        source.push_str(&format!(
            "s{k} = input.string(\"A\", options = [\"A\", \"B\", \"C\", \"D\"])\nX{k} = switch s{k}\n"
        ));
        for (name, set) in NAMES.iter().zip(&choices[..3]) {
            source.push_str(&format!("    \"{name}\" => {}\n", pine_set(*set)));
        }
        source.push_str(&format!("    => {}\n", pine_set(choices[3])));
    }
    // A sum and a difference, so neither operation hides the other.
    source.push_str("D = X0 + X1 - X2\nplot(close, display = D)\n");
    source.push_str(&decode_d());
    for picks in [[0, 0, 0], [1, 2, 3], [3, 1, 0], [2, 3, 1]] {
        let overrides: HashMap<u32, InputValue> = picks
            .iter()
            .enumerate()
            .map(|(k, pick)| (k as u32, InputValue::String(NAMES[*pick].into())))
            .collect();
        let expected = (sets[0][picks[0]] | sets[1][picks[1]]) & !sets[2][picks[2]];
        let (bound, runtime, resolved, engine) = bindings_and_engine_set(&source, &overrides);
        assert!(bound.is_none() && runtime, "{picks:?}: {bound:?}");
        assert_eq!(engine, expected, "{picks:?}");
        assert_eq!(host_mask(&resolved, &overrides), Ok(expected), "{picks:?}");
    }
}

#[test]
fn other_style_bindings_past_the_size_limit_are_resolved_after_the_run_too() {
    // The same guard covers every style argument: an 80-way input `switch`
    // for `linewidth` is a binding of more than 256 nodes, so the engine
    // resolves it after the run.
    let mut source =
        String::from("//@version=6\nindicator(\"Wide width\")\nn = input.int(1)\nw = switch n\n");
    for k in 1..80 {
        source.push_str(&format!("    {k} => {}\n", k % 5 + 1));
    }
    source.push_str("    => 1\nplot(close, linewidth = w)\n");
    let compiled = compile(&source, CompileOptions::default());
    assert!(compiled.success, "{:?}", compiled.diagnostics);
    let style = &compiled.program.as_ref().unwrap().plots[0].style;
    assert!(style.bound_scalars.iter().all(|b| b.field != "linewidth"));
    assert!(style.runtime_scalars.iter().any(|b| b.field == "linewidth"));
    let options = RunOptions {
        inputs: [(0, InputValue::Int(7))].into_iter().collect(),
        ..RunOptions::default()
    };
    let run = compile_and_run(&source, &market(), CompileOptions::default(), &options);
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    let width = run.plots[0]
        .style
        .bound_scalars
        .iter()
        .find(|b| b.field == "linewidth")
        .map(|b| b.expression.clone());
    // 7 % 5 + 1
    assert_eq!(
        width,
        Some(InputActiveExpr::Value {
            value: InputValue::Int(3)
        })
    );
}

#[test]
fn mixed_display_arithmetic_binds_the_set_algebra_under_every_input() {
    // Random `+` / `-` trees over operands chosen by a bool ternary, a
    // `switch` on an input.string, an input.int chain, an `if` expression and
    // a reassignment, plus constants. The expected set comes from the
    // union/difference rule applied here. For every input assignment the
    // engine's value, the compile-time binding (when there is one) and the
    // post-run binding must all name that set, through the strict host.
    enum Tree {
        Const(u8),
        Operand(usize),
        Op(bool, Box<Tree>, Box<Tree>),
    }
    #[derive(Clone, Copy)]
    struct Inputs {
        b0: bool,
        b1: bool,
        b2: bool,
        s: usize,
        n: i64,
    }
    fn operand(choices: &[[u8; 3]], index: usize, at: Inputs) -> u8 {
        let c = choices[index];
        match index {
            0 => c[usize::from(!at.b0)],
            1 => c[usize::from(!at.b1)],
            2 => c[at.s],
            3 => {
                c[match at.n {
                    1 => 0,
                    2 => 1,
                    _ => 2,
                }]
            }
            4 => c[usize::from(!at.b2)],
            _ => c[usize::from(!at.b0)],
        }
    }
    fn expected(tree: &Tree, choices: &[[u8; 3]], at: Inputs) -> u8 {
        match tree {
            Tree::Const(mask) => *mask,
            Tree::Operand(index) => operand(choices, *index, at),
            Tree::Op(union, lhs, rhs) => {
                let (l, r) = (expected(lhs, choices, at), expected(rhs, choices, at));
                if *union {
                    l | r
                } else {
                    l & !r
                }
            }
        }
    }
    fn source(tree: &Tree) -> String {
        match tree {
            Tree::Const(mask) => format!("({})", pine_set(*mask)),
            Tree::Operand(index) => format!("P{index}"),
            Tree::Op(union, lhs, rhs) => format!(
                "({} {} {})",
                source(lhs),
                if *union { "+" } else { "-" },
                source(rhs)
            ),
        }
    }
    fn generate(next: &mut dyn FnMut(u64) -> u64, depth: u32, operands: u64) -> Tree {
        if depth == 0 || next(3) == 0 {
            if next(4) == 0 {
                Tree::Const(next(16) as u8)
            } else {
                Tree::Operand(next(operands) as usize)
            }
        } else {
            Tree::Op(
                next(2) == 0,
                Box::new(generate(next, depth - 1, operands)),
                Box::new(generate(next, depth - 1, operands)),
            )
        }
    }
    let mut seed: u64 = 0x434;
    let mut next = move |n: u64| {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (seed >> 33) % n
    };
    let assignments: Vec<Inputs> = (0..6)
        .map(|k| Inputs {
            b0: k & 1 != 0,
            b1: k & 2 != 0,
            b2: k & 4 != 0,
            s: k % 3,
            n: (k % 4) as i64,
        })
        .collect();
    let (mut bound_cases, mut runtime_cases) = (0, 0);
    for case in 0..36 {
        let choices: Vec<[u8; 3]> = (0..6)
            .map(|_| [next(16) as u8, next(16) as u8, next(16) as u8])
            .collect();
        // Every third case may use the engine-resolved operands (P4, P5).
        let operands = if case % 3 == 0 { 6 } else { 4 };
        let tree = loop {
            let tree = generate(&mut next, 4, operands);
            if matches!(tree, Tree::Op(..)) {
                break tree;
            }
        };
        let set = |index: usize, choice: usize| pine_set(choices[index][choice]);
        let mut script = String::from("//@version=6\nindicator(\"Mixed display arithmetic\")\nb0 = input.bool(true)\nb1 = input.bool(false)\nb2 = input.bool(true)\ns = input.string(\"A\", options = [\"A\", \"B\", \"C\"])\nn = input.int(1)\n");
        script += &format!("P0 = b0 ? {} : {}\n", set(0, 0), set(0, 1));
        script += &format!("P1 = b1 ? {} : {}\n", set(1, 0), set(1, 1));
        script += &format!(
            "P2 = switch s\n    \"A\" => {}\n    \"B\" => {}\n    => {}\n",
            set(2, 0),
            set(2, 1),
            set(2, 2)
        );
        script += &format!(
            "P3 = n == 1 ? {} : n == 2 ? {} : {}\n",
            set(3, 0),
            set(3, 1),
            set(3, 2)
        );
        script += &format!("P4 = if b2\n    {}\nelse\n    {}\n", set(4, 0), set(4, 1));
        script += &format!(
            "P5 = {} + display.none\nif not b0\n    P5 := {}\n",
            set(5, 0),
            set(5, 1)
        );
        script += &format!("D = {}\nplot(close, display = D)\n", source(&tree));
        script += &decode_d();
        for at in &assignments {
            let overrides: HashMap<u32, InputValue> = [
                (0, InputValue::Bool(at.b0)),
                (1, InputValue::Bool(at.b1)),
                (2, InputValue::Bool(at.b2)),
                (3, InputValue::String(["A", "B", "C"][at.s].into())),
                (4, InputValue::Int(at.n)),
            ]
            .into_iter()
            .collect();
            let want = expected(&tree, &choices, *at);
            let (bound, runtime, resolved, engine) = bindings_and_engine_set(&script, &overrides);
            let context = source(&tree);
            assert!(bound.is_some() != runtime, "{context}");
            assert_eq!(engine, want, "engine: {context}");
            assert_eq!(host_mask(&resolved, &overrides), Ok(want), "run: {context}");
            if let Some(bound) = &bound {
                assert_member_union_shape(bound, &context);
                assert_eq!(host_mask(bound, &overrides), Ok(want), "bound: {context}");
            }
            if bound.is_some() {
                bound_cases += 1;
            } else {
                runtime_cases += 1;
            }
        }
    }
    // Both host paths were exercised.
    assert!(
        bound_cases > 50 && runtime_cases > 50,
        "{bound_cases} {runtime_cases}"
    );
}

#[test]
fn runtime_resolved_screener_only_level_is_hidden_with_a_diagnostic() {
    let mut source = String::from("//@version=6\nindicator(\"Wide level display\")\n");
    for k in 0..7 {
        source.push_str(&format!(
            "b{k} = input.bool(false)\nD{k} = b{k} ? display.pine_screener : display.none\n"
        ));
    }
    source.push_str("hline(70, display=D0 + D1 + D2 + D3 + D4 + D5 + D6)\n");
    let invalid_default = source.replace("b4 = input.bool(false)", "b4 = input.bool(true)");
    let rejected = compile(&invalid_default, CompileOptions::default());
    assert!(!rejected.success);
    assert!(rejected.diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("does not accept a screener display destination")
    }));

    let aliased = invalid_default.replace(
        "hline(70, display=D0 + D1 + D2 + D3 + D4 + D5 + D6)",
        "destination = D0 + D1 + D2 + D3 + D4 + D5 + D6\nhline(70, display=destination)",
    );
    assert!(!compile(&aliased, CompileOptions::default()).success);
    let reassigned = aliased.replace("hline(70,", "destination := display.none\nhline(70,");
    let compiled = compile(&reassigned, CompileOptions::default());
    assert!(!compiled.success, "{:?}", compiled.diagnostics);

    // A level that the source rules reject can still reach the runtime
    // safeguard. Compile the same display arithmetic on the broader plot
    // surface, then mark the plot as a level without relaxing source admission.
    let legacy = |source: &str| {
        let result = compile(
            &source.replace("hline(70,", "plot(70,"),
            CompileOptions::default(),
        );
        assert!(result.success, "{:?}", result.diagnostics);
        let mut program = result.program.unwrap();
        program.plots[0].style.kind = DisplayKind::Hline;
        program.plots[0].style.plot_color_mode = None;
        program
    };

    let valid = source.replace(
        "D0 = b0 ? display.pine_screener : display.none",
        "D0 = b0 ? display.pine_screener : display.all",
    );
    let run = run_program(&legacy(&valid), &market(), &RunOptions::default());
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    let style = &run.plots[0].style;
    assert!(style.unsupported.is_empty(), "{:?}", style.unsupported);
    assert!(style.bound_scalars.iter().any(|binding| {
        binding.field == "display"
            && Some(binding.expression.clone()) == pinewasm_hir::display_set::binding_expr(ALL)
    }));

    let opts = RunOptions {
        inputs: [(4, InputValue::Bool(true))].into_iter().collect(),
        ..RunOptions::default()
    };
    let run = run_program(&legacy(&source), &market(), &opts);
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    let style = &run.plots[0].style;
    assert_eq!(style.display, PlotDisplay::None);
    assert!(style
        .unsupported
        .iter()
        .any(|option| { option.field == "display" && option.reason.contains("screener") }));
}
