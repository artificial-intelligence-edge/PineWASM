//! A statically-int value that holds a fraction (Pine Script `int / int`), and
//! the consumers that read its whole part. The lowering rewrite under test is
//! `pinewasm_csil::fraction::truncate_for_integer_consumers`.
//!
//! Tests of whole-part truncation for int-typed fractional values.

use super::{compile, data_from_close, floats, plot_one_bar, run, security_expr_plot};
use crate::RunOptions;
use pinewasm_csil::{CsilInstr, PureFunc};
use pretty_assertions::assert_eq;

/// Run `source` over `bars` bars whose closes are 10, 11, 12, ...
fn run_source(source: &str, bars: usize) -> crate::RunOutcome {
    let closes: Vec<f64> = (0..bars).map(|bar| 10.0 + bar as f64).collect();
    run(
        &compile(source),
        &data_from_close(&closes),
        &RunOptions::default(),
    )
}

/// One plotted bar of a script with `n = input.int(7)` followed by `body`.
fn plot_n7(body: &str) -> Vec<Option<f64>> {
    plot_one_bar(&format!(
        "//@version=6\nindicator(\"D\")\nn = input.int(7, \"N\")\n{body}"
    ))
}

/// The number of `int(...)` casts in the compiled main function.
fn int_casts(source: &str) -> usize {
    compile(source)
        .main()
        .code
        .iter()
        .filter(|instr| {
            matches!(
                instr,
                CsilInstr::CallPure {
                    func: PureFunc::CastInt,
                    argc: 1
                }
            )
        })
        .count()
}

#[test]
fn an_int_variable_keeps_a_fractional_quotient() {
    // PineWASM keeps the fraction in an int-typed variable: `int k = n / 2`
    // holds 3.5 for an input operand and for two literals.
    let source = "//@version=6\nindicator(\"intdiv\")\nn = input.int(7, \"N\")\nint k = n / 2\na = 7 / 2\nplot(k, \"k\")\nplot(a, \"a\")\n";
    let outcome = run_source(source, 3);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    for plot in &outcome.plots {
        assert_eq!(floats(&plot.values), vec![Some(3.5); 3], "{:?}", plot.title);
    }
}

#[test]
fn a_fractional_int_flows_through_int_arithmetic_and_its_consumers() {
    let plot = plot_n7;
    // Statically-int arithmetic after the quotient keeps the fraction.
    assert_eq!(plot("k = n / 2 + 1\nplot(k)\n"), vec![Some(4.5)]);
    assert_eq!(plot("k = n / 2\nplot(k * 2)\n"), vec![Some(7.0)]);
    assert_eq!(plot("k = n / 2\nplot(-k % 2)\n"), vec![Some(-1.5)]);
    assert_eq!(plot("k = n / 2 / 2\nplot(k)\n"), vec![Some(1.75)]);
    // The explicit integer spellings.
    assert_eq!(plot("plot(int(n / 2))\n"), vec![Some(3.0)]);
    assert_eq!(plot("plot(int(-n / 2))\n"), vec![Some(-3.0)]);
    assert_eq!(plot("plot(math.floor(-n / 2))\n"), vec![Some(-4.0)]);
    assert_eq!(plot("plot(math.round(n / 2))\n"), vec![Some(4.0)]);
    assert_eq!(plot("plot(math.round(n * 3 / 4))\n"), vec![Some(5.0)]);
    // Comparisons see the fraction; a zero divisor is na.
    assert_eq!(plot("k = n / 2\nplot(k > 3 ? 1 : 0)\n"), vec![Some(1.0)]);
    assert_eq!(plot("k = n / 2\nplot(k == 3 ? 1 : 0)\n"), vec![Some(0.0)]);
    assert_eq!(plot("k = n / 2\nplot(k != 3 ? 1 : 0)\n"), vec![Some(1.0)]);
    assert_eq!(plot("k = n / (n - 7)\nplot(k)\n"), vec![None]);
    // A history offset truncates toward zero: close[3.5] is close[3], which
    // is na on the first bar and is the bar-0 close on bar 3.
    assert_eq!(plot("plot(close[n / 2])\n"), vec![None]);
    let outcome = run_source(
        "//@version=6\nindicator(\"D\")\nn = input.int(7, \"N\")\nplot(close[n / 2] - close[3])\n",
        6,
    );
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(floats(&outcome.plots[0].values[3..]), vec![Some(0.0); 3]);
}

#[test]
fn a_const_int_expression_that_held_a_fraction_compares_by_value() {
    // `7 / 2 * 2` folds to the float 7.0 in a statically-int expression; the
    // const folder must compare it with the int 7 by value, as the VM does.
    let plot = |body: &str| plot_one_bar(&format!("//@version=6\nindicator(\"D\")\n{body}"));
    assert_eq!(
        plot("b = input.bool((7 / 2) * 2 == 7, \"B\")\nplot(b ? 1 : 0)\n"),
        vec![Some(1.0)]
    );
    assert_eq!(
        plot("b = input.bool((7 / 2) * 2 != 7, \"B\")\nplot(b ? 1 : 0)\n"),
        vec![Some(0.0)]
    );
    assert_eq!(plot("plot((7 / 2) * 2 == 7 ? 1 : 0)\n"), vec![Some(1.0)]);
    // Two ints still compare exactly, past 2^53 too.
    assert_eq!(
        plot("b = input.bool(9007199254740993 == 9007199254740992, \"B\")\nplot(b ? 1 : 0)\n"),
        vec![Some(0.0)]
    );
    assert_eq!(
        plot("b = input.bool(9007199254740993 != 9007199254740992, \"B\")\nplot(b ? 1 : 0)\n"),
        vec![Some(1.0)]
    );
}

