//! Differential witness for the flow-alias (argument-copy elision) and
//! jump-threading passes: each program runs on the VM twice, once as
//! compiled and once as its unoptimized lowering (the same lowering
//! before any optimization pass), and the observable results must agree
//! exactly while the optimized program executes fewer instructions.

use pinewasm_compiler::{compile, run_program, CompileOptions, MarketData, Program, RunOptions};

fn market(bars: usize) -> MarketData {
    let close: Vec<f64> = (0..bars)
        .map(|bar| 100.0 + ((bar * 37 % 23) as f64 - 11.0) * 0.75 + bar as f64 * 0.1)
        .collect();
    let open: Vec<f64> = close
        .iter()
        .enumerate()
        .map(|(bar, c)| c + if bar % 3 == 0 { -0.5 } else { 0.4 })
        .collect();
    MarketData {
        high: close
            .iter()
            .zip(&open)
            .map(|(c, o)| c.max(*o) + 1.0)
            .collect(),
        low: close
            .iter()
            .zip(&open)
            .map(|(c, o)| c.min(*o) - 1.0)
            .collect(),
        volume: vec![1000.0; bars],
        time: (0..bars as i64)
            .map(|bar| 1_700_000_000_000 + bar * 60_000)
            .collect(),
        open,
        close,
        ..MarketData::default()
    }
}

/// The program as the unoptimized lowering produced it.
fn unoptimized_of(program: &Program) -> Program {
    let mut reference = program.clone();
    let main = &mut reference.functions[0];
    let budget = main
        .budget_reference
        .take()
        .expect("the optimizer changed this program");
    main.code = budget.code;
    main.source_map = budget.source_map;
    main.max_stack = budget.max_stack;
    pinewasm_csil::validate(&reference).expect("unoptimized program validates");
    reference
}

fn observable(result: &pinewasm_compiler::RunResult) -> serde_json::Value {
    serde_json::json!({
        "errors": result.runtime_errors,
        "plots": result.plots,
        "visuals": result.visuals,
        "bars": result.bars_executed,
        "logs": result.logs,
        "alerts": result.alerts,
        "strategy": result.strategy,
    })
}

fn executed(result: &pinewasm_compiler::RunResult) -> f64 {
    result
        .profile
        .as_ref()
        .expect("profile requested")
        .count
        .iter()
        .sum()
}

/// Compile `source`, run it and its unoptimized form, and return how many fewer
/// instructions the optimized program executed.
fn assert_equivalent(name: &str, source: &str) -> f64 {
    let compiled = compile(source, CompileOptions::default());
    assert!(compiled.success, "{name}: {:?}", compiled.diagnostics);
    let program = compiled.program.expect("program");
    let reference = unoptimized_of(&program);
    let market = market(120);
    let options = RunOptions {
        profile: true,
        ..RunOptions::default()
    };
    let optimized_run = run_program(&program, &market, &options);
    let reference_run = run_program(&reference, &market, &options);
    assert!(
        optimized_run.runtime_errors.is_empty(),
        "{name}: {:?}",
        optimized_run.runtime_errors
    );
    assert_eq!(
        observable(&optimized_run),
        observable(&reference_run),
        "{name}: optimized and unoptimized runs differ"
    );
    let saved = executed(&reference_run) - executed(&optimized_run);
    assert!(saved >= 0.0, "{name}: optimized program executed more");
    saved
}

const HISTORY_PARAM: &str = r#"//@version=6
indicator("history-indexed parameter")
f(x) => x + nz(x[1]) - nz(x[2], 1)
s = 0.0
for i = 0 to 4
    v = close + i
    s += f(v)
plot(s)
"#;

const VAR_IN_CALLEE: &str = r#"//@version=6
indicator("var in callee")
acc(x) =>
    var total = 0.0
    total += x
    total
t = 0.0
for i = 0 to 3
    t += acc(i * close)
plot(t)
"#;

const OBJECT_AND_ARRAY_ARGS: &str = r#"//@version=6
indicator("object and array arguments")
type Features
    float a
    float b
