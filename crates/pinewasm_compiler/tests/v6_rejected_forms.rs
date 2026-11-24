//! Rules about where calls may appear and what a script may declare,
//! checked by these tests:
//!
//! - an enum may not take the name of a built-in type or namespace;
//! - a library must export at least one definition, and `library()` binds
//!   `title`, `overlay` and `dynamic_requests` by name;
//! - when any declaration sets `dynamic_requests = false`, a `request.*()`
//!   call may not run in a local scope;
//! - `alertcondition()` is global-only, and global-only calls may not be
//!   ternary operands;
//! - `timeframe_gaps` needs a `timeframe` argument;
//! - a function may not reassign its parameters;
//! - a field default is a literal, a built-in variable or constant, or an
//!   enum member;
//! - a script uses at most 64 plot counts, and a scope declares at most
//!   1,000 variables;
//! - `syminfo.current_contract` exists.

use pinewasm_compiler::{
    compile, compile_and_run, compile_with_libraries, CompileOptions, CompileResult, MarketData,
    RunOptions, SymInfo,
};

fn compile_src(src: &str) -> CompileResult {
    compile(&format!("//@version=6\n{src}\n"), CompileOptions::default())
}

fn assert_compiles(src: &str) {
    let result = compile_src(src);
    assert!(result.success, "{src}\n{:?}", result.diagnostics);
}

fn assert_rejected(src: &str, code: &str) -> CompileResult {
    let result = compile_src(src);
    assert!(
        !result.success
            && result
                .diagnostics
                .iter()
                .any(|d| d.code.as_deref() == Some(code)),
        "{src}\nexpected {code}, got {:?}",
        result.diagnostics
    );
    result
}

fn has_message(result: &CompileResult, text: &str) -> bool {
    result.diagnostics.iter().any(|d| d.message.contains(text))
}

fn repeat(n: usize, line: &str) -> String {
    (0..n).map(|_| format!("{line}\n")).collect()
}

#[test]
fn an_enum_may_not_take_a_built_in_namespace_name() {
    for name in [
        "ta",
        "math",
        "syminfo",
        "strategy",
        "str",
        "request",
        "timeframe",
        "barstate",
        "input",
        "log",
    ] {
        assert_rejected(
            &format!("indicator(\"E\")\nenum {name}\n    a\n    b\nplot(close)"),
            "E0239",
        );
    }
    for name in ["array", "line", "table"] {
        let source = format!("indicator(\"E\")\nenum {name}\n    a\n    b\nplot(close)");
        assert!(!compile_src(&source).success, "{source}");
    }
    assert_compiles(
        "indicator(\"E\")\nenum Signal\n    buy\n    sell\nSignal s = Signal.buy\nplot(s == Signal.buy ? 1 : 0)",
    );
}

#[test]
fn an_imported_enum_with_a_namespace_name_is_reported_not_a_crash() {
    let library = "//@version=6\nlibrary(\"L\")\nexport enum ta\n    a\n    b\nexport f() => 1\n";
    let main =
        "//@version=6\nindicator(\"I\")\nimport user/L/1 as lib\nx = lib.ta.a\nplot(lib.f())\n";
    let result = compile_with_libraries(
        main,
        &[("user/L/1".to_string(), library.to_string())],
        CompileOptions::default(),
    );
    assert!(
        !result.success
            && result
                .diagnostics
                .iter()
                .any(|d| d.code.as_deref() == Some("E0239")),
        "{:?}",
        result.diagnostics
    );
}

#[test]
fn a_library_must_export_something_and_binds_its_arguments_by_name() {
    assert_rejected("library(\"Empty\")\nf() => 1", "E0243");
    assert_compiles("library(\"Lib\")\nexport f() => 1");
    assert_compiles(
        "library(title = \"Lib\", overlay = true, dynamic_requests = false)\nexport f() => 1",
    );
    assert_rejected(
        "library(\"Lib\", dynamic_requests = 1)\nexport f() => 1",
        "E0205",
    );
    assert_rejected("library(\"Lib\", scale = 1)\nexport f() => 1", "E0228");
}

