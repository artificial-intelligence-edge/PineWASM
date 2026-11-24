//! Scripts that keep the result of dividing two ints, shared by the suites
//! that compare a module with the interpreter: the standalone module
//! (`pine_int_div_carried.rs`) and the full-run module (`differential.rs`).
//! Each suite prepends its own header, which declares `n = input.int(...)`
//! (and `big` for the large-int bodies).

/// Everyday shapes that keep the result of dividing two ints. `n` is an int
/// input; every body ends in exactly one plot.
pub const KEPT_INT_DIVISION_BODIES: &[(&str, &str)] = &[
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
    // Integer-only consumers read the whole part.
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

/// More shapes, for consumers that the front end and typed pass handle in
/// their own way: `ta.*` sources and slot-linked windows, history of the variable
/// itself, a dynamic history offset on a `ta.*` result, an int element
/// written through the whole-part cast and the same value read as a number,
/// a typed user function, and a `var int` seeded through `nz`.
pub const KEPT_INT_DIVISION_BODIES_ON_MAIN: &[(&str, &str)] = &[
    ("ta.sma source", "k = n / 2 + 1\nplot(ta.sma(k, 3))\n"),
    ("ta.change source", "k = n / 2 + 1\nplot(ta.change(k))\n"),
    (
        "ta.highest source",
        "k = n / 2 + 1\nplot(ta.highest(k, 3))\n",
    ),
    ("ta.cum source", "k = n / 2 + 1\nplot(ta.cum(k))\n"),
    ("own history", "k = bar_index / 2 + n\nplot(k[1] + k)\n"),
    (
        "offset into a ta result",
        "k = n / 2 + 1\nplot(ta.ema(close, 3)[k])\n",
    ),
    (
        "int element and number",
        "k = n / 2 + 1\na = array.new_int(3, 1)\narray.set(a, 0, k)\nplot(array.get(a, 0) + k)\n",
    ),
    (
        "float parameters",
        "k = n / 2 + 1\nplot(math.pow(k, 2) + math.avg(k, 1))\n",
    ),
    (
        "typed function",
        "f(int x) => x * 2\nk = n / 2 + 1\nplot(f(k) + k)\n",
    ),
    (
        "var int through nz",
        "k = n / 2 + 1\nvar int m = na\nm := nz(m, k) + k\nplot(m)\n",
    ),
    // `float(x)` reads the value as a number, fraction included.
    ("float cast", "k = n / 2 + 1\nplot(float(k))\n"),
    (
        "float cast in arithmetic",
        "k = n / (bar_index % 3) - 1\nplot(float(k) * 2 + k)\n",
    ),
    (
        "index and number",
        "k = n / 2\na = array.new_float(6, 2.5)\nplot(array.get(a, math.abs(k)) + k)\n",
    ),
];

/// Whole ints around the edges of what f64 holds exactly.
pub const LARGE_INTS: &[i64] = &[
    i64::MAX,
    9_223_372_036_854_774_784,
    -9_223_372_036_854_774_784,
    9_007_199_254_740_990,
    9_007_199_254_740_992,
    9_007_199_254_740_994,
    18_014_398_509_481_984,
    21_000_000_000_000_000,
    21_000_000_000_000_007,
    1_152_921_504_606_846_976,
    4_611_686_018_427_387_904,
    4_611_686_018_427_387_905,
    9_223_372_036_854_775_000,
    -9_007_199_254_740_992,
    -21_000_000_000_000_000,
    -4_611_686_018_427_387_904,
    3_000_000_000,
    94_906_267,
];

/// Kept quotients of `big` meeting whole-int arithmetic.
pub const LARGE_INT_BODIES: &[&str] = &[
    "plot(big / 2)\n",
    "k = big / 2\nplot(k + 1)\n",
    "k = big / 2\nplot(k - big)\n",
    "k = big / 4\nplot(k * 3)\n",
    "k = big / 4\nplot(k * big)\n",
    "k = big / 3\nplot(k * 3)\n",
    "k = big / 8\nplot(k % 7)\n",
    "k = big / 4 - big / 8\nplot(k)\n",
    "k = big / 2 / 2 / 2\nplot(k)\n",
    "k = n / 2 + big\nplot(k)\n",
    "k = n / 2 * big\nplot(k)\n",
    "k = big / n\nplot(k)\n",
    "k = big / -1\nplot(k + 1)\n",
    "k = big / (n - 7)\nplot(k)\n",
    "k = -(big / 2)\nplot(k)\n",
    "plot(big / 2 > big / 4 ? 1 : 0)\n",
    "plot(big / 2 == big - big / 2 ? 1 : 0)\n",
    "plot(int(big / 2) % 1000)\n",
    "k = big / 2\nplot(math.max(k, big))\n",
    "var int acc = 0\nacc := acc + big / 2\nplot(acc)\n",
    "k = big / 2\nplot(k * k)\n",
    // A product of exactly -2^63 is `i64::MIN`, whose negation and
    // division by -1 are `na` in the interpreter.
    "k = big / 2 * -4\nplot(-k)\n",
    "k = big / 2 * -4\nplot(k / -1)\n",
];
