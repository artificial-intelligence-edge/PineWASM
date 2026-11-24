//! The int forms of `ta.mode`, `ta.range` and `ta.median` (selected by the
//! checker for an `int` source) when that source holds a fraction.
//!
//! `ta.mode` and `ta.range` keep the fraction and equal their float twins.
//! `ta.median` truncates an int source's result to a whole number (see
//! docs/language-support.md, "Integer `ta.median`").

use super::{compile, data_from_close, floats, run};
use crate::RunOptions;
use pinewasm_mir::Value;
use pretty_assertions::assert_eq;

const BARS: usize = 12;

/// The plotted values of `plot(<call>)` where `x` is `source`, after
/// `n = <declaration>`.
fn series(declaration: &str, source: &str, call: &str) -> Vec<Value> {
    let program = compile(&format!(
        "//@version=6\nindicator(\"T\")\nn = {declaration}\nx = {source}\nplot({call})\n"
    ));
    let closes: Vec<f64> = (0..BARS).map(|bar| 10.0 + bar as f64).collect();
    let outcome = run(&program, &data_from_close(&closes), &RunOptions::default());
    assert!(outcome.errors.is_empty(), "{call}: {:?}", outcome.errors);
    outcome.plots[0].values.clone()
}

fn bits(values: &[Value]) -> Vec<Option<u64>> {
    floats(values)
        .into_iter()
        .map(|value| value.map(f64::to_bits))
        .collect()
}

/// Sources that are statically int and hold fractions on some or all bars.
const FRACTIONAL: [&str; 4] = [
    "n / 2 + bar_index",
    "bar_index % 2 == 0 ? n / 2 : bar_index",
    "n / 2 + bar_index % 3",
    "(n + bar_index) / 2 - bar_index % 4",
];

/// Both length forms: a literal, and a run-time length.
const LENGTHS: [&str; 3] = ["3", "4", "bar_index % 2 + 2"];

#[test]
fn mode_and_range_of_a_fractional_int_source_equal_the_float_twin() {
    let mut fractional = 0;
    for func in ["ta.mode", "ta.range"] {
        for source in FRACTIONAL {
            for length in LENGTHS {
                let call = format!("{func}(x, {length})");
                let int = bits(&series("input.int(7, \"N\")", source, &call));
                let float = bits(&series("input.float(7.0, \"N\")", source, &call));
                assert_eq!(int, float, "{call} over {source}");
                fractional += int
                    .iter()
                    .flatten()
                    .filter(|bits| f64::from_bits(**bits).fract() != 0.0)
                    .count();
            }
        }
    }
    // The comparison means something only if fractions reached the plot.
    assert!(fractional > 60, "only {fractional} fractional results");
}

#[test]
fn mode_and_range_of_a_whole_int_source_are_unchanged() {
    // A few hundred pseudo-random whole-int windows: the int form equals the
    // float twin's value (always whole here).
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let mut next = move |modulus: u64| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % modulus
    };
    let mut windows = 0;
    for _ in 0..14 {
        let (a, b, c) = (next(7) + 1, next(11) + 2, next(5) + 2);
        let offset = next(9);
        let source = format!("(bar_index * {a} + n) % {b} - (bar_index / 1 % {c}) + {offset}");
        for func in ["ta.mode", "ta.range"] {
            for length in LENGTHS {
                let call = format!("{func}(x, {length})");
                let int = series("input.int(7, \"N\")", &source, &call);
                let float = series("input.float(7.0, \"N\")", &source, &call);
                for (bar, (int, float)) in int.iter().zip(&float).enumerate() {
                    match (int, float) {
                        (Value::Na, Value::Na) => {}
                        (Value::Float(int), Value::Float(float)) => {
                            // Plots are floats; the value must be whole and
                            // the float twin's.
                            assert_eq!(int.fract(), 0.0, "{call} over {source}, bar {bar}");
                            assert_eq!(int, float, "{call} over {source}, bar {bar}");
                            assert_eq!(*int, float.trunc());
                            windows += 1;
                        }
                        other => panic!("{call} over {source}, bar {bar}: {other:?}"),
                    }
                }
            }
        }
    }
    assert!(windows > 300, "only {windows} windows compared");
}

/// The four int forms whose result can hold the fraction of their source,
/// in both length forms where they have one.
const CARRIERS: [&str; 6] = [
    "ta.mode(x, 3)",
    "ta.range(x, 3)",
    "ta.mode(x, bar_index % 2 + 2)",
    "ta.range(x, bar_index % 2 + 2)",
    "fixnan(x)",
    "ta.valuewhen(bar_index % 2 == 0, x, 0)",
];

/// Sinks that can only hold a whole int; `C` is the value written.
const INT_SINKS: [&str; 6] = [
    "a = array.new_int()\na.push(C)\nplot(a.get(0))\n",
    "a = array.new_int(2, 0)\na.set(0, C)\nplot(a.get(0))\n",
    "r = C\na = array.new_int(2, 0)\na.set(0, r)\nplot(a.get(0))\n",
    "type T\n    int k\nt = T.new(C)\nplot(t.k)\n",
    "type T\n    int k\nvar t = T.new(0)\nr = C\nt.k := r\nplot(t.k)\n",
    "a = array.from(C, 1)\nplot(a.get(0))\n",
];