dist(Features p, array<float> arr, int i, bool useB) =>
    d = math.abs(p.a - array.get(arr, i))
    if useB
        d += math.log(1 + math.abs(p.b - array.get(arr, i)))
    d
bump(Features p, array<float> arr) =>
    p.a := p.a + 1
    array.push(arr, p.a)
var arr = array.new_float(0)
array.push(arr, close)
pt = Features.new(close, open)
total = 0.0
for i = 0 to array.size(arr) - 1
    total += dist(pt, arr, i, i % 2 == 0)
    if i == 3
        bump(pt, arr)
        total += pt.a
if array.size(arr) > 40
    array.shift(arr)
plot(total)
"#;

const NESTED_CALLS: &str = r#"//@version=6
indicator("nested inlined calls")
inner(x, y) => x * y + y[1]
mid(a, b, c) => inner(a, b) + inner(b, c)
outer(i, s) =>
    r = mid(i, s, i + 1)
    r > 0 ? r : -r
acc = 0.0
for i = 0 to 5
    acc += outer(i, close)
k = 0
while k < 3
    acc += outer(k, open)
    k += 1
plot(acc)
"#;

const REASSIGNED_ARGUMENT: &str = r#"//@version=6
indicator("argument reassigned after the call")
twice(p) => p * 2
x = close
y = 0.0
for i = 0 to 4
    y += twice(x)
    x := x + i
    if y > 1000
        break
    if i == 1
        continue
    y += twice(x)
plot(y)
plot(x)
"#;

const TERNARY_TEMP: &str = r#"//@version=6
indicator("ternary temporary")
pick(flag, a, b) => flag ? a : b
s = 0.0
useDown = input.bool(false, "Down")
for i = 0 to 7
    isDown = useDown ? i % 4 == 0 : bool(i % 4)
    s += pick(isDown, close, open)
plot(s)
"#;

const STRATEGY_LOOP: &str = r#"//@version=6
strategy("loop strategy", overlay = true)
score(i, src) => src - src[i]
s = 0.0
for i = 1 to 5
    s += score(i, close)
if s > 0
    strategy.entry("L", strategy.long)
if s < 0
    strategy.close("L")
plot(s)
"#;

#[test]
fn optimized_programs_match_their_unoptimized_lowering() {
    let mut total_saved = 0.0;
    for (name, source) in [
        ("history param", HISTORY_PARAM),
        ("var in callee", VAR_IN_CALLEE),
        ("object and array args", OBJECT_AND_ARRAY_ARGS),
        ("nested calls", NESTED_CALLS),
        ("reassigned argument", REASSIGNED_ARGUMENT),
        ("ternary temp", TERNARY_TEMP),
        ("strategy loop", STRATEGY_LOOP),
    ] {
        total_saved += assert_equivalent(name, source);
    }
    assert!(total_saved > 0.0);
}

/// The inlined argument copies of a helper called in a loop are gone from
/// the loop: its parameters read the caller's slots directly.
#[test]
fn loop_call_arguments_are_not_copied_per_iteration() {
    let source = r#"//@version=6
indicator("loop call")
type Feat
    float f1
    float f2
d(int i, Feat fs, array<float> arrA, array<float> arrB, bool u1, bool u2) =>
    dist = 0.0
    if u1
        dist += math.abs(fs.f1 - array.get(arrA, i))
    if u2
        dist += math.abs(fs.f2 - array.get(arrB, i))
    dist
var arrA = array.new_float(0)
var arrB = array.new_float(0)
array.push(arrA, close)
array.push(arrB, open)
fs = Feat.new(close, open)
u1 = input.bool(true, "u1")
u2 = input.bool(false, "u2")
s = 0.0
for i = 0 to array.size(arrA) - 1
    s += d(i, fs, arrA, arrB, u1, u2)
plot(s)
"#;
    let saved = assert_equivalent("loop call", source);
    // Six argument copies (two instructions each) per iteration, plus the
    // jumps threading removes, over 120 bars of a growing loop.
    assert!(saved > 12.0 * 120.0, "saved only {saved}");
}
