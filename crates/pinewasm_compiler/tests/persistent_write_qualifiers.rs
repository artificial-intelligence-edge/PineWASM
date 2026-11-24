//! Regression tests for persistent-write admission. Admission and
//! error expectations are kept separate from local runtime safeguards.

use pinewasm_compiler::{
    compile, compile_and_run, CompileOptions, InputActiveExpr, InputValue, MarketData, RunOptions,
    Value,
};

struct AdmissionCase {
    id: &'static str,
    source: &'static str,
    accepted: bool,
    qualifier: Option<&'static str>,
    argument_start: Option<u32>,
}

const ADMISSION_CASES: &[AdmissionCase] = &[
    AdmissionCase {
        id: "persistent-write-admission/var_unwritten_bool_const",
        source: r#"//@version=6
indicator("Persistent qualifier discriminator")
var bool flag = true
plot(flag ? 1 : 0, force_overlay=flag)
"#,
        accepted: true,
        qualifier: None,
        argument_start: None,
    },
    AdmissionCase {
        id: "persistent-write-admission/varip_unwritten_bool_const",
        source: r#"//@version=6
indicator("Persistent qualifier discriminator")
varip bool flag = true
plot(flag ? 1 : 0, force_overlay=flag)
"#,
        accepted: true,
        qualifier: None,
        argument_start: None,
    },
    AdmissionCase {
        id: "persistent-write-admission/var_unwritten_int_simple",
        source: r#"//@version=6
indicator("Persistent qualifier discriminator")
var int shift = 1
plot(close, offset=shift)
"#,
        accepted: true,
        qualifier: None,
        argument_start: None,
    },
    AdmissionCase {
        id: "persistent-write-admission/varip_unwritten_int_simple",
        source: r#"//@version=6
indicator("Persistent qualifier discriminator")
varip int shift = 1
plot(close, offset=shift)
"#,
        accepted: true,
        qualifier: None,
        argument_start: None,
    },
    AdmissionCase {
        id: "persistent-write-admission/var_overwrite_literal",
        source: r#"//@version=6
indicator("Persistent qualifier discriminator")
var int shift = 1
shift := 2
plot(close, offset=shift)
"#,
        accepted: true,
        qualifier: None,
        argument_start: None,
    },
    AdmissionCase {
        id: "persistent-write-admission/varip_overwrite_literal",
        source: r#"//@version=6
indicator("Persistent qualifier discriminator")
varip int shift = 1
shift := 2
plot(close, offset=shift)
"#,
        accepted: true,
        qualifier: None,
        argument_start: None,
    },
    AdmissionCase {
        id: "persistent-write-admission/var_overwrite_input",
        source: r#"//@version=6
indicator("Persistent qualifier discriminator")
replacement = input.int(2)
var int shift = 1
shift := replacement
plot(close, offset=shift)
"#,
        accepted: true,
        qualifier: None,
        argument_start: None,
    },
    AdmissionCase {
        id: "persistent-write-admission/varip_overwrite_input",
        source: r#"//@version=6
indicator("Persistent qualifier discriminator")
replacement = input.int(2)
varip int shift = 1
shift := replacement
plot(close, offset=shift)
"#,
        accepted: true,
        qualifier: None,
        argument_start: None,
    },
    AdmissionCase {
        id: "persistent-write-admission/var_overwrite_simple",
        source: r#"//@version=6
indicator("Persistent qualifier discriminator")
simple int replacement = 2
var int shift = 1
shift := replacement
plot(close, offset=shift)
"#,
        accepted: true,
        qualifier: None,
        argument_start: None,
    },
    AdmissionCase {
        id: "persistent-write-admission/varip_overwrite_simple",
        source: r#"//@version=6
indicator("Persistent qualifier discriminator")
simple int replacement = 2
varip int shift = 1
shift := replacement
plot(close, offset=shift)
"#,
        accepted: true,
        qualifier: None,
        argument_start: None,
    },
    AdmissionCase {
        id: "persistent-write-admission/var_series_rhs",
        source: r#"//@version=6
indicator("Persistent qualifier discriminator")
var int shift = 1
shift := int(close)
plot(close, offset=shift)
"#,
        accepted: false,
        qualifier: Some("series"),
        argument_start: Some(118),
    },
    AdmissionCase {
        id: "persistent-write-admission/varip_series_rhs",
        source: r#"//@version=6
indicator("Persistent qualifier discriminator")
varip int shift = 1
shift := int(close)
plot(close, offset=shift)
"#,
        accepted: false,
        qualifier: Some("series"),
        argument_start: Some(120),
    },
    AdmissionCase {
        id: "persistent-write-admission/var_self_recurrence",
        source: r#"//@version=6
indicator("Persistent qualifier discriminator")
var int shift = 1
shift += 1
plot(close, offset=shift)
"#,
        accepted: false,
        qualifier: Some("series"),
        argument_start: Some(109),
    },
    AdmissionCase {
        id: "persistent-write-admission/varip_self_recurrence",
        source: r#"//@version=6
indicator("Persistent qualifier discriminator")
varip int shift = 1
shift += 1
plot(close, offset=shift)
"#,
        accepted: false,
        qualifier: Some("series"),
        argument_start: Some(111),
    },
    AdmissionCase {
        id: "persistent-write-admission/var_history_rhs",
        source: r#"//@version=6
indicator("Persistent qualifier discriminator")
var int shift = 1
shift := nz(shift[1], 1) + 1
plot(close, offset=shift)
"#,
        accepted: false,
        qualifier: Some("series"),
        argument_start: Some(127),
    },
    AdmissionCase {
        id: "persistent-write-admission/var_input_initial_self_recurrence",
        source: r#"//@version=6
indicator("Persistent qualifier discriminator")
var int shift = input.int(1)
shift += 1
plot(close, offset=shift)
"#,
        accepted: false,
        qualifier: Some("series"),
        argument_start: Some(120),
    },
    AdmissionCase {
        id: "persistent-write-admission/var_cross_persistent_cycle",
        source: r#"//@version=6
indicator("Persistent qualifier discriminator")
var int a = 1
var int b = 2
a := b
b := a + 1
plot(close, offset=a)
"#,
        accepted: false,
        qualifier: Some("series"),
        argument_start: Some(126),
    },
    AdmissionCase {
        id: "persistent-write-admission/var_input_controlled_write",
        source: r#"//@version=6
indicator("Persistent qualifier discriminator")
condition = input.bool(true)
var int shift = 1
if condition
    shift := 2
plot(close, offset=shift)
"#,
        accepted: true,
        qualifier: None,
        argument_start: None,
    },
    AdmissionCase {
        id: "persistent-write-admission/var_simple_controlled_write",
        source: r#"//@version=6
indicator("Persistent qualifier discriminator")
simple bool condition = true
var int shift = 1
if condition
    shift := 2
plot(close, offset=shift)
"#,
        accepted: true,
        qualifier: None,
        argument_start: None,
    },
    AdmissionCase {
        id: "persistent-write-admission/var_series_controlled_write",
        source: r#"//@version=6
indicator("Persistent qualifier discriminator")
condition = close > open
var int shift = 1
if condition
    shift := 2
plot(close, offset=shift)
"#,
        accepted: false,
        qualifier: Some("series"),
        argument_start: Some(151),
    },
    AdmissionCase {
        id: "persistent-write-admission/var_series_then_constant_reset",
        source: r#"//@version=6
indicator("Persistent qualifier discriminator")
var int shift = 1
shift := int(close)
shift := 2
plot(close, offset=shift)
"#,
        accepted: false,
        qualifier: Some("series"),
        argument_start: Some(129),
    },
    AdmissionCase {
        id: "persistent-write-admission/var_two_offsets_around_write",
        source: r#"//@version=6
indicator("Persistent qualifier discriminator")
var int shift = 1
plot(close, offset=shift)
shift := 2
plot(open, offset=shift)
"#,
        accepted: false,
        qualifier: Some("series"),
        argument_start: Some(134),
    },
    AdmissionCase {
        id: "persistent-write-admission/var_const_consumer_before_write",
        source: r#"//@version=6
indicator("Persistent qualifier discriminator")
var bool flag = true
plot(flag ? 1 : 0, force_overlay=flag)
flag := false
plot(flag ? 1 : 0)
"#,
        accepted: false,
        qualifier: Some("series"),
        argument_start: Some(115),
    },
    AdmissionCase {
        id: "persistent-write-admission/varip_const_consumer_before_write",
        source: r#"//@version=6
indicator("Persistent qualifier discriminator")
varip bool flag = true
plot(flag ? 1 : 0, force_overlay=flag)
flag := false
plot(flag ? 1 : 0)
"#,
        accepted: false,
        qualifier: Some("series"),
        argument_start: Some(117),
    },
    AdmissionCase {
        id: "persistent-write-admission/ordinary_early_const_then_input",
        source: r#"//@version=6
indicator("Persistent qualifier addendum")
flag = true
plot(close, force_overlay=flag)
flag := input.bool(false)
plot(flag ? 1 : 0)
"#,
        accepted: true,
        qualifier: None,
        argument_start: None,
    },
    AdmissionCase {
        id: "persistent-write-admission/ordinary_early_const_then_series",
        source: r#"//@version=6
indicator("Persistent qualifier addendum")
flag = true
plot(close, force_overlay=flag)
flag := close > open
plot(flag ? 1 : 0)
"#,
        accepted: true,
        qualifier: None,
        argument_start: None,
    },
    AdmissionCase {
        id: "persistent-write-admission/var_one_time_initializer",
        source: r#"//@version=6
indicator("Persistent qualifier addendum")
var int a = 1
var int b = a
a := 2
plot(close, offset=b)
"#,
        accepted: false,
        qualifier: Some("series"),
        argument_start: Some(110),
    },
    AdmissionCase {
        id: "persistent-write-admission/varip_one_time_initializer",
        source: r#"//@version=6
indicator("Persistent qualifier addendum")
varip int a = 1
varip int b = a
a := 2
plot(close, offset=b)
"#,
        accepted: false,
        qualifier: Some("series"),
        argument_start: Some(114),
    },
    AdmissionCase {
        id: "persistent-write-admission/var_kill_read_future_series",
        source: r#"//@version=6
indicator("Persistent qualifier addendum")
var int shift = 1
shift := 2
plot(close, offset=shift)
shift := int(close)
plot(shift)
"#,
        accepted: false,
        qualifier: Some("series"),
        argument_start: Some(104),
    },
    AdmissionCase {
        id: "persistent-write-admission/varip_kill_read_future_series",
        source: r#"//@version=6
indicator("Persistent qualifier addendum")
varip int shift = 1
shift := 2
plot(close, offset=shift)
shift := int(close)
plot(shift)
"#,
        accepted: false,
        qualifier: Some("series"),
        argument_start: Some(106),
    },
    AdmissionCase {
        id: "persistent-write-admission/var_two_stable_call_captures",
        source: r#"//@version=6
indicator("Persistent qualifier addendum")
var int shift = 1
shift := 2
plot(close, offset=shift)
shift := 3
plot(open, offset=shift)
"#,
        accepted: false,
        qualifier: Some("series"),
        argument_start: Some(140),
    },
    AdmissionCase {
        id: "persistent-write-admission/varip_two_stable_call_captures",
        source: r#"//@version=6
indicator("Persistent qualifier addendum")
varip int shift = 1
shift := 2
plot(close, offset=shift)
shift := 3
plot(open, offset=shift)
"#,
        accepted: false,
        qualifier: Some("series"),
        argument_start: Some(142),
    },
    AdmissionCase {
        id: "persistent-write-admission/var_early_simple_no_later_consumer",
        source: r#"//@version=6
indicator("Persistent early-read discriminator")
var int shift = 1
plot(close, offset=shift)
shift := 2
"#,
        accepted: false,
        qualifier: Some("series"),
        argument_start: Some(99),
    },
    AdmissionCase {
        id: "persistent-write-admission/varip_early_simple_no_later_consumer",
        source: r#"//@version=6
indicator("Persistent early-read discriminator")
varip int shift = 1
plot(close, offset=shift)
shift := 2
"#,
        accepted: false,
        qualifier: Some("series"),
        argument_start: Some(101),
    },
    AdmissionCase {
        id: "persistent-write-admission/var_kill_read_later_literal_no_second_consumer",
        source: r#"//@version=6
indicator("Persistent early-read discriminator")
var int shift = 1
shift := 2
plot(close, offset=shift)
shift := 3
"#,
        accepted: false,
        qualifier: Some("series"),
        argument_start: Some(110),
    },
    AdmissionCase {
        id: "persistent-write-admission/varip_kill_read_later_literal_no_second_consumer",
        source: r#"//@version=6
indicator("Persistent early-read discriminator")
varip int shift = 1
shift := 2
plot(close, offset=shift)
shift := 3
"#,
        accepted: false,
        qualifier: Some("series"),
        argument_start: Some(112),
    },
    AdmissionCase {
        id: "persistent-write-admission/var_two_writes_before_read",
        source: r#"//@version=6
indicator("Persistent early-read discriminator")
var int shift = 1
shift := 2
shift := 3
plot(close, offset=shift)
"#,
        accepted: true,
        qualifier: None,
        argument_start: None,
    },
    AdmissionCase {
        id: "persistent-write-admission/varip_two_writes_before_read",
        source: r#"//@version=6
indicator("Persistent early-read discriminator")
varip int shift = 1
shift := 2
shift := 3
plot(close, offset=shift)
"#,
        accepted: true,
        qualifier: None,
        argument_start: None,
    },
    AdmissionCase {
        id: "persistent-write-admission/var_early_const_then_true_write",
        source: r#"//@version=6
indicator("Persistent early-read discriminator")
var bool flag = true
plot(close, force_overlay=flag)
if true
    flag := false
plot(close)
"#,
        accepted: false,
        qualifier: Some("series"),
        argument_start: Some(109),
    },
    AdmissionCase {
        id: "persistent-write-admission/var_early_const_then_false_write",
        source: r#"//@version=6
indicator("Persistent early-read discriminator")
var bool flag = true
plot(close, force_overlay=flag)
if false
    flag := false
plot(close)
"#,
        accepted: true,
        qualifier: None,
        argument_start: None,
    },
];

