//! Chart-context history cases and local runtime semantics.
//! Each case checks whether the compiler accepts the source. The runtime
//! values below are the expected VM results for each accepted source. Each
//! source is pinned by SHA-256, so an edited source fails the test. The
//! literal/color/enum negatives are reused unchanged.

use pinewasm_compiler::{
    compile, run_program, CompileOptions, InputValue, MarketData, RunOptions, RunResult,
    RuntimeErrorKind, SourceId, Value,
};
use pinewasm_hir::{Qualifier, Type};
use pinewasm_semantics::BuiltinRegistry;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

struct AdmissionCase {
    id: &'static str,
    source: &'static str,
    source_sha256: &'static str,
    accepted: bool,
    // Local diagnostics point at the source target, which is narrower than
    // the whole history access.
    rejected_target: Option<(u32, u32)>,
}

const ADMISSION_CASES: &[AdmissionCase] = &[
    AdmissionCase {
        id: "chart-context-history/chart_is_heikinashi_zero",
        source: r#"//@version=6
indicator("Chart context history discriminator")
plot(chart.is_heikinashi[0] ? 1 : 0)
"#,
        source_sha256: "97d43827180c98585b985be660f2b855ab2738cd2bb6393204c8ed6b0431dc79",
        accepted: true,
        rejected_target: None,
    },
    AdmissionCase {
        id: "chart-context-history/chart_is_heikinashi_one",
        source: r#"//@version=6
indicator("Chart context history discriminator")
plot(chart.is_heikinashi[1] ? 1 : 0)
"#,
        source_sha256: "a995aa9e54b7c0e589c524c05d7e4953f3a0c0b00d9f1efcc23afde624492a11",
        accepted: true,
        rejected_target: None,
    },
    AdmissionCase {
        id: "chart-context-history/chart_bg_color_zero",
        source: r#"//@version=6
indicator("Chart context history discriminator")
plot(color.r(chart.bg_color[0]))
"#,
        source_sha256: "b2f5c6728866409fd8e90f6b94b92a998f555f851fc1de720218c175036ad199",
        accepted: false,
        rejected_target: Some((75, 89)),
    },
    AdmissionCase {
        id: "chart-context-history/chart_bg_color_one",
        source: r#"//@version=6
indicator("Chart context history discriminator")
plot(color.r(chart.bg_color[1]))
"#,
        source_sha256: "0ceec9ac1fdf083f1f00d9bd7b18d0ff5358674acd29e5eca623a5dce7091772",
        accepted: false,
        rejected_target: Some((75, 89)),
    },
    AdmissionCase {
        id: "chart-context-history/chart_fg_color_zero",
        source: r#"//@version=6
indicator("Chart context history discriminator")
plot(color.r(chart.fg_color[0]))
"#,
        source_sha256: "d104b1cf2145546dcc8c1813f06b92e462cbc442bee7210fc9125dfab6b1b4be",
        accepted: false,
        rejected_target: Some((75, 89)),
    },
    AdmissionCase {
        id: "chart-context-history/chart_fg_color_one",
        source: r#"//@version=6
indicator("Chart context history discriminator")
plot(color.r(chart.fg_color[1]))
"#,
        source_sha256: "adb1d4fb06ac12709001f144d30664bb9f3d3bc6de852e4fdad218c912f6e7ff",
        accepted: false,
        rejected_target: Some((75, 89)),
    },
    AdmissionCase {
        id: "chart-context-history-admission-v2/chart_is_standard_zero",
        source: r#"//@version=6
indicator("Chart type predicate history family")
plot(chart.is_standard[0] ? 1 : 0)
"#,
        source_sha256: "a5ffe1d0eca83109dd1abb100dc1e90b9435228e52080d3a3cd4949cdb855587",
        accepted: true,
        rejected_target: None,
    },
    AdmissionCase {
        id: "chart-context-history-admission-v2/chart_is_standard_one",
        source: r#"//@version=6
indicator("Chart type predicate history family")
plot(chart.is_standard[1] ? 1 : 0)
"#,
        source_sha256: "9149dcbed6c2adc0dede1dd96b7ecf61ff7315fb95c8d50405605b5aede3ca25",
        accepted: true,
        rejected_target: None,
    },
    AdmissionCase {
        id: "chart-context-history-admission-v2/chart_is_kagi_zero",
        source: r#"//@version=6
indicator("Chart type predicate history family")
plot(chart.is_kagi[0] ? 1 : 0)
"#,
        source_sha256: "edf6661fc54d99b5e22d3c490479ab6119a7a6df79c73fc83fc68f24db105ab9",
        accepted: true,
        rejected_target: None,
    },
    AdmissionCase {
        id: "chart-context-history-admission-v2/chart_is_kagi_one",
        source: r#"//@version=6
indicator("Chart type predicate history family")
plot(chart.is_kagi[1] ? 1 : 0)
"#,
        source_sha256: "f8146a708fe5d8fba59697a968f2996406a606c9c4434b63e02b60efd5d6e0d7",
        accepted: true,
        rejected_target: None,
    },
    AdmissionCase {
        id: "chart-context-history-admission-v2/chart_is_linebreak_zero",
        source: r#"//@version=6
indicator("Chart type predicate history family")
plot(chart.is_linebreak[0] ? 1 : 0)
"#,
        source_sha256: "2d1fa515e807b4961d5b0712b436720790dbd3d6b80171d0bfc2d760c97b0d9b",
        accepted: true,
        rejected_target: None,
    },
    AdmissionCase {
        id: "chart-context-history-admission-v2/chart_is_linebreak_one",
        source: r#"//@version=6
indicator("Chart type predicate history family")
plot(chart.is_linebreak[1] ? 1 : 0)
"#,
        source_sha256: "19e55db1141e8d1f9e08a96b1abe4f6e95df15c9021e87a0f2f2764f047da7cf",
        accepted: true,
        rejected_target: None,
    },
    AdmissionCase {
        id: "chart-context-history-admission-v2/chart_is_pnf_zero",
        source: r#"//@version=6
indicator("Chart type predicate history family")
plot(chart.is_pnf[0] ? 1 : 0)
"#,
        source_sha256: "8e9d64b58c8638d2b0ca8b7214c30f552398f1aeb91857ac06e806d1f19c6b4a",
        accepted: true,
        rejected_target: None,
    },
    AdmissionCase {
        id: "chart-context-history-admission-v2/chart_is_pnf_one",
        source: r#"//@version=6
indicator("Chart type predicate history family")
plot(chart.is_pnf[1] ? 1 : 0)
"#,
        source_sha256: "04a753a3faaed20295fb650b8390bb6287f22b995d209b32116e7d06783755d5",
        accepted: true,
        rejected_target: None,
    },
    AdmissionCase {
        id: "chart-context-history-admission-v2/chart_is_range_zero",
        source: r#"//@version=6
indicator("Chart type predicate history family")
plot(chart.is_range[0] ? 1 : 0)
"#,
        source_sha256: "0b5f35363f2a60a4e8324ba54f42a3638bc2d932119712a56524475888104d54",
        accepted: true,
        rejected_target: None,
    },
    AdmissionCase {
        id: "chart-context-history-admission-v2/chart_is_range_one",
        source: r#"//@version=6
indicator("Chart type predicate history family")
plot(chart.is_range[1] ? 1 : 0)
"#,
        source_sha256: "861507920c180ad31d5da470521b23ca7347fda98a38c3337a135f613ac29431",
        accepted: true,
        rejected_target: None,
    },
    AdmissionCase {
        id: "chart-context-history-admission-v2/chart_is_renko_zero",
        source: r#"//@version=6
indicator("Chart type predicate history family")
plot(chart.is_renko[0] ? 1 : 0)
"#,
        source_sha256: "3f46d88741e657fc225d2247dab6e66bf0ee5404efccd5fffb551eaaaaf7a213",
        accepted: true,
        rejected_target: None,
    },
    AdmissionCase {
        id: "chart-context-history-admission-v2/chart_is_renko_one",
        source: r#"//@version=6
indicator("Chart type predicate history family")
plot(chart.is_renko[1] ? 1 : 0)
"#,
        source_sha256: "997a15039a0aa1db4b2b9939db3ee3978ef7ae9b1f58a9cb7bb1f650b89fa83b",
        accepted: true,
        rejected_target: None,
    },
    AdmissionCase {
        id: "chart-context-history-admission-v2/chart_is_heikinashi_input_lag",
        source: r#"//@version=6
indicator("Chart type predicate history family")
lag = input.int(1)
plot(chart.is_heikinashi[lag] ? 1 : 0)
"#,
        source_sha256: "22dfa8c9cd9ce64de8c6687d23ab5568911fbb7e1ebf576401ce788f7c1d87c8",
        accepted: true,
        rejected_target: None,
    },
    AdmissionCase {
        id: "compound-history-zero-offset-v1/literal_zero_offset",
        source: r#"//@version=6
indicator("Zero offset history boundary")
plot(1.0[0])
"#,
        source_sha256: "2f0a30a24ebcbfdfe15fc3ec3ef5e7447a8221b1146e157c44ca63e9ea546424",
        accepted: false,
        rejected_target: Some((60, 63)),
    },
    AdmissionCase {
        id: "compound-history-boundary-addendum-v1/direct_float_literal_history",
        source: r#"//@version=6
indicator("Compound history syntax boundary")
plot(1.0[1])
"#,
        source_sha256: "7223793202690e50c0c37740e30b9a2410e4729b6381bb1a1642e18cc2944c3d",
        accepted: false,
        rejected_target: Some((64, 67)),
    },
    AdmissionCase {
        id: "compound-history-zero-offset-v1/builtin_color_zero_offset",
        source: r#"//@version=6
indicator("Zero offset history boundary")
plot(color.r(color.red[0]))
"#,
        source_sha256: "5ff48e6f0fd238691947e528490123c681d0e474ab754cd79ac5ca73c160f3da",
        accepted: false,
        rejected_target: Some((68, 77)),
    },
    AdmissionCase {
        id: "compound-history-target-shapes-v1/builtin_color_constant_history",
        source: r#"//@version=6
indicator("History target shape boundary")
color c = color.red[1]
plot(c == color.red ? 1 : 0)
"#,
        source_sha256: "c22872c9fd293bc19ec763fec0e088a934985fd73ae85f86d1833783675348d9",
        accepted: false,
        rejected_target: Some((66, 75)),
    },
    AdmissionCase {
        id: "history-enum-drawings-boundary-v1/enum_member_literal_zero",
        source: r#"//@version=6
indicator("Enum and drawing collection history boundary")
enum Choice
    first
    second
value = Choice.first[0]
plot(value == Choice.first ? 1 : 0)
"#,
        source_sha256: "72737d327f2bd80a3e5ee05d9532b4270935019d32a9f8ae565d0d9c5453b781",
        accepted: false,
        rejected_target: Some((112, 124)),
    },
    AdmissionCase {
        id: "history-enum-drawings-boundary-v1/enum_member_literal_one",
        source: r#"//@version=6
indicator("Enum and drawing collection history boundary")
enum Choice
    first
    second
value = Choice.first[1]
plot(value == Choice.first ? 1 : 0)
"#,
        source_sha256: "2d6b976ab5b4b08f3edb0f8e2a68ba5745fa36565b3e6aa0f316382bcfe15ba6",
        accepted: false,
        rejected_target: Some((112, 124)),
    },
];

