//! Integer-division test scripts: 47 bodies, each run for eight values of
//! `n` over 24 bars, checked against the expected-output fixture.
//!
//! The fixture module below holds the committed regression output of `render`. It records,
//! per script, either `error` or the bit pattern of every plotted value.

use super::{compile, data_from_close, run};
use crate::RunOptions;
use pinewasm_mir::Value;
use std::fmt::Write as _;

#[path = "int_division_expected.rs"]
mod int_division_expected;

const INT_DIVISION_BODIES: &[(&str, &str)] = &[
    ("kept and plotted", "int k = n / 2\nplot(k)\n"),
    (
        "beside unreachable ternary",
        "z = 0\nfor i = 0 to 3\n    if i > 1\n        break\n    else\n        continue\n    z := z > 1 ? 2 : 3\nk = n / 2\nplot(k + z)\n",
    ),
    (
        "unreachable quotient",
        "z = 0\nfor i = 0 to 3\n    if i > 1\n        break\n    else\n        continue\n    z := z + n / 2\nk = n / 2 + 1\nplot(k + z)\n",
    ),
    (
        "int array beside unreachable ternary",
        "z = 0\nfor i = 0 to 3\n    if i > 1\n        break\n    else\n        continue\n    z := z > 1 ? 2 : 3\nplot(n / 2 > 3 ? 1 : int(n / 2) + z)\n",
    ),
    ("literal over literal", "a = 7 / 2\nplot(a + n)\n"),
    ("plotted directly", "plot(n / 2)\n"),
    ("int() cast", "k = int(n / 2)\nplot(k)\n"),
    ("math.round", "k = math.round(n / 2)\nplot(k)\n"),
    ("math.floor", "k = math.floor(n / 2)\nplot(k)\n"),
    ("math.ceil", "k = math.ceil(n / 2)\nplot(k)\n"),
    ("history offset", "plot(close[n / 2])\n"),
    ("int() history offset", "plot(close[int(n / 2)])\n"),
    ("then int arithmetic", "k = n / 2 + 1\nplot(k)\n"),
    ("chained arithmetic", "k = (n / 2 - 1) * 3 % 4\nplot(-k)\n"),
    (
        "modulo by quotient",
        "half = n / 2\nplot(bar_index % half == 0 ? 1 : 0)\n",
    ),
    (
        "quotient modulo",
        "half = n / 2\nplot((bar_index - 5) % 3 + half % 2)\n",
    ),
    (
        "compare with slot",
        "half = n / 2\nplot(bar_index > half ? 1 : -1)\n",
    ),
    (
        "equality with slot",
        "half = n / 2\nplot(bar_index == half ? 1 : half != 3 ? 2 : 3)\n",
    ),
    ("compare inline", "plot(n / 2 > 3 ? 1 : -1)\n"),
    ("series quotient", "x = bar_index / 2\nplot(x + 1)\n"),
    ("series quotient history", "x = bar_index / 2\nplot(x[1])\n"),
    ("declared float", "float k = n / 2\nplot(k)\n"),
    (
        "accumulated",
        "var int acc = 0\nacc := acc + bar_index / 2\nplot(acc)\n",
    ),
    (
        "loop bound",
        "s = 0.0\nfor i = 0 to n / 2\n    s += close[i]\nplot(s)\n",
    ),
    (
        "midpoint compare",
        "mid = (bar_index + n) / 2\nplot(close > mid ? 1 : -1)\n",
    ),
    ("times float", "plot(n / 2 * 2.0)\n"),
    (
        "ternary arms",
        "k = bar_index % 2 == 0 ? n / 2 : n\nplot(k * 2)\n",
    ),
    (
        "nz and extremes",
        "k = nz(n / (bar_index % 3), 1)\nplot(math.max(k, 2) + math.abs(k) - math.min(k, n))\n",
    ),
    ("quotient of quotient", "k = n / 2 / 2\nplot(k)\n"),
    ("reassigned", "int k = n\nk := k / 2\nk := k / 2\nplot(k)\n"),
    (
        "zero divisor",
        "k = n / (bar_index % 2)\nplot(nz(k + 1, -9))\n",
    ),
    ("na test", "k = n / (bar_index % 3)\nplot(na(k) ? -1 : k)\n"),
    ("math.sign", "k = n / 2 - 3\nplot(math.sign(k) + k)\n"),
    ("bool cast", "k = n / 2 - 3\nplot(bool(k) ? 1 : 0)\n"),
    (
        "float step",
        "s = 0\nfor i = 0 to 10 by n / 2 + 4\n    s += 1\nplot(s)\n",
    ),
    (
        "while bound",
        "k = n / 2\ni = 0\nwhile i < k\n    i += 1\nplot(i)\n",
    ),
    (
        "switch arm",
        "k = n / 2\nx = switch\n    k > 3 => 1\n    => 0\nplot(x)\n",
    ),
    ("typed parameter", "f(int x) => x + 1\nplot(f(n / 2))\n"),
    (
        "returned int",
        "f() =>\n    int r = n / 2\n    r\nplot(f() * 2)\n",
    ),
    // Integer-only consumers truncate the fractional part, as PineWASM defines it.
    (
        "array size",
        "a = array.new_float(n / 2 + 4, 1.0)\nplot(array.size(a))\n",
    ),
    (
        "int array element",
        "a = array.new_int(2, 0)\narray.set(a, 0, n / 2)\nplot(array.get(a, 0))\n",
    ),
    (
        "int array initial value",
        "a = array.new_int(2, n / 2)\nplot(array.get(a, 1))\n",
    ),
    (
        "int array push",
        "a = array.new_int(0)\narray.push(a, n / 2)\nplot(array.get(a, 0))\n",
    ),
    (
        "int field",
        "type T\n    int k\nt = T.new(n / 2)\nplot(t.k)\n",
    ),
    (
        "int field assigned",
        "type T\n    int k = 0\nvar t = T.new()\nt.k := n / 2\nplot(t.k + 1)\n",
    ),
    ("round precision", "plot(math.round(close, n / 2 + 4))\n"),
    (
        "color component",
        "plot(close, color = color.rgb(n / 2 + 100, 0, 0))\n",
    ),
];