#[test]
fn exact_persistent_admissions_and_named_qualifiers() {
    assert_eq!(ADMISSION_CASES.len(), 40);
    for case in ADMISSION_CASES {
        let result = compile(case.source, CompileOptions::default());
        assert_eq!(
            result.success, case.accepted,
            "{}: {:?}",
            case.id, result.diagnostics
        );
        if let Some(qualifier) = case.qualifier {
            assert!(
                result
                    .diagnostics
                    .iter()
                    .any(|d| d.code.as_deref() == Some("E0206")
                        && d.message.ends_with(&format!("found {qualifier}"))
                        && d.labels
                            .iter()
                            .any(|label| Some(label.span.start) == case.argument_start)),
                "{}: expected the argument token/qualifier: {:?}",
                case.id,
                result.diagnostics
            );
        }
    }
}

#[test]
fn adjacent_member_reference_and_persistent_controls_keep_their_admissions() {
    // Bool self-toggle diagnostics are an open case: do not reinterpret the
    // Simple-qualifier diagnostic as a general license for recurrent state.
    let cases: &[(&str, &str, bool, Option<&str>)] = &[
        (
            "udt_field_direct",
            r#"//@version=6
indicator("Adjacent qualifier port")
type Cfg
    bool flag
c = Cfg.new(true)
value = c.flag
plot(value ? 1 : 0, force_overlay=value)
"#,
            false,
            Some("series"),
        ),
        (
            "udt_field_method",
            r#"//@version=6
indicator("Adjacent qualifier port")
type Cfg
    bool flag
method getFlag(Cfg self) => self.flag
c = Cfg.new(true)
value = c.getFlag()
plot(value ? 1 : 0, force_overlay=value)
"#,
            false,
            Some("series"),
        ),
        (
            "udt_constructor_field",
            r#"//@version=6
indicator("Adjacent qualifier port")
type Cfg
    bool flag
value = Cfg.new(true).flag
plot(value ? 1 : 0, force_overlay=value)
"#,
            false,
            Some("series"),
        ),
        (
            "array_get",
            r#"//@version=6
indicator("Adjacent qualifier port")
flags = array.from(true)
value = array.get(flags, 0)
plot(value ? 1 : 0, force_overlay=value)
"#,
            false,
            Some("series"),
        ),
        (
            "const_array_get",
            r#"//@version=6
indicator("Adjacent qualifier port")
const array<bool> flags = array.from(true)
value = array.get(flags, 0)
plot(value ? 1 : 0, force_overlay=value)
"#,
            false,
            Some("series"),
        ),
        (
            "array_size",
            r#"//@version=6
indicator("Adjacent qualifier port")
flags = array.from(true)
value = array.size(flags) > 0
plot(value ? 1 : 0, force_overlay=value)
"#,
            false,
            Some("series"),
        ),
        (
            "var_literal_write",
            r#"//@version=6
indicator("Adjacent qualifier port")
var bool value = true
value := false
plot(value ? 1 : 0, force_overlay=value)
"#,
            false,
            Some("simple"),
        ),
        (
            "var_toggle",
            r#"//@version=6
indicator("Adjacent qualifier port")
var bool value = true
value := not value
plot(value ? 1 : 0, force_overlay=value)
"#,
            false,
            None,
        ),
        (
            "varip_literal_write",
            r#"//@version=6
indicator("Adjacent qualifier port")
varip bool value = true
value := false
plot(value ? 1 : 0, force_overlay=value)
"#,
            false,
            Some("simple"),
        ),
        (
            "varip_toggle",
            r#"//@version=6
indicator("Adjacent qualifier port")
varip bool value = true
value := not value
plot(value ? 1 : 0, force_overlay=value)
"#,
            false,
            None,
        ),
        (
            "var_literal_simple_sink",
            r#"//@version=6
indicator("Adjacent qualifier port")
var int shift = 1
shift := 2
plot(close, offset=shift)
"#,
            true,
            None,
        ),
        (
            "varip_literal_simple_sink",
            r#"//@version=6
indicator("Adjacent qualifier port")
varip int shift = 1
shift := 2
plot(close, offset=shift)
"#,
            true,
            None,
        ),
    ];
    for (id, source, accepted, qualifier) in cases {
        let result = compile(source, CompileOptions::default());
        assert_eq!(result.success, *accepted, "{id}: {:?}", result.diagnostics);
        if let Some(qualifier) = qualifier {
            assert!(
                result
                    .diagnostics
                    .iter()
                    .any(|d| d.message.ends_with(&format!("found {qualifier}"))),
                "{id}: {:?}",
                result.diagnostics
            );
        }
    }
}

