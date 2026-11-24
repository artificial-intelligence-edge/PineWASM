//! Numerical contracts, using synthetic bars. Expected values are written by
//! hand from the language rules.
use pinewasm_compiler::{
    compile_and_run, CompileOptions, MarketData, RunOptions, RuntimeErrorKind, Value,
};
use pinewasm_migrate::{migrate, MigrationOptions, Status};

fn values(source: &str, closes: &[f64]) -> Vec<Vec<Option<f64>>> {
    let migrated = migrate(source, &MigrationOptions::default());
    assert_eq!(
        migrated.status,
        Status::Migrated,
        "{:?}",
        migrated.parse_errors
    );
    assert!(!migrated.findings.iter().any(|f| f.rule == "internal"));
    let n = closes.len();
    let market = MarketData {
        open: vec![0.0; n],
        high: closes.to_vec(),
        low: closes.to_vec(),
        close: closes.to_vec(),
        volume: vec![100.0; n],
        time: (0..n).map(|i| i as i64 * 60_000).collect(),
        mintick: 0.01,
        ..Default::default()
    };
    let result = compile_and_run(
        &migrated.output,
        &market,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        !result
            .diagnostics
            .iter()
            .any(|d| d.severity == pinewasm_span::Severity::Error),
        "{:?}\n{}",
        result.diagnostics,
        migrated.output
    );
    assert!(
        result.runtime_errors.is_empty(),
        "{:?}",
        result.runtime_errors
    );
    result
        .plots
        .iter()
        .map(|p| {
            p.values
                .iter()
                .map(|v| match v {
                    Value::Float(x) => Some(*x),
                    Value::Int(x) => Some(*x as f64),
                    Value::Na => None,
                    other => panic!("unexpected {other:?}"),
                })
                .collect()
        })
        .collect()
}

#[test]
fn expanded_compound_assignments_preserve_rhs_evaluation() {
    let source = "//@version=5\nindicator(\"Compound values\")\nx = 24.0\na = 2.0\nb = 3.0\nx += a * b\nplot(x)\nx -= a - b\nplot(x)\nx *= a + b\nplot(x)\nx /= a / b\nplot(x)\nx %= a + b\nplot(x)\n";
    assert_eq!(
        values(source, &[1.0]),
        vec![
            vec![Some(30.0)],
            vec![Some(31.0)],
            vec![Some(155.0)],
            vec![Some(232.5)],
            vec![Some(2.5)],
        ]
    );
}

#[test]
fn literal_history_offset_past_the_default_buffer_needs_no_declaration() {
    // A constant offset sizes its own buffer, so the script
    // migrates without a `max_bars_back` and reads the same bars: nothing for
    // the first 501, then the close from 501 bars earlier.
    let closes: Vec<f64> = (0..520).map(f64::from).collect();
    let source = "//@version=5\nindicator(\"history\")\nplot(close[501])\n";
    let migrated = migrate(source, &MigrationOptions::default());
    assert_eq!(migrated.status, Status::Migrated);
    assert!(!migrated.output.contains("max_bars_back"));
    let plotted = &values(source, &closes)[0];
    assert_eq!(plotted.len(), closes.len());
    for (bar, value) in plotted.iter().enumerate() {
        let expected = bar.checked_sub(501).map(|back| back as f64);
        assert_eq!(*value, expected, "bar {bar}");
    }
}

#[test]
fn migrated_negative_step_preserves_runtime_failure() {
    let source = "//@version=5\nindicator(\"negative step\")\ns = 0\nfor i = 0 to 6 by -2\n    s += 1\nplot(s)\n";
    let migrated = migrate(source, &MigrationOptions::default());
    assert_eq!(migrated.status, Status::Migrated);
    assert!(migrated.output.contains("by -2"));
    let market = MarketData {
        open: vec![1.0],
        high: vec![1.0],
        low: vec![1.0],
        close: vec![1.0],
        volume: vec![1.0],
        time: vec![0],
        ..Default::default()
    };
    let result = compile_and_run(
        &migrated.output,
        &market,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        !result
            .diagnostics
            .iter()
            .any(|d| d.severity == pinewasm_span::Severity::Error),
        "{:?}",
        result.diagnostics
    );
    assert_eq!(result.runtime_errors.len(), 1);
    assert_eq!(result.runtime_errors[0].kind, RuntimeErrorKind::UserError);
    assert_eq!(result.runtime_errors[0].bar_index, Some(0));
    assert_eq!(
        result.runtime_errors[0].message,
        "'step' in loop must be greater than zero."
    );
}

