//! Regression coverage for the two metadata sections embedded in every AOT
//! artifact.
//!
//! `module_metadata` has a documented hard size limit, while
//! `parameter_schema` does not. The latter still gets a generous sanity bound
//! so an accidental serialization blow-up is caught without inventing a host
//! requirement that does not exist.

use pinewasm_aot::schema::{
    module_metadata_json, parameter_schema_json, MODULE_METADATA_MAX_BYTES,
};
use pinewasm_compiler::{compile, CompileOptions};

fn representative_program() -> pinewasm_csil::CsilProgram {
    let source = r#"indicator("Parameter Schema Example")
fast = input.int(14, "Fast Length", minval = 1, maxval = 100)
slow = input.int(50, "Slow Length", minval = 2, maxval = 300)
factor = input.float(2.0, "Factor", minval = 0.1, maxval = 10.0, step = 0.1)
threshold = input.float(0.0, "Threshold", minval = -10.0, maxval = 10.0)
enabled = input.bool(true, "Enabled")
invert = input.bool(false, "Invert")
lookback = input.int(5, "Lookback", minval = 1, maxval = 25)
offset = input.float(1.0, "Offset", minval = -5.0, maxval = 5.0)
basis = ta.sma(close, fast)
comparison = ta.sma(close, slow)
signal = enabled and basis > comparison + threshold ? factor + offset : -factor + offset
plot(invert ? -signal : signal)
"#;
    let result = compile(source, CompileOptions::all());
    if let Some(diagnostic) = result.diagnostics.iter().find(|d| d.is_error()) {
        panic!(
            "representative program failed to compile: {:?} {}",
            diagnostic.code, diagnostic.message
        );
    }
    result.program.expect("representative program compiles")
}

#[test]
fn module_metadata_fits_the_documented_cap() {
    let bytes = module_metadata_json(&representative_program()).len();
    assert!(
        bytes <= MODULE_METADATA_MAX_BYTES,
        "module_metadata must fit the {MODULE_METADATA_MAX_BYTES}-byte cap, got {bytes}"
    );
}

#[test]
fn representative_parameter_schema_stays_within_reason() {
    let program = representative_program();
    let bytes = parameter_schema_json(&program).len();
    let inputs = program.inputs.len();

    assert_eq!(inputs, 8, "representative input coverage regressed");
    assert!(
        (1..64 * 1024).contains(&bytes),
        "schema size looks wrong ({bytes} bytes over {inputs} inputs)"
    );
}

#[test]
fn every_emitted_parameter_matches_the_public_contract() {
    let program = representative_program();
    let json = parameter_schema_json(&program);
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
    let entries = parsed.as_array().expect("a top-level array");

    assert_eq!(
        entries.len(),
        8,
        "all numeric and boolean inputs are emitted"
    );
    for entry in entries {
        assert!(
            entry["name"].is_string(),
            "parameter is missing a name: {entry}"
        );
        assert!(
            entry["label"].is_string(),
            "parameter is missing a label: {entry}"
        );
        assert!(
            matches!(entry["type"].as_str(), Some("float" | "int" | "bool")),
            "parameter has an unsupported type: {entry}"
        );
    }
}

#[test]
fn an_indicator_with_no_inputs_still_declares_itself() {
    let result = compile("indicator(\"Bare\")\nplot(close)\n", CompileOptions::all());
    let program = result.program.expect("compiles");
    assert_eq!(parameter_schema_json(&program), "[]");
    assert!(module_metadata_json(&program).contains("\"module_id\":\"bare\""));
}

fn schema_for(source: &str) -> serde_json::Value {
    let result = compile(source, CompileOptions::all());
    if let Some(diagnostic) = result
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.is_error())
    {
        panic!(
            "input-bound fixture failed to compile: {:?} {}",
            diagnostic.code, diagnostic.message
        );
    }
    let program = result.program.expect("input-bound fixture compiles");
    serde_json::from_str(&parameter_schema_json(&program)).expect("valid parameter schema")
}

#[test]
fn parameter_schema_preserves_each_declared_bound_independently() {
    let cases = [
        (
            "min only",
            "indicator(\"Bounds\")\nvalue = input.float(1.0, \"Value\", minval = 0.5, step = 0.25)\nplot(value)\n",
            Some(0.5),
            None,
        ),
        (
            "max only",
            "indicator(\"Bounds\")\nvalue = input.float(1.0, \"Value\", maxval = 5.0, step = 0.25)\nplot(value)\n",
            None,
            Some(5.0),
        ),
        (
            "both bounds",
            "indicator(\"Bounds\")\nvalue = input.float(1.0, \"Value\", minval = 0.5, maxval = 5.0, step = 0.25)\nplot(value)\n",
            Some(0.5),
            Some(5.0),
        ),
        (
            "no bounds",
            "indicator(\"Bounds\")\nvalue = input.float(1.0, \"Value\", step = 0.25)\nplot(value)\n",
            None,
            None,
        ),
    ];

    for (label, source, expected_min, expected_max) in cases {
        let schema = schema_for(source);
        let parameter = &schema[0];
        assert_eq!(
            parameter.get("min").and_then(serde_json::Value::as_f64),
            expected_min,
            "{label}"
        );
        assert_eq!(
            parameter.get("max").and_then(serde_json::Value::as_f64),
            expected_max,
            "{label}"
        );
        assert_eq!(parameter["step"].as_f64(), Some(0.25), "{label}");
    }
}
