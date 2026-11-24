//! Pine `int / int` whose result is kept as an `int`.
//!
//! `PINE_INT_DIV` is statically an int, but the interpreter keeps an uneven
//! quotient as a float (`7 / 2` is `3.5`), an exact one as an int, and
//! answers `na` for an `na` operand or a zero divisor. The standalone backend
//! has no cell that holds either an int or a float, so it lowers the division
//! only where nothing downstream can tell the two apart, or where the one
//! consumer that can (a comparison with an integer) is fused with it:
//!
//! * the quotient is widened, rounded or discarded at once;
//! * it is turned into a genuine integer at once by `int()`, `math.floor`,
//!   `math.ceil` or `math.round`, after which it is an ordinary integer for
//!   arithmetic, storage, history and comparisons;
//! * it is stored to a slot whose every read is widened at once (bound to an
//!   `int`, a `var int`, read back with `[1]`, plotted, used in float
//!   arithmetic);
//! * it is one operand of a comparison, the other being a constant, an
//!   input, a builtin series or a variable.
//!
//! Every accepted program here runs on the interpreter and as a standalone
//! module, and every plot must be bit-equal on every bar. The typed pass
//! refuses everything else as `UnsupportedOpcode`, a declared capability
//! limit. Both tiers then carry the quotient as float code
//! (`tests/pine_int_div_carried.rs`, and the chart-tier tests in
//! `tests/differential.rs`).

use pinewasm_aot::{
    compile_standalone_wasm_program, AotError, RefusalClass, StandaloneWasmModuleType,
    StandaloneWasmOptions,
};
use pinewasm_compiler::{compile, CompileOptions};
use pinewasm_csil::{CsilInstr, CsilProgram};
use pinewasm_mir::Value;
use pinewasm_vm::{run, RunOptions, RunOutcome};

mod support;
use support::{standalone_wasm_wire, synthetic_data, StandaloneWasmHost};

const BARS: usize = 12;

fn csil(source: &str) -> CsilProgram {
    let result = compile(source, CompileOptions::all());
    let errors: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|d| pinewasm_span::Diagnostic::is_error(d))
        .collect();
    assert!(errors.is_empty(), "{source}\n{errors:?}");
    let program = result.program.expect("the front end produced a program");
    pinewasm_csil::validate(&program).expect("front-end output validates");
    assert!(
        program
            .main()
            .code
            .iter()
            .any(|instr| matches!(instr, CsilInstr::PineIntDiv)),
        "the program must contain PINE_INT_DIV:\n{source}"
    );
    program
}

fn standalone(program: &CsilProgram) -> Result<Vec<u8>, AotError> {
    compile_standalone_wasm_program(
        program,
        &StandaloneWasmOptions {
            signal_plot: None,
            module_type: StandaloneWasmModuleType::Indicator,
        },
    )
}

/// Build the module, run both, require bit-equal plots, and return the
/// interpreter's outcome for value assertions.
fn agree(source: &str) -> RunOutcome {
    let program = csil(source);
    let wasm = standalone(&program).unwrap_or_else(|error| panic!("{source}\nrefused: {error}"));
    let data = synthetic_data(BARS);
    let reference = run(&program, &data, &RunOptions::default());
    assert!(reference.errors.is_empty(), "{:?}", reference.errors);
    assert!(!reference.plots.is_empty(), "no plots:\n{source}");
    let mut host = StandaloneWasmHost::new(&wasm);
    host.evaluate(&standalone_wasm_wire(&data, None))
        .unwrap_or_else(|error| panic!("{source}\nmodule failed: {error}"));
    for plot in &reference.plots {
        let lane = host.plot_lane(plot.slot as usize);
        assert_eq!(lane.len(), BARS);
        assert_eq!(plot.values.len(), BARS);
        for (bar, (expected, got)) in plot.values.iter().zip(&lane).enumerate() {
            let equal = match expected {
                Value::Na => got.is_nan(),
                Value::Float(x) => x.to_bits() == got.to_bits(),
                other => panic!("plot {} bar {bar}: interpreter {other:?}", plot.slot),
            };
            assert!(
                equal,
                "{source}\nplot {} bar {bar}: interpreter {expected:?}, module {got:?}",
                plot.slot
            );
        }
    }
    reference
}

fn plot(outcome: &RunOutcome, index: usize) -> Vec<Option<f64>> {
    outcome.plots[index]
        .values
        .iter()
        .map(|value| match value {
            Value::Na => None,
            Value::Float(x) => Some(*x),
            other => panic!("plot value {other:?}"),
        })
        .collect()
}

fn refused(source: &str) {
    let error = match standalone(&csil(source)) {
        Ok(_) => panic!("unexpectedly accepted:\n{source}"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), "UnsupportedOpcode", "{source}\n{error}");
    assert_eq!(error.class(), RefusalClass::Capability, "{source}\n{error}");
    assert!(error.to_string().contains("PineIntDiv"), "{error}");
}

#[test]
fn a_quotient_bound_to_an_int_keeps_its_fraction() {
    let outcome = agree(
        "//@version=6\nindicator(\"bound\")\n\
         int half = bar_index / 2\n\
         int seven = input.int(7) / 2\n\
         int even = input.int(8) / 2\n\
         plot(half)\nplot(seven)\nplot(even)\nplot(close / half)\n",
    );
    let half = plot(&outcome, 0);
    assert_eq!(&half[..4], &[Some(0.0), Some(0.5), Some(1.0), Some(1.5)]);
    assert!(plot(&outcome, 1).iter().all(|v| *v == Some(3.5)));
    assert!(plot(&outcome, 2).iter().all(|v| *v == Some(4.0)));
    // `close / 0` on bar 0 is `na`, and the quotient is a real divisor after.
    assert_eq!(plot(&outcome, 3)[0], None);
    assert!(plot(&outcome, 3)[1].is_some());
}

