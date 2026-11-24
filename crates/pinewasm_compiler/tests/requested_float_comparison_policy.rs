//! Supported requested-context comparisons and their explicit boundaries.

use pinewasm_compiler::{compile, compile_and_run, CompileOptions, MarketData, RunOptions, Value};
use sha2::{Digest, Sha256};

const BARS: usize = 9;
const START: i64 = 1_704_067_200_000; // 2024-01-01 00:00 UTC, on a 15-minute boundary.
const FIVE_MINUTES: i64 = 300_000;

fn feed() -> MarketData {
    let time: Vec<_> = (0..BARS)
        .map(|bar| START + bar as i64 * FIVE_MINUTES)
        .collect();
    let time_close: Vec<_> = time.iter().map(|open| open + FIVE_MINUTES).collect();
    let requested_close: Vec<_> = (0..BARS)
        .map(|bar| START + (bar / 3 + 1) as i64 * 3 * FIVE_MINUTES)
        .collect();
    MarketData {
        open: vec![100.0; BARS],
        high: vec![100.0; BARS],
        low: vec![100.0; BARS],
        close: vec![100.0; BARS],
        volume: vec![1.0; BARS],
        time,
        time_close: time_close.clone(),
        timeframe: "5".into(),
        request_time_close: [("5".into(), time_close), ("15".into(), requested_close)].into(),
        ..MarketData::default()
    }
}

fn run(source: &str) -> Vec<Vec<Value>> {
    let result = compile_and_run(
        source,
        &feed(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        result
            .diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.is_error()),
        "{source}: {:?}",
        result.diagnostics
    );
    assert!(
        result.runtime_errors.is_empty(),
        "{source}: {:?}",
        result.runtime_errors
    );
    assert_eq!(
        result.bars_executed,
        u32::try_from(BARS).expect("bounded bar count")
    );
    result.plots.into_iter().map(|plot| plot.values).collect()
}

fn flags(value: bool, available_from: usize) -> Vec<Value> {
    (0..BARS)
        .map(|bar| {
            Value::Float(if value && bar >= available_from {
                1.0
            } else {
                0.0
            })
        })
        .collect()
}

#[test]
fn supported_same_and_higher_contexts_use_script_float_comparisons() {
    // The cases below check == at an epsilon of 5e-11 in the main, same and
    // 15-minute contexts. This literal numeric projection tests the supported
    // HTF subset; the six operator cases cover each comparison operator. Scalar
    // input binding remains an E0244 admission limit, tested separately below.
    let cases = [
        (
            "close*0.0+0.00000000005",
            "0.0",
            [true, false, false, true, false, true],
        ),
        (
            "close*0.0+0.00000000015",
            "0.0",
            [false, true, false, false, true, true],
        ),
        (
            "close*0.0-0.00000000015",
            "0.0",
            [false, true, true, true, false, false],
        ),
        (
            "close*0.0+1.0000000001",
            "1.0",
            [false, true, false, false, true, true],
        ),
    ];
    for (lhs, rhs, expected) in cases {
        let mut source =
            "//@version=6\nindicator(\"Requested float comparison policy\")\n".to_string();
        for op in ["==", "!=", "<", "<=", ">", ">="] {
            let expression = format!("{lhs}{op}{rhs}");
            source.push_str(&format!(
                "plot(({expression})?1:0)\nplot(request.security(syminfo.tickerid,timeframe.period,{expression})?1:0)\nplot(request.security(syminfo.tickerid,\"15\",{expression})?1:0)\n"
            ));
        }
        let plots = run(&source);
        assert_eq!(plots.len(), 18);
        for (operator, expected) in expected.into_iter().enumerate() {
            assert_eq!(
                plots[operator * 3],
                flags(expected, 0),
                "main {operator}: {lhs}"
            );
            assert_eq!(
                plots[operator * 3 + 1],
                flags(expected, 0),
                "same {operator}: {lhs}"
            );
            // Explicit 5-minute closes confirm 15-minute buckets at bars2,5,8.
            assert_eq!(
                plots[operator * 3 + 2],
                flags(expected, 2),
                "higher {operator}: {lhs}"
            );
        }
    }
}

#[test]
fn missing_requested_operands_preserve_false_conditions_for_every_operator() {
    let expected = [true, false, false, true, false, true];
    let mut source =
        "//@version=6\nindicator(\"Requested missing comparison operands\")\n".to_string();
    for op in ["==", "!=", "<", "<=", ">", ">="] {
        let expression = format!("close[1]*0.0+0.00000000005{op}0.0");
        source.push_str(&format!(
            "plot(({expression})?1:0)\nplot(request.security(syminfo.tickerid,timeframe.period,{expression})?1:0)\nplot(request.security(syminfo.tickerid,\"15\",{expression})?1:0)\n"
        ));
    }
    let plots = run(&source);
    for (operator, expected) in expected.into_iter().enumerate() {
        assert_eq!(plots[operator * 3], flags(expected, 1), "main {operator}");
        assert_eq!(
            plots[operator * 3 + 1],
            flags(expected, 1),
            "same {operator}"
        );
        // The first HTF history operand is absent. The second bucket's
        // comparison first becomes available at its confirmed close, bar5.
        assert_eq!(
            plots[operator * 3 + 2],
            flags(expected, 5),
            "higher {operator}"
        );
    }
}