// Expected values for the standard chart context.
// There is no compiler option to change chart type; theme options change only
// colors. In the default chart context, chart.is_standard is true and the other predicates are false.
const PREDICATES: &[(&str, bool)] = &[
    ("chart.is_standard", true),
    ("chart.is_heikinashi", false),
    ("chart.is_kagi", false),
    ("chart.is_linebreak", false),
    ("chart.is_pnf", false),
    ("chart.is_range", false),
    ("chart.is_renko", false),
];

fn source(body: &str) -> String {
    format!("//@version=6\nindicator(\"Chart context history safeguards\")\n{body}\n")
}

fn market() -> MarketData {
    MarketData {
        open: vec![1.0, 2.0, 1.0, 2.0],
        high: vec![3.0; 4],
        low: vec![0.0; 4],
        close: vec![2.0, 1.0, 2.0, 1.0],
        volume: vec![1.0; 4],
        mintick: 0.01,
        ..MarketData::default()
    }
}

fn assert_run(result: &RunResult, expected: &[Vec<f64>], id: &str) {
    assert!(
        result.runtime_errors.is_empty(),
        "{id}: {:?}",
        result.runtime_errors
    );
    assert_eq!(result.bars_executed, 4, "{id}");
    assert_eq!(result.plots.len(), expected.len(), "{id}");
    for (plot, values) in result.plots.iter().zip(expected) {
        assert_eq!(plot.values.len(), values.len(), "{id}");
        for (actual, expected) in plot.values.iter().zip(values) {
            let actual = match actual {
                Value::Float(value) => *value,
                Value::Int(value) => *value as f64,
                other => panic!("{id}: expected numeric plot value, got {other:?}"),
            };
            assert!(
                (actual - expected).abs() < 1e-12,
                "{id}: {actual} != {expected}"
            );
        }
    }
}