const INPUTS: [i64; 8] = [7, 8, 9, 1, 0, -7, -8, 6];

fn render() -> String {
    let closes: Vec<f64> = (0..24)
        .map(|i| 100.0 + f64::from(i) * 0.05 + (f64::from(i) * 1.3).cos() * 0.5)
        .collect();
    let data = data_from_close(&closes);
    let mut out = String::new();
    for n in INPUTS {
        for (name, body) in INT_DIVISION_BODIES {
            let source = format!(
                "//@version=6\nindicator(\"integer division\")\nn = input.int({n}, \"N\")\n{body}"
            );
            let outcome = run(&compile(&source), &data, &RunOptions::default());
            write!(out, "{name} | n = {n} |").unwrap();
            if outcome.errors.is_empty() {
                for plot in &outcome.plots {
                    out.push_str(" plot");
                    for value in &plot.values {
                        match value {
                            Value::Float(x) => write!(out, " {:016x}", x.to_bits()).unwrap(),
                            Value::Na => out.push_str(" na"),
                            other => write!(out, " {other:?}").unwrap(),
                        }
                    }
                }
            } else {
                out.push_str(" error");
            }
            out.push('\n');
        }
    }
    out
}

#[test]
fn kept_integer_division_matches_expected_outputs() {
    let rendered = render();
    if std::env::var_os("KEPT_INT_DIVISION_PRINT").is_some() {
        print!("{rendered}");
    }
    let expected = int_division_expected::EXPECTED;
    assert_eq!(expected.lines().count(), 47 * 8);
    // The comparison is only meaningful if uneven quotients reached a plot
    // and few scripts stopped.
    let fractional = expected
        .split_whitespace()
        .filter(|token| token.len() == 16)
        .filter_map(|token| u64::from_str_radix(token, 16).ok())
        .map(f64::from_bits)
        .filter(|x| x.is_finite() && x.fract() != 0.0)
        .count();
    assert!(fractional > 400, "only {fractional} fractional samples");
    let errors = expected.lines().filter(|l| l.ends_with("| error")).count();
    assert!(errors <= 4, "{errors} scripts stopped");
    for (ours, theirs) in rendered.lines().zip(expected.lines()) {
        assert_eq!(ours, theirs);
    }
    assert_eq!(rendered, expected);
}
