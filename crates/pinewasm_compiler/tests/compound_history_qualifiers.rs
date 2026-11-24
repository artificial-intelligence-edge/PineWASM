//! Compile-acceptance cases, followed by local history semantics.
//! The 26 sources are grouped in five namespaces, and each is pinned by its
//! SHA-256 digest. The tests check PineWASM's compile results and the local VM
//! values.

use pinewasm_compiler::{
    compile, run_program, CompileOptions, InputValue, MarketData, RunOptions, RunResult,
    RuntimeErrorKind, Value,
};
use sha2::{Digest, Sha256};

struct AdmissionCase {
    id: &'static str,
    source: &'static str,
    source_sha256: &'static str,
    accepted: bool,
    // Expected values assert the local VM result for this source.
    expected: Option<&'static [Option<f64>]>,
}

const ADMISSION_CASES: &[AdmissionCase] = &[
    AdmissionCase {
        id: "persistent-compound-history-admission-v1/var_float_compound_history",
        source: r#"//@version=6
indicator("Persistent compound history discriminator")
var float k = 1.0
k := 2.0
plot((k * 2)[1])
"#,
        source_sha256: "5e225d5f0d8e0d49bf1d6077f4c86bd1400ea09dadff0fe353c1f987e477be3f",
        accepted: true,
        expected: Some(&[None, Some(4.0), Some(4.0), Some(4.0)]),
    },
    AdmissionCase {
        id: "persistent-compound-history-admission-v1/varip_float_compound_history",
        source: r#"//@version=6
indicator("Persistent compound history discriminator")
varip float k = 1.0
k := 2.0
plot((k * 2)[1])
"#,
        source_sha256: "738efbe98f79241089141c06ee9e674c40bfe6e461a26e63d7fca9cbe9a51fe1",
        accepted: true,
        expected: Some(&[None, Some(4.0), Some(4.0), Some(4.0)]),
    },
    AdmissionCase {
        id: "persistent-compound-history-admission-v1/var_float_direct_history",
        source: r#"//@version=6
indicator("Persistent compound history discriminator")
var float k = 1.0
k := 2.0
plot(k[1])
"#,
        source_sha256: "6fbed7d3e37cda5821aaaf521ec67d4368d48d6eff077f3b022970cd3370a0d8",
        accepted: true,
        expected: Some(&[None, Some(2.0), Some(2.0), Some(2.0)]),
    },
    AdmissionCase {
        id: "persistent-compound-history-admission-v1/ordinary_float_compound_history",
        source: r#"//@version=6
indicator("Persistent compound history discriminator")
float k = 1.0
k := 2.0
plot((k * 2)[1])
"#,
        source_sha256: "df7dc0d974e9a68b09ffe90b4fa87b3f1445d534bb7fe15ca6e3d87f76cb75c1",
        accepted: true,
        expected: Some(&[None, Some(4.0), Some(4.0), Some(4.0)]),
    },
    AdmissionCase {
        id: "persistent-compound-history-admission-v1/constant_compound_history",
        source: r#"//@version=6
indicator("Persistent compound history discriminator")
plot((1.0 * 2)[1])
"#,
        source_sha256: "f3b2f3477b324207d8f15b51eae71304f59e1765f6ec355231347eeca5e7ea6f",
        accepted: true,
        expected: Some(&[None, Some(2.0), Some(2.0), Some(2.0)]),
    },
    AdmissionCase {
        id: "persistent-compound-history-admission-v1/var_float_compound_input_offset",
        source: r#"//@version=6
indicator("Persistent compound history discriminator")
int lag = input.int(1)
var float k = 1.0
k := 2.0
plot((k * 2)[lag])
"#,
        source_sha256: "d44d833de9aae21d3780cb70be04c257d1c951b424e563e5ecd1f0844fe83926",
        accepted: true,
        expected: Some(&[None, Some(4.0), Some(4.0), Some(4.0)]),
    },
    AdmissionCase {
        id: "compound-history-boundary-addendum-v1/direct_float_literal_history",
        source: r#"//@version=6
indicator("Compound history syntax boundary")
plot(1.0[1])
"#,
        source_sha256: "7223793202690e50c0c37740e30b9a2410e4729b6381bb1a1642e18cc2944c3d",
        accepted: false,
        expected: None,
    },
    AdmissionCase {
        id: "compound-history-boundary-addendum-v1/parenthesized_float_literal_history",
        source: r#"//@version=6
indicator("Compound history syntax boundary")
plot((1.0)[1])
"#,
        source_sha256: "66f49a802fbddc9bcea0d40c89625608e935bb32d0f241ce3576a711bf0beb4d",
        accepted: false,
        expected: None,
    },
    AdmissionCase {
        id: "compound-history-boundary-addendum-v1/const_identifier_history",
        source: r#"//@version=6
indicator("Compound history syntax boundary")
const float k = 1.0
plot(k[1])
"#,
        source_sha256: "618fbe44df2c826c946fcbb8f6780f534e8e22ba9cd9577e6a0a6fe1f631c879",
        accepted: true,
        expected: Some(&[None, Some(1.0), Some(1.0), Some(1.0)]),
    },
    AdmissionCase {
        id: "compound-history-boundary-addendum-v1/input_scalar_compound_history",
        source: r#"//@version=6
indicator("Compound history syntax boundary")
float k = input.float(1.0)
plot((k * 2)[1])
"#,
        source_sha256: "6f45737cfa51349f1e29fb53fc8683e1b3ffcd67c4aa8723b95050c9dfabe18a",
        accepted: true,
        expected: Some(&[None, Some(2.0), Some(2.0), Some(2.0)]),
    },
    AdmissionCase {
        id: "compound-history-boundary-addendum-v1/genuine_simple_compound_history",
        source: r#"//@version=6
indicator("Compound history syntax boundary")
plot((syminfo.mintick * 2)[1])
"#,
        source_sha256: "6bf7ba6f77ca064c0f077b2d61b2df21f9b8e4106850bc614953bdc370af7ecc",
        accepted: true,
        expected: Some(&[None, Some(0.02), Some(0.02), Some(0.02)]),
    },
    AdmissionCase {
        id: "compound-history-boundary-addendum-v1/constant_bool_compound_history",
        source: r#"//@version=6
indicator("Compound history syntax boundary")
plot((true and true)[1] ? 1 : 0)
"#,
        source_sha256: "837736cc1df919230e6f9653843571ba5c4f88e26fe59bd6b335df44cad67511",
        accepted: true,
        expected: Some(&[Some(0.0), Some(1.0), Some(1.0), Some(1.0)]),
    },
    AdmissionCase {
        id: "compound-history-target-shapes-v1/negative_numeric_unary_history",
        source: r#"//@version=6
indicator("History target shape boundary")
plot((-2.0)[1])
"#,
        source_sha256: "36662a87d6fd522efc0356081a3e45d9a90393ff3d0af7453df2df461be5635c",
        accepted: true,
        expected: Some(&[None, Some(-2.0), Some(-2.0), Some(-2.0)]),
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
        expected: None,
    },
    AdmissionCase {
        id: "compound-history-target-shapes-v1/grouped_builtin_color_constant_history",
        source: r#"//@version=6
indicator("History target shape boundary")
color c = (color.red)[1]
plot(c == color.red ? 1 : 0)
"#,
        source_sha256: "26d35da9f0e418ee437133ad8e8de2aead4ed5a1ca25b0da8aae4adba1ce84e0",
        accepted: false,
        expected: None,
    },
    AdmissionCase {
        id: "compound-history-target-shapes-v1/pure_float_call_history",
        source: r#"//@version=6
indicator("History target shape boundary")
plot(float(2)[1])
"#,
        source_sha256: "eadb769abeab7a143de38f529c90074ee543ad428aafcf43fa602f53fbc6032d",
        accepted: true,
        expected: Some(&[None, Some(2.0), Some(2.0), Some(2.0)]),
    },
    AdmissionCase {
        id: "compound-history-target-shapes-v1/computed_const_color_history",
        source: r#"//@version=6
indicator("History target shape boundary")
color c = color.new(color.red, 0)[1]
plot(c == color.red ? 1 : 0)
"#,
        source_sha256: "dea20f2ed24d55fbeb3a3070aa0e7091605c2b4435980d2896089fecc6c7770f",
        accepted: true,
        expected: Some(&[Some(0.0), Some(1.0), Some(1.0), Some(1.0)]),
    },
    AdmissionCase {
        id: "compound-history-target-shapes-v1/bare_genuine_simple_history",
        source: r#"//@version=6
indicator("History target shape boundary")
plot(syminfo.mintick[1])
"#,
        source_sha256: "1d6a0c45552d039a9bd9a5d2f52b7e157d3791b12d35930ff565610ebe4400c2",
        accepted: true,
        expected: Some(&[None, Some(0.01), Some(0.01), Some(0.01)]),
    },
    AdmissionCase {
        id: "compound-history-zero-offset-v1/literal_zero_offset",
        source: r#"//@version=6
indicator("Zero offset history boundary")
plot(1.0[0])
"#,
        source_sha256: "2f0a30a24ebcbfdfe15fc3ec3ef5e7447a8221b1146e157c44ca63e9ea546424",
        accepted: false,
        expected: None,
    },
    AdmissionCase {
        id: "compound-history-zero-offset-v1/builtin_color_zero_offset",
        source: r#"//@version=6
indicator("Zero offset history boundary")
plot(color.r(color.red[0]))
"#,
        source_sha256: "5ff48e6f0fd238691947e528490123c681d0e474ab754cd79ac5ca73c160f3da",
        accepted: false,
        expected: None,
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
        expected: None, // Compile-only check; raw array-size history can be missing on bar zero.
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
        expected: None, // Compile-only check; raw array-size history can be missing on bar zero.
    },
    AdmissionCase {
        id: "history-enum-drawings-boundary-v1/line_all_literal_zero",
        source: r#"//@version=6
indicator("Enum and drawing collection history boundary")
values = line.all[0]
plot(array.size(values))
"#,
        source_sha256: "4ba6a7c5f20811191f3d1eddce0a5a333d4524d652010dcde3ceba781e60d446",
        accepted: true,
        expected: None, // Compile-only check; raw array-size history can be missing on bar zero.
    },
    AdmissionCase {
        id: "history-enum-drawings-boundary-v1/line_all_literal_one",
        source: r#"//@version=6
indicator("Enum and drawing collection history boundary")
values = line.all[1]
plot(array.size(values))
"#,
        source_sha256: "146269112bafaa95c7d9ffdc59128d7882ce8c7bbab77ab2a753856667e15600",
        accepted: true,
        expected: None, // Compile-only check; raw array-size history can be missing on bar zero.
    },
    AdmissionCase {
        id: "history-enum-drawings-boundary-v1/line_all_input_zero",
        source: r#"//@version=6
indicator("Enum and drawing collection history boundary")
lag = input.int(0)
values = line.all[lag]
plot(array.size(values))
"#,
        source_sha256: "1c514ac4676b16bf5a12dd926cff6091579917138d87a3a79683c12508f7db5c",
        accepted: true,
        expected: None, // Compile-only check; raw array-size history can be missing on bar zero.
    },
    AdmissionCase {
        id: "history-enum-drawings-boundary-v1/line_all_input_one",
        source: r#"//@version=6
indicator("Enum and drawing collection history boundary")
lag = input.int(1)
values = line.all[lag]
plot(array.size(values))
"#,
        source_sha256: "4f8a1af09422c2c253b5df22f401b540513f593d62a326b947a031698daf8655",
        accepted: true,
        expected: None, // Compile-only check; raw array-size history can be missing on bar zero.
    },
];

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

