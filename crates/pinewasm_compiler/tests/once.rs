use pinewasm_compiler::{
    compile, compile_and_run, run_program, CompileOptions, IncrementalSession, InputValue,
    MarketData, PreparedProgram, RunOptions, RunResult, Tick, Value,
};

fn market(bars: usize) -> MarketData {
    MarketData {
        open: vec![1.0; bars],
        high: vec![2.0; bars],
        low: vec![0.0; bars],
        close: vec![1.5; bars],
        volume: vec![1.0; bars],
        time: (0..bars)
            .map(|bar| 1_000_000_000_000 + bar as i64 * 60_000)
            .collect(),
        ..MarketData::default()
    }
}

fn run(body: &str) -> RunResult {
    let source = format!("//@version=6\nindicator(\"Once behavior\")\n{body}\n");
    let result = compile_and_run(
        &source,
        &market(6),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        result.diagnostics.iter().all(|d| !d.is_error()),
        "{source}: {:?}",
        result.diagnostics
    );
    assert!(
        result.runtime_errors.is_empty(),
        "{source}: {:?}",
        result.runtime_errors
    );
    assert_eq!(result.bars_executed, 6);
    result
}

fn values(result: &RunResult, slot: usize, expected: &[f64]) {
    assert_eq!(
        result.plots[slot].values,
        expected
            .iter()
            .copied()
            .map(Value::Float)
            .collect::<Vec<_>>()
    );
}

#[test]
fn omitted_and_delayed_conditions_execute_once_per_run() {
    values(&run("var int n=0\nonce\n    n+=1\nplot(n)"), 0, &[1.0; 6]);
    values(
        &run("var int n=0\nonce bar_index>=2\n    n+=1\nplot(n)"),
        0,
        &[0.0, 0.0, 1.0, 1.0, 1.0, 1.0],
    );
    values(
        &run("var int n=0\nonce bar_index%2==1\n    n+=1\nplot(n)"),
        0,
        &[0.0, 1.0, 1.0, 1.0, 1.0, 1.0],
    );
}

#[test]
fn contextual_once_names_remain_available_beside_the_statement() {
    values(&run("once=1\nplot(once)"), 0, &[1.0; 6]);
    values(
        &run("var int once=0\nonce+=1\nplot(once)"),
        0,
        &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0],
    );
    values(&run("f(int once) =>\n    once\nplot(f(2))"), 0, &[2.0; 6]);
    values(&run("once(int x) => x+1\nplot(once(2))"), 0, &[3.0; 6]);
    values(
        &run("type T\n    int once\nt=T.new(1)\nt.once+=1\nplot(t.once)"),
        0,
        &[2.0; 6],
    );
    values(
        &run("type T\n    int n\nmethod once(T this) => this.n\nt=T.new(2)\nplot(t.once())"),
        0,
        &[2.0; 6],
    );
    values(
        &run("once=bar_index>=2\nvar int n=0\nonce once\n    n+=1\nplot(n)"),
        0,
        &[0.0, 0.0, 1.0, 1.0, 1.0, 1.0],
    );
    values(
        &run("f(bool a,bool b) => a and b\nvar int n=0\nonce f(a=true,b=true)\n    n+=1\nplot(n)"),
        0,
        &[1.0; 6],
    );
}

#[test]
fn independent_nested_and_conditionally_reached_structures_have_separate_activation() {
    values(&run("var int a=0\nvar int b=0\nonce bar_index>=1\n    a+=1\nonce bar_index>=3\n    b+=10\nplot(a+b)"), 0, &[0.0,1.0,1.0,11.0,11.0,11.0]);
    values(
        &run("var int n=0\nonce\n    once bar_index>=2\n        n+=1\nplot(n)"),
        0,
        &[0.0; 6],
    );
    values(
        &run("var int n=0\nif bar_index>=2\n    once\n        n+=1\nplot(n)"),
        0,
        &[0.0, 0.0, 1.0, 1.0, 1.0, 1.0],
    );
}