#[test]
fn signed_constant_division_matrix_truncates_toward_zero() {
    // Includes exact quotients, fractions, negative operands and zeros.
    // Runtime bars prevent a vacuous compile-only success.
    for numerator in [-17_i64, -7, -1, 0, 1, 7, 17] {
        for denominator in [-5_i64, -3, -1, 1, 3, 5] {
            let source = format!(
                "//@version=5\nindicator(\"division\")\nplot({numerator} / {denominator})\n"
            );
            let expected = (numerator / denominator) as f64;
            assert_eq!(
                values(&source, &[1.0, 2.0]),
                vec![vec![Some(expected); 2]],
                "{source}"
            );
        }
    }
}

#[test]
fn qualifier_and_nested_division_contracts() {
    let source = "//@version=5\nindicator(\"qualifiers\")\na = 7\na := 8\nx = input.int(7)\nvar counter = 0\ncounter += 1\nplot(a / 3)\nplot(x / 3)\nplot(counter / 2)\nplot(close / 3)\nplot((7 / 3) / 2)\nplot(7.0 / 3)\n";
    let actual = values(source, &[3.0, 6.0, -3.0, -6.0]);
    assert_eq!(
        actual,
        vec![
            vec![Some(2.0); 4],
            vec![Some(7.0 / 3.0); 4],
            vec![Some(0.5), Some(1.0), Some(1.5), Some(2.0)],
            vec![Some(1.0), Some(2.0), Some(-1.0), Some(-2.0)],
            vec![Some(1.0); 4],
            vec![Some(7.0 / 3.0); 4],
        ]
    );
}

#[test]
fn typed_function_int_parameters_keep_fractional_v5_division() {
    // The typed call keeps fractional division (1.5), while the untyped,
    // explicitly const, and literal expressions truncate to 1. An unqualified
    // typed parameter is at least simple despite const arguments.
    let source = "//@version=5\nindicator(\"division qualifiers\")\nfTyped(int a, int b) => (b - a) / 2\nfUntyped(a, b) => (b - a) / 2\nfConst(const int a, const int b) => (b - a) / 2\nplot(fTyped(1, 4))\nplot(fUntyped(1, 4))\nplot(fConst(1, 4))\nplot((4 - 1) / 2)\n";
    let migrated = migrate(source, &MigrationOptions::default());
    assert!(
        !migrated.output.contains("fTyped(int a, int b) => int("),
        "{}",
        migrated.output
    );
    assert!(
        migrated
            .output
            .contains("fUntyped(a, b) => int((b - a) / 2)"),
        "{}",
        migrated.output
    );
    assert!(
        migrated
            .output
            .contains("fConst(const int a, const int b) => int((b - a) / 2)"),
        "{}",
        migrated.output
    );
    assert_eq!(
        values(source, &[1.0, 2.0]),
        vec![
            vec![Some(1.5); 2],
            vec![Some(1.0); 2],
            vec![Some(1.0); 2],
            vec![Some(1.0); 2]
        ]
    );
}

#[test]
fn eager_boolean_history_updates_even_when_left_operand_decides() {
    let source = "//@version=5\nindicator(\"eager\")\na = close > 0 and ta.sma(close, 3) > 0\nb = close < 0 or ta.sma(close, 3) > 0\nplot(a ? 1 : 0)\nplot(b ? 1 : 0)\n";
    let actual = values(source, &[-6.0, -3.0, 12.0, -6.0, 3.0, 9.0]);
    assert_eq!(
        actual,
        vec![
            vec![
                Some(0.0),
                Some(0.0),
                Some(1.0),
                Some(0.0),
                Some(1.0),
                Some(1.0)
            ],
            vec![Some(1.0); 6],
        ]
    );
}

#[test]
fn literal_history_preserves_leading_missing_values() {
    let source = "//@version=5\nindicator(\"history\")\nplot(6[2])\n";
    assert_eq!(
        values(source, &[1.0; 5]),
        vec![vec![None, None, Some(6.0), Some(6.0), Some(6.0)]]
    );
}