#[test]
fn tolerance_does_not_change_gaps_or_lookahead_publication_clocks() {
    let source = "//@version=6\nindicator(\"Requested comparison clocks\")\nplot(request.security(syminfo.tickerid,\"15\",close*0.0+0.00000000005==0.0)?1:0)\nplot(request.security(syminfo.tickerid,\"15\",close*0.0+0.00000000005==0.0,gaps=barmerge.gaps_on)?1:0)\nplot(request.security(syminfo.tickerid,\"15\",close*0.0+0.00000000005==0.0,lookahead=barmerge.lookahead_on)?1:0)\nplot(request.security(syminfo.tickerid,\"15\",close*0.0+0.00000000005==0.0,gaps=barmerge.gaps_on,lookahead=barmerge.lookahead_on)?1:0)\n";
    let plots = run(source);
    assert_eq!(plots[0], flags(true, 2));
    assert_eq!(
        plots[1],
        [0., 0., 1., 0., 0., 1., 0., 0., 1.].map(Value::Float)
    );
    assert_eq!(plots[2], flags(true, 0));
    assert_eq!(
        plots[3],
        [1., 0., 0., 1., 0., 0., 1., 0., 0.].map(Value::Float)
    );
}

#[test]
fn existing_float_only_requested_integer_precision_boundary_is_not_represented_as_exact() {
    // Known limitation: HtfExpr stores numeric literals as f64, so requested
    // integers above 2^53 are not exact. The test keeps that limitation
    // explicit.
    let source = "//@version=6\nindicator(\"Requested integer precision boundary\")\nplot(9007199254740993>9007199254740992?1:0)\nplot(request.security(syminfo.tickerid,timeframe.period,9007199254740993>9007199254740992)?1:0)\nplot(request.security(syminfo.tickerid,\"15\",9007199254740993>9007199254740992)?1:0)\n";
    let plots = run(source);
    assert_eq!(plots[0], flags(true, 0));
    assert_eq!(plots[1], flags(false, 0));
    assert_eq!(plots[2], flags(false, 0));
}

// Inline test source for the E0244 admission check. Scalar input binding
// remains unsupported by the requested-expression representation, so the
// compiler rejects this source. It is kept as an explicit admission-gap marker.
const SCALAR_INPUT_ADMISSION_GAP_SOURCE: &str = r##"//@version=6
indicator("Requested comparison and BBW units", overlay=false)
float eps = input.float(0.00000000005, "epsilon")
bool main = close * 0.0 + eps == 0.0
bool same = request.security(syminfo.tickerid, timeframe.period, close * 0.0 + eps == 0.0)
bool higher = request.security(syminfo.tickerid, "15", close * 0.0 + eps == 0.0)
float x = 10.0 * (bar_index + 1)
[basis, upper, lower] = ta.bb(x, 3, 2.0)
float width = ta.bbw(x, 3, 2.0)
if bar_index == 9
    runtime.error("MARKER8 | main=" + str.tostring(main) + ";same=" + str.tostring(same) + ";higher=" + str.tostring(higher) + ";bbw-factor=" + str.tostring(width / ((upper-lower)/basis), "#.##########"))
plot(width)
"##;

#[test]
fn scalar_input_requested_comparison_is_rejected_with_e0244() {
    assert_eq!(
        Sha256::digest(SCALAR_INPUT_ADMISSION_GAP_SOURCE.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
        "8446b1e1d2b5b449d3b45ad6e491b69f40a1fcb5a815ccb44b3d98a572831375"
    );
    let result = compile(SCALAR_INPUT_ADMISSION_GAP_SOURCE, CompileOptions::default());
    assert!(!result.success);
    let errors: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.is_error())
        .collect();
    assert_eq!(errors.len(), 2, "{:?}", result.diagnostics);
    assert!(
        errors
            .iter()
            .all(|diagnostic| diagnostic.code.as_deref() == Some("E0244")),
        "{:?}",
        result.diagnostics
    );
}

#[test]
fn existing_bbw_percent_units_remain_distinct_from_raw_bandwidth_ratio() {
    // The expected bbw-factor is 100: this numeric projection checks that BBW
    // keeps its percent units.
    let source = "//@version=6\nindicator(\"BBW percent preservation\")\nx=10.0*(bar_index+1)\n[basis,upper,lower]=ta.bb(x,3,2.0)\nwidth=ta.bbw(x,3,2.0)\nplot(width/((upper-lower)/basis))\n";
    let plots = run(source);
    assert_eq!(&plots[0][..2], &[Value::Na, Value::Na]);
    for value in &plots[0][2..] {
        let Value::Float(value) = value else {
            panic!("missing warmed-up BBW ratio")
        };
        assert!((*value - 100.0).abs() < 1e-10, "BBW factor: {value}");
    }
}