fn source(body: &str) -> String {
    format!("//@version=6\nindicator(\"Computed history safeguard\")\n{body}\n")
}

fn assert_values(actual: &[Value], expected: &[Option<f64>], id: &str) {
    assert_eq!(actual.len(), expected.len(), "{id}: {actual:?}");
    for (bar, (actual, expected)) in actual.iter().zip(expected).enumerate() {
        let Some(expected) = expected else {
            assert!(actual.is_na(), "{id} bar {bar}: {actual:?} must be missing");
            continue;
        };
        let actual = match actual {
            Value::Float(value) => *value,
            Value::Int(value) => *value as f64,
            other => panic!("{id} bar {bar}: {other:?} is not numeric"),
        };
        assert!(
            (actual - expected).abs() < 1e-12,
            "{id} bar {bar}: {actual} != {expected}"
        );
    }
}

fn assert_run(run: &RunResult, expected: &[&[Option<f64>]], id: &str) {
    assert!(
        run.runtime_errors.is_empty(),
        "{id}: {:?}",
        run.runtime_errors
    );
    assert_eq!(run.bars_executed, 4, "{id}");
    assert_eq!(run.plots.len(), expected.len(), "{id}");
    for (plot, values) in run.plots.iter().zip(expected) {
        assert_values(&plot.values, values, id);
    }
}