#[test]
fn numeric_conditions_keep_zero_and_missing_false() {
    let source = "//@version=5\nindicator(\"conditions\")\nfloat x = close == 99 ? na : close\nplot(x ? 10 : 20)\nplot(x == na ? 1 : 0)\n";
    assert_eq!(
        values(source, &[0.0, -2.0, 3.0, 99.0]),
        vec![
            vec![Some(20.0), Some(10.0), Some(10.0), Some(20.0)],
            vec![Some(0.0); 4],
        ]
    );
}

#[test]
fn comparisons_with_na_remain_false_for_every_operator_and_operand_order() {
    // The expected plot value is zero for all five expressions.
    // The migration rewrites the equality and inequality forms.
    let source = "//@version=5\nindicator(\"NA comparison migration case\")\nplot(close == na ? 1 : 0, \"EQ\")\nplot(close != na ? 1 : 0, \"NE\")\nplot(na != close ? 1 : 0, \"reverse NE\")\nplot(close < na ? 1 : 0, \"LT\")\nplot(close > na ? 1 : 0, \"GT\")\n";
    let migrated = migrate(source, &MigrationOptions::default());
    assert_eq!(migrated.output.matches("plot(false ? 1 : 0").count(), 5);
    assert_eq!(
        migrated
            .findings
            .iter()
            .filter(|finding| finding.rule == "D3" && finding.applied)
            .count(),
        5
    );
    assert_eq!(
        values(source, &[1.0, 2.0, 3.0]),
        vec![vec![Some(0.0); 3]; 5]
    );
}

#[test]
fn comparison_with_na_still_evaluates_a_mutating_operand() {
    // The comparison is false, but array.pop() still empties the array.
    // The migrated output must plot 0, 0; without the pop, elements stay in
    // the array.
    let source = "//@version=5\nindicator(\"NA comparison side effect\")\nvar a = array.new_float(0)\narray.push(a, close)\nplot(array.pop(a) == na ? 1 : 0, \"comparison\")\nplot(array.size(a), \"remaining\")\n";
    let migrated = migrate(source, &MigrationOptions::default());
    assert!(
        migrated
            .output
            .contains("__v6_1 = array.pop(a)\nplot(false ? 1 : 0"),
        "{}",
        migrated.output
    );
    assert_eq!(
        values(source, &[1.0, 2.0, 3.0]),
        vec![vec![Some(0.0); 3]; 2]
    );
}

#[test]
fn boolean_missing_values_and_comparison_warmup_keep_v5_results() {
    // A missing bool is treated as false in conditions, and na(comparison)
    // observes a missing operand. v6 has no missing bools, so these results
    // depend on three distinct migration rewrites.
    let source = "//@version=5\nindicator(\"bool missing\")\nbool flag = na\nchoice = close > 0 ? true : na\nprevious = nz(choice[1])\nmissing_average = na(close > ta.sma(close, 3))\nplot(flag ? 1 : 0)\nplot(choice ? 1 : 0)\nplot(previous ? 1 : 0)\nplot(missing_average ? 1 : 0)\n";
    assert_eq!(
        values(source, &[-2.0, 3.0, 4.0, -1.0, 5.0]),
        vec![
            vec![Some(0.0); 5],
            vec![Some(0.0), Some(1.0), Some(1.0), Some(0.0), Some(1.0)],
            vec![Some(0.0), Some(0.0), Some(1.0), Some(1.0), Some(0.0)],
            vec![Some(1.0), Some(1.0), Some(0.0), Some(0.0), Some(0.0)],
        ]
    );
}

#[test]
fn mixed_boolean_numeric_equality_preserves_truthiness() {
    let source = "//@version=5\nindicator(\"bool comparisons\")\nb = close > open\nn = input.int(2)\nplot(b == 0 ? 1 : 0)\nplot(1 == b ? 1 : 0)\nplot(b != n ? 1 : 0)\nplot(b == close ? 1 : 0)\n";
    assert_eq!(
        values(source, &[0.0, 1.0, 2.0, -1.0]),
        vec![
            vec![Some(1.0), Some(0.0), Some(0.0), Some(1.0)],
            vec![Some(0.0), Some(1.0), Some(1.0), Some(0.0)],
            vec![Some(1.0), Some(0.0), Some(0.0), Some(1.0)],
            vec![Some(1.0), Some(1.0), Some(1.0), Some(0.0)],
        ]
    );
}