#[test]
fn a_security_history_offset_keeps_large_int_arithmetic_exact() {
    // 9007199254740993 % 10 is 3; folded through f64 it would be 2.
    let big = security_expr_plot(
        "//@version=6\nindicator(\"S\")\nplot(request.security(syminfo.tickerid, \"1W\", close[9007199254740993 % 10]))\n",
    );
    let three = security_expr_plot(
        "//@version=6\nindicator(\"S\")\nplot(request.security(syminfo.tickerid, \"1W\", close[3]))\n",
    );
    let two = security_expr_plot(
        "//@version=6\nindicator(\"S\")\nplot(request.security(syminfo.tickerid, \"1W\", close[2]))\n",
    );
    assert_eq!(big, three);
    assert_ne!(big, two);
}

#[test]
fn unreachable_code_does_not_switch_off_whole_part_truncation() {
    // The statement after `break`/`continue` is unreachable and holds a
    // `?:`. The provenance analysis records no operands for it; it must be
    // skipped, not abandon the pass, or `array.push` would receive a float.
    let dead = "z = 0\nfor i = 0 to 3\n    if i > 1\n        break\n    else\n        continue\n    z := z > 1 ? 2 : 3\n";
    assert_eq!(
        plot_n7(&format!(
            "{dead}aa = array.new_int()\naa.push(n / 2)\nplot(aa.get(0))\n"
        )),
        vec![Some(3.0)]
    );
    assert_eq!(
        plot_n7(&format!("{dead}k = n / 2\nplot(k + z)\n")),
        vec![Some(3.5)]
    );
}

#[test]
fn an_integer_only_consumer_reads_the_whole_part_of_a_fractional_int() {
    // `array.get(array.from(10, 20, 30, 40), 5 / 2)` reads index
    // 2. Every consumer that needs a whole int (an index, a size, an
    // int-typed element or field, a precision, a table cell) gets the whole
    // part through an explicit `int(...)` the front end inserts, so none of
    // them fails or misreads a fraction.
    let plot = plot_n7;
    assert_eq!(
        plot("a = array.from(10, 20, 30, 40, 50)\nplot(array.get(a, n / 2))\n"),
        vec![Some(40.0)]
    );
    assert_eq!(
        plot("a = array.new_float(n / 2, 1.0)\nplot(array.size(a))\n"),
        vec![Some(3.0)]
    );
    assert_eq!(
        plot("a = array.new_int(2, 0)\narray.set(a, 0, n / 2)\nplot(array.get(a, 0))\n"),
        vec![Some(3.0)]
    );
    assert_eq!(
        plot("a = array.new_int(0)\narray.push(a, n / 2)\nplot(array.get(a, 0))\n"),
        vec![Some(3.0)]
    );
    assert_eq!(
        plot(
            "m = matrix.new<int>(2, 2, 0)\nmatrix.set(m, 0, 0, n / 2)\nplot(matrix.get(m, 0, 0))\n"
        ),
        vec![Some(3.0)]
    );
    assert_eq!(
        plot("type T\n    int k\nt = T.new(n / 2)\nplot(t.k)\n"),
        vec![Some(3.0)]
    );
    assert_eq!(
        plot("plot(math.round(1.23456, n / 2))\n"),
        vec![Some(1.235)]
    );
    assert_eq!(
        plot("var t = table.new(position.top_right, 4, 4)\ntable.cell(t, n / 2, 0, \"x\")\nplot(1)\n"),
        vec![Some(1.0)]
    );
    // The same value read as a number keeps its fraction, including as text.
    assert_eq!(
        plot("k = n / 2\nplot(str.length(str.tostring(k)) + k)\n"),
        vec![Some(6.5)]
    );
    assert_eq!(
        plot("k = n / 2\nplot(na(k) ? 0 : math.sign(k) + k)\n"),
        vec![Some(4.5)]
    );
    // A constant used as an integer-only setting is whole.
    assert_eq!(
        plot("m = input.int(7 / 2, \"M\")\nplot(m)\n"),
        vec![Some(3.0)]
    );
    assert_eq!(
        plot("x = 7 / 2\nm = input.int(x * 2, \"M\")\nplot(m)\n"),
        vec![Some(7.0)]
    );
}

