//! Shadowing, copy, fill and sort rules covered by these tests:
//!
//! - a variable may shadow a built-in variable, with a warning, unless the
//!   script also uses the name for the built-in;
//! - `obj.copy()` is the method form of `Type.copy(obj)`, and an unset `bool`
//!   field of a user-defined type is `false`;
//! - `array.fill`'s `index_to` defaults to `na`, which fills to the end;
//! - `array.sort` sorts string arrays, and `na` elements and empty strings
//!   go to the end of an ascending sort and the beginning of a descending one.

use pinewasm_compiler::{
    compile, compile_and_run, CompileOptions, CompileResult, MarketData, RunOptions,
};

fn market() -> MarketData {
    let closes = [101.0, 99.0, 102.5];
    MarketData {
        open: vec![100.0; closes.len()],
        high: closes.iter().map(|c| c + 1.0).collect(),
        low: closes.iter().map(|c| c - 2.0).collect(),
        close: closes.to_vec(),
        volume: vec![1000.0; closes.len()],
        time: (0..closes.len() as i64)
            .map(|bar| 1_700_000_000_000 + bar * 86_400_000)
            .collect(),
        ..MarketData::default()
    }
}

fn source(body: &str) -> String {
    format!("//@version=6\nindicator(\"Front-end rules\")\n{body}\n")
}