#[test]
fn loop_end_is_fixed_at_entry_when_the_body_changes_it() {
    // The end bound is evaluated once at loop entry, so the loop runs for
    // i = 0..3 (4 iterations) even though the body sets `last` to 0 on the
    // first one. A bound re-read before each iteration would stop after 1.
    let source = "//@version=5\nindicator(\"fixed bound\")\nlast = 3\ncount = 0\nfor i = 0 to last\n    count += 1\n    last := 0\nplot(count)\n";
    assert_eq!(values(source, &[1.0; 3]), vec![vec![Some(4.0); 3]]);

    // The loop runs i = 1..2 (2 iterations) from the entry value of `limit - 1`;
    // a bound re-read before each iteration would see the increased `limit`
    // and continue through i == 3.
    let growing = "//@version=5\nindicator(\"Changing bound runtime\")\nlimit = 3\ncount = 0\nfor i = 1 to limit - 1\n    count += 1\n    if i == 1\n        limit += 1\nplot(count)\n";
    assert_eq!(values(growing, &[1.0; 3]), vec![vec![Some(2.0); 3]]);

    // A collection bound is fixed at entry: `array.size(a) - 1` is 2, so all
    // three elements are popped. A bound re-read after each pop would stop
    // after two pops as the size shrinks.
    let shrinking = "//@version=5\nindicator(\"Shrinking array bound\")\na = array.from(1.0, 2.0, 3.0)\npopped = 0\nfor i = 0 to array.size(a) - 1\n    array.pop(a)\n    popped += 1\nplot(popped)\nplot(array.size(a))\n";
    assert_eq!(
        values(shrinking, &[1.0; 2]),
        vec![vec![Some(3.0); 2], vec![Some(0.0); 2]]
    );
}

#[test]
fn loop_end_the_body_cannot_change_stays_inline() {
    // The body reads but never changes the array, so the bound is left in
    // place without a temporary and the loop still sums all three elements.
    let source = "//@version=5\nindicator(\"Stable bound\")\na = array.from(1.0, 2.0, 3.0)\ns = 0.0\nfor i = 0 to array.size(a) - 1\n    s += array.get(a, i)\nplot(s)\n";
    let migrated = migrate(source, &MigrationOptions::default());
    assert!(
        migrated
            .output
            .contains("for i = 0 to array.size(a) - 1 by 1\n"),
        "{}",
        migrated.output
    );
    assert!(!migrated.output.contains("__v6_"), "{}", migrated.output);
    assert!(!migrated.findings.iter().any(|f| f.rule == "E4"));
    assert_eq!(values(source, &[1.0; 2]), vec![vec![Some(6.0); 2]]);
}

#[test]
fn full_indicator_keeps_warmup_history_and_signals() {
    // A complete first-party indicator composes qualifier, division, eager
    // evaluation, history, palette and transparency migration paths.
    let source = "//@version=5\nindicator(\"Range signal\", overlay = false)\nlength = input.int(3)\nthreshold = 7 / 3\naverage = ta.sma(close, length)\nsignal = close > threshold and ta.sma(close, length) > threshold\nplot(average, color = color.red, transp = 25)\nplot(signal ? 1 : 0, color = color.teal)\nplot(5[1])\n";
    assert_eq!(
        values(source, &[1.0, 2.0, 6.0, 1.0, 5.0, -3.0]),
        vec![
            vec![None, None, Some(3.0), Some(3.0), Some(4.0), Some(1.0)],
            vec![
                Some(0.0),
                Some(0.0),
                Some(1.0),
                Some(0.0),
                Some(1.0),
                Some(0.0)
            ],
            vec![None, Some(5.0), Some(5.0), Some(5.0), Some(5.0), Some(5.0)],
        ]
    );
}

