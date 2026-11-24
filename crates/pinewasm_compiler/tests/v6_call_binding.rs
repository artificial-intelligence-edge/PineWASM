//! Scalar and declaration binding regressions.
use pinewasm_compiler::{compile, CompileOptions};

#[test]
fn reference_scalar_parameter_names_compile() {
    for body in [
        "plot(str.tonumber(string=\"1.25\"))",
        "max_bars_back(var=close, num=20)\nplot(close[1])",
        "max_bars_back(num=20, var=close)\nplot(close[1])",
    ] {
        let result = compile(
            &format!("//@version=6\nindicator(\"Parameter names\")\n{body}\n"),
            CompileOptions::default(),
        );
        assert!(result.success, "{body}: {:?}", result.diagnostics);
    }
}

#[test]
fn undocumented_scalar_parameter_aliases_are_rejected() {
    for body in [
        "plot(str.tonumber(s=\"1.25\"))",
        "max_bars_back(series=close, length=20)\nplot(close)",
    ] {
        let result = compile(
            &format!("//@version=6\nindicator(\"Parameter names\")\n{body}\n"),
            CompileOptions::default(),
        );
        assert!(!result.success, "unexpected acceptance: {body}");
    }
}

#[test]
fn indicator_rejects_strategy_only_tick_parameter() {
    let result = compile("//@version=6\nindicator(\"Invalid indicator parameter\", calc_on_every_tick=true)\nplot(close)\n", CompileOptions::default());
    assert!(!result.success);
    let strategy = compile("//@version=6\nstrategy(\"Valid strategy parameter\", calc_on_every_tick=true)\nplot(close)\n", CompileOptions::default());
    assert!(strategy.success, "{:?}", strategy.diagnostics);
}

#[test]
fn kagi_overloads_bind_their_own_parameter_names() {
    for call in [
        "ticker.kagi(\"ACME\", 3)",
        "ticker.kagi(reversal=3, symbol=\"ACME\")",
        "ticker.kagi(\"ACME\", 3, \"ATR\")",
        "ticker.kagi(style=\"ATR\", param=3, symbol=\"ACME\")",
        "ticker.kagi(\"ACME\", param=3, style=\"ATR\")",
        "ticker.kagi(bar_index > 1 ? \"ACME\" : \"OTHER\", close)",
        "ticker.kagi(symbol=\"ACME\", param=close, style=\"ATR\")",
    ] {
        let result = compile(
            &format!("//@version=6\nindicator(\"Kagi\")\ns = {call}\nplot(str.length(s))\n"),
            CompileOptions::default(),
        );
        assert!(result.success, "{call}: {:?}", result.diagnostics);
    }
    for call in [
        "ticker.kagi(symbol=\"ACME\", param=3)",
        "ticker.kagi(symbol=\"ACME\", reversal=3, style=\"ATR\")",
        "ticker.kagi(symbol=\"ACME\", style=\"ATR\")",
        "ticker.kagi(symbol=\"ACME\", reversal=3, param=3)",
        "ticker.kagi(symbol=\"ACME\", param=true, style=\"ATR\")",
    ] {
        let result = compile(
            &format!("//@version=6\nindicator(\"Kagi\")\ns = {call}\nplot(str.length(s))\n"),
            CompileOptions::default(),
        );
        assert!(!result.success, "unexpected acceptance: {call}");
    }
}

#[test]
fn kagi_result_qualifier_follows_symbol_and_amount_overloads() {
    let prelude = "//@version=6\nindicator(\"Kagi qualifiers\")\n";
    for call in [
        "ticker.kagi(\"ACME\", 3)",
        "ticker.kagi(\"ACME\", 3, close > open ? \"ATR\" : \"Traditional\")",
    ] {
        let result = compile(
            &format!("{prelude}simple string t = {call}\nplot(str.length(t))\n"),
            CompileOptions::default(),
        );
        assert!(result.success, "{call}: {:?}", result.diagnostics);
    }
    for call in [
        "ticker.kagi(\"ACME\", close)",
        "ticker.kagi(\"ACME\", close, \"ATR\")",
    ] {
        let result = compile(
            &format!("{prelude}simple string t = {call}\nplot(str.length(t))\n"),
            CompileOptions::default(),
        );
        assert!(!result.success, "series result accepted as simple: {call}");
    }
    let result = compile("//@version=6\nindicator(\"Kagi const\")\ns = input.string(ticker.kagi(\"ACME\", 3))\nplot(str.length(s))\n", CompileOptions::default());
    assert!(
        !result.success,
        "simple ticker result accepted as const input default"
    );
}