fn expected_history(value: bool, offset: usize) -> Vec<f64> {
    // The VM coerces absent bool history to false, so bars before the offset read false.
    (0..4)
        .map(|bar| if value && bar >= offset { 1.0 } else { 0.0 })
        .collect()
}

#[test]
fn exact_chart_context_admissions_and_negative_spans_are_preserved() {
    let mut hashes = BTreeSet::new();
    let mut accepted = 0;
    for case in ADMISSION_CASES {
        let actual_hash = Sha256::digest(case.source.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(
            actual_hash, case.source_sha256,
            "{} source changed",
            case.id
        );
        assert!(hashes.insert(actual_hash), "duplicate source: {}", case.id);
        let result = compile(case.source, CompileOptions::default());
        assert_eq!(
            result.success, case.accepted,
            "{}: {:?}",
            case.id, result.diagnostics
        );
        if let Some((start, end)) = case.rejected_target {
            assert!(
                result.diagnostics.iter().any(|diagnostic| {
                    diagnostic.code.as_deref() == Some("E0209")
                        && diagnostic.labels.iter().any(|label| {
                            label.span.source == SourceId(0)
                                && label.span.start == start
                                && label.span.end == end
                        })
                }),
                "{}: {:?}",
                case.id,
                result.diagnostics
            );
        } else {
            accepted += 1;
            assert!(result.program.is_some(), "{}", case.id);
        }
    }
    assert_eq!(hashes.len(), 25);
    assert_eq!(accepted, 15);
}

#[test]
fn chart_type_predicates_stay_simple_and_chart_colors_stay_input() {
    let registry = BuiltinRegistry::standard_ref();
    for &(name, expected) in PREDICATES {
        let value = registry.lookup_constant(name).unwrap();
        assert_eq!(value.ty, Type::Bool, "{name}");
        assert_eq!(value.bool_value, Some(expected), "{name}");
        assert_eq!(value.qualifier(), Qualifier::Simple, "{name}");
    }
    for name in ["chart.bg_color", "chart.fg_color"] {
        let value = registry.lookup_constant(name).unwrap();
        assert_eq!(value.ty, Type::Color, "{name}");
        assert_eq!(value.qualifier(), Qualifier::Input, "{name}");
    }
    assert_eq!(
        registry.lookup_constant("color.red").unwrap().qualifier(),
        Qualifier::Const
    );
}

#[test]
fn all_seven_static_context_histories_keep_local_boolean_warmup() {
    for &(name, value) in PREDICATES {
        let text = source(&format!(
            "plot({name}[0] ? 1 : 0)\nplot({name}[1] ? 1 : 0)\nplot({name}[2] ? 1 : 0)"
        ));
        let compiled = compile(&text, CompileOptions::default());
        assert!(compiled.success, "{name}: {:?}", compiled.diagnostics);
        assert_run(
            &run_program(
                &compiled.program.unwrap(),
                &market(),
                &RunOptions::default(),
            ),
            &[
                expected_history(value, 0),
                expected_history(value, 1),
                expected_history(value, 2),
            ],
            name,
        );
    }
}

#[test]
fn true_and_false_context_dynamic_history_respects_each_input_override() {
    for &(name, value) in &PREDICATES[..2] {
        let text = source(&format!("lag = input.int(1)\nplot({name}[lag] ? 1 : 0)"));
        let compiled = compile(&text, CompileOptions::default());
        assert!(compiled.success, "{name}: {:?}", compiled.diagnostics);
        let program = compiled.program.unwrap();
        assert_eq!(program.inputs.len(), 1);
        for (lag, offset) in [(0, 0), (1, 1), (2, 2)] {
            let mut options = RunOptions::default();
            options
                .inputs
                .insert(program.inputs[0].id, InputValue::Int(lag));
            assert_run(
                &run_program(&program, &market(), &options),
                &[expected_history(value, offset)],
                name,
            );
        }
    }
}

#[test]
fn even_zero_context_history_is_series_while_plain_reads_remain_simple() {
    for &(name, _) in PREDICATES {
        let plain = source(&format!(
            "keep(simple bool value) => value\nplot(keep({name}) ? 1 : 0)"
        ));
        let compiled = compile(&plain, CompileOptions::default());
        assert!(compiled.success, "{name}: {:?}", compiled.diagnostics);
        for offset in ["0", "1", "lag"] {
            let text = source(&format!(
                "lag = input.int(0)\nkeep(simple bool value) => value\nplot(keep({name}[{offset}]) ? 1 : 0)"
            ));
            let result = compile(&text, CompileOptions::default());
            assert!(
                !result.success,
                "{name}[{offset}] lost its Series qualifier"
            );
            assert!(
                result.diagnostics.iter().any(|diagnostic| {
                    diagnostic.code.as_deref() == Some("E0206")
                        && diagnostic.message.contains("found series")
                }),
                "{name}[{offset}]: {:?}",
                result.diagnostics
            );
        }
    }
}

#[test]
fn literal_color_enum_and_object_field_history_restrictions_are_not_relaxed() {
    // Local policy checks, separate from the admission cases above.
    for target in [
        "true",
        "1.0",
        "\"literal\"",
        "#123456",
        "math.pi",
        "color.red",
        "chart.bg_color",
        "chart.fg_color",
        "Choice.first",
        "p.x",
        "text.format_bold",
    ] {
        for offset in ["0", "1", "lag"] {
            let text = source(&format!(
                "enum Choice\n    first\ntype Point\n    float x\np = Point.new(close)\nlag = input.int(1)\nvalue = {target}[{offset}]\nplot(1)"
            ));
            let result = compile(&text, CompileOptions::default());
            assert!(!result.success, "unexpected admission: {target}[{offset}]");
            assert!(
                result
                    .diagnostics
                    .iter()
                    .any(|diagnostic| { diagnostic.code.as_deref() == Some("E0209") }),
                "{target}[{offset}]: {:?}",
                result.diagnostics
            );
        }
    }
}

#[test]
fn themed_chart_colors_keep_their_current_values_and_direct_history_rejection() {
    for options in [
        CompileOptions::default(),
        CompileOptions {
            chart_background: Some([16, 17, 18]),
            chart_foreground: Some([210, 211, 212]),
            ..CompileOptions::default()
        },
    ] {
        let expected = if options.chart_background.is_some() {
            [16.0, 211.0]
        } else {
            [255.0, 0.0]
        };
        let plain = compile(&source("plot(color.r(chart.bg_color))\nplot(color.g(chart.fg_color))\nplot(chart.is_standard ? 1 : 0)\nplot(chart.is_heikinashi ? 1 : 0)"), options);
        assert!(plain.success, "{:?}", plain.diagnostics);
        assert_run(
            &run_program(&plain.program.unwrap(), &market(), &RunOptions::default()),
            &[
                vec![expected[0]; 4],
                vec![expected[1]; 4],
                vec![1.0; 4],
                vec![0.0; 4],
            ],
            "theme values",
        );
        for name in ["chart.bg_color", "chart.fg_color"] {
            for offset in ["0", "1", "lag"] {
                let text = source(&format!(
                    "lag = input.int(1)\nplot(color.r({name}[{offset}]))"
                ));
                let result = compile(&text, options);
                assert!(
                    !result.success,
                    "{name}[{offset}] was admitted under theme options"
                );
                assert!(
                    result
                        .diagnostics
                        .iter()
                        .any(|d| d.code.as_deref() == Some("E0209")),
                    "{:?}",
                    result.diagnostics
                );
            }
        }
    }
}

#[test]
fn positive_and_dynamic_context_history_keep_the_loop_guard() {
    for offset in ["1", "lag"] {
        let text = source(&format!(
            "lag = input.int(1)\nfor i = 0 to 1\n    value = chart.is_standard[{offset}]\nplot(1)"
        ));
        let result = compile(&text, CompileOptions::default());
        assert!(!result.success);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.code.as_deref() == Some("E0222")),
            "{:?}",
            result.diagnostics
        );
    }
    // Offset zero is a current context read and allocates no history slot.
    let text = source("bool value = false\nfor i = 0 to 1\n    value := chart.is_standard[0]\nplot(value ? 1 : 0)");
    let compiled = compile(&text, CompileOptions::default());
    assert!(compiled.success, "{:?}", compiled.diagnostics);
    assert_run(
        &run_program(
            &compiled.program.unwrap(),
            &market(),
            &RunOptions::default(),
        ),
        &[vec![1.0; 4]],
        "zero in loop",
    );
}