fn sink_program(source: &str, sink: &str, value: &str) -> pinewasm_csil::CsilProgram {
    compile(&format!(
        "//@version=6\nindicator(\"T\")\nn = input.int(7, \"N\")\nint x = {source}\n{}",
        sink.replace('C', value)
    ))
}

fn sink_run(program: &pinewasm_csil::CsilProgram) -> (Vec<Option<u64>>, usize) {
    let closes: Vec<f64> = (0..BARS).map(|bar| 10.0 + bar as f64).collect();
    let outcome = run(program, &data_from_close(&closes), &RunOptions::default());
    (bits(&outcome.plots[0].values), outcome.errors.len())
}

fn int_casts(program: &pinewasm_csil::CsilProgram) -> usize {
    program
        .main()
        .code
        .iter()
        .filter(|instr| {
            matches!(
                instr,
                pinewasm_csil::CsilInstr::CallPure {
                    func: pinewasm_csil::PureFunc::CastInt,
                    argc: 1
                }
            )
        })
        .count()
}

#[test]
fn an_int_only_sink_reads_the_whole_part_of_a_fractional_ta_result() {
    // A fractional result of these int forms reaches an int array, an int
    // field or `array.from` as its whole part, exactly as the script that
    // spells `int(...)` does. Without the cast, the array writes would stop
    // the script with a type error, and the field and `array.from` would store
    // the fraction.
    let mut whole_parts = 0;
    for call in CARRIERS {
        for sink in INT_SINKS {
            let context = format!("{call} into {sink}");
            let implicit = sink_program("n / 2 + bar_index % 2", sink, call);
            let explicit = sink_program("n / 2 + bar_index % 2", sink, &format!("int({call})"));
            if !sink.starts_with("r = ") && !sink.contains("\nr = ") {
                // Written straight into the sink, the rewrite produces the
                // very code of the explicit cast. Through a variable the
                // cast sits on the read instead, with the same values.
                assert_eq!(implicit.main().code, explicit.main().code, "{context}");
            }
            let (values, errors) = sink_run(&implicit);
            assert_eq!(errors, 0, "{context}");
            assert_eq!((values.clone(), errors), sink_run(&explicit), "{context}");
            for value in values.iter().flatten() {
                assert_eq!(f64::from_bits(*value).fract(), 0.0, "{context}");
                whole_parts += 1;
            }
        }
    }
    assert!(whole_parts > 300, "only {whole_parts} values checked");
    // The values of the first program: the mode of 3.5, 4.5, 3.5, ... over
    // three bars alternates 3.5 and 4.5, stored as 3 and 4.
    let (values, _) = sink_run(&sink_program(
        "n / 2 + bar_index % 2",
        INT_SINKS[0],
        CARRIERS[0],
    ));
    let values: Vec<Option<f64>> = values.iter().map(|v| v.map(f64::from_bits)).collect();
    assert_eq!(
        values[..6],
        [None, None, Some(3.0), Some(4.0), Some(3.0), Some(4.0)]
    );
}

#[test]
fn a_fractional_ta_result_read_as_a_number_keeps_its_fraction() {
    // The cast is only for the int-only reading: the same result plotted or
    // added keeps the fraction, and a value read both ways is truncated only
    // at the sink.
    for call in CARRIERS {
        let program = compile(&format!(
            "//@version=6\nindicator(\"T\")\nn = input.int(7, \"N\")\nint x = bar_index % 2 == 0 ? n / 2 : n\nr = {call}\na = array.new_int()\na.push(r)\nplot(r + a.get(0))\n"
        ));
        let (values, errors) = sink_run(&program);
        assert_eq!(errors, 0, "{call}");
        assert!(
            values
                .iter()
                .flatten()
                .any(|value| f64::from_bits(*value).fract() != 0.0),
            "{call}"
        );
        assert_eq!(int_casts(&program), 1, "{call}");
    }
}

#[test]
fn a_whole_int_source_into_these_engines_gets_no_cast() {
    // The source cannot hold a fraction, so nothing is inserted, even with a
    // division elsewhere in the script; and `ta.median`, whose int form
    // truncates, is never a carrier.
    for call in CARRIERS {
        for sink in INT_SINKS {
            let program = sink_program("n + bar_index % 2", sink, call);
            assert_eq!(int_casts(&program), 0, "{call} into {sink}");
            let (_, errors) = sink_run(&program);
            assert_eq!(errors, 0, "{call} into {sink}");
        }
        let unrelated = compile(&format!(
            "//@version=6\nindicator(\"T\")\nn = input.int(7, \"N\")\nint x = n + bar_index % 2\nk = n / 2\na = array.new_int()\na.push({call})\nplot(a.get(0) + k)\n"
        ));
        assert_eq!(int_casts(&unrelated), 0, "{call}");
    }
    for call in ["ta.median(x, 3)", "ta.median(x, bar_index % 2 + 2)"] {
        for sink in INT_SINKS {
            let program = sink_program("n / 2 + bar_index % 2", sink, call);
            assert_eq!(int_casts(&program), 0, "{call} into {sink}");
            let (_, errors) = sink_run(&program);
            assert_eq!(errors, 0, "{call} into {sink}");
        }
    }
}