#[test]
fn requests_stay_global_when_a_declaration_turns_dynamic_requests_off() {
    let request = "request.security(syminfo.tickerid, \"1D\", close)";
    for decl in [
        "indicator(\"R\", dynamic_requests = false)",
        "strategy(\"R\", dynamic_requests = false)",
    ] {
        assert_rejected(
            &format!("{decl}\nx = 0.0\nif close > open\n    x := {request}\nplot(x)"),
            "E0242",
        );
        // A request wrapped in a function is still a request in that scope.
        assert_rejected(
            &format!("{decl}\nf() => {request}\nx = 0.0\nif close > open\n    x := f()\nplot(x)"),
            "E0242",
        );
        assert_compiles(&format!("{decl}\nplot({request})"));
    }
    assert_compiles(&format!(
        "indicator(\"R\")\nx = 0.0\nif close > open\n    x := {request}\nplot(x)"
    ));
}

#[test]
fn global_only_calls_stay_out_of_local_scopes_and_ternaries() {
    assert_rejected(
        "indicator(\"A\")\nif close > open\n    alertcondition(close > open, \"Up\")\nplot(close)",
        "E0254",
    );
    assert_rejected(
        "indicator(\"A\")\nx = 0\nif close > open\n    alertcondition(close > open, \"Up\")\n    x := 1\nplot(x)",
        "E0254",
    );
    assert_rejected(
        "indicator(\"A\")\nf() =>\n    alertcondition(close > open, \"Up\")\nf()\nplot(close)",
        "E0254",
    );
    assert_compiles("indicator(\"A\")\nalertcondition(close > open, \"Up\")\nplot(close)");
    assert_rejected(
        "indicator(\"A\")\np = close > open ? plot(close) : plot(open)\nplot(close)",
        "E0254",
    );
    assert_rejected(
        "indicator(\"A\")\nh = true ? hline(1) : hline(2)\nplot(close)",
        "E0254",
    );
}

#[test]
fn timeframe_gaps_needs_a_timeframe() {
    assert_rejected(
        "indicator(\"G\", timeframe_gaps = false)\nplot(close)",
        "E0204",
    );
    assert_compiles("indicator(\"G\", timeframe = \"1D\", timeframe_gaps = false)\nplot(close)");
}

#[test]
fn a_function_may_not_reassign_its_parameters() {
    assert_rejected(
        "indicator(\"P\")\nf(x) =>\n    x := x + 1\n    x\nplot(f(close))",
        "E0217",
    );
    assert_rejected(
        "indicator(\"P\")\nf(x) =>\n    x += 1\n    x\nplot(f(close))",
        "E0217",
    );
    assert_compiles("indicator(\"P\")\nf(x) =>\n    y = x\n    y := y + 1\n    y\nplot(f(close))");
}

#[test]
fn a_field_default_is_a_literal_a_built_in_or_an_enum_member() {
    assert_compiles(
        "indicator(\"F\")\n\
         enum Mode\n    fast\n    slow\n\
         type T\n    int a = 1\n    float b = -1.5\n    color c = color.red\n    string s = \"x\"\n    Mode m = Mode.fast\n    float d = na\n    bool e = true\n    color h = #ff0000\n\
         t = T.new()\nplot(t.a + t.b)",
    );
    for default in [
        "float f = math.abs(-1)",
        "int g = 1 + 2",
        "color c = color.new(color.red, 50)",
    ] {
        assert_rejected(
            &format!("indicator(\"F\")\ntype T\n    {default}\nplot(close)"),
            "E0236",
        );
    }
}