#[test]
fn explicit_user_parameter_qualifiers_constrain_arguments_and_results() {
    for body in [
        "f(simple string s) => str.length(s)\nplot(f(close > open ? \"up\" : \"down\"))",
        "f(const int n) => n\nplot(f(input.int(2)))",
        "f(series string s) => s\nsimple string t = f(\"ACME\")\nplot(str.length(t))",
        "f(simple string s) => s\nx = input.string(f(\"ACME\"))\nplot(str.length(x))",
    ] {
        let result = compile(
            &format!("//@version=6\nindicator(\"Parameter qualifiers\")\n{body}\n"),
            CompileOptions::default(),
        );
        assert!(!result.success, "unexpected acceptance: {body}");
    }
    for body in [
        "f(simple string s) => str.length(s)\nplot(f(input.string(\"ACME\")))",
        "f(const int n) => n\nplot(f(2))",
        "f(series float s) => s\nplot(f(close))",
    ] {
        let result = compile(
            &format!("//@version=6\nindicator(\"Parameter qualifiers\")\n{body}\n"),
            CompileOptions::default(),
        );
        assert!(result.success, "{body}: {:?}", result.diagnostics);
    }
}

#[test]
fn declared_user_parameter_types_reject_wrong_argument_types() {
    for body in [
        "f(int n) => 1\nplot(f(1.5))",
        "f(float n) => 1\nplot(f(true))",
        "f(string s) => 1\nplot(f(3))",
        "f(array<float> a) => 1\nplot(f(array.from(1, 2)))",
        "f(matrix<float> m) => 1\nplot(f(array.from(1.0)))",
        "f(line l) => 1\nplot(f(label.new(0, 0)))",
        "f(x) => 1\nplot(f(log.info(\"void\")))",
    ] {
        let result = compile(
            &format!("//@version=6\nindicator(\"Parameter types\")\n{body}\n"),
            CompileOptions::default(),
        );
        assert!(!result.success, "unexpected acceptance: {body}");
    }
}

#[test]
fn declared_user_parameters_accept_widening_typed_na_and_matching_references() {
    for body in [
        "f(float n) => n\nplot(f(2))",
        "f(float n=na) => na(n) ? 1 : 0\nplot(f())",
        "f(array<float> a) => array.size(a)\nplot(f(array.from(1.0, 2.0)))",
        "f(line l) => na(l) ? 1 : 0\nplot(f(na))",
        "f(line l) => 1\nplot(f(line.new(0, 0, 1, 1)))",
    ] {
        let result = compile(
            &format!("//@version=6\nindicator(\"Parameter types\")\n{body}\n"),
            CompileOptions::default(),
        );
        assert!(result.success, "{body}: {:?}", result.diagnostics);
    }
}

#[test]
fn typed_user_arguments_execute_widening_defaults_and_reference_aliasing() {
    use pinewasm_compiler::{compile_and_run, MarketData, RunOptions};
    let source = "//@version=6\nindicator(\"Typed values\")\nf(float n) => n + 0.25\ng(float n=na) => na(n) ? 7.0 : n\nappend(array<float> a, float v) =>\n    array.push(a, v)\n    array.sum(a)\na = array.from(1.0)\nx = append(a, 2)\nplot(f(2))\nplot(g())\nplot(x)\nplot(array.sum(a))\n";
    let market = MarketData {
        open: vec![1.0; 2],
        high: vec![2.0; 2],
        low: vec![0.0; 2],
        close: vec![1.0; 2],
        volume: vec![1.0; 2],
        ..MarketData::default()
    };
    let result = compile_and_run(
        source,
        &market,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        result.compile.as_ref().is_some_and(|c| c.success),
        "{:?}",
        result.diagnostics
    );
    assert!(
        result.runtime_errors.is_empty(),
        "{:?}",
        result.runtime_errors
    );
    let actual: Vec<_> = result
        .plots
        .iter()
        .map(|p| serde_json::to_value(&p.values).unwrap())
        .collect();
    assert_eq!(
        actual,
        vec![
            serde_json::json!([2.25, 2.25]),
            serde_json::json!([7.0, 7.0]),
            serde_json::json!([3.0, 3.0]),
            serde_json::json!([3.0, 3.0])
        ]
    );
}