#[test]
fn negative_operands_divide_toward_the_float_quotient() {
    let outcome = agree(
        "//@version=6\nindicator(\"negative\")\n\
         int a = (0 - bar_index) / 2\n\
         int b = bar_index / (0 - 2)\n\
         int c = (0 - bar_index) / (0 - 4)\n\
         int d = input.int(-7) / 2\n\
         plot(a)\nplot(b)\nplot(c)\nplot(d)\n",
    );
    assert_eq!(
        &plot(&outcome, 0)[..4],
        &[Some(0.0), Some(-0.5), Some(-1.0), Some(-1.5)]
    );
    assert_eq!(plot(&outcome, 0), plot(&outcome, 1));
    assert_eq!(
        &plot(&outcome, 2)[..5],
        &[Some(0.0), Some(0.25), Some(0.5), Some(0.75), Some(1.0)]
    );
    assert!(plot(&outcome, 3).iter().all(|v| *v == Some(-3.5)));
}

#[test]
fn division_by_zero_and_an_na_operand_are_na() {
    let outcome = agree(
        "//@version=6\nindicator(\"missing\")\n\
         int zero = bar_index / (bar_index - bar_index)\n\
         int literal_zero = bar_index / 0\n\
         int from_na = bar_index[1] / 2\n\
         int by_na = 6 / bar_index[2]\n\
         plot(zero)\nplot(literal_zero)\nplot(from_na)\nplot(by_na)\n\
         plot(bar_index / 0 > 1 ? 1 : 0)\n\
         plot(bar_index / 0 == 1 ? 1 : 0)\n\
         plot(bar_index / 0 != 1 ? 1 : 0)\n\
         plot(1 < bar_index[1] / 2 ? 1 : 0)\n",
    );
    assert!(plot(&outcome, 0).iter().all(Option::is_none));
    assert!(plot(&outcome, 1).iter().all(Option::is_none));
    assert_eq!(
        &plot(&outcome, 2)[..4],
        &[None, Some(0.0), Some(0.5), Some(1.0)]
    );
    // 6 / 0 on bar 2 is `na`, like the two bars with no history.
    assert_eq!(
        &plot(&outcome, 3)[..6],
        &[None, None, None, Some(6.0), Some(3.0), Some(2.0)]
    );
    // A comparison with `na` is false for every operator, `!=` included.
    for index in 4..7 {
        assert!(plot(&outcome, index).iter().all(|v| *v == Some(0.0)));
    }
    assert_eq!(
        &plot(&outcome, 7)[..5],
        &[Some(0.0), Some(0.0), Some(0.0), Some(0.0), Some(1.0)]
    );
}