#[test]
fn helper_call_sites_have_independent_flags_and_void_results() {
    values(&run("var a=array.new_int()\nf(int v) =>\n    once\n        array.push(a,v)\nf(1)\nf(2)\nplot(array.size(a))"), 0, &[2.0;6]);
    let compiled = compile("//@version=6\nindicator(\"Void once\")\nf() =>\n    once\n        log.info(\"effect\")\nx=f()\nplot(close)\n", CompileOptions::default());
    assert!(!compiled.success);
    assert!(
        compiled
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0212")),
        "{:?}",
        compiled.diagnostics
    );
    assert!(compiled
        .diagnostics
        .iter()
        .all(|d| d.code.as_deref() != Some("E9999")));
    let compiled = compile(
        "//@version=6\nindicator(\"Void argument\")\nf() =>\n    once\n        42\nplot(f())\n",
        CompileOptions::default(),
    );
    assert!(
        !compiled.success,
        "a trailing body expression must not turn once into a value"
    );
    assert!(compiled
        .diagnostics
        .iter()
        .all(|d| d.code.as_deref() != Some("E9999")));
}

#[test]
fn prepared_runs_reset_activation_and_respecialize_conditions() {
    let source = "//@version=6\nindicator(\"Fresh once\")\nat=input.int(2)\nvar int n=0\nonce bar_index>=at\n    n+=1\nplot(n)\n";
    let compiled = compile(source, CompileOptions::default());
    assert!(compiled.success, "{:?}", compiled.diagnostics);
    let program = compiled.program.unwrap();
    let input_id = program.inputs[0].id;
    let prepared = PreparedProgram::new(program).unwrap();
    for (at, expected) in [
        (2, [0.0, 0.0, 1.0, 1.0, 1.0, 1.0]),
        (4, [0.0, 0.0, 0.0, 0.0, 1.0, 1.0]),
        (2, [0.0, 0.0, 1.0, 1.0, 1.0, 1.0]),
    ] {
        let mut options = RunOptions::default();
        options.inputs.insert(input_id, InputValue::Int(at));
        let result = prepared.run_profiled(&market(6), &options, None);
        assert!(
            result.runtime_errors.is_empty(),
            "{:?}",
            result.runtime_errors
        );
        values(&result, 0, &expected);
    }
}

#[test]
fn loop_control_cannot_bypass_deactivation() {
    for control in ["break", "continue"] {
        values(
            &run(&format!(
                "var int n=0\nfor i=0 to 2\n    once\n        n+=1\n        {control}\nplot(n)"
            )),
            0,
            &[1.0; 6],
        );
    }
}

#[test]
fn inactive_condition_prefixes_and_false_bodies_do_not_execute() {
    values(&run("var a=array.new_int()\nf() =>\n    array.push(a,1)\n    true\nonce f()\n    array.push(a,10)\nplot(array.size(a))"), 0, &[2.0;6]);
    values(
        &run("once false\n    runtime.error(\"inactive once\")\nplot(close)"),
        0,
        &[1.5; 6],
    );
}

#[test]
fn once_condition_is_boolean_and_body_does_not_escape_scope() {
    for (body, code) in [
        ("once 1\n    log.info(\"effect\")", "E0208"),
        ("once\n    local=1\nplot(local)", "E0201"),
    ] {
        let source = format!("//@version=6\nindicator(\"Invalid once\")\n{body}\n");
        let compiled = compile(&source, CompileOptions::default());
        assert!(!compiled.success, "{source}");
        assert!(
            compiled
                .diagnostics
                .iter()
                .any(|d| d.code.as_deref() == Some(code)),
            "{:?}",
            compiled.diagnostics
        );
    }
}

fn tick(bar: usize, close: f64, commit: bool) -> Tick {
    Tick {
        bar_index: bar,
        open: 1.0,
        high: 2.0,
        low: 0.0,
        close,
        volume: 1.0,
        time: 1_000_000_000_000 + bar as i64 * 60_000,
        trading_day: None,
        is_bar_close: commit,
    }
}