#[test]
fn invalid_parameter_declarations_fail_even_when_the_function_is_unused() {
    for definition in [
        "f(simple x) => x",
        "f(input int x) => x",
        "f(x=na) => x",
        "f(x=(na)) => x",
        "method f(array<float> a, simple x) => x",
    ] {
        let result = compile(
            &format!("//@version=6\nindicator(\"Declarations\")\n{definition}\nplot(close)\n"),
            CompileOptions::default(),
        );
        assert!(!result.success, "unexpected acceptance: {definition}");
    }
}

#[test]
fn library_export_parameter_declarations_are_checked_before_calls() {
    use pinewasm_compiler::compile_with_libraries;
    for definition in ["export f(x) => x", "export f(const int x) => x"] {
        let library = format!("//@version=6\nlibrary(\"Bad\")\n{definition}\n");
        let local = compile(&library, CompileOptions::default());
        assert!(
            !local.success,
            "invalid standalone library accepted: {definition}"
        );
        let imported = compile_with_libraries(
            "//@version=6\nindicator(\"Import\")\nimport me/Bad/1 as bad\nplot(close)\n",
            &[("me/Bad/1".to_string(), library)],
            CompileOptions::default(),
        );
        assert!(
            !imported.success,
            "invalid unused import accepted: {definition}"
        );
    }
}

#[test]
fn log_message_overload_accepts_its_reference_parameter_name() {
    for level in ["info", "warning", "error"] {
        let source = format!(
            "//@version=6\nindicator(\"Logs\")\nlog.{level}(message=\"hello\")\nplot(close)\n"
        );
        let result = compile(&source, CompileOptions::default());
        assert!(result.success, "{source}: {:?}", result.diagnostics);
    }
}

#[test]
fn log_substitutions_reject_color_values() {
    for level in ["info", "warning", "error"] {
        let source = format!(
            "//@version=6\nindicator(\"Logs\")\nlog.{level}(\"{{0}}\", color.red)\nplot(close)\n"
        );
        let result = compile(&source, CompileOptions::default());
        assert!(!result.success, "unexpected acceptance: {source}");
    }
}

#[test]
fn log_message_binding_preserves_runtime_levels_and_text() {
    use pinewasm_compiler::{compile_and_run, MarketData, RunOptions};
    let source = "//@version=6\nindicator(\"Logs\")\nlog.info(message=\"hello\")\nlog.warning(message=\"warning\")\nlog.error(message=\"error\")\nplot(close)\n";
    let market = MarketData {
        open: vec![1.0],
        high: vec![2.0],
        low: vec![0.0],
        close: vec![1.0],
        volume: vec![1.0],
        ..MarketData::default()
    };
    let result = compile_and_run(
        source,
        &market,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        result.compile.as_ref().is_some_and(|c| c.success),
        "{:?}",
        result.diagnostics
    );
    assert!(
        result.runtime_errors.is_empty(),
        "{:?}",
        result.runtime_errors
    );
    assert_eq!(
        result
            .logs
            .iter()
            .map(|entry| entry.message.as_str())
            .collect::<Vec<_>>(),
        ["hello", "warning", "error"]
    );
    assert_eq!(
        result
            .logs
            .iter()
            .map(|entry| entry.level.member())
            .collect::<Vec<_>>(),
        ["info", "warning", "error"]
    );
    // A sole argument may use either overload's name; substitution
    // arguments are positional-only, so `message` cannot take them.
    let format_string = compile(
        "//@version=6\nindicator(\"Format string name\")\nlog.info(formatString=\"hello\")\nplot(close)\n",
        CompileOptions::default(),
    );
    assert!(format_string.success, "{:?}", format_string.diagnostics);
    for call in [
        "log.info(text=\"hello\")",
        "log.info(message=\"{0}\", arg0=1)",
    ] {
        let result = compile(
            &format!("//@version=6\nindicator(\"Invalid log name\")\n{call}\nplot(close)\n"),
            CompileOptions::default(),
        );
        assert!(!result.success, "unexpected acceptance: {call}");
    }
}