#[test]
fn exact_history_admissions_keep_every_case_source() {
    let mut accepted = 0;
    let mut rejected = 0;
    for case in ADMISSION_CASES {
        assert_eq!(
            Sha256::digest(case.source.as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
            case.source_sha256,
            "{} source bytes changed",
            case.id
        );
        let result = compile(case.source, CompileOptions::default());
        assert_eq!(
            result.success, case.accepted,
            "{}: {:?}",
            case.id, result.diagnostics
        );
        if case.accepted {
            accepted += 1;
        } else {
            rejected += 1;
            assert!(
                result
                    .diagnostics
                    .iter()
                    .any(|d| d.code.as_deref() == Some("E0209")),
                "{}: {:?}",
                case.id,
                result.diagnostics
            );
        }
    }
    assert_eq!((accepted, rejected), (18, 8));
}

#[test]
fn admitted_cases_keep_local_values_without_restoring_series_floors() {
    for case in ADMISSION_CASES {
        let Some(expected) = case.expected else {
            continue;
        };
        let compiled = compile(
            case.source,
            CompileOptions {
                emit_hir: true,
                ..CompileOptions::default()
            },
        );
        assert!(compiled.success, "{}: {:?}", case.id, compiled.diagnostics);
        if case.source.contains("\nvar float k") || case.source.contains("\nvarip float k") {
            assert!(
                compiled
                    .hir_debug
                    .as_deref()
                    .is_some_and(|hir| hir.contains("`k`: simple float")),
                "{}: {:?}",
                case.id,
                compiled.hir_debug
            );
        }
        let program = compiled.program.unwrap();
        let run = run_program(&program, &market(), &RunOptions::default());
        assert_run(&run, &[expected], case.id);
    }
}

#[test]
fn dynamic_input_offsets_and_values_bind_independently_on_each_run() {
    let text = source(
        "simple float value = input.float(1.0, \"Value\")\n\
         int lag = input.int(1, \"Lag\")\n\
         plot((value * 2)[lag])",
    );
    let compiled = compile(&text, CompileOptions::default());
    assert!(compiled.success, "{:?}", compiled.diagnostics);
    let program = compiled.program.unwrap();
    assert_eq!(program.inputs.len(), 2);
    for (lag, expected) in [
        (0, vec![Some(7.0); 4]),
        (1, vec![None, Some(7.0), Some(7.0), Some(7.0)]),
        (2, vec![None, None, Some(7.0), Some(7.0)]),
        (-1, vec![None; 4]),
    ] {
        let mut options = RunOptions::default();
        options.inputs.insert(0, InputValue::Float(3.5));
        options.inputs.insert(1, InputValue::Int(lag));
        assert_run(
            &run_program(&program, &market(), &options),
            &[&expected],
            "bound inputs",
        );
    }
    // The same compiled input IDs retain their defaults after override runs.
    assert_run(
        &run_program(&program, &market(), &RunOptions::default()),
        &[&[None, Some(2.0), Some(2.0), Some(2.0)]],
        "default inputs after overrides",
    );

    let fractional = source("float lag = input.float(1.9)\nplot((1.0 + 2.0)[lag])");
    let compiled = compile(&fractional, CompileOptions::default());
    assert!(compiled.success, "{:?}", compiled.diagnostics);
    assert_run(
        &run_program(
            &compiled.program.unwrap(),
            &market(),
            &RunOptions::default(),
        ),
        &[&[None, Some(3.0), Some(3.0), Some(3.0)]],
        "fractional offset truncates toward zero",
    );
}

#[test]
fn computed_boolean_history_exposes_false_before_its_first_value() {
    let text =
        source("flag = input.bool(true)\nlag = input.int(1)\nplot((flag and true)[lag] ? 1 : 0)");
    let compiled = compile(&text, CompileOptions::default());
    assert!(compiled.success, "{:?}", compiled.diagnostics);
    let program = compiled.program.unwrap();
    for (flag, lag, expected) in [
        (true, 0, vec![Some(1.0); 4]),
        (true, 1, vec![Some(0.0), Some(1.0), Some(1.0), Some(1.0)]),
        (true, 2, vec![Some(0.0), Some(0.0), Some(1.0), Some(1.0)]),
        (false, 1, vec![Some(0.0); 4]),
    ] {
        let mut options = RunOptions::default();
        options.inputs.insert(0, InputValue::Bool(flag));
        options.inputs.insert(1, InputValue::Int(lag));
        assert_run(
            &run_program(&program, &market(), &options),
            &[&expected],
            "Bool history",
        );
    }
}

#[test]
fn folded_computed_reads_snapshot_the_site_before_later_writes() {
    // Reading the direct variable slot for (+k)[lag] would return [na, 3, 3, 3]
    // after the later k := 3 write.
    // The compound site sees 2, so its hidden history must retain that value.
    // These expected values assert the local runtime semantics.
    for target in ["+k", "float(k)", "identity(k)"] {
        for offset in ["1", "lag"] {
            let text = source(&format!(
                "identity(float value) => value\n\
                 float k = 2.0\n\
                 int lag = input.int(1)\n\
                 held = ({target})[{offset}]\n\
                 k := 3.0\n\
                 plot(held)\n\
                 plot(k[{offset}])"
            ));
            let compiled = compile(
                &text,
                CompileOptions {
                    emit_hir: true,
                    ..CompileOptions::default()
                },
            );
            assert!(
                compiled.success,
                "{target} [{offset}]: {:?}",
                compiled.diagnostics
            );
            assert!(compiled
                .hir_debug
                .as_deref()
                .unwrap()
                .contains(if offset == "lag" {
                    "MaterializeHistoryDynamic"
                } else {
                    "MaterializeHistory"
                }));
            let program = compiled.program.unwrap();
            assert_run(
                &run_program(&program, &market(), &RunOptions::default()),
                &[
                    &[None, Some(2.0), Some(2.0), Some(2.0)],
                    &[None, Some(3.0), Some(3.0), Some(3.0)],
                ],
                "computed site versus final variable slot",
            );
            if offset == "lag" {
                let mut options = RunOptions::default();
                options.inputs.insert(0, InputValue::Int(0));
                assert_run(
                    &run_program(&program, &market(), &options),
                    &[&[Some(2.0); 4], &[Some(3.0); 4]],
                    "zero offset still snapshots a computed site",
                );
            }
        }
    }
}

#[test]
fn computed_history_and_its_offset_execute_once_per_bar() {
    for offset in ["1", "lag"] {
        let text = source(&format!(
            "var calls = array.new_int()\ntap() =>\n    array.push(calls, 1)\n    2.0\nlag = input.int(1)\nplot(tap()[{offset}])\nplot(array.size(calls))"
        ));
        let compiled = compile(&text, CompileOptions::default());
        assert!(compiled.success, "{:?}", compiled.diagnostics);
        assert_run(
            &run_program(
                &compiled.program.unwrap(),
                &market(),
                &RunOptions::default(),
            ),
            &[
                &[None, Some(2.0), Some(2.0), Some(2.0)],
                &[Some(1.0), Some(2.0), Some(3.0), Some(4.0)],
            ],
            "target called once",
        );
    }
    let text = source(
        "var calls = array.new_int()\nlag() =>\n    array.push(calls, 1)\n    1\nplot((1 + 2)[lag()])\nplot(array.size(calls))",
    );
    let compiled = compile(&text, CompileOptions::default());
    assert!(compiled.success, "{:?}", compiled.diagnostics);
    assert_run(
        &run_program(
            &compiled.program.unwrap(),
            &market(),
            &RunOptions::default(),
        ),
        &[
            &[None, Some(3.0), Some(3.0), Some(3.0)],
            &[Some(1.0), Some(2.0), Some(3.0), Some(4.0)],
        ],
        "offset called once",
    );

    // Both builtin effects remain inline. Target pops 2 before offset pops 1.
    let text = source(
        "values = array.from(1.0, 2.0)\n\
         plot((array.pop(values) + 0.0)[int(array.pop(values))])\n\
         plot(array.size(values))",
    );
    let compiled = compile(&text, CompileOptions::default());
    assert!(compiled.success, "{:?}", compiled.diagnostics);
    assert_run(
        &run_program(
            &compiled.program.unwrap(),
            &market(),
            &RunOptions::default(),
        ),
        &[&[None, Some(2.0), Some(2.0), Some(2.0)], &[Some(0.0); 4]],
        "inline target/offset order",
    );
}

#[test]
fn computed_weak_history_keeps_series_branch_gaps_and_scheduling_warnings() {
    for (one, two) in [("1", "2"), ("lag", "lag + 1")] {
        let text = source(&format!(
            "lag = input.int(1)\nfloat a = 0.0\nfloat b = 0.0\nif bar_index % 2 == 0\n    a := (1 + 2)[{one}]\n    b := (1 + 2)[{two}]\nplot(a)\nplot(b)"
        ));
        let compiled = compile(&text, CompileOptions::default());
        assert!(compiled.success, "{:?}", compiled.diagnostics);
        assert_eq!(
            compiled
                .diagnostics
                .iter()
                .filter(|d| d.code.as_deref() == Some("W0004"))
                .count(),
            2
        );
        assert_run(
            &run_program(
                &compiled.program.unwrap(),
                &market(),
                &RunOptions::default(),
            ),
            &[
                &[None, Some(0.0), None, Some(0.0)],
                &[None, Some(0.0), Some(3.0), Some(0.0)],
            ],
            "skipped history sites keep gaps",
        );
    }

    let text = source("enabled = input.bool(true)\nfloat value = 0.0\nif enabled\n    value := (1 + 2)[1]\nplot(value)");
    let compiled = compile(&text, CompileOptions::default());
    assert!(compiled.success, "{:?}", compiled.diagnostics);
    assert!(!compiled
        .diagnostics
        .iter()
        .any(|d| d.code.as_deref() == Some("W0004")));
    let program = compiled.program.unwrap();
    assert_run(
        &run_program(&program, &market(), &RunOptions::default()),
        &[&[None, Some(3.0), Some(3.0), Some(3.0)]],
        "run-stable enabled branch",
    );
    let mut disabled = RunOptions::default();
    disabled.inputs.insert(0, InputValue::Bool(false));
    assert_run(
        &run_program(&program, &market(), &disabled),
        &[&[Some(0.0); 4]],
        "disabled branch",
    );

    let text = source("var value = (1 + 2)[1]\nplot(value)");
    let compiled = compile(&text, CompileOptions::default());
    assert!(compiled.success, "{:?}", compiled.diagnostics);
    assert_eq!(
        compiled
            .diagnostics
            .iter()
            .filter(|d| d.code.as_deref() == Some("W0004"))
            .count(),
        1
    );
    assert_run(
        &run_program(
            &compiled.program.unwrap(),
            &market(),
            &RunOptions::default(),
        ),
        &[&[None; 4]],
        "var initializer materializes only on its first execution",
    );
}

#[test]
fn computed_scalar_values_are_not_limited_to_float_binary_expressions() {
    for body in ["plot((1 + 2)[1])", "plot((true ? 3 : 4)[1])"] {
        let compiled = compile(&source(body), CompileOptions::default());
        assert!(compiled.success, "{:?}", compiled.diagnostics);
        assert_run(
            &run_program(
                &compiled.program.unwrap(),
                &market(),
                &RunOptions::default(),
            ),
            &[&[None, Some(3.0), Some(3.0), Some(3.0)]],
            body,
        );
    }
    for body in [
        "plot((\"a\" + \"b\")[1] == \"ab\" ? 1 : 0)",
        "plot((not false)[1] ? 1 : 0)",
        "enum Choice\n    first\n    second\nvalue = (true ? Choice.first : Choice.second)[1]\nplot(value == Choice.first ? 1 : 0)",
    ] {
        let compiled = compile(&source(body), CompileOptions::default());
        assert!(compiled.success, "{:?}", compiled.diagnostics);
        assert_run(
            &run_program(&compiled.program.unwrap(), &market(), &RunOptions::default()),
            &[&[Some(0.0), Some(1.0), Some(1.0), Some(1.0)]],
            body,
        );
    }
}

#[test]
fn drawing_collection_history_keeps_per_site_array_snapshots() {
    // The .all cases below check compilation only. These
    // guarded local values follow the existing slot/reference semantics:
    // create one line each bar, then retain the separately collected array.
    for target in ["line.all", "collect()"] {
        for (offset, warmup) in [("0", "0"), ("1", "1"), ("lag", "lag")] {
            let text = source(&format!(
                "collect() => line.all\nlag = input.int(1)\nline.new(bar_index, open, bar_index, close)\nvalues = {target}[{offset}]\nplot(bar_index >= {warmup} ? array.size(values) : -1)\nplot(array.size(line.all))"
            ));
            let compiled = compile(&text, CompileOptions::default());
            assert!(
                compiled.success,
                "{target}[{offset}]: {:?}",
                compiled.diagnostics
            );
            let program = compiled.program.unwrap();
            if offset == "lag" {
                for (lag, values) in [
                    (0, vec![Some(1.0), Some(2.0), Some(3.0), Some(4.0)]),
                    (1, vec![Some(-1.0), Some(1.0), Some(2.0), Some(3.0)]),
                    (2, vec![Some(-1.0), Some(-1.0), Some(1.0), Some(2.0)]),
                ] {
                    let mut options = RunOptions::default();
                    options.inputs.insert(0, InputValue::Int(lag));
                    assert_run(
                        &run_program(&program, &market(), &options),
                        &[&values, &[Some(1.0), Some(2.0), Some(3.0), Some(4.0)]],
                        "dynamic drawing snapshots",
                    );
                }
            } else {
                let values = if offset == "0" {
                    [Some(1.0), Some(2.0), Some(3.0), Some(4.0)]
                } else {
                    [Some(-1.0), Some(1.0), Some(2.0), Some(3.0)]
                };
                assert_run(
                    &run_program(&program, &market(), &RunOptions::default()),
                    &[&values, &[Some(1.0), Some(2.0), Some(3.0), Some(4.0)]],
                    "literal drawing snapshots",
                );
            }
        }
    }
}

#[test]
fn forbidden_source_targets_stay_rejected_at_static_dynamic_and_zero_offsets() {
    let targets = [
        "1.0",
        "(1.0)",
        "1",
        "true",
        "\"literal\"",
        "#123456",
        "color.red",
        "(color.red)",
        "math.pi",
        "format.price",
        "Choice.first",
        "(Choice.first)",
        "p.x",
        "(p.x)",
        "text.format_bold",
        "(text.format_bold + text.format_italic)",
    ];
    for target in targets {
        for offset in ["0", "1", "lag"] {
            let text = source(&format!(
                "enum Choice\n    first\n\
                 type Point\n    float x\n\
                 p = Point.new(2.0)\n\
                 lag = input.int(0)\n\
                 value = {target}[{offset}]\n\
                 plot(1)"
            ));
            let result = compile(&text, CompileOptions::default());
            assert!(!result.success, "unexpectedly accepted {target}[{offset}]");
            assert!(
                result
                    .diagnostics
                    .iter()
                    .any(|d| d.code.as_deref() == Some("E0209")),
                "{target}[{offset}]: {:?}",
                result.diagnostics
            );
        }
    }
    // The guard belongs to [] and must never change ordinary constant reads.
    let text = source("plot(color.r(color.red) + math.pi)\nplot(syminfo.mintick[0])");
    let compiled = compile(&text, CompileOptions::default());
    assert!(compiled.success, "{:?}", compiled.diagnostics);
    let result = run_program(
        &compiled.program.unwrap(),
        &market(),
        &RunOptions::default(),
    );
    assert_values(
        &result.plots[1].values,
        &[Some(0.01); 4],
        "plain context builtin history",
    );
}

#[test]
fn newly_materialized_weak_values_keep_loop_and_buffer_limits() {
    for offset in ["1", "lag"] {
        let text = source(&format!(
            "lag = input.int(1)\nfor i = 0 to 1\n    value = (1 + 2)[{offset}]\nplot(1)"
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
    let bad_offset = compile(
        &source("value = (1 + 2)[\"bad\"]\nplot(1)"),
        CompileOptions::default(),
    );
    assert!(!bad_offset.success);
    assert!(bad_offset
        .diagnostics
        .iter()
        .any(|d| d.code.as_deref() == Some("E0210")));

    // A literal offset on the materialized value is a buffer request like
    // any other: it lifts a smaller declared ceiling, and past the hard limit
    // it is refused.
    let literal_request =
        "//@version=6\nindicator(\"Weak history ceiling\", max_bars_back=1)\nplot((1 + 2)[2])\n";
    let compiled = compile(literal_request, CompileOptions::default());
    assert!(compiled.success, "{:?}", compiled.diagnostics);
    assert_eq!(compiled.program.unwrap().metadata.max_bars_back, 2);

    let static_limit = "//@version=6\nindicator(\"Weak history limit\")\nplot((1 + 2)[5001])\n";
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

    let dynamic_limit = "//@version=6\nindicator(\"Weak dynamic history ceiling\", max_bars_back=1)\nlag = input.int(1)\nplot((1 + 2)[lag])\n";
    let compiled = compile(dynamic_limit, CompileOptions::default());
    assert!(compiled.success, "{:?}", compiled.diagnostics);
    let program = compiled.program.unwrap();
    let mut options = RunOptions::default();
    options.inputs.insert(0, InputValue::Int(2));
    let result = run_program(&program, &market(), &options);
    assert_eq!(result.bars_executed, 0);
    assert_eq!(result.runtime_errors.len(), 1);
    assert_eq!(
        result.runtime_errors[0].kind,
        RuntimeErrorKind::HistoryBufferLimitExceeded
    );
    assert_eq!(result.runtime_errors[0].bar_index, Some(0));
}