#[test]
fn strict_search_alias_raises_instead_of_reading_the_last_element() {
    let source = "//@version=5\nindicator(\"strict search\")\na = array.from(1.0, 2.0)\ni = array.indexof(a, 9.0)\nplot(array.get(a, i))\n";
    let market = MarketData {
        open: vec![1.0],
        high: vec![1.0],
        low: vec![1.0],
        close: vec![1.0],
        volume: vec![1.0],
        time: vec![0],
        ..Default::default()
    };
    {
        let migrated = migrate(
            source,
            &MigrationOptions {
                strict_indices: true,
                ..Default::default()
            },
        );
        assert_eq!(migrated.status, Status::Migrated);
        let result = compile_and_run(
            &migrated.output,
            &market,
            CompileOptions::default(),
            &RunOptions::default(),
        );
        assert!(
            !result
                .diagnostics
                .iter()
                .any(|d| d.severity == pinewasm_span::Severity::Error),
            "{:?}",
            result.diagnostics
        );
        assert!(
            result
                .runtime_errors
                .iter()
                .any(|e| e.message.contains("negative array index")),
            "{:?}",
            result.runtime_errors
        );
    }
}

#[test]
fn strict_negative_literal_raises_instead_of_reading_the_last_element() {
    let source = "//@version=5\nindicator(\"negative literal\")\na = array.from(10.0, 20.0)\nplot(array.get(a, -1))\n";
    let migrated = migrate(
        source,
        &MigrationOptions {
            strict_indices: true,
            ..Default::default()
        },
    );
    assert_eq!(migrated.status, Status::Migrated);
    let market = MarketData {
        open: vec![1.0],
        high: vec![1.0],
        low: vec![1.0],
        close: vec![1.0],
        volume: vec![1.0],
        time: vec![0],
        ..Default::default()
    };
    let result = compile_and_run(
        &migrated.output,
        &market,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        !result
            .diagnostics
            .iter()
            .any(|d| d.severity == pinewasm_span::Severity::Error),
        "{:?}",
        result.diagnostics
    );
    assert!(
        result
            .runtime_errors
            .iter()
            .any(|e| e.message.contains("negative array index")),
        "{:?}",
        result.runtime_errors
    );
}

#[test]
fn strict_reordered_named_index_raises_on_execution() {
    let source = "//@version=5\nindicator(\"named index\")\na = array.from(10.0, 20.0)\nplot(array.get(index = -1, id = a))\n";
    let migrated = migrate(
        source,
        &MigrationOptions {
            strict_indices: true,
            ..Default::default()
        },
    );
    assert_eq!(migrated.status, Status::Migrated);
    let market = MarketData {
        open: vec![1.0],
        high: vec![1.0],
        low: vec![1.0],
        close: vec![1.0],
        volume: vec![1.0],
        time: vec![0],
        ..Default::default()
    };
    let result = compile_and_run(
        &migrated.output,
        &market,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        !result
            .diagnostics
            .iter()
            .any(|d| d.severity == pinewasm_span::Severity::Error),
        "{:?}",
        result.diagnostics
    );
    assert!(
        result
            .runtime_errors
            .iter()
            .any(|e| e.message.contains("negative array index")),
        "{:?}",
        result.runtime_errors
    );
}

#[test]
fn reordered_named_array_arguments_keep_parameter_evaluation_order() {
    let source = "//@version=5\nindicator(\"Named argument order\")\na = array.from(10.0, 20.0)\nevents = array.new_int()\nmakeIndex() =>\n    array.push(events, 1)\n    0\nmakeId() =>\n    array.push(events, 2)\n    a\nvalue = array.get(index = makeIndex(), id = makeId())\nplot(array.get(events, 0) * 10 + array.get(events, 1))\n";
    // The v5 source and its v6 migration must both plot 21: `id`
    // executes before `index`, even though `index` appears first in source.
    assert_eq!(values(source, &[1.0, 2.0]), vec![vec![Some(21.0); 2]]);
}

#[test]
fn reordered_named_array_index_operations_bind_by_name() {
    let source = "//@version=5\nindicator(\"Named array operations\")\na = array.from(10.0, 20.0)\narray.set(index = 0, id = a, value = 11.0)\narray.insert(index = 1, id = a, value = 15.0)\nremoved = array.remove(index = 2, id = a)\nplot(array.get(index = 0, id = a) + array.get(index = 1, id = a) + removed)\n";
    // The v5 source and its v6 migration must both plot 46.
    assert_eq!(values(source, &[1.0, 2.0]), vec![vec![Some(46.0); 2]]);
}