#[test]
fn a_quotient_compares_as_the_interpreter_orders_it() {
    let outcome = agree(
        "//@version=6\nindicator(\"compare\")\n\
         n = bar_index\n\
         limit = input.int(2)\n\
         plot(n / 2 > 1 ? 1 : 0)\n\
         plot(n / 2 >= 1 ? 1 : 0)\n\
         plot(n / 2 < 1 ? 1 : 0)\n\
         plot(n / 2 <= 1 ? 1 : 0)\n\
         plot(n / 2 == 1 ? 1 : 0)\n\
         plot(n / 2 != 1 ? 1 : 0)\n\
         plot(1 > n / 2 ? 1 : 0)\n\
         plot(1 == n / 2 ? 1 : 0)\n\
         plot(n / 2 > limit ? 1 : 0)\n\
         plot(n / 3 == n ? 1 : 0)\n\
         plot(n / 2 > 1.25 ? 1 : 0)\n\
         plot(n / 2 > close ? 1 : 0)\n\
         plot(n / 2 < n[1] ? 1 : 0)\n",
    );
    // n / 2 on bars 0..: 0, 0.5, 1, 1.5, 2, 2.5
    let bits = |index: usize| -> Vec<f64> {
        plot(&outcome, index)[..6]
            .iter()
            .map(|v| v.unwrap())
            .collect()
    };
    assert_eq!(bits(0), [0.0, 0.0, 0.0, 1.0, 1.0, 1.0]);
    assert_eq!(bits(1), [0.0, 0.0, 1.0, 1.0, 1.0, 1.0]);
    assert_eq!(bits(2), [1.0, 1.0, 0.0, 0.0, 0.0, 0.0]);
    assert_eq!(bits(3), [1.0, 1.0, 1.0, 0.0, 0.0, 0.0]);
    assert_eq!(bits(4), [0.0, 0.0, 1.0, 0.0, 0.0, 0.0]);
    assert_eq!(bits(5), [1.0, 1.0, 0.0, 1.0, 1.0, 1.0]);
    assert_eq!(bits(6), [1.0, 1.0, 0.0, 0.0, 0.0, 0.0]);
    assert_eq!(bits(7), [0.0, 0.0, 1.0, 0.0, 0.0, 0.0]);
    assert_eq!(bits(8), [0.0, 0.0, 0.0, 0.0, 0.0, 1.0]);
    assert_eq!(bits(9), [1.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
    assert_eq!(bits(10), [0.0, 0.0, 0.0, 1.0, 1.0, 1.0]);
}

/// The reason the comparison is fused instead of run on the f64 value: above
/// `2^53` an exact quotient and the integer it is compared with are distinct
/// integers that round to the same float. The interpreter compares them as
/// integers; an uneven quotient there is a float and compares as one.
#[test]
fn an_exact_quotient_beyond_f64_precision_compares_as_an_integer() {
    let outcome = agree(
        "//@version=6\nindicator(\"wide\")\n\
         big = input.int(9007199254740993)\n\
         edge = input.int(9007199254740992)\n\
         plot(big / 1 > 9007199254740992 ? 1 : 0)\n\
         plot(big / 1 == 9007199254740992 ? 1 : 0)\n\
         plot(big / 1 != 9007199254740992 ? 1 : 0)\n\
         plot(9007199254740992 < big / 1 ? 1 : 0)\n\
         plot(edge >= big / 1 ? 1 : 0)\n\
         plot(big / 1 > edge ? 1 : 0)\n\
         plot(big / 2 > 4503599627370496 ? 1 : 0)\n\
         plot(big / 2 == 4503599627370496 ? 1 : 0)\n\
         plot(4503599627370497 > big / 2 ? 1 : 0)\n\
         deep = input.int(-9007199254740993)\n\
         shelf = input.int(-9007199254740992)\n\
         plot(deep / 1 < shelf ? 1 : 0)\n\
         plot(-9007199254740992 <= deep / 1 ? 1 : 0)\n\
         plot(deep / -1 > edge ? 1 : 0)\n",
    );
    let first = |index: usize| plot(&outcome, index)[0].unwrap();
    assert_eq!(first(0), 1.0);
    assert_eq!(first(1), 0.0);
    assert_eq!(first(2), 1.0);
    assert_eq!(first(3), 1.0);
    assert_eq!(first(4), 0.0);
    assert_eq!(first(5), 1.0);
    // 2^53 + 1 over 2 is uneven: the float quotient rounds to 2^52, and the
    // promoted integers 2^52 and 2^52 + 1 are both exact.
    assert_eq!(first(6), 0.0);
    assert_eq!(first(7), 1.0);
    assert_eq!(first(8), 1.0);
    assert_eq!(first(9), 1.0);
    assert_eq!(first(10), 0.0);
    assert_eq!(first(11), 1.0);
}

#[test]
fn a_quotient_in_a_var_int_persists_and_reads_back_with_history() {
    let outcome = agree(
        "//@version=6\nindicator(\"var\")\n\
         var int once = (bar_index + 7) / 2\n\
         var int held = 0\n\
         if bar_index % 3 == 1\n    held := bar_index / 2\n\
         int fresh = bar_index / 2\n\
         plot(once)\nplot(held)\nplot(held[1])\nplot(fresh[1])\nplot(fresh[2] * 2.0)\n",
    );
    assert!(plot(&outcome, 0).iter().all(|v| *v == Some(3.5)));
    // Written on bars 1, 4, 7 with 0.5, 2, 3.5 and carried forward between.
    assert_eq!(
        &plot(&outcome, 1)[..8],
        &[
            Some(0.0),
            Some(0.5),
            Some(0.5),
            Some(0.5),
            Some(2.0),
            Some(2.0),
            Some(2.0),
            Some(3.5)
        ]
    );
    assert_eq!(
        &plot(&outcome, 2)[..5],
        &[None, Some(0.0), Some(0.5), Some(0.5), Some(0.5)]
    );
    assert_eq!(
        &plot(&outcome, 3)[..4],
        &[None, Some(0.0), Some(0.5), Some(1.0)]
    );
    assert_eq!(&plot(&outcome, 4)[..4], &[None, None, Some(0.0), Some(1.0)]);
}

#[test]
fn an_integer_stored_to_a_quotient_slot_is_the_same_number() {
    let outcome = agree(
        "//@version=6\nindicator(\"mixed stores\")\n\
         int k = 3\n\
         if close > open\n    k := bar_index / 2\n\
         else\n    k := bar_index * 3\n\
         int wide = input.int(9007199254740993)\n\
         var int w = wide\n\
         if bar_index == 4\n    w := bar_index / 3\n\
         plot(k)\nplot(w)\n",
    );
    // A wide integer reaches the plot as the float `IntToF64` makes of it.
    assert_eq!(plot(&outcome, 1)[0], Some(9_007_199_254_740_993_i64 as f64));
    assert_eq!(plot(&outcome, 1)[4], Some(4.0 / 3.0));
}

/// A conditional stores each arm to one temporary slot and copies that to
/// the variable, so a quotient in either arm makes both quotient slots.
#[test]
fn a_quotient_in_one_arm_of_a_conditional_is_a_quotient_slot() {
    let outcome = agree(
        "//@version=6\nindicator(\"conditional\")\n\
         int k = bar_index % 2 == 1 ? bar_index / 2 : 3\n\
         int j = bar_index % 2 == 0 ? 5 : bar_index / 4\n\
         plot(k)\nplot(j)\n",
    );
    assert_eq!(
        &plot(&outcome, 0)[..4],
        &[Some(3.0), Some(0.5), Some(3.0), Some(1.5)]
    );
    assert_eq!(
        &plot(&outcome, 1)[..4],
        &[Some(5.0), Some(0.25), Some(5.0), Some(0.75)]
    );
}

#[test]
fn a_quotient_copied_from_slot_to_slot_stays_a_quotient_slot() {
    let outcome = agree(
        "//@version=6\nindicator(\"copy\")\n\
         int k = bar_index / 2\n\
         int j = k\n\
         var int last = 0\n\
         last := j[1]\n\
         plot(j)\nplot(j[1])\nplot(k)\nplot(last)\n",
    );
    let k = plot(&outcome, 2);
    assert_eq!(&k[..4], &[Some(0.0), Some(0.5), Some(1.0), Some(1.5)]);
    assert_eq!(plot(&outcome, 0), k);
    assert_eq!(&plot(&outcome, 1)[..3], &[None, Some(0.0), Some(0.5)]);
    assert_eq!(plot(&outcome, 3), plot(&outcome, 1));
}

#[test]
fn a_discarded_quotient_is_accepted() {
    agree(
        "//@version=6\nindicator(\"discarded\")\n\
         f(int x) =>\n    x / 2\n    close\n\
         plot(f(bar_index))\n",
    );
}

/// `int()`, `math.floor`, `math.ceil` and `math.round` differ on an uneven
/// negative quotient and agree on an exact one.
#[test]
fn a_cast_or_rounded_quotient_is_the_interpreters_integer() {
    let outcome = agree(
        "//@version=6\nindicator(\"casts\")\n\
         n = bar_index - 5\n\
         plot(int(n / 2))\nplot(math.floor(n / 2))\n\
         plot(math.ceil(n / 2))\nplot(math.round(n / 2))\n\
         plot(int(n / -2))\nplot(math.floor(n / -2))\nplot(math.ceil(n / -2))\n\
         plot(int(n / 3))\nplot(math.floor(n / 3))\nplot(math.ceil(n / 3))\n",
    );
    // n = -5, -4, -3, -2, -1, 0, 1, 2, 3
    let head = |index: usize| -> Vec<f64> {
        plot(&outcome, index)[..9]
            .iter()
            .map(|v| v.expect("present"))
            .collect()
    };
    assert_eq!(head(0), [-2.0, -2.0, -1.0, -1.0, 0.0, 0.0, 0.0, 1.0, 1.0]);
    assert_eq!(head(1), [-3.0, -2.0, -2.0, -1.0, -1.0, 0.0, 0.0, 1.0, 1.0]);
    assert_eq!(head(2), [-2.0, -2.0, -1.0, -1.0, 0.0, 0.0, 1.0, 1.0, 2.0]);
    assert_eq!(head(3), [-3.0, -2.0, -2.0, -1.0, -1.0, 0.0, 1.0, 1.0, 2.0]);
    assert_eq!(head(4), [2.0, 2.0, 1.0, 1.0, 0.0, 0.0, 0.0, -1.0, -1.0]);
    assert_eq!(head(5), [2.0, 2.0, 1.0, 1.0, 0.0, 0.0, -1.0, -1.0, -2.0]);
    assert_eq!(head(6), [3.0, 2.0, 2.0, 1.0, 1.0, 0.0, 0.0, -1.0, -1.0]);
    assert_eq!(head(7), [-1.0, -1.0, -1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0]);
    assert_eq!(head(8), [-2.0, -2.0, -1.0, -1.0, -1.0, 0.0, 0.0, 0.0, 1.0]);
    assert_eq!(head(9), [-1.0, -1.0, -1.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0]);
}

#[test]
fn a_cast_quotient_of_a_zero_divisor_or_an_na_operand_is_na() {
    let outcome = agree(
        "//@version=6\nindicator(\"cast missing\")\n\
         plot(int(bar_index / 0))\n\
         plot(math.floor(bar_index / (bar_index - bar_index)))\n\
         plot(math.ceil(bar_index[1] / 2))\n\
         plot(int(6 / bar_index[2]))\n\
         plot(math.floor(bar_index / 0) + 1)\n\
         plot(nz(int(bar_index[1] / 2), -7))\n",
    );
    assert!(plot(&outcome, 0).iter().all(Option::is_none));
    assert!(plot(&outcome, 1).iter().all(Option::is_none));
    assert_eq!(
        &plot(&outcome, 2)[..5],
        &[None, Some(0.0), Some(1.0), Some(1.0), Some(2.0)]
    );
    // 6 / 0 on bar 2 is `na`; 6 / 4 truncates to 1 on bar 6.
    assert_eq!(
        &plot(&outcome, 3)[..7],
        &[None, None, None, Some(6.0), Some(3.0), Some(2.0), Some(1.0)]
    );
    assert!(plot(&outcome, 4).iter().all(Option::is_none));
    assert_eq!(
        &plot(&outcome, 5)[..4],
        &[Some(-7.0), Some(0.0), Some(0.0), Some(1.0)]
    );
}

/// An exact quotient stays the exact integer past 2^53, where the f64 lane
/// could not hold it; an uneven one is the interpreter's f64 division, which
/// has already rounded, so floor and ceil agree there.
#[test]
fn a_cast_quotient_beyond_f64_precision_keeps_the_interpreters_value() {
    let outcome = agree(
        "//@version=6\nindicator(\"wide casts\")\n\
         big = input.int(9007199254740993)\n\
         deep = input.int(-9007199254740993)\n\
         plot(int(big / 1) - 9007199254740992)\n\
         plot(math.floor(big / 1) - 9007199254740992)\n\
         plot(math.ceil(big / -1) + 9007199254740992)\n\
         plot(math.round(deep / 1) + 9007199254740992)\n\
         plot(math.ceil(big / 2) - 4503599627370496)\n\
         plot(math.floor(big / 2) - 4503599627370496)\n\
         plot(int(deep / 2) + 4503599627370496)\n\
         plot(math.floor(deep / 2) + 4503599627370496)\n\
         huge = input.int(9223372036854775807)\n\
         plot(int(huge / 2) - 4611686018427387904)\n\
         plot(math.ceil(huge / -2) + 4611686018427387904)\n\
         plot(int(huge / -1) + 9223372036854775806)\n",
    );
    let first = |index: usize| plot(&outcome, index)[0].unwrap();
    assert_eq!(first(0), 1.0);
    assert_eq!(first(1), 1.0);
    assert_eq!(first(2), -1.0);
    assert_eq!(first(3), -1.0);
    // (2^53 + 1) / 2 divides as 2^53 / 2 in f64: exactly 2^52 either way.
    assert_eq!(first(4), 0.0);
    assert_eq!(first(5), 0.0);
    assert_eq!(first(6), 0.0);
    assert_eq!(first(7), 0.0);
    // (2^63 - 1) / 2 divides as 2^63 / 2 = 2^62, the largest magnitude an
    // uneven quotient reaches.
    assert_eq!(first(8), 0.0);
    assert_eq!(first(9), 0.0);
    assert_eq!(first(10), -1.0);
}

/// The fused result is a genuine integer: integer arithmetic, negation,
/// remainder, comparisons, a history offset and a second division all read it.
#[test]
fn a_cast_quotient_feeds_integer_arithmetic() {
    let outcome = agree(
        "//@version=6\nindicator(\"cast arith\")\n\
         n = bar_index\n\
         plot(int(n / 2) + 1)\n\
         plot(math.floor(n / 2) * 2)\n\
         plot(-math.ceil(n / 2))\n\
         plot(math.ceil(n / 3) % 2)\n\
         plot(int(n / 2) > 1 ? 1 : 0)\n\
         plot(int(n / 2) == math.floor(n / 2) ? 1 : 0)\n\
         plot(math.ceil(n / 2) > math.floor(n / 2) ? 1 : 0)\n\
         plot(close[int(n / 2)])\n\
         plot(math.round(int(n / 2) / 2))\n\
         plot(n - int(n / 3) * 3)\n",
    );
    assert_eq!(
        &plot(&outcome, 0)[..5],
        &[Some(1.0), Some(1.0), Some(2.0), Some(2.0), Some(3.0)]
    );
    assert_eq!(
        &plot(&outcome, 1)[..5],
        &[Some(0.0), Some(0.0), Some(2.0), Some(2.0), Some(4.0)]
    );
    assert_eq!(
        &plot(&outcome, 2)[..5],
        &[Some(0.0), Some(-1.0), Some(-1.0), Some(-2.0), Some(-2.0)]
    );
    assert_eq!(
        &plot(&outcome, 3)[..5],
        &[Some(0.0), Some(1.0), Some(1.0), Some(1.0), Some(0.0)]
    );
    assert_eq!(
        &plot(&outcome, 4)[..5],
        &[Some(0.0), Some(0.0), Some(0.0), Some(0.0), Some(1.0)]
    );
    assert!(plot(&outcome, 5).iter().all(|v| *v == Some(1.0)));
    assert_eq!(
        &plot(&outcome, 6)[..4],
        &[Some(0.0), Some(1.0), Some(0.0), Some(1.0)]
    );
    assert!(plot(&outcome, 7).iter().all(Option::is_some));
    assert_eq!(
        &plot(&outcome, 8)[..7],
        &[
            Some(0.0),
            Some(0.0),
            Some(1.0),
            Some(1.0),
            Some(1.0),
            Some(1.0),
            Some(2.0)
        ]
    );
    assert_eq!(
        &plot(&outcome, 9)[..5],
        &[Some(0.0), Some(1.0), Some(2.0), Some(0.0), Some(1.0)]
    );
}

/// A cast quotient bound to an `int`, held in a `var int`, reassigned in a
/// branch and read back with history is an integer slot, not a quotient slot.
#[test]
fn a_cast_quotient_is_stored_and_read_back_as_an_integer() {
    let outcome = agree(
        "//@version=6\nindicator(\"cast slots\")\n\
         int half = int(bar_index / 2)\n\
         var int once = math.ceil((bar_index + 7) / 2)\n\
         var int held = 0\n\
         if bar_index % 3 == 1\n    held := math.floor((0 - bar_index) / 2)\n\
         var int total = 0\n\
         total += int(bar_index / 2)\n\
         plot(half)\nplot(half[1])\nplot(half[2] + 1)\nplot(once)\n\
         plot(held)\nplot(held[1] * 2)\nplot(total)\nplot(half > 1 ? 1 : 0)\n",
    );
    assert_eq!(
        &plot(&outcome, 0)[..5],
        &[Some(0.0), Some(0.0), Some(1.0), Some(1.0), Some(2.0)]
    );
    assert_eq!(
        &plot(&outcome, 1)[..5],
        &[None, Some(0.0), Some(0.0), Some(1.0), Some(1.0)]
    );
    assert_eq!(
        &plot(&outcome, 2)[..5],
        &[None, None, Some(1.0), Some(1.0), Some(2.0)]
    );
    assert!(plot(&outcome, 3).iter().all(|v| *v == Some(4.0)));
    // Written on bars 1, 4, 7 with floor(-0.5), -2, floor(-3.5).
    assert_eq!(
        &plot(&outcome, 4)[..8],
        &[
            Some(0.0),
            Some(-1.0),
            Some(-1.0),
            Some(-1.0),
            Some(-2.0),
            Some(-2.0),
            Some(-2.0),
            Some(-4.0)
        ]
    );
    assert_eq!(
        &plot(&outcome, 5)[..6],
        &[
            None,
            Some(0.0),
            Some(-2.0),
            Some(-2.0),
            Some(-2.0),
            Some(-4.0)
        ]
    );
    assert_eq!(
        &plot(&outcome, 6)[..6],
        &[
            Some(0.0),
            Some(0.0),
            Some(1.0),
            Some(2.0),
            Some(4.0),
            Some(6.0)
        ]
    );
    assert_eq!(
        &plot(&outcome, 7)[..5],
        &[Some(0.0), Some(0.0), Some(0.0), Some(0.0), Some(1.0)]
    );
}

/// In a program that needs the fraction carry for another quotient, a cast
/// quotient is still left to the typed pass: it stays exact past 2^53, where
/// a carried quotient would stop the run.
#[test]
fn a_cast_quotient_beside_a_carried_one_stays_exact() {
    let source = "//@version=6\nindicator(\"mixed\")\n\
         big = input.int(9007199254740993)\n\
         plot(bar_index / 2 + 1)\n\
         plot(int(big / 1) - 9007199254740992)\n\
         plot(math.floor(big / -1) + 9007199254740992)\n\
         plot(math.ceil(bar_index / 2) + bar_index / 2)\n";
    let error = pinewasm_aot::analysis::analyze(&csil(source))
        .err()
        .unwrap_or_else(|| panic!("the typed pass accepts:\n{source}"));
    assert_eq!(error.kind(), "UnsupportedOpcode", "{source}\n{error}");
    let outcome = agree(source);
    assert_eq!(&plot(&outcome, 0)[..3], &[Some(1.0), Some(1.5), Some(2.0)]);
    assert!(plot(&outcome, 1).iter().all(|v| *v == Some(1.0)));
    assert!(plot(&outcome, 2).iter().all(|v| *v == Some(-1.0)));
    assert_eq!(
        &plot(&outcome, 3)[..4],
        &[Some(0.0), Some(1.5), Some(2.0), Some(3.5)]
    );
}

/// A cast does not reach through a stored quotient in the typed pass: the
/// slot holds the f64 image, which cannot give back an exact integer past
/// 2^53. The typed pass refuses it by name; both tiers then carry the
/// quotient as float code, and the module agrees with the interpreter.
#[test]
fn a_cast_of_a_stored_quotient_is_carried() {
    for source in [
        "//@version=6\nindicator(\"cast slot\")\nint k = bar_index / 2\nplot(int(k))\nplot(k)\n",
        "//@version=6\nindicator(\"floor slot\")\nint k = bar_index / 2\nplot(math.floor(k) + 1)\nplot(k)\n",
    ] {
        let error = pinewasm_aot::analysis::analyze(&csil(source))
            .err()
            .unwrap_or_else(|| panic!("the typed pass accepts:\n{source}"));
        assert_eq!(error.kind(), "UnsupportedOpcode", "{source}\n{error}");
        assert!(error.to_string().contains("PineIntDiv"), "{error}");
        agree(source);
    }
}

/// What still needs a cell that holds either an int or a float. The typed
/// pass declines each of these by name. Both tiers then carry the
/// quotient as float code (`src/fraction.rs`,
/// `tests/pine_int_div_carried.rs`), and the module agrees with the
/// interpreter.
#[test]
fn a_quotient_that_stays_an_int_for_another_integer_consumer_is_carried() {
    for source in [
        // Integer arithmetic on the quotient: a float when it is uneven.
        "//@version=6\nindicator(\"arith\")\nplot(bar_index / 2 + 1)\n",
        "//@version=6\nindicator(\"arith slot\")\nint k = bar_index / 2\nplot(k + 1)\n",
        "//@version=6\nindicator(\"neg\")\nint k = -(bar_index / 2)\nplot(k)\n",
        // A stored quotient read by a comparison (a variable read once is
        // inlined by the front end, so this one is read twice), and one
        // copied to a slot that integer arithmetic reads.
        "//@version=6\nindicator(\"cmp slot\")\nint k = bar_index / 2\nplot(k > 3 ? 1 : 0)\nplot(k)\n",
        "//@version=6\nindicator(\"copy\")\nint k = bar_index / 2\nint j = k\nplot(j * 2)\nplot(j)\nplot(k)\n",
        // Two quotients compared with each other.
        "//@version=6\nindicator(\"both\")\nplot(bar_index / 2 > bar_index / 3 ? 1 : 0)\n",
        // A comparison whose other operand is itself computed.
        "//@version=6\nindicator(\"computed\")\nplot(bar_index / 2 > bar_index - 3 ? 1 : 0)\n",
        // A history offset.
        "//@version=6\nindicator(\"offset\")\nplot(close[bar_index / 2])\n",
    ] {
        let error = pinewasm_aot::analysis::analyze(&csil(source))
            .err()
            .unwrap_or_else(|| panic!("the typed pass accepts:\n{source}"));
        assert_eq!(error.kind(), "UnsupportedOpcode", "{source}\n{error}");
        assert_eq!(error.class(), RefusalClass::Capability, "{source}\n{error}");
        assert!(error.to_string().contains("PineIntDiv"), "{error}");
        agree(source);
    }
}

/// What the standalone compile still refuses by name: a carried value that
/// an int-result `ta.*` engine would return through an integer lane.
#[test]
fn a_quotient_returned_by_an_int_ta_engine_is_refused() {
    let source = "//@version=6\nindicator(\"valuewhen\")\nint k = bar_index / 2 + 1\nint v = ta.valuewhen(close > open, k, 0)\nplot(v)\n";
    let program = csil(source);
    let reference = run(&program, &synthetic_data(BARS), &RunOptions::default());
    assert!(
        reference.errors.is_empty(),
        "{source}\n{:?}",
        reference.errors
    );
    refused(source);
}

/// Build the standalone module if the backend accepts the program, and then
/// require every plot to equal the interpreter's on every bar, `na` as the
/// canonical NaN. A refusal must be the declared `PineIntDiv` limit. Returns
/// whether the program was accepted. Wrong values are never acceptable;
/// a refusal is.
fn correct_or_refused(source: &str) -> bool {
    let program = csil(source);
    let data = synthetic_data(BARS);
    let reference = run(&program, &data, &RunOptions::default());
    assert!(
        reference.errors.is_empty(),
        "{source}\n{:?}",
        reference.errors
    );
    let wasm = match standalone(&program) {
        Ok(wasm) => wasm,
        Err(error) => {
            assert_eq!(
                error,
                AotError::UnsupportedOpcode("PineIntDiv".to_string()),
                "{source}"
            );
            // The native tier shares the typed pass, so it refuses as well.
            assert!(pinewasm_aot::compile_program(&program).is_err(), "{source}");
            return false;
        }
    };
    let mut host = StandaloneWasmHost::new(&wasm);
    host.evaluate(&standalone_wasm_wire(&data, None))
        .unwrap_or_else(|error| panic!("{source}\nmodule failed: {error}"));
    for plot in &reference.plots {
        let lane = host.plot_lane(plot.slot as usize);
        for (bar, (expected, got)) in plot.values.iter().zip(&lane).enumerate() {
            let expected = match expected {
                Value::Na => f64::NAN,
                Value::Float(x) => *x,
                Value::Int(x) => *x as f64,
                other => panic!("plot {} bar {bar}: interpreter {other:?}", plot.slot),
            };
            assert_eq!(
                format!("{:016x}", got.to_bits()),
                format!("{:016x}", expected.to_bits()),
                "{source}\nplot {} bar {bar}: interpreter {expected:?}, module {got:?}",
                plot.slot
            );
        }
    }
    true
}

/// `ta.valuewhen` and `fixnan` on an int source return that source, and the
/// interpreter keeps its fraction (`decode` in `pinewasm_vm::ta`). The module
/// returns an int-result engine's value through an `i64` lane. The front end
/// widens the source first, so a quotient reaches the engine as
/// `PineIntDiv; IntToF64`, the shape the typed pass lowers on its own; it
/// must not when the widened value is such an engine's source.
#[test]
fn a_quotient_returned_by_an_int_result_engine_is_never_truncated() {
    let cond = "bar_index % 2 == 1";
    let mut accepted = 0;
    for engine in [
        format!("ta.valuewhen({cond}, Q, 0)"),
        "fixnan(Q)".to_string(),
    ] {
        for (form, script) in [
            ("direct", "plot(ENGINE(bar_index / 2))\n"),
            ("variable", "k = bar_index / 2\nplot(ENGINE(k))\nplot(k)\n"),
            (
                "int variable",
                "int k = bar_index / 2\nplot(ENGINE(k))\nplot(k)\n",
            ),
            (
                "var int",
                "var int k = 0\nk := bar_index / 2\nplot(ENGINE(k))\nplot(k)\n",
            ),
            ("negated", "plot(ENGINE(-(bar_index / 2)))\n"),
            ("input", "n = input.int(7, \"N\")\nplot(ENGINE(n / 2))\n"),
            (
                "bound result",
                "int v = ENGINE(bar_index / 2)\nplot(v)\nplot(v + 1)\n",
            ),
        ] {
            let body = script
                .replace(
                    "ENGINE(bar_index / 2)",
                    &engine.replace('Q', "bar_index / 2"),
                )
                .replace("ENGINE(k)", &engine.replace('Q', "k"))
                .replace(
                    "ENGINE(-(bar_index / 2))",
                    &engine.replace('Q', "-(bar_index / 2)"),
                )
                .replace("ENGINE(n / 2)", &engine.replace('Q', "n / 2"));
            assert!(!body.contains("ENGINE"), "{form}: {body}");
            let source = format!("//@version=6\nindicator(\"engine\")\n{body}");
            accepted += usize::from(correct_or_refused(&source));
        }
    }
    // Every form is refused today: computing it needs a cell that holds an
    // int or a float. An accepted form would have passed the value check.
    assert_eq!(accepted, 0);
}

/// The same engines beside a quotient the standalone compile carries: the
/// carry leaves a quotient that is widened at once to the typed pass, so the
/// rule above has to hold in a rewritten program too.
#[test]
fn a_fused_quotient_into_an_int_result_engine_beside_a_carried_one_is_never_truncated() {
    let header = "//@version=6\nindicator(\"r\")\nn = input.int(7, \"N\")\n";
    for body in [
        "plot(n / 2 + 1)\nplot(ta.valuewhen(bar_index % 2 == 1, bar_index / 2, 0))\n",
        "plot(n / 2 + 1)\nplot(fixnan(bar_index / 2))\n",
        "k = n / 2 + 1\nplot(fixnan(n / 2) + k)\n",
        "v = ta.valuewhen(bar_index % 2 == 1, bar_index / 2, 0)\nplot(v + n / 2)\n",
    ] {
        // All four are refused, like the engine on its own.
        assert!(!correct_or_refused(&format!("{header}{body}")), "{body}");
    }
}

/// The refusal is for a quotient that reaches the engine, nothing wider. An
/// int source with no quotient in the script, and a quotient elsewhere in
/// the script that does not reach the engine (typed-pass shapes and carried
/// ones), are accepted and agree with the interpreter.
#[test]
fn an_int_source_engine_without_a_quotient_source_is_still_accepted() {
    let cond = "bar_index % 2 == 1";
    for body in [
        // No quotient anywhere (these do not contain PINE_INT_DIV).
        format!("plot(ta.valuewhen({cond}, bar_index, 0))\n"),
        "s = bar_index % 3 == 0 ? na : bar_index\nplot(fixnan(s))\n".to_string(),
        format!("int v = ta.valuewhen({cond}, bar_index * 2 + 1, 1)\nplot(v + 1)\n"),
        // A quotient the typed pass lowers, beside the engine.
        format!("plot(ta.valuewhen({cond}, bar_index, 0))\nplot(bar_index / 2)\n"),
        "s = bar_index % 3 == 0 ? na : bar_index\nint k = bar_index / 2\nplot(fixnan(s))\nplot(k)\n"
            .to_string(),
        // A carried quotient beside the engine.
        format!("plot(ta.valuewhen({cond}, bar_index, 0))\nplot(bar_index / 2 + 1)\n"),
        "s = bar_index % 3 == 0 ? na : bar_index\nk = bar_index / 2 + 1\nplot(fixnan(s) + 1)\nplot(k)\n"
            .to_string(),
        // The engine's result meets a carried quotient afterwards.
        format!("v = ta.valuewhen({cond}, bar_index, 0)\nplot(v + bar_index / 2)\n"),
        // An explicitly whole source made from a quotient.
        format!("plot(ta.valuewhen({cond}, int(bar_index / 2), 0))\n"),
        "plot(fixnan(math.round(bar_index / 2)))\n".to_string(),
    ] {
        let source = format!("//@version=6\nindicator(\"narrow\")\n{body}");
        let result = compile(&source, CompileOptions::all());
        let program = result.program.expect("program");
        let wasm = standalone(&program).unwrap_or_else(|e| panic!("{source}\nrefused: {e}"));
        let data = synthetic_data(BARS);
        let reference = run(&program, &data, &RunOptions::default());
        assert!(reference.errors.is_empty(), "{source}");
        let mut host = StandaloneWasmHost::new(&wasm);
        host.evaluate(&standalone_wasm_wire(&data, None))
            .unwrap_or_else(|error| panic!("{source}\nmodule failed: {error}"));
        for plot in &reference.plots {
            let lane = host.plot_lane(plot.slot as usize);
            for (bar, (expected, got)) in plot.values.iter().zip(&lane).enumerate() {
                let expected = match expected {
                    Value::Na => f64::NAN,
                    Value::Float(x) => *x,
                    Value::Int(x) => *x as f64,
                    other => panic!("{other:?}"),
                };
                assert_eq!(
                    got.to_bits(),
                    expected.to_bits(),
                    "{source}\nplot {} bar {bar}: interpreter {expected:?}, module {got:?}",
                    plot.slot
                );
            }
        }
    }
}

/// Other consumers that hand an int-typed operand back, each fed a quotient
/// directly, through a variable, and beside a carried quotient. None may
/// return a truncated value.
#[test]
fn consumers_that_return_their_int_operand_keep_a_quotient_or_refuse() {
    let mut accepted = 0;
    let mut total = 0;
    for consumer in [
        "ta.median(Q, 3)",
        "ta.mode(Q, 3)",
        "ta.range(Q, 3)",
        "ta.highest(Q, 3)",
        "ta.lowest(Q, 3)",
        "ta.change(Q)",
        "ta.cum(Q)",
        "ta.max(Q)",
        "ta.min(Q)",
        "math.sum(Q, 3)",
        "nz(Q)",
        "nz(Q, 1)",
        "math.abs(Q)",
        "math.max(Q, 1)",
        "math.min(Q, 5)",
        "math.sign(Q)",
        "(bar_index % 2 == 0 ? Q : 1)",
        "Q[1]",
        "ta.valuewhen(bar_index % 2 == 1, Q, 1)",
        "request.security(syminfo.tickerid, timeframe.period, Q)",
    ] {
        for script in [
            "plot(C1)\n",
            "k = bar_index / 2\nx = C2\nplot(x)\nplot(k)\n",
            "n = input.int(7, \"N\")\nint x = C1\nplot(x + n / 2)\n",
        ] {
            let body = script
                .replace("C1", &consumer.replace('Q', "bar_index / 2"))
                .replace("C2", &consumer.replace('Q', "k"));
            let source = format!("//@version=6\nindicator(\"sibling\")\n{body}");
            let result = compile(&source, CompileOptions::all());
            if result
                .diagnostics
                .iter()
                .any(pinewasm_span::Diagnostic::is_error)
            {
                continue;
            }
            total += 1;
            let program = result.program.expect("program");
            if standalone(&program).is_err() {
                // Refused for any declared reason: nothing wrong is shipped.
                assert_eq!(
                    standalone(&program).unwrap_err().class(),
                    RefusalClass::Capability,
                    "{source}"
                );
                continue;
            }
            accepted += usize::from(correct_or_refused(&source));
        }
    }
    eprintln!("{accepted} of {total} accepted with interpreter values");
    assert!(accepted >= 20, "{accepted} of {total}");
}

/// `ta.mode` and `ta.range` over an int source return an int-lane result
/// computed from that source: the mode is one of the source's values, the
/// range a difference of two. With a fractional source the result can be
/// fractional (the mode of 3.5, 3.5, 4.5 is 3.5), which the `i64` result
/// lane cannot return, so a quotient that reaches either engine refuses the
/// program by name, as for `ta.valuewhen` and `fixnan`. The chart tier then
/// hosts it.
#[test]
fn a_quotient_reaching_int_ta_mode_or_range_is_refused() {
    let header = "//@version=6\nindicator(\"mode range\")\nn = input.int(7)\n";
    for body in [
        "x = n / 2 + bar_index % 3\nplot(ta.mode(x, 3))\n",
        "x = bar_index % 2 == 0 ? n / 2 : bar_index\nplot(ta.mode(x, 3))\n",
        "x = bar_index % 2 == 0 ? n / 2 : bar_index\nplot(ta.range(x, 3))\n",
        // Directly, through a quotient slot, and beside a carried quotient.
        "plot(ta.mode(bar_index / 2, 3))\n",
        "plot(ta.range(bar_index / 2, 3))\n",
        "int k = bar_index / 2\nplot(ta.range(k, 3))\nplot(k)\n",
        "plot(n / 2 + 1)\nplot(ta.mode(bar_index / 2, 4))\n",
    ] {
        let source = format!("{header}{body}");
        let program = csil(&source);
        let reference = run(&program, &synthetic_data(BARS), &RunOptions::default());
        assert!(
            reference.errors.is_empty(),
            "{source}\n{:?}",
            reference.errors
        );
        assert_eq!(
            standalone(&program).err(),
            Some(AotError::UnsupportedOpcode("PineIntDiv".to_string())),
            "{source}"
        );
        assert_eq!(
            pinewasm_aot::compile_program(&program).err(),
            Some(AotError::UnsupportedOpcode("PineIntDiv".to_string())),
            "{source}"
        );
        let hosted = pinewasm_aot::compile_program_to_wasm(&program).expect("hosted");
        assert_eq!(
            hosted.layout.execution_mode,
            pinewasm_aot::ExecutionMode::InterpreterHosted,
            "{source}"
        );
    }
}

/// The refusal is for a possibly fractional source, nothing wider: `ta.mode`
/// and `ta.range` over a whole int source, with no quotient in the script,
/// with a quotient elsewhere that does not reach the engine, and with a
/// quotient made whole first, stay native and agree with the interpreter on
/// every plot. So does `ta.median`, whose int form is left out of the rule.
#[test]
fn int_ta_mode_and_range_without_a_quotient_source_are_still_accepted() {
    for body in [
        // No quotient anywhere (no PINE_INT_DIV in these).
        "plot(ta.mode(bar_index % 4, 3))\nplot(ta.range(bar_index % 5, 3))\n",
        "x = bar_index % 2 == 0 ? 7 : bar_index\nplot(ta.mode(x, 3))\nplot(ta.range(x, 3))\n",
        // A quotient the typed pass lowers, elsewhere in the script.
        "plot(ta.mode(bar_index % 4, 3))\nplot(bar_index / 2)\n",
        "int k = bar_index / 2\nplot(ta.range(bar_index % 5, 3))\nplot(k)\n",
        // A carried quotient elsewhere.
        "plot(ta.mode(bar_index % 4, 3))\nplot(bar_index / 2 + 1)\n",
        "k = bar_index / 2 + 1\nplot(ta.range(bar_index % 5, 3) + 1)\nplot(k)\n",
        // The engine's result meets a carried quotient afterwards.
        "m = ta.mode(bar_index % 4, 3)\nplot(m + bar_index / 2)\n",
        // A quotient made whole before it reaches the engine.
        "plot(ta.mode(int(bar_index / 2), 3))\n",
        "plot(ta.range(math.floor(bar_index / 2), 3))\n",
        "plot(ta.mode(math.round(bar_index / 3), 4))\n",
        // `ta.median` on a quotient: not part of the rule.
        "plot(ta.median(bar_index / 2, 3))\n",
    ] {
        let source = format!("//@version=6\nindicator(\"narrow\")\n{body}");
        let result = compile(&source, CompileOptions::all());
        let program = result.program.expect("program");
        let wasm = standalone(&program).unwrap_or_else(|e| panic!("{source}\nrefused: {e}"));
        // The chart tier builds it natively too, through the carry where
        // the typed pass refuses the program as written.
        let chart = pinewasm_aot::compile_program(&program)
            .unwrap_or_else(|e| panic!("{source}\nchart tier refused: {e}"));
        assert_eq!(
            chart.layout.execution_mode,
            pinewasm_aot::ExecutionMode::Native,
            "{source}"
        );
        let data = synthetic_data(BARS);
        let reference = run(&program, &data, &RunOptions::default());
        assert!(reference.errors.is_empty(), "{source}");
        let mut host = StandaloneWasmHost::new(&wasm);
        host.evaluate(&standalone_wasm_wire(&data, None))
            .unwrap_or_else(|error| panic!("{source}\nmodule failed: {error}"));
        for plot in &reference.plots {
            let lane = host.plot_lane(plot.slot as usize);
            for (bar, (expected, got)) in plot.values.iter().zip(&lane).enumerate() {
                let expected = match expected {
                    Value::Na => f64::NAN,
                    Value::Float(x) => *x,
                    Value::Int(x) => *x as f64,
                    other => panic!("{other:?}"),
                };
                assert_eq!(
                    got.to_bits(),
                    expected.to_bits(),
                    "{source}\nplot {} bar {bar}: interpreter {expected:?}, module {got:?}",
                    plot.slot
                );
            }
        }
    }
}