#[test]
fn closed_guard_and_lexical_safeguards_keep_safe_routes_and_reject_carried_reads() {
    // Local source/runtime safeguards.
    let cases: &[(&str, &str, bool)] = &[
        (
            "const_false_alias",
            r#"//@version=6
indicator("Local qualifier safeguard")
const bool NEVER = false
var bool flag = true
plot(close, force_overlay=flag)
if NEVER
    flag := false
plot(flag ? 1 : 0)
"#,
            true,
        ),
        (
            "ordinary_false_alias",
            r#"//@version=6
indicator("Local qualifier safeguard")
never = false
var bool flag = true
plot(close, force_overlay=flag)
if never
    flag := false
plot(flag ? 1 : 0)
"#,
            true,
        ),
        (
            "not_true_guard",
            r#"//@version=6
indicator("Local qualifier safeguard")
var bool flag = true
plot(close, force_overlay=flag)
if not true
    flag := false
plot(flag ? 1 : 0)
"#,
            true,
        ),
        (
            "literal_comparison_false",
            r#"//@version=6
indicator("Local qualifier safeguard")
var bool flag = true
plot(close, force_overlay=flag)
if 2 > 3
    flag := false
plot(flag ? 1 : 0)
"#,
            true,
        ),
        (
            "const_comparison_aliases",
            r#"//@version=6
indicator("Local qualifier safeguard")
const int LEFT = 2
const int RIGHT = 3
var bool flag = true
plot(close, force_overlay=flag)
if LEFT > RIGHT
    flag := false
plot(flag ? 1 : 0)
"#,
            true,
        ),
        (
            "lazy_false_series_rhs",
            r#"//@version=6
indicator("Local qualifier safeguard")
var bool flag = true
plot(close, force_overlay=flag)
if false and close > open
    flag := false
plot(flag ? 1 : 0)
"#,
            true,
        ),
        (
            "mutable_persistent_guard",
            r#"//@version=6
indicator("Local qualifier safeguard")
var bool gate = false
var bool flag = true
plot(close, force_overlay=flag)
if gate
    flag := false
gate := true
plot(flag ? 1 : 0)
"#,
            false,
        ),
        (
            "alias_of_mutable_persistent_guard",
            r#"//@version=6
indicator("Local qualifier safeguard")
var bool gate = false
condition = gate
var bool flag = true
plot(close, force_overlay=flag)
if condition
    flag := false
gate := true
plot(flag ? 1 : 0)
"#,
            false,
        ),
        (
            "unused_parameter_shadow",
            r#"//@version=6
indicator("Local qualifier safeguard")
var bool flag = true
unused(bool flag) =>
    flag := not flag
    flag
plot(close, force_overlay=flag)
plot(flag ? 1 : 0)
"#,
            true,
        ),
        (
            "unused_persistent_local_shadow",
            r#"//@version=6
indicator("Local qualifier safeguard")
var bool flag = true
unused() =>
    var bool flag = true
    flag := false
    flag
plot(close, force_overlay=flag)
plot(flag ? 1 : 0)
"#,
            true,
        ),
        (
            "untouched_persistent_alias",
            r#"//@version=6
indicator("Local qualifier safeguard")
var bool flag = true
held = flag
plot(close, force_overlay=held)
plot(held ? 1 : 0)
"#,
            true,
        ),
        (
            "ordinary_early_alias_future_series",
            r#"//@version=6
indicator("Local qualifier safeguard")
flag = true
held = flag
plot(close, force_overlay=held)
flag := close > open
plot(flag ? 1 : 0)
"#,
            true,
        ),
    ];
    for (id, source, accepted) in cases {
        let result = compile(source, CompileOptions::default());
        assert_eq!(result.success, *accepted, "{id}: {:?}", result.diagnostics);
    }
}