#[test]
fn a_script_uses_at_most_64_plot_counts() {
    assert_compiles(&format!("indicator(\"C\")\n{}", repeat(64, "plot(close)")));
    let result = assert_rejected(
        &format!("indicator(\"C\")\n{}", repeat(65, "plot(close)")),
        "E0253",
    );
    assert!(
        has_message(&result, "65 plot counts"),
        "{:?}",
        result.diagnostics
    );

    // A color argument in the series form adds a count.
    let series = "c = close > open ? color.green : color.red\n";
    assert_compiles(&format!(
        "indicator(\"C\")\n{series}{}",
        repeat(32, "plot(close, color = c)")
    ));
    let result = assert_rejected(
        &format!(
            "indicator(\"C\")\n{series}{}",
            repeat(33, "plot(close, color = c)")
        ),
        "E0253",
    );
    assert!(
        has_message(&result, "66 plot counts"),
        "{:?}",
        result.diagnostics
    );

    // 8 * 7 (plotcandle with three series colors) + 1 (bgcolor) + 1
    // (alertcondition) + 2 (plots) + 0 (fill with a fixed color) + 0 (hline)
    // + 3 (plotshape with two series colors) + 1 (plotarrow with its default
    // colors) = 64.
    let mixed = format!(
        "indicator(\"C\")\n{series}{}bgcolor(c)\nalertcondition(close > open, \"Up\")\np1 = plot(open)\np2 = plot(close)\nhline(1)\nplotshape(close > open, color = c, textcolor = c)\nplotarrow(close - open)\n",
        repeat(
            8,
            "plotcandle(open, high, low, close, color = c, wickcolor = c, bordercolor = c)"
        )
    );
    assert_compiles(&format!("{mixed}fill(p1, p2, color = color.gray)"));
    let result = assert_rejected(&format!("{mixed}fill(p1, p2, color = c)"), "E0253");
    assert!(
        has_message(&result, "65 plot counts"),
        "{:?}",
        result.diagnostics
    );
    let result = assert_rejected(&format!("{mixed}barcolor(color.gray)"), "E0253");
    assert!(
        has_message(&result, "65 plot counts"),
        "{:?}",
        result.diagnostics
    );
}

#[test]
fn a_scope_declares_at_most_1000_variables() {
    let variables = |n: usize, indent: &str| -> String {
        (0..n).map(|i| format!("{indent}v{i} = {i}\n")).collect()
    };
    assert_compiles(&format!(
        "indicator(\"V\")\n{}plot(close)",
        variables(1000, "")
    ));
    // The compiler's own `for...in` temporaries are not script variables:
    // 997 + `a` + `m` + `last` is exactly 1,000.
    assert_compiles(&format!(
        "indicator(\"V\")\n{}a = array.from(1)\nm = map.new<int, int>()\nfor x in a\n    x\nfor [k, v] in m\n    k\nlast = 0\nplot(close)",
        variables(997, "")
    ));
    assert_rejected(
        &format!("indicator(\"V\")\n{}plot(close)", variables(1001, "")),
        "E0253",
    );
    assert_compiles(&format!(
        "indicator(\"V\")\nif close > open\n{}plot(close)",
        variables(1000, "    ")
    ));
    assert_rejected(
        &format!(
            "indicator(\"V\")\nif close > open\n{}plot(close)",
            variables(1001, "    ")
        ),
        "E0253",
    );
}

#[test]
fn syminfo_current_contract_is_the_hosts_contract_or_na() {
    let source = "//@version=6\nindicator(\"S\")\nstring contract = syminfo.current_contract\n\
                  int length = na(contract) ? 0 : str.length(contract)\n\
                  if barstate.islast and length != {length}\n\
                  \x20   runtime.error(\"unexpected contract\")\n\
                  plot(na(contract) ? 0 : 1)\n";
    let closes = [101.0, 99.0, 102.5];
    let market = |contract: &str| MarketData {
        open: vec![100.0; closes.len()],
        high: closes.iter().map(|c| c + 1.0).collect(),
        low: closes.iter().map(|c| c - 2.0).collect(),
        close: closes.to_vec(),
        volume: vec![1000.0; closes.len()],
        time: (0..closes.len() as i64)
            .map(|bar| 1_700_000_000_000 + bar * 86_400_000)
            .collect(),
        syminfo: SymInfo {
            current_contract: contract.to_string(),
            ..SymInfo::default()
        },
        ..MarketData::default()
    };
    for (contract, present) in [("", 0.0), ("CME_MINI:ESZ2026", 1.0)] {
        let source = source.replace("{length}", &contract.len().to_string());
        let result = compile_and_run(
            &source,
            &market(contract),
            CompileOptions::default(),
            &RunOptions::default(),
        );
        let compiled = result.compile.clone().expect("compile result");
        assert!(compiled.success, "{:?}", result.diagnostics);
        assert!(
            result.runtime_errors.is_empty(),
            "{contract}: {:?}",
            result.runtime_errors
        );
        assert_eq!(
            serde_json::to_value(&result.plots[0].values).unwrap(),
            serde_json::json!([present, present, present]),
            "{contract}"
        );
    }
}