#[test]
fn context_history_keeps_series_branch_and_var_initializer_scheduling_warnings() {
    for offset in ["1", "lag"] {
        let changing = source(&format!("lag = input.int(1)\nbool value = false\nif close > open\n    value := chart.is_standard[{offset}]\nplot(value ? 1 : 0)"));
        let compiled = compile(&changing, CompileOptions::default());
        assert!(compiled.success, "{:?}", compiled.diagnostics);
        assert_eq!(
            compiled
                .diagnostics
                .iter()
                .filter(|d| d.code.as_deref() == Some("W0004"))
                .count(),
            1,
            "{:?}",
            compiled.diagnostics
        );
        let stable = source(&format!("lag = input.int(1)\nenabled = input.bool(true)\nbool value = false\nif enabled\n    value := chart.is_standard[{offset}]\nplot(value ? 1 : 0)"));
        let compiled = compile(&stable, CompileOptions::default());
        assert!(compiled.success, "{:?}", compiled.diagnostics);
        assert!(
            !compiled
                .diagnostics
                .iter()
                .any(|d| d.code.as_deref() == Some("W0004")),
            "{:?}",
            compiled.diagnostics
        );
    }
    let compiled = compile(
        &source("var value = chart.is_standard[1]\nplot(value ? 1 : 0)"),
        CompileOptions::default(),
    );
    assert!(compiled.success, "{:?}", compiled.diagnostics);
    assert_eq!(
        compiled
            .diagnostics
            .iter()
            .filter(|d| d.code.as_deref() == Some("W0004"))
            .count(),
        1,
        "{:?}",
        compiled.diagnostics
    );
}