fn market() -> MarketData {
    MarketData {
        open: vec![1.0, 2.0, 1.0, 2.0],
        high: vec![3.0; 4],
        low: vec![0.0; 4],
        close: vec![2.0, 1.0, 2.0, 1.0],
        volume: vec![1.0; 4],
        ..MarketData::default()
    }
}

fn assert_offset(source: &str, options: &RunOptions, expected: i64) {
    let run = compile_and_run(source, &market(), CompileOptions::default(), options);
    assert!(
        run.diagnostics.iter().all(|d| !d.is_error()),
        "{:?}",
        run.diagnostics
    );
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    assert_eq!(run.bars_executed, 4);
    let plot = run.plots.first().expect("one output");
    assert_eq!(plot.values.len(), 4);
    assert!(
        plot.style.runtime_scalars.is_empty(),
        "runtime slots must not escape the engine"
    );
    let offset = plot
        .style
        .bound_scalars
        .iter()
        .find(|binding| binding.field == "offset")
        .expect("resolved offset");
    assert!(
        matches!(&offset.expression,
        InputActiveExpr::Value { value: InputValue::Int(value) } if *value == expected),
        "{:?}",
        offset.expression
    );
}

#[test]
fn stable_write_prefixes_resolve_actual_engine_offsets_and_input_choices() {
    for id in [
        "var_overwrite_literal",
        "varip_overwrite_literal",
        "var_overwrite_input",
        "varip_overwrite_input",
        "var_overwrite_simple",
        "varip_overwrite_simple",
        "var_input_controlled_write",
        "var_simple_controlled_write",
        "var_two_writes_before_read",
        "varip_two_writes_before_read",
    ] {
        let case = ADMISSION_CASES
            .iter()
            .find(|case| case.id.ends_with(&format!("/{id}")))
            .expect("source case");
        assert_offset(
            case.source,
            &RunOptions::default(),
            if id.contains("two_writes") { 3 } else { 2 },
        );
        if id.ends_with("overwrite_input") {
            let mut options = RunOptions::default();
            options.inputs.insert(0, InputValue::Int(7));
            assert_offset(case.source, &options, 7);
        }
        if id == "var_input_controlled_write" {
            let mut options = RunOptions::default();
            options.inputs.insert(0, InputValue::Bool(false));
            assert_offset(case.source, &options, 1);
        }
    }
}