#[test]
fn open_ticks_restore_activation_but_keep_varip_effects() {
    let source="//@version=6\nindicator(\"Realtime once\")\nvar int n=0\nvarip int ip=0\nonce bar_index>=2\n    n+=1\n    ip+=1\nplot(n)\nplot(ip)\n";
    let compiled = compile(source, CompileOptions::default());
    assert!(compiled.success, "{:?}", compiled.diagnostics);
    let mut session = IncrementalSession::new(
        compiled.program.as_ref().unwrap(),
        &market(2),
        &RunOptions::default(),
    )
    .unwrap();
    for (ordinal, close) in [1.1, 1.2, 1.3].into_iter().enumerate() {
        let result = session.advance(&[tick(2, close, ordinal == 2)]);
        assert!(
            result.runtime_errors.is_empty(),
            "{:?}",
            result.runtime_errors
        );
        values(&result, 0, &[0.0, 0.0, 1.0]);
        values(&result, 1, &[0.0, 0.0, (ordinal + 1) as f64]);
    }
    let result = session.advance(&[tick(3, 1.4, true)]);
    values(&result, 0, &[0.0, 0.0, 1.0, 1.0]);
    values(&result, 1, &[0.0, 0.0, 3.0, 3.0]);
}

#[test]
fn a_true_open_tick_does_not_deactivate_once_when_the_closing_condition_is_false() {
    let source="//@version=6\nindicator(\"False closing condition\")\nvar int n=0\nvarip int ip=0\nonce bar_index>=2 and close>1.5\n    n+=1\n    ip+=1\nplot(n)\nplot(ip)\n";
    let compiled = compile(source, CompileOptions::default());
    assert!(compiled.success, "{:?}", compiled.diagnostics);
    let mut session = IncrementalSession::new(
        compiled.program.as_ref().unwrap(),
        &market(2),
        &RunOptions::default(),
    )
    .unwrap();
    let open = session.advance(&[tick(2, 2.0, false)]);
    values(&open, 0, &[0.0, 0.0, 1.0]);
    values(&open, 1, &[0.0, 0.0, 1.0]);
    let close = session.advance(&[tick(2, 1.0, true)]);
    values(&close, 0, &[0.0, 0.0, 0.0]);
    values(&close, 1, &[0.0, 0.0, 1.0]);
    let next = session.advance(&[tick(3, 2.0, true)]);
    values(&next, 0, &[0.0, 0.0, 0.0, 1.0]);
    values(&next, 1, &[0.0, 0.0, 1.0, 2.0]);
    let later = session.advance(&[tick(4, 2.0, true)]);
    values(&later, 0, &[0.0, 0.0, 0.0, 1.0, 1.0]);
    values(&later, 1, &[0.0, 0.0, 1.0, 2.0, 2.0]);
}

#[test]
fn failed_body_terminates_the_session_and_new_runs_start_fresh() {
    let source="//@version=6\nindicator(\"Once error\")\nonce bar_index>=2\n    runtime.error(\"once-fault\")\nplot(close)\n";
    let compiled = compile(source, CompileOptions::default());
    assert!(compiled.success, "{:?}", compiled.diagnostics);
    let program = compiled.program.as_ref().unwrap();
    let mut session = IncrementalSession::new(program, &market(2), &RunOptions::default()).unwrap();
    let failed = session.advance(&[tick(2, 1.4, true)]);
    assert_eq!(failed.runtime_errors.len(), 1);
    let again = session.advance(&[tick(3, 1.4, true)]);
    assert_eq!(again.runtime_errors, failed.runtime_errors);
    let first = run_program(program, &market(2), &RunOptions::default());
    let second = run_program(program, &market(2), &RunOptions::default());
    assert!(first.runtime_errors.is_empty());
    assert_eq!(first.plots, second.plots);
}

#[test]
fn once_control_retains_the_qualifier_of_carried_writes() {
    for condition in ["", " true", " bar_index>0"] {
        let source=format!("//@version=6\nindicator(\"Once carried shift\")\nvar int shift=0\nplot(close,offset=shift)\nonce{condition}\n    shift:=1\n");
        let result = compile(&source, CompileOptions::default());
        assert!(!result.success, "{source}");
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.code.as_deref() == Some("E0206")),
            "{source}: {:?}",
            result.diagnostics
        );
    }
    let inactive = compile(
        "//@version=6\nindicator(\"Inactive once shift\")\nvar int shift=0\nplot(close,offset=shift)\nonce false\n    shift:=bar_index\n",
        CompileOptions::default(),
    );
    assert!(inactive.success, "{:?}", inactive.diagnostics);
}