#[test]
fn context_history_keeps_literal_ceilings_and_dynamic_buffer_errors() {
    // A literal offset on a context value is a buffer request like any
    // other: it lifts a smaller declared ceiling, and past the hard limit it
    // is refused.
    let literal_request = "//@version=6\nindicator(\"Context history ceiling\", max_bars_back=1)\nplot(chart.is_standard[2] ? 1 : 0)\n";
    let compiled = compile(literal_request, CompileOptions::default());
    assert!(compiled.success, "{:?}", compiled.diagnostics);
    assert_eq!(compiled.program.unwrap().metadata.max_bars_back, 2);

    let static_limit = "//@version=6\nindicator(\"Context history limit\")\nplot(chart.is_standard[5001] ? 1 : 0)\n";
    let result = compile(static_limit, CompileOptions::default());
    assert!(!result.success);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0233")),
        "{:?}",
        result.diagnostics
    );
    let dynamic_limit = "//@version=6\nindicator(\"Dynamic context ceiling\", max_bars_back=1)\nlag = input.int(1)\nplot(chart.is_standard[lag] ? 1 : 0)\n";
    let compiled = compile(dynamic_limit, CompileOptions::default());
    assert!(compiled.success, "{:?}", compiled.diagnostics);
    let program = compiled.program.unwrap();
    let mut options = RunOptions::default();
    options
        .inputs
        .insert(program.inputs[0].id, InputValue::Int(2));
    let result = run_program(&program, &market(), &options);
    assert_eq!(result.bars_executed, 0);
    assert_eq!(result.runtime_errors.len(), 1);
    assert_eq!(
        result.runtime_errors[0].kind,
        RuntimeErrorKind::HistoryBufferLimitExceeded
    );
    assert_eq!(result.runtime_errors[0].bar_index, Some(0));
    let bad_offset = compile(
        &source("plot(chart.is_standard[\"bad\"] ? 1 : 0)"),
        CompileOptions::default(),
    );
    assert!(!bad_offset.success);
    assert!(
        bad_offset
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0210")),
        "{:?}",
        bad_offset.diagnostics
    );
}

#[test]
fn chart_color_history_label_does_not_call_the_input_qualified_color_a_const() {
    let result = compile(
        &source("plot(chart.bg_color[1])"),
        CompileOptions::default(),
    );
    assert!(!result.success);
    let diagnostic = result
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code.as_deref() == Some("E0209"))
        .expect("chart color history should report E0209");
    assert_eq!(
        diagnostic.labels[0].message,
        "`chart.bg_color` is a built-in value"
    );
}