#[test]
fn ordinary_simple_call_positions_keep_distinct_captures() {
    // A lawful ordinary neighbor, not a positive for persistent writes
    // interleaved with reads (persistent forms with a read between writes are rejected).
    let source = "//@version=6\nindicator(\"Ordinary call captures\")\nsimple int shift = 1\nplot(close, offset=shift)\nshift := 2\nplot(open, offset=shift)\nshift := 3\nplot(close, offset=shift)\n";
    let run = compile_and_run(
        source,
        &market(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        run.diagnostics.iter().all(|d| !d.is_error()),
        "{:?}",
        run.diagnostics
    );
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    assert_eq!(run.plots.len(), 3);
    for (plot, expected) in run.plots.iter().zip(1..=3) {
        assert!(plot.style.runtime_scalars.is_empty());
        assert!(plot.style.bound_scalars.iter().any(|binding| binding.field == "offset"
            && matches!(&binding.expression, InputActiveExpr::Value { value: InputValue::Int(value) } if *value == expected)),
            "{:?}", plot.style);
    }
}

#[test]
fn shadowing_loop_and_unknown_call_boundaries_remain_separate() {
    for prelude in [
        "var int shift = 1\nif true\n    var int shift = 1\n    shift := 2",
        "var int shift = 1\nfor index = 0 to 1\n    var int shift = 1\n    shift := 2",
        "var int shift = 1\nshift := 2\nunused() => shift",
    ] {
        let source = format!(
            "//@version=6\nindicator(\"Scope safeguard\")\n{prelude}\nplot(close, offset=shift)\n"
        );
        let result = compile(&source, CompileOptions::default());
        assert!(result.success, "{prelude}: {:?}", result.diagnostics);
    }
    for prelude in [
        "var int shift = 1\nfor index = 0 to 1\n    shift := 2",
        "helper() =>\n    var int local = 1\n    local := 2\n    local\nshift = helper()",
        "var int shift = 1\nshift := 2\nreadValue() => shift\nignored = readValue()",
    ] {
        let source = format!("//@version=6\nindicator(\"Conservative scope safeguard\")\n{prelude}\nplot(close, offset=shift)\n");
        let result = compile(&source, CompileOptions::default());
        assert!(!result.success, "scope unexpectedly broadened: {prelude}");
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.code.as_deref() == Some("E0206")),
            "{:?}",
            result.diagnostics
        );
    }
}