#[test]
fn a_security_history_offset_truncates_once() {
    // `close[n / 2 * 2]` with n = 3 is close[3] (1.5 * 2), not
    // close[2] (each quotient truncated). The line pins this on the folded
    // CSIL text; this compiler evaluates the offset at run time, so the pin
    // is on the values.
    let security = |n: i64, offset: &str| {
        security_expr_plot(&format!(
            "//@version=6\nindicator(\"S\")\nn = input.int({n}, \"N\")\nplot(request.security(syminfo.tickerid, \"1W\", close[{offset}]))\n"
        ))
    };
    let literal = |offset: u32| security(0, &offset.to_string());
    assert_ne!(literal(0), literal(1));
    assert_ne!(literal(1), literal(2));
    assert_ne!(literal(2), literal(3));
    for (n, offset, expected) in [
        (3, "n / 2 * 2", 3),
        (3, "n / 2", 1),
        (3, "n / 2 + 1 / 2", 2),
        (3, "int(n / 2) * 2", 2),
        (7, "n % 4", 3),
        (1, "n / 2 * 2", 1),
        (1, "n / 2", 0),
    ] {
        assert_eq!(security(n, offset), literal(expected), "n = {n}: {offset}");
    }
}

#[test]
fn a_consumer_that_failed_on_a_fraction_reads_the_whole_part() {
    let plot = plot_n7;
    // Each of these reads the whole part of the fraction.
    assert_eq!(
        plot("a = array.new_int(2, n / 2)\nplot(array.get(a, 0))\n"),
        vec![Some(3.0)]
    );
    assert_eq!(
        plot("a = array.from(1, 3, 5)\nplot(array.includes(a, n / 2) ? 1 : 0)\n"),
        vec![Some(1.0)]
    );
    assert_eq!(
        plot("a = array.from(1, 3, 5)\nplot(array.indexof(a, n / 2))\n"),
        vec![Some(1.0)]
    );
    assert_eq!(
        plot("a = array.from(1, 3, 5)\narray.fill(a, n / 2)\nplot(array.get(a, 2))\n"),
        vec![Some(3.0)]
    );
    assert_eq!(
        plot("a = array.from(1, 3, 5)\narray.insert(a, 0, n / 2)\nplot(array.get(a, 0))\n"),
        vec![Some(3.0)]
    );
    assert_eq!(
        plot("a = array.from(1, 3, 5)\narray.unshift(a, n / 2)\nplot(array.get(a, 0))\n"),
        vec![Some(3.0)]
    );
    assert_eq!(
        plot("m = map.new<string, int>()\nmap.put(m, \"a\", n / 2)\nplot(map.get(m, \"a\"))\n"),
        vec![Some(3.0)]
    );
    assert_eq!(
        plot("m = map.new<int, int>()\nmap.put(m, n / 2, 9)\nplot(map.get(m, 3))\n"),
        vec![Some(9.0)]
    );
    assert_eq!(
        plot("m = matrix.new<int>(2, 2, n / 2)\nplot(matrix.get(m, 0, 0))\n"),
        vec![Some(3.0)]
    );
    // PineWASM truncates the dimension argument of matrix.new to a whole number.
    assert_eq!(
        plot("m = matrix.new<int>(n / 2, 2, 0)\nplot(matrix.rows(m))\n"),
        vec![Some(3.0)]
    );
    // PineWASM truncates the seed argument of math.random to a whole number.
    assert_eq!(
        plot("plot(math.random(0, 1, n / 2) > 2 ? 1 : 0)\n"),
        vec![Some(0.0)]
    );
    // PineWASM truncates the length argument of ta.ema to a whole number: n / 2 is length 3.
    let length = |length: &str| {
        let outcome = run_source(
            &format!(
                "//@version=6\nindicator(\"D\")\nn = input.int(7, \"N\")\nplot(ta.ema(close, {length}))\n"
            ),
            6,
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        floats(&outcome.plots[0].values)
    };
    assert_eq!(length("n / 2"), length("int(n / 2)"));
    assert_ne!(length("n / 2"), length("n / 2 + 1"));
}

#[test]
fn a_consumer_that_misread_a_fraction_reads_the_whole_part() {
    // These consumers read the whole part of the fraction.
    // An array of size 0.
    assert_eq!(
        plot_n7("k = n / 2\na = array.new_float(k, 1.0)\nplot(array.size(a))\n"),
        vec![Some(3.0)]
    );
    // na.
    assert_eq!(
        plot_n7("plot(timestamp(\"UTC\", 2020, 1, n / 2, 0, 0, 0))\n"),
        plot_n7("plot(timestamp(\"UTC\", 2020, 1, 3, 0, 0, 0))\n")
    );
    assert_ne!(
        plot_n7("plot(timestamp(\"UTC\", 2020, 1, n / 2, 0, 0, 0))\n"),
        vec![None]
    );
    // na.
    assert_eq!(
        plot_n7("plot(dayofmonth(time + n / 2))\n"),
        plot_n7("plot(dayofmonth(time + 3))\n")
    );
}

#[test]
fn a_consumer_that_already_truncated_reads_the_same_value() {
    // These consumers truncated the fraction themselves; the value is the
    // same, and the script is charged one more instruction per cast.
    let plot = plot_n7;
    assert_eq!(
        plot("k = n / 2\na = array.from(10, 20, 30, 40, 50)\nplot(array.get(a, k))\n"),
        vec![Some(40.0)]
    );
    assert_eq!(
        plot("a = array.from(10, 20, 30, 40, 50)\nb = array.slice(a, 0, n / 2)\nplot(array.size(b))\n"),
        vec![Some(3.0)]
    );
    assert_eq!(
        plot("plot(str.length(str.repeat(\"ab\", n / 2)))\n"),
        vec![Some(6.0)]
    );
    assert_eq!(
        plot("plot(str.length(str.substring(\"abcdefgh\", n / 2)))\n"),
        vec![Some(5.0)]
    );
    for func in ["ta.sma(close", "ta.highest(high"] {
        let series = |length: &str| {
            let outcome = run_source(
                &format!(
                    "//@version=6\nindicator(\"D\")\nn = input.int(7, \"N\")\nplot({func}, {length}))\n"
                ),
                6,
            );
            assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
            floats(&outcome.plots[0].values)
        };
        assert_eq!(series("n / 2"), series("int(n / 2)"), "{func}");
    }
}

// The group `pinewasm_csil::fraction::Use::Stored`: a fraction written to a
// field, an element or a drawing coordinate. Each consumer has its own test
// so that a decision to keep the fraction is a change to `Use::reads_whole`
// plus these pins.

#[test]
fn stored_group_an_int_field_set_by_the_constructor_holds_the_whole_part() {
    assert_eq!(
        plot_n7("type T\n    int k\nt = T.new(n / 2)\nplot(t.k)\n"),
        vec![Some(3.0)]
    );
    // A float field takes the value as a number.
    assert_eq!(
        plot_n7("type T\n    float k\nt = T.new(n / 2)\nplot(t.k)\n"),
        vec![Some(3.5)]
    );
}

#[test]
fn stored_group_an_int_field_set_by_assignment_holds_the_whole_part() {
    assert_eq!(
        plot_n7("type T\n    int k\nvar t = T.new(0)\nt.k := n / 2\nplot(t.k)\n"),
        vec![Some(3.0)]
    );
}

#[test]
fn stored_group_an_int_element_built_by_array_from_holds_the_whole_part() {
    assert_eq!(
        plot_n7("a = array.from(n / 2, 1)\nplot(array.get(a, 0))\n"),
        vec![Some(3.0)]
    );
}

#[test]
fn stored_group_a_drawing_coordinate_set_by_the_constructor_holds_the_whole_part() {
    // bar_index is 0 on the only bar.
    assert_eq!(
        plot_n7("l = label.new(bar_index + n / 2, close, \"x\")\nplot(label.get_x(l))\n"),
        vec![Some(3.0)]
    );
    assert_eq!(
        plot_n7(
            "ln = line.new(bar_index, close, bar_index + n / 2, close)\nplot(line.get_x2(ln))\n"
        ),
        vec![Some(3.0)]
    );
    assert_eq!(
        plot_n7("b = box.new(bar_index, high, bar_index + n / 2, low)\nplot(box.get_right(b))\n"),
        vec![Some(3.0)]
    );
}

#[test]
fn stored_group_a_drawing_coordinate_set_by_a_setter_holds_the_whole_part() {
    assert_eq!(
        plot_n7(
            "var l = label.new(0, close, \"x\")\nlabel.set_x(l, bar_index + n / 2)\nplot(label.get_x(l))\n"
        ),
        vec![Some(3.0)]
    );
}

#[test]
fn a_value_read_both_ways_is_truncated_only_for_the_integer_consumer() {
    // `y` indexes the array as 3 and adds as 3.5.
    assert_eq!(
        plot_n7(
            "x = n / 2\ny = x\na = array.from(10, 20, 30, 40, 50)\nplot(array.get(a, y) + y)\n"
        ),
        vec![Some(43.5)]
    );
    // Through `?:` and `math.max`, which forward the fraction.
    assert_eq!(
        plot_n7("a = array.from(10, 20, 30, 40, 50)\nplot(array.get(a, n > 3 ? n / 2 : 1))\n"),
        vec![Some(40.0)]
    );
    assert_eq!(
        plot_n7("a = array.from(10, 20, 30, 40, 50)\nplot(array.get(a, math.max(n / 2, 1)))\n"),
        vec![Some(40.0)]
    );
    // A float-typed destination is converted, not truncated.
    assert_eq!(plot_n7("plot(math.pow(n / 2, 2))\n"), vec![Some(12.25)]);
    assert_eq!(plot_n7("plot(math.avg(n / 2, 1))\n"), vec![Some(2.25)]);
}

#[test]
fn the_rewrite_inserts_a_cast_only_where_an_integer_consumer_reads_a_quotient() {
    let script =
        |body: &str| format!("//@version=6\nindicator(\"D\")\nn = input.int(7, \"N\")\n{body}");
    // No division, or a division read only as a number: nothing inserted.
    assert_eq!(int_casts(&script("plot(n * 2)\n")), 0);
    assert_eq!(int_casts(&script("k = n / 2\nplot(k + 1)\n")), 0);
    assert_eq!(int_casts(&script("plot(close[n / 2])\n")), 0);
    // The script's own cast is the only one.
    assert_eq!(int_casts(&script("plot(int(n / 2))\n")), 1);
    // One integer-only reader, one cast.
    assert_eq!(
        int_casts(&script(
            "a = array.from(10, 20, 30, 40, 50)\nplot(array.get(a, n / 2))\n"
        )),
        1
    );
}

#[test]
fn an_inserted_cast_is_a_charged_instruction() {
    // The rewrite produces exactly the code of the script that spells the
    // cast, so both engines charge it like any other instruction: the least
    // per-bar budget each script completes under is the same, and it is one
    // more than the instructions executed without the cast would need.
    let script = |value: &str| {
        format!(
            "//@version=6\nindicator(\"D\")\nn = input.int(7, \"N\")\na = array.new_int(0)\narray.push(a, {value})\nplot(array.get(a, 0))\n"
        )
    };
    let implicit = compile(&script("n / 2"));
    let explicit = compile(&script("int(n / 2)"));
    assert_eq!(implicit.main().code, explicit.main().code);
    let least_budget = |program: &pinewasm_csil::CsilProgram| {
        (1..500u64)
            .find(|limit| {
                run(
                    program,
                    &data_from_close(&[1.0]),
                    &RunOptions {
                        max_instructions_per_bar: *limit,
                        ..RunOptions::default()
                    },
                )
                .errors
                .is_empty()
            })
            .expect("the script completes under some budget")
    };
    let whole = compile(&script("n - 4"));
    assert_eq!(least_budget(&implicit), least_budget(&explicit));
    assert_eq!(least_budget(&implicit), least_budget(&whole) + 1);
}

// -- a consumer that takes a number is never truncated -------------------------

/// The log messages of a script with `n = input.int(7)` followed by `body`,
/// run on one bar.
fn logs_n7(body: &str) -> Vec<String> {
    let outcome = run_source(
        &format!("//@version=6\nindicator(\"D\")\nn = input.int(7, \"N\")\n{body}plot(close)\n"),
        1,
    );
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    outcome.logs.into_iter().map(|log| log.message).collect()
}

#[test]
fn float_of_a_fractional_int_keeps_the_fraction() {
    // `float()` takes its argument as a number, so the fraction is kept.
    assert_eq!(plot_n7("plot(float(n / 2))\n"), vec![Some(3.5)]);
    assert_eq!(plot_n7("x = n / 2\nplot(float(x) + 1)\n"), vec![Some(4.5)]);
    assert_eq!(
        plot_n7("float y = float(n / 2) * 2\nplot(y)\n"),
        vec![Some(7.0)]
    );
}

#[test]
fn float_of_a_constant_quotient_folds_and_runs_to_the_same_value() {
    // The const folder and the interpreter agree on `float(7 / 2)`.
    let plot = |body: &str| plot_one_bar(&format!("//@version=6\nindicator(\"D\")\n{body}"));
    assert_eq!(plot("plot(float(7 / 2))\n"), vec![Some(3.5)]);
    assert_eq!(
        plot("f = input.float(float(7 / 2), \"F\")\nplot(f)\n"),
        vec![Some(3.5)]
    );
}

#[test]
fn a_logged_fractional_int_is_formatted_with_its_fraction() {
    // `log.*` formats its arguments by `str.format`'s rules, so the fraction
    // is printed.
    assert_eq!(logs_n7("x = n / 2\nlog.info(\"{0}\", x)\n"), ["3.5"]);
    assert_eq!(
        logs_n7("log.info(\"v={0} w={1}\", n / 2, n / 2 + 1)\n"),
        ["v=3.5 w=4.5"]
    );
    assert_eq!(logs_n7("log.error(\"x {0}\", n * 3 / 4)\n"), ["x 5.25"]);
    assert_eq!(logs_n7("log.warning(\"{0}\", n / 2)\n"), ["3.5"]);
    // The same rules as `str.format` and `str.tostring`.
    assert_eq!(
        logs_n7("log.info(str.format(\"{0}\", n / 2) + \" \" + str.tostring(n / 2))\n"),
        ["3.5 3.5"]
    );
}

/// Consumers that accept a number. `Q` is replaced by `(n / 2)` with an int
/// input and by `(nf / 2)` with a float input of the same value; the script
/// must plot and log the same either way. A consumer that truncated the int
/// quotient would differ from the float one.
const NUMBER_CONSUMERS: &[&str] = &[
    "plot(Q)\n",
    "plot(float(Q))\n",
    "plot(float(Q) + 1)\n",
    "x = Q\nplot(float(x) * 2)\n",
    "plot(math.round(Q, 1))\n",
    "plot(math.pow(Q, 2))\n",
    "plot(math.sqrt(Q))\n",
    "plot(math.exp(Q))\n",
    "plot(math.log(Q))\n",
    "plot(math.log10(Q))\n",
    "plot(math.sin(Q))\n",
    "plot(math.cos(Q))\n",
    "plot(math.tan(Q))\n",
    "plot(math.atan(Q))\n",
    "plot(math.avg(Q, 1))\n",
    "plot(math.avg(Q, Q, 2))\n",
    "plot(math.sum(Q + bar_index, 2))\n",
    "plot(math.todegrees(Q))\n",
    "plot(math.toradians(Q))\n",
    "plot(math.round_to_mintick(Q))\n",
    "plot(math.abs(Q) + math.sign(Q))\n",
    "plot(math.max(Q, 1) + math.min(Q, 10))\n",
    "plot(nz(Q) + (na(Q) ? 1 : 0))\n",
    "plot(ta.change(Q + bar_index))\n",
    "plot(ta.sma(Q + bar_index, 2))\n",
    "plot(ta.ema(Q + bar_index, 2))\n",
    "plot(ta.highest(Q + bar_index, 2))\n",
    "plot(ta.lowest(Q + bar_index, 2))\n",
    "plot(ta.cum(Q))\n",
    "plot(ta.wma(Q + bar_index, 2))\n",
    "plot(ta.rsi(Q + bar_index, 2))\n",
    "plot(ta.stdev(Q + bar_index, 2))\n",
    "plot(fixnan(Q))\n",
    // `ta.median` is deliberately absent: PineWASM's int form truncates
    // (`ta_int_forms.rs`, docs/language-support.md "Integer `ta.median`").
    "plot(ta.mode(Q + bar_index % 2, 3))\n",
    "plot(ta.range(bar_index % 2 == 0 ? Q : Q + 1, 3))\n",
    "plot(ta.mode(Q + bar_index % 2, bar_index % 2 + 2))\n",
    "plot(ta.range(bar_index % 2 == 0 ? Q : Q + 1, bar_index % 2 + 2))\n",
    "x = Q + bar_index % 3\nplot(ta.mode(x, 4) + ta.range(x, 4))\n",
    "plot(ta.valuewhen(true, Q, 0))\n",
    "x = Q\nplot(x[1] + x)\n",
    "a = array.new_float(2, Q)\nplot(array.get(a, 0))\n",
    "a = array.from(Q, 1.5)\nplot(array.get(a, 0))\n",
    "a = array.new_float(0)\narray.push(a, Q)\nplot(array.get(a, 0))\n",
    "a = array.new_float(0)\narray.unshift(a, Q)\nplot(array.get(a, 0))\n",
    "a = array.new_float(2, 0.0)\narray.set(a, 0, Q)\nplot(array.get(a, 0))\n",
    "a = array.new_float(2, 0.0)\narray.insert(a, 0, Q)\nplot(array.get(a, 0))\n",
    "a = array.new_float(2, 0.0)\narray.fill(a, Q)\nplot(array.get(a, 1))\n",
    "a = array.from(1.5, 3.5, 4.0)\nplot(array.includes(a, Q) ? 1 : 0)\n",
    "a = array.from(1.5, 3.5, 4.0)\nplot(array.indexof(a, Q))\n",
    "a = array.from(1.5, 3.5, 4.0)\nplot(array.lastindexof(a, Q))\n",
    "a = array.from(1.5, 3.5, 4.0)\nplot(array.binary_search(a, Q))\n",
    "a = array.from(1.5, 3.0, 4.0)\nplot(array.binary_search_leftmost(a, Q))\n",
    "a = array.from(10.0, 20.0, 30.0, 40.0)\nplot(array.percentile_linear_interpolation(a, Q * 10))\n",
    "m = matrix.new<float>(2, 2, Q)\nplot(matrix.get(m, 0, 0))\n",
    "m = matrix.new<float>(2, 2, 0.0)\nmatrix.set(m, 0, 0, Q)\nplot(matrix.get(m, 0, 0))\n",
    "m = matrix.new<float>(2, 2, 0.0)\nmatrix.fill(m, Q)\nplot(matrix.get(m, 1, 1))\n",
    "m = matrix.new<float>(2, 2, 1.0)\nr = matrix.mult(m, Q)\nplot(matrix.get(r, 0, 0))\n",
    "m = matrix.new<float>(2, 2, 1.0)\nr = matrix.sum(m, Q)\nplot(matrix.get(r, 0, 0))\n",
    "m = matrix.new<float>(2, 2, 1.0)\nr = matrix.diff(m, Q)\nplot(matrix.get(r, 0, 0))\n",
    "m = map.new<string, float>()\nmap.put(m, \"a\", Q)\nplot(map.get(m, \"a\"))\n",
    "m = map.new<float, float>()\nmap.put(m, Q, 2.0)\nplot(map.contains(m, 3.5) ? 1 : 0)\n",
    "type T\n    float k\nt = T.new(Q)\nplot(t.k)\n",
    "type T\n    float k\nvar t = T.new(0.0)\nt.k := Q\nplot(t.k)\n",
    "plot(color.t(color.new(color.red, Q * 10)))\n",
    "plot(color.t(color.rgb(10, 20, 30, Q * 10)))\n",
    "plot(color.r(color.from_gradient(Q, 0, 7, color.black, color.white)))\n",
    "f(float v) => v * 2\nplot(f(Q))\n",
    "f(v) => v + 1\nplot(f(Q))\n",
    "f(v) =>\n    w = v\n    [w, w * 2]\n[a, b] = f(Q)\nplot(a + b)\n",
    "x = switch\n    close > 0 => Q\n    => 0.0\nplot(x)\n",
    "x = if close > 0\n    Q\nelse\n    0.0\nplot(x)\n",
    "x = close > 0 ? Q : 0.0\nplot(x)\n",
    "float x = Q\nplot(x)\n",
    "var float x = 0.0\nx := x + Q\nplot(x)\n",
    "plot(Q > 3 ? 1 : 0)\n",
    "plot(Q == 3.5 ? 1 : 0)\n",
    "plot(close[Q] - close[3])\n",
    "plot(str.length(str.tostring(Q)))\n",
    "plot(str.length(str.tostring(Q, \"#.##\")))\n",
    "plot(str.length(str.format(\"{0}\", Q)))\n",
    "log.info(\"{0}\", Q)\nplot(close)\n",
    "log.warning(\"v={0} w={1}\", Q, Q + 1)\nplot(close)\n",
    "log.error(str.tostring(Q))\nplot(close)\n",
    "l = label.new(bar_index, Q, \"x\")\nplot(label.get_y(l))\n",
    "ln = line.new(bar_index, Q, bar_index + 1, Q + 1)\nplot(line.get_y2(ln))\n",
    "b = box.new(bar_index, Q + 1, bar_index + 1, Q)\nplot(box.get_top(b))\n",
    "plot(close, linewidth = 1)\nhline(1)\nplot(Q * close)\n",
    "plot(close + Q)\n",
];

#[test]
fn an_int_quotient_reads_like_the_same_float_quotient_wherever_a_number_is_accepted() {
    let observe = |declaration: &str, quotient: &str, body: &str| {
        let source = format!(
            "//@version=6\nindicator(\"Q\")\n{declaration}\n{}",
            body.replace('Q', quotient)
        );
        let outcome = run_source(&source, 8);
        assert!(outcome.errors.is_empty(), "{source}: {:?}", outcome.errors);
        let plots: Vec<Vec<Option<u64>>> = outcome
            .plots
            .iter()
            .map(|plot| {
                floats(&plot.values)
                    .into_iter()
                    .map(|value| value.map(f64::to_bits))
                    .collect()
            })
            .collect();
        let logs: Vec<String> = outcome.logs.into_iter().map(|log| log.message).collect();
        (plots, logs)
    };
    let mut fractional = 0;
    for body in NUMBER_CONSUMERS {
        let int = observe("n = input.int(7, \"N\")", "(n / 2)", body);
        let float = observe("nf = input.float(7.0, \"N\")", "(nf / 2)", body);
        assert_eq!(int, float, "{body}");
        fractional += usize::from(int != observe("n = input.int(6, \"N\")", "(n / 2)", body));
    }
    // The comparison means something only if the quotient reaches the output:
    // most shapes must give a different result for the whole quotient 6 / 2.
    assert!(
        fractional * 10 >= NUMBER_CONSUMERS.len() * 9,
        "{fractional} of {} shapes depend on the quotient",
        NUMBER_CONSUMERS.len()
    );
}

#[test]
fn every_compiled_script_with_a_division_is_analyzed() {
    // The rewrite declines (and leaves integer-only consumers to meet a
    // fraction) when its provenance analysis cannot describe the code. No
    // script here may take that path: lazy `and`/`or`, loops with `break`
    // and `continue`, `switch`, `var`, tuples, methods, nested ternaries.
    use pinewasm_csil::fraction::{whole_part_producers, Outcome};
    let bodies = [
        "a = array.new_int(0)\nif n > 3 and n / 2 > 1 or n / 3 > 5\n    array.push(a, n / 2)\nplot(array.size(a))\n",
        "a = array.new_int(0)\nfor i = 0 to n / 2\n    if i > 2\n        break\n    if i == 1\n        continue\n    array.push(a, i / 2)\nplot(array.size(a))\n",
        "a = array.new_int(0)\ni = 0\nwhile i < n / 2\n    i += 1\n    array.push(a, i / 2)\nplot(array.size(a))\n",
        "var a = array.new_int(0)\nvar int k = n / 2\nk := k + n / 3\nx = switch\n    k > 3 => n / 2\n    k > 2 => n / 3\n    => 0\narray.push(a, x)\nplot(array.size(a))\n",
        "f(v) =>\n    w = v / 2\n    [w, w / 2]\n[p, q] = f(n)\na = array.from(10, 20, 30, 40, 50)\nplot(array.get(a, p) + array.get(a, q))\n",
        "a = array.from(10, 20, 30, 40, 50)\nplot(a.get(n > 5 ? n > 6 ? n / 2 : n / 3 : 0))\n",
        "a = array.from(10, 20, 30, 40, 50)\nfor [i, v] in a\n    if v > n / 2 and not (i == n / 3)\n        array.set(a, i / 2, v)\nplot(array.sum(a))\n",
    ];
    for body in bodies {
        let source = format!("//@version=6\nindicator(\"D\")\nn = input.int(7, \"N\")\n{body}");
        let program = compile(&source);
        let outcome = run(
            &program,
            &data_from_close(&[1.0, 2.0]),
            &RunOptions::default(),
        );
        assert!(outcome.errors.is_empty(), "{body}: {:?}", outcome.errors);
        // The shipped code and its pre-optimization code both analyze.
        let main = program.main();
        let mut bodies = vec![main.code.clone()];
        bodies.extend(main.budget_reference.iter().map(|r| r.code.clone()));
        for code in bodies {
            assert!(
                whole_part_producers(&code, &program.ta_states, program.series_slots.len()).is_ok(),
                "{body}"
            );
        }
        let _ = Outcome::NotAnalyzed;
    }
}

#[test]
fn consumers_outside_the_whole_part_list_see_what_they_saw_before() {
    // Not on the list, so the value is not touched. PineWASM passes the scalar
    // of `matrix.mult` and `matrix.sum` on a `matrix<int>` as a number.
    assert_eq!(
        plot_n7(
            "m = matrix.new<int>(2, 2, 2)\nr = matrix.mult(m, n / 2)\nplot(matrix.get(r, 0, 0))\n"
        ),
        vec![Some(7.0)]
    );
    assert_eq!(
        plot_n7(
            "m = matrix.new<int>(2, 2, 2)\nr = matrix.sum(m, n / 2)\nplot(matrix.get(r, 0, 0))\n"
        ),
        vec![Some(5.5)]
    );
    // A fraction that comes back out of `ta.valuewhen` or `fixnan` is
    // followed into an int container: the whole part is pushed.
    for call in ["ta.valuewhen(true, x, 0)", "fixnan(x)"] {
        let outcome = run_source(
            &format!(
                "//@version=6\nindicator(\"D\")\nx = bar_index / 2\na = array.new_int()\na.push({call})\nplot(a.get(0))\n"
            ),
            4,
        );
        assert!(outcome.errors.is_empty(), "{call}: {:?}", outcome.errors);
        assert_eq!(
            floats(&outcome.plots[0].values),
            [Some(0.0), Some(0.0), Some(1.0), Some(1.0)],
            "{call}"
        );
    }
    // A fraction read back from history is followed as any slot read is.
    let outcome = run_source(
        "//@version=6\nindicator(\"D\")\nx = bar_index / 2\na = array.new_int()\na.push(x[1])\nplot(a.get(0))\n",
        4,
    );
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
}

#[test]
fn a_ticker_count_reads_the_whole_part_and_a_price_amount_keeps_its_fraction() {
    // The test reads back the ticker id strings PineWASM builds, through the log.
    let length = |call: &str| logs_n7(&format!("log.info({call})\n"));
    assert_ne!(
        length("ticker.linebreak(syminfo.tickerid, 3)"),
        length("ticker.linebreak(syminfo.tickerid, 4)")
    );
    // PineWASM truncates the `number_of_lines` argument of `ticker.linebreak`
    // to a whole number, so `n / 2` gives the id for 3. A fraction reaches it
    // only through an int-typed quotient. The test checks that `ticker.kagi`
    // ids differ for 3.5 and 3.
    assert_eq!(
        length("ticker.linebreak(syminfo.tickerid, n / 2)"),
        length("ticker.linebreak(syminfo.tickerid, 3)")
    );
    assert_ne!(
        length("ticker.kagi(syminfo.tickerid, 3.5)"),
        length("ticker.kagi(syminfo.tickerid, 3)")
    );
    // PineWASM truncates the reversal argument of `ticker.pointfigure` to a
    // whole number, and keeps the box size as a number.
    assert_eq!(
        length("ticker.pointfigure(syminfo.tickerid, \"hl\", \"ATR\", 14, n / 2)"),
        length("ticker.pointfigure(syminfo.tickerid, \"hl\", \"ATR\", 14, 3)")
    );
    assert_eq!(
        length("ticker.pointfigure(syminfo.tickerid, \"hl\", \"Traditional\", n / 2, 3)"),
        length("ticker.pointfigure(syminfo.tickerid, \"hl\", \"Traditional\", 3.5, 3)")
    );
    // The test checks that the fractional `ticker.renko` param `n / 2` gives
    // the same id as 3.5 and a different id from 3.
    assert_eq!(
        length("ticker.renko(syminfo.tickerid, \"ATR\", n / 2)"),
        length("ticker.renko(syminfo.tickerid, \"ATR\", 3.5)")
    );
    assert_ne!(
        length("ticker.renko(syminfo.tickerid, \"ATR\", n / 2)"),
        length("ticker.renko(syminfo.tickerid, \"ATR\", 3)")
    );
    assert_eq!(
        length("ticker.kagi(syminfo.tickerid, n / 2)"),
        length("ticker.kagi(syminfo.tickerid, 3.5)")
    );
}

#[test]
fn the_whole_part_of_a_quotient_beyond_the_int_range_is_na() {
    // `int(x)` maps a value outside the int range to na, so this push stores
    // na.
    assert_eq!(
        plot_one_bar(
            "//@version=6\nindicator(\"D\")\na = array.new_int()\na.push(9223372036854775807 / 2 * 4)\nplot(na(a.get(0)) ? 1 : 0)\n"
        ),
        vec![Some(1.0)]
    );
}

#[test]
fn table_and_window_coordinates_read_the_whole_part() {
    // `table.clear` and `table.merge_cells` coordinates, `str.replace`
    // occurrence, `array.max` nth, `timeframe.from_seconds`.
    assert_eq!(
        plot_n7("var t = table.new(position.top_right, 4, 4)\ntable.clear(t, 0, 0, n / 2, 1)\nplot(1)\n"),
        vec![Some(1.0)]
    );
    assert_eq!(
        plot_n7("var t = table.new(position.top_right, 4, 4)\ntable.merge_cells(t, 0, 0, n / 2, 1)\nplot(1)\n"),
        vec![Some(1.0)]
    );
    assert_eq!(
        plot_n7("plot(timeframe.in_seconds(timeframe.from_seconds(n * 60 / 2)))\n"),
        vec![Some(210.0)]
    );
    assert_eq!(
        plot_n7("a = array.from(1, 3, 5)\nplot(array.binary_search(a, n / 2))\n"),
        vec![Some(1.0)]
    );
}
