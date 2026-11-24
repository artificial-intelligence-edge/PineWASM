use pinewasm_compiler::{
    compile, run_program_profiled, CompileOptions, MarketData, PreparedProgram, RunOptions,
};

#[test]
fn resident_program_retains_only_code_and_preserves_errors() {
    let compiled = compile("indicator(\"Resident\")\nx = input.int(2, \"Length\")\nvar float sum = 0.0\nsum += close\nplot(ta.sma(close, x))\nplot(sum)\n", CompileOptions::default());
    let program = compiled.program.expect("compile");
    let resident = PreparedProgram::new(program.clone()).expect("admit");
    let data: MarketData = serde_json::from_value(serde_json::json!({
        "open": [1,2,3,4], "high": [1,2,3,4], "low": [1,2,3,4],
        "close": [1,2,3,4], "volume": [1,1,1,1]
    }))
    .unwrap();
    let mut changed = RunOptions::default();
    changed
        .inputs
        .insert(program.inputs[0].id, pinewasm_compiler::InputValue::Int(3));
    let limited = RunOptions {
        max_instructions_per_bar: 2,
        ..RunOptions::default()
    };
    for options in [
        RunOptions::default(),
        changed,
        limited,
        RunOptions::default(),
    ] {
        let expected = run_program_profiled(&program, &data, &options, None);
        let actual = resident.run_profiled(&data, &options, None);
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }
    let mut invalid = program;
    invalid.functions.clear();
    assert!(PreparedProgram::new(invalid).is_err());
}