#[test]
fn closed_scalar_guards_survive_unrelated_calls_and_merge_possible_updates() {
    let safe = [
        "noop() => 0\nnever = false\nvar bool flag = true\nint ignored = noop()\nplot(close, force_overlay=flag)\nif never\n    flag := false\nplot(flag ? 1 : 0)",
        "condition = input.bool(true)\nnever = false\nif condition\n    never := false\nelse\n    never := false\nvar bool flag = true\nplot(close, force_overlay=flag)\nif never\n    flag := false\nplot(flag ? 1 : 0)",
    ];
    for body in safe {
        let source = format!("//@version=6\nindicator(\"Closed guard state\")\n{body}\n");
        let result = compile(&source, CompileOptions::default());
        assert!(result.success, "{body}: {:?}", result.diagnostics);
    }
    let changing = [
        "condition = input.bool(true)\nnever = false\nif condition\n    never := true\nvar bool flag = true\nplot(close, force_overlay=flag)\nif never\n    flag := false\nplot(flag ? 1 : 0)",
        "gate = false\nvar bool flag = true\nplot(close, force_overlay=flag)\nfor index = 0 to 1\n    if gate\n        flag := false\n    gate := true\nplot(flag ? 1 : 0)",
    ];
    for body in changing {
        let source = format!("//@version=6\nindicator(\"Changing guard state\")\n{body}\n");
        let result = compile(&source, CompileOptions::default());
        assert!(!result.success, "carried write was pruned: {body}");
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.code.as_deref() == Some("E0206")),
            "{:?}",
            result.diagnostics
        );
    }
}