/// Compile and run `body`, which signals a wrong result with `runtime.error`,
/// and return the values of its first plot and the compile diagnostics.
fn run(body: &str) -> (serde_json::Value, CompileResult) {
    let source = source(body);
    let result = compile_and_run(
        &source,
        &market(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    let compiled = result.compile.clone().expect("compile result");
    assert!(compiled.success, "{source}\n{:?}", result.diagnostics);
    assert!(
        result.runtime_errors.is_empty(),
        "{source}\n{:?}",
        result.runtime_errors
    );
    (
        serde_json::to_value(&result.plots[0].values).unwrap(),
        compiled,
    )
}

fn has(result: &CompileResult, code: &str) -> bool {
    result
        .diagnostics
        .iter()
        .any(|d| d.code.as_deref() == Some(code))
}

#[test]
fn a_variable_may_shadow_a_built_in_with_a_warning() {
    let (values, compiled) = run("float close = open\nvolume = 5.0\nplot(close + volume)");
    assert_eq!(values, serde_json::json!([105.0, 105.0, 105.0]));
    assert!(has(&compiled, "W0006"), "{:?}", compiled.diagnostics);
    // Inside a local scope the shadow ends with the scope's last statement.
    let (values, _) = run("x = 0.0\n\
         if bar_index >= 0\n\
         \x20   high = low\n\
         \x20   x := high\n\
         plot(x)");
    assert_eq!(values, serde_json::json!([99.0, 97.0, 100.5]));
}

#[test]
fn a_variable_that_shadows_a_built_in_may_be_reassigned() {
    let (values, compiled) = run("var float volume = 0.0\nvolume := volume + 1\nplot(volume)");
    assert_eq!(values, serde_json::json!([1.0, 2.0, 3.0]));
    assert!(has(&compiled, "W0006"), "{:?}", compiled.diagnostics);
    let (values, _) = run("float high = 0.0\nhigh += 2\nplot(high)");
    assert_eq!(values, serde_json::json!([2.0, 2.0, 2.0]));
}

#[test]
fn an_input_source_follows_shadowing() {
    let codes = |body: &str| -> Vec<String> {
        compile(&source(body), CompileOptions::default())
            .diagnostics
            .iter()
            .filter(|d| d.is_error())
            .filter_map(|d| d.code.clone())
            .collect()
    };
    // A script variable named `close` is not a built-in source: the generic
    // `input` reads its constant value, and `input.source` rejects it.
    assert!(!codes("float close = 0.0\ns = input.source(close)\nplot(s)").is_empty());
    let (values, _) = run("float close = 0.0\ns = input(close)\nplot(s)");
    assert_eq!(values, serde_json::json!([0.0, 0.0, 0.0]));
    // A built-in source used as a default is a use of the built-in, so a
    // later declaration shadowing it is E0211.
    for body in [
        "s = input.source(close)\nfloat close = 0.0\nplot(s + close)",
        "s = input(close)\nfloat close = 0.0\nplot(s + close)",
    ] {
        assert!(
            codes(body).contains(&"E0211".to_string()),
            "{body}: {:?}",
            codes(body)
        );
    }
    let (values, _) = run("s = input.source(close)\nplot(s)");
    assert_eq!(values, serde_json::json!([101.0, 99.0, 102.5]));
}

#[test]
fn the_compiler_still_reads_a_shadowed_built_in_for_its_own_defaults() {
    // `chart.point.now()` reads the built-in `close`, not the script's variable.
    let (values, _) = run("float close = open\n\
         p = chart.point.now()\n\
         plot(p.price)");
    assert_eq!(values, serde_json::json!([101.0, 99.0, 102.5]));
}

#[test]
fn a_built_in_cannot_be_both_used_and_shadowed() {
    for body in [
        "ma = ta.sma(close, 2)\nfloat close = open\nplot(ma)",
        "float close = open\nf() => hl2\nplot(f() + close)\nhl2 = 1.0",
        "if bar_index > 0\n    float high = low\nplot(high)",
    ] {
        let result = compile(&source(body), CompileOptions::default());
        assert!(!result.success, "accepted:\n{body}");
        assert!(has(&result, "E0211"), "{body}\n{:?}", result.diagnostics);
    }
}

#[test]
fn copy_is_a_method_of_user_defined_types() {
    let (values, _) = run("type Pair\n\
         \x20   float a\n\
         \x20   float b\n\
         first = Pair.new(close, open)\n\
         second = first.copy()\n\
         second.a := 0\n\
         if first.a != close or first.copy().b != open\n\
         \x20   runtime.error(\"copy shares or loses fields\")\n\
         plot(second.a + second.b)");
    assert_eq!(values, serde_json::json!([100.0, 100.0, 100.0]));
}

#[test]
fn array_fill_with_an_na_end_fills_to_the_end() {
    run("a = array.from(1, 2, 3, 4)\n\
         a.fill(9, 2, na)\n\
         b = array.from(1, 2, 3, 4)\n\
         b.fill(7, 1, int(na))\n\
         c = array.from(1, 2, 3, 4)\n\
         c.fill(5, 1)\n\
         if a.get(1) != 2 or a.get(2) != 9 or a.get(3) != 9\n\
         \x20   runtime.error(\"bare na\")\n\
         if b.get(0) != 1 or b.get(3) != 7 or c.get(3) != 5\n\
         \x20   runtime.error(\"typed na or omitted end\")\n\
         plot(close)");
}

#[test]
fn an_unset_bool_field_is_false() {
    run("type Flags\n\
         \x20   bool seen\n\
         \x20   float level\n\
         f = Flags.new(level = close)\n\
         if f.seen or f.seen != false or not (not f.seen)\n\
         \x20   runtime.error(\"an unset bool field is not false\")\n\
         plot(f.level)");
}

#[test]
fn sorting_moves_na_and_empty_strings_by_direction() {
    // `array.join` writes an `na` element as "NaN"; missing elements keep
    // their relative order (the sort is stable).
    run("words = array.from(\"pear\", \"\", \"apple\", \"fig\")\n\
         string missing = na\n\
         words.push(missing)\n\
         up = words.copy()\n\
         up.sort()\n\
         down = words.copy()\n\
         down.sort(order.descending)\n\
         if up.join(\",\") != \"apple,fig,pear,,NaN\" or down.join(\",\") != \",NaN,pear,fig,apple\"\n\
         \x20   runtime.error(up.join(\",\") + \" / \" + down.join(\",\"))\n\
         if up.get(4) != \"\" and not na(up.get(4))\n\
         \x20   runtime.error(\"the missing elements are not last\")\n\
         idx = words.sort_indices()\n\
         if idx.get(0) != 2 or idx.get(1) != 3 or idx.get(2) != 0\n\
         \x20   runtime.error(\"sort_indices disagrees with sort\")\n\
         numbers = array.from(3.0, 1.0, 2.0)\n\
         numbers.push(na)\n\
         numbers.sort(order.descending)\n\
         if not na(numbers.get(0)) or numbers.get(1) != 3 or numbers.get(3) != 1\n\
         \x20   runtime.error(\"descending na is not first\")\n\
         plot(close)");
}