#[test]
fn string_formatting_accepts_series_format_strings() {
    for expression in [
        "str.tostring(close, bar_index % 2 == 0 ? \"#.0\" : \"#.00\")",
        "str.format(bar_index % 2 == 0 ? \"x={0}\" : \"y={0}\", close)",
    ] {
        let source =
            format!("//@version=6\nindicator(\"Series format\")\nplot(str.length({expression}))\n");
        let result = compile(&source, CompileOptions::default());
        assert!(result.success, "{source}: {:?}", result.diagnostics);
    }
}

#[test]
fn string_formatting_rejects_invalid_overloads_and_const_results() {
    for body in [
        "plot(str.length(str.tostring(true, \"#\")))",
        "plot(str.length(str.tostring(\"text\", \"#\")))",
        "plot(str.length(str.format(\"{0}\")))",
        "s = input.string(str.format(\"{0}\", 1))\nplot(str.length(s))",
        "s = input.string(str.tostring(1))\nplot(str.length(s))",
    ] {
        let result = compile(
            &format!("//@version=6\nindicator(\"Format contract\")\n{body}\n"),
            CompileOptions::default(),
        );
        assert!(!result.success, "unexpected acceptance: {body}");
    }
}

#[test]
fn series_format_strings_are_evaluated_on_each_bar() {
    use pinewasm_compiler::{compile_and_run, MarketData, RunOptions};
    let source = "//@version=6\nindicator(\"Per-bar formatting\")\nf = bar_index == 0 ? \"#.00\" : \"#.0\"\nprefix = bar_index == 0 ? \"x={0}\" : \"y={0}\"\nlog.info(\"{0}|{1}\", str.tostring(close, f), str.format(prefix, close))\nplot(close)\n";
    let market = MarketData {
        open: vec![1.25, 2.5],
        high: vec![2.0, 3.0],
        low: vec![1.0, 2.0],
        close: vec![1.25, 2.5],
        volume: vec![1.0; 2],
        ..MarketData::default()
    };
    let result = compile_and_run(
        source,
        &market,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        result.compile.as_ref().is_some_and(|c| c.success),
        "{:?}",
        result.diagnostics
    );
    assert!(
        result.runtime_errors.is_empty(),
        "{:?}",
        result.runtime_errors
    );
    assert_eq!(
        result
            .logs
            .iter()
            .map(|entry| entry.message.as_str())
            .collect::<Vec<_>>(),
        ["1.25|x=1.25", "2.5|y=2.5"]
    );
}

#[test]
fn format_time_accepts_series_options_and_returns_series() {
    for body in [
        "s = str.format_time(0, bar_index == 0 ? \"yyyy\" : \"MM\", bar_index == 0 ? \"UTC\" : \"UTC+1\")\nplot(str.length(s))",
        "s = str.format_time(timezone=\"UTC\", time=0, format=\"yyyy\")\nplot(str.length(s))",
    ] {
        let result = compile(&format!("//@version=6\nindicator(\"Time format\")\n{body}\n"), CompileOptions::default());
        assert!(result.success, "{body}: {:?}", result.diagnostics);
    }
    for body in [
        "f(simple string x) => str.length(x)\nplot(f(str.format_time(0)))",
        "plot(str.length(str.format_time(0, 1)))",
        "plot(str.length(str.format_time(0, \"yyyy\", 1)))",
    ] {
        let result = compile(
            &format!("//@version=6\nindicator(\"Time format\")\n{body}\n"),
            CompileOptions::default(),
        );
        assert!(!result.success, "unexpected acceptance: {body}");
    }
}