#[test]
fn unsupported_closed_guards_abstain_from_new_backwards_qualification() {
    for guard in ["1 + 1 != 2", "math.max(0, 0) != 0", "bool(0)", "never()"] {
        let source = format!("//@version=6\nindicator(\"Opaque closed guard\")\nnever() => false\nvar bool flag = true\nplot(close, force_overlay=flag)\nif {guard}\n    flag := false\nplot(flag ? 1 : 0)\n");
        let result = compile(&source, CompileOptions::default());
        assert!(
            result.success,
            "new tightening for unsupported guard {guard}: {:?}",
            result.diagnostics
        );
        let run = compile_and_run(
            &source,
            &market(),
            CompileOptions::default(),
            &RunOptions::default(),
        );
        assert!(
            run.runtime_errors.is_empty(),
            "{guard}: {:?}",
            run.runtime_errors
        );
        assert!(
            run.plots
                .last()
                .expect("flag output")
                .values
                .iter()
                .all(|v| matches!(v, Value::Float(value) if *value == 1.0)),
            "{guard}: {:?}",
            run.plots
        );
    }
}

#[test]
fn stable_prefix_floor_connects_other_value_scalars_to_the_engine() {
    // Local lattice/runtime consequences for these scalar types. Expected
    // values follow from this compiler's own rules, and explicit qualifier
    // ceilings are not broadened by this rule.
    for (body, expected) in [
        ("var float value = 1.0\nvalue := 2.5\nplot(value)", 2.5),
        ("var string value = \"a\"\nvalue := \"second\"\nplot(str.length(value))", 6.0),
        ("var color value = #ff0000\nvalue := #0000ff\nplot(value == #0000ff ? 1 : 0)", 1.0),
        ("enum Choice\n    first\n    second\nvar Choice value = Choice.first\nvalue := Choice.second\nplot(value == Choice.second ? 1 : 0)", 1.0),
    ] {
        let source = format!("//@version=6\nindicator(\"Scalar prefix consequence\")\n{body}\n");
        let result = compile(&source, CompileOptions { emit_hir: true, ..CompileOptions::default() });
        assert!(result.success, "{body}: {:?}", result.diagnostics);
        assert!(result.hir_debug.as_deref().is_some_and(|hir| hir.contains("`value`: simple")), "{body}: {:?}", result.hir_debug);
        let run = compile_and_run(&source, &market(), CompileOptions::default(), &RunOptions::default());
        assert!(run.runtime_errors.is_empty(), "{body}: {:?}", run.runtime_errors);
        assert!(run.plots.first().expect("one output").values.iter().all(|v| matches!(v, Value::Float(value) if *value == expected)), "{body}: {:?}", run.plots);
    }
}