#[test]
fn the_carrier_list_is_exactly_the_int_forms_whose_decoder_keeps_a_fraction() {
    // Every TA function has a decision: it is a fraction carrier for the
    // whole-part rewrite exactly when it is an int form whose decoder
    // returns a float for a fractional engine result. A new fraction-keeping
    // int form fails here until it is listed in `ta_result_carries_fraction`.
    use pinewasm_csil::fraction::ta_result_carries_fraction;
    use pinewasm_csil::TaFunc;
    let mut carriers = Vec::new();
    for func in TaFunc::ALL {
        let int_form = func.engine_result() == pinewasm_hir::TaResult::Int;
        let keeps = int_form && matches!(crate::ta::decode(func, 2.5), Value::Float(_));
        assert_eq!(ta_result_carries_fraction(func), keeps, "{func:?}");
        if keeps {
            carriers.push(func);
        }
    }
    assert_eq!(
        carriers,
        [
            TaFunc::ValueWhenInt,
            TaFunc::FixnanInt,
            TaFunc::ModeInt,
            TaFunc::RangeInt
        ]
    );
    assert!(!ta_result_carries_fraction(TaFunc::MedianInt));
    assert!(matches!(
        crate::ta::decode(TaFunc::MedianInt, 2.5),
        Value::Int(2)
    ));
}

#[test]
fn a_run_time_length_range_over_a_window_with_no_value_is_na() {
    // The fold's start values are -inf, which the int form would decode as
    // i64::MIN. The literal-length form gives na.
    for (declaration, source) in [
        (
            "input.int(7, \"N\")",
            "bar_index < 4 ? na : n / 2 + bar_index % 3",
        ),
        (
            "input.int(7, \"N\")",
            "bar_index < 4 ? na : n + bar_index % 3",
        ),
        (
            "input.float(7.0, \"N\")",
            "bar_index < 4 ? na : n / 2 + bar_index % 3",
        ),
    ] {
        let program = compile(&format!(
            "//@version=6\nindicator(\"T\")\nn = {declaration}\nx = {source}\nplot(ta.range(x, bar_index % 2 + 2))\n"
        ));
        let closes: Vec<f64> = (0..BARS).map(|bar| 10.0 + bar as f64).collect();
        let outcome = run(&program, &data_from_close(&closes), &RunOptions::default());
        assert!(outcome.errors.is_empty(), "{source}");
        let values = floats(&outcome.plots[0].values);
        assert_eq!(
            values[..4],
            [None, None, None, None],
            "{source}: {values:?}"
        );
        assert!(values[6].is_some(), "{source}: {values:?}");
        assert!(
            values.iter().flatten().all(|value| value.abs() < 10.0),
            "{source}: {values:?}"
        );
    }
}

#[test]
fn int_median_truncates_an_even_window_of_whole_ints() {
    // The median of 1 and 2 is 1 for an int source
    // and 1.5 for a float one.
    let int = floats(&series(
        "input.int(1, \"N\")",
        "n + bar_index",
        "ta.median(x, 2)",
    ));
    let float = floats(&series(
        "input.float(1.0, \"N\")",
        "n + bar_index",
        "ta.median(x, 2)",
    ));
    assert_eq!(int[..4], [None, Some(1.0), Some(2.0), Some(3.0)]);
    assert_eq!(float[..4], [None, Some(1.5), Some(2.5), Some(3.5)]);
    // An odd window of whole ints has a whole median either way.
    assert_eq!(
        floats(&series(
            "input.int(1, \"N\")",
            "n + bar_index",
            "ta.median(x, 3)"
        )),
        floats(&series(
            "input.float(1.0, \"N\")",
            "n + bar_index",
            "ta.median(x, 3)"
        ))
    );
}

#[test]
fn int_median_truncates_a_fractional_int_source() {
    // The same truncation applies when the int
    // source itself holds a fraction, for both length forms.
    for length in ["3", "bar_index % 2 + 3 - bar_index % 2"] {
        let call = format!("ta.median(x, {length})");
        let int = floats(&series("input.int(7, \"N\")", "n / 2 + bar_index", &call));
        let float = floats(&series(
            "input.float(7.0, \"N\")",
            "n / 2 + bar_index",
            &call,
        ));
        assert_eq!(
            int[..5],
            [None, None, Some(4.0), Some(5.0), Some(6.0)],
            "{call}"
        );
        assert_eq!(
            float[..5],
            [None, None, Some(4.5), Some(5.5), Some(6.5)],
            "{call}"
        );
    }
}