#[test]
fn separately_measured_finite_float_equality_guards_preserve_their_compile_results() {
    // Separate case set; not part of ADMISSION_CASES, and
    // not a universal rule for runtime float comparisons or mixed arithmetic.
    let cases: &[(&str, &str, bool)] = &[
        (
            "persistent_float_within_tolerance_eq",
            r#"//@version=6
indicator("Persistent tolerance guard")
var bool flag=true
plot(close, force_overlay=flag)
if 0.00000000005 == 0.0
    flag := false
plot(flag ? 1 : 0)
"#,
            false,
        ),
        (
            "persistent_float_within_tolerance_ne",
            r#"//@version=6
indicator("Persistent inverse tolerance guard")
var bool flag=true
plot(close, force_overlay=flag)
if 0.00000000005 != 0.0
    flag := false
plot(flag ? 1 : 0)
"#,
            true,
        ),
        (
            "persistent_float_eq",
            r#"//@version=6
indicator("Persistent float guard")
var bool flag=true
plot(close, force_overlay=flag)
if 1.0000000001 == 1.0
    flag := false
plot(flag ? 1 : 0)
"#,
            true,
        ),
        (
            "persistent_float_ne",
            r#"//@version=6
indicator("Persistent float guard")
var bool flag=true
plot(close, force_overlay=flag)
if 1.0000000001 != 1.0
    flag := false
plot(flag ? 1 : 0)
"#,
            false,
        ),
    ];
    for (id, source, accepted) in cases {
        let result = compile(source, CompileOptions::default());
        assert_eq!(result.success, *accepted, "{id}: {:?}", result.diagnostics);
        if !accepted {
            assert!(
                result
                    .diagnostics
                    .iter()
                    .any(|d| d.code.as_deref() == Some("E0206")
                        && d.message.ends_with("found series")),
                "{id}: {:?}",
                result.diagnostics
            );
        }
    }
}
