//! Linefill lifetime regressions: each pair has one fill, and deleting either
//! boundary line removes it. These assertions inspect script-visible live arrays
//! as well as serialized ids, rather than relying on renderer deduplication.
use super::{compile, data_from_close, drawing_ids, floats, run_outcome, tick};
use crate::{run, RealtimeSim, RunOptions};
use pinewasm_mir::Value;

#[test]
fn linefill_same_pair_replaces_with_a_fresh_id_and_stale_delete_is_harmless() {
    let source = r#"//@version=6
indicator("Replacement")
a = line.new(0, 1, 1, 2)
b = line.new(0, 3, 1, 4)
old = linefill.new(a, b, color.red)
latest = linefill.new(a, b, color.blue)
plot(array.size(linefill.all))
linefill.set_color(old, color.yellow)
linefill.delete(old)
plot(array.size(linefill.all))
plot(line.get_y1(linefill.get_line1(latest)))
plot(line.get_y1(linefill.get_line2(latest)))
"#;
    let outcome = run_outcome(source, &[1.0], &RunOptions::default());
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(drawing_ids(&outcome.drawings.linefills), [3]);
    assert_eq!(
        outcome.drawings.linefills[0].line_refs,
        Some([Some(0), Some(1)])
    );
    assert_eq!(
        outcome.drawings.linefills[0].fields["color"],
        Value::Color {
            r: 0x29,
            g: 0x62,
            b: 0xFF,
            t: 0.0,
            input_origin: None
        }
    );
    for (plot, expected) in outcome.plots.iter().zip([1.0, 1.0, 1.0, 3.0]) {
        assert_eq!(floats(&plot.values), [Some(expected)]);
    }
}

#[test]
fn linefill_reversed_pair_replaces_but_preserves_new_reference_order() {
    let source = r#"//@version=6
indicator("Reversed replacement")
a = line.new(0, 1, 1, 2)
b = line.new(0, 3, 1, 4)
c = line.new(0, 5, 1, 6)
old = linefill.new(a, b, color.red)
other = linefill.new(a, c, color.green)
latest = linefill.new(b, a, color.blue)
plot(array.size(linefill.all))
linefill.delete(old)
line.set_y1(b, 7)
plot(array.size(linefill.all))
plot(line.get_y1(linefill.get_line1(latest)))
"#;
    let outcome = run_outcome(source, &[1.0], &RunOptions::default());
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(drawing_ids(&outcome.drawings.linefills), [4, 5]);
    assert_eq!(
        outcome.drawings.linefills[0].line_refs,
        Some([Some(0), Some(2)])
    );
    assert_eq!(
        outcome.drawings.linefills[1].line_refs,
        Some([Some(1), Some(0)])
    );
    assert_eq!(floats(&outcome.plots[0].values), [Some(2.0)]);
    assert_eq!(floats(&outcome.plots[1].values), [Some(2.0)]);
    assert_eq!(floats(&outcome.plots[2].values), [Some(7.0)]);
}

#[test]
fn linefill_replacement_stays_unique_across_historical_bars() {
    let source = r#"//@version=6
indicator("Historical replacement")
var a = line.new(0, 1, 1, 2)
var b = line.new(0, 3, 1, 4)
linefill.new(a, b, color.blue)
plot(array.size(linefill.all))
"#;
    let outcome = run_outcome(source, &[1.0, 2.0, 3.0], &RunOptions::default());
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(drawing_ids(&outcome.drawings.linefills), [4]);
    assert_eq!(floats(&outcome.plots[0].values), [Some(1.0); 3]);
}

#[test]
fn linefill_replacement_precedes_live_cap_eviction() {
    // Fifty distinct pairs fill the current engine's live cap. Replacing the
    // newest pair must retain the oldest unrelated fill, not evict it first.
    let source = r#"//@version=6
indicator("Replacement at capacity", max_lines_count=500)
a = line.new(0, 1, 1, 2)
line last = na
for i = 0 to 49
    last := line.new(0, i + 3, 1, i + 4)
    linefill.new(a, last, color.red)
linefill.new(a, last, color.blue)
plot(array.size(linefill.all))
"#;
    let outcome = run_outcome(source, &[1.0], &RunOptions::default());
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    let mut expected = (2..100).step_by(2).collect::<Vec<_>>();
    expected.push(101);
    assert_eq!(drawing_ids(&outcome.drawings.linefills), expected);
    assert_eq!(floats(&outcome.plots[0].values), [Some(50.0)]);
    assert_eq!(
        outcome.drawings.linefills[49].line_refs,
        Some([Some(0), Some(99)])
    );
}

#[test]
fn linefill_null_references_do_not_form_one_accidental_pair() {
    // Unknown/na references carry no pair identity. Do not merge unrelated
    // incomplete objects under a shared null/null key.
    let source = r#"//@version=6
indicator("Missing boundaries")
a = line.new(0, 1, 1, 2)
b = line.new(0, 3, 1, 4)
linefill.new(na, na, color.red)
linefill.new(na, na, color.green)
linefill.new(a, b, color.red)
linefill.new(a, b, color.blue)
plot(array.size(linefill.all))
"#;
    let outcome = run_outcome(source, &[1.0], &RunOptions::default());
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(drawing_ids(&outcome.drawings.linefills), [2, 3, 5]);
    assert_eq!(outcome.drawings.linefills[0].line_refs, Some([None, None]));
    assert_eq!(outcome.drawings.linefills[1].line_refs, Some([None, None]));
    assert_eq!(floats(&outcome.plots[0].values), [Some(3.0)]);
}

#[test]
fn linefill_self_pair_replaces_without_double_removal() {
    let source = r#"//@version=6
indicator("Self pair")
a = line.new(0, 1, 1, 2)
linefill.new(a, a, color.red)
linefill.new(a, a, color.blue)
plot(array.size(linefill.all))
"#;
    let outcome = run_outcome(source, &[1.0], &RunOptions::default());
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(drawing_ids(&outcome.drawings.linefills), [2]);
    assert_eq!(
        outcome.drawings.linefills[0].line_refs,
        Some([Some(0), Some(0)])
    );
}

#[test]
fn linefill_line_deletion_removes_every_attached_fill_and_keeps_other_pairs() {
    for (deleted, expected) in [
        ("a", vec![6]),
        ("b", vec![5, 6]),
        ("c", vec![4]),
        ("d", vec![4, 5]),
    ] {
        let source = format!(
            r#"//@version=6
indicator("Boundary deletion")
a = line.new(0, 1, 1, 2)
b = line.new(0, 3, 1, 4)
c = line.new(0, 5, 1, 6)
d = line.new(0, 7, 1, 8)
linefill.new(a, b, color.red)
linefill.new(a, c, color.green)
linefill.new(c, d, color.blue)
line.delete({deleted})
line.delete({deleted})
plot(array.size(linefill.all))
"#
        );
        let outcome = run_outcome(&source, &[1.0], &RunOptions::default());
        assert!(outcome.errors.is_empty(), "{deleted}: {:?}", outcome.errors);
        assert_eq!(
            drawing_ids(&outcome.drawings.linefills),
            expected,
            "{deleted}"
        );
        assert_eq!(
            floats(&outcome.plots[0].values),
            [Some(expected.len() as f64)]
        );
    }
}

#[test]
fn linefill_line_eviction_ends_its_attached_fill_lifetime() {
    let source = r#"//@version=6
indicator("Boundary eviction", max_lines_count=3)
a = line.new(0, 1, 1, 2)
b = line.new(0, 3, 1, 4)
linefill.new(a, b, color.red)
line.new(0, 5, 1, 6)
line.new(0, 7, 1, 8)
plot(array.size(linefill.all))
"#;
    let outcome = run_outcome(source, &[1.0], &RunOptions::default());
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(drawing_ids(&outcome.drawings.lines), [1, 3, 4]);
    assert!(outcome.drawings.linefills.is_empty());
    assert_eq!(floats(&outcome.plots[0].values), [Some(0.0)]);
}

#[test]
fn linefill_replacement_and_boundary_deletion_restore_between_realtime_ticks() {
    let source = r#"//@version=6
indicator("Realtime lifetime")
var a = line.new(0, 1, 1, 2)
var b = line.new(0, 3, 1, 4)
linefill.new(a, b, close > 13 ? color.blue : color.red)
if barstate.isrealtime and close < 13
    line.delete(a)
plot(array.size(linefill.all))
"#;
    let options = RunOptions {
        realtime: Some(RealtimeSim {
            ticks: vec![
                tick(2, 12.0, false),
                tick(2, 13.0, false),
                tick(2, 14.0, true),
            ],
        }),
        ..RunOptions::default()
    };
    let outcome = run(&compile(source), &data_from_close(&[10.0, 11.0]), &options);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(drawing_ids(&outcome.drawings.lines), [0, 1]);
    assert_eq!(drawing_ids(&outcome.drawings.linefills), [4]);
    assert_eq!(
        outcome.drawings.linefills[0].line_refs,
        Some([Some(0), Some(1)])
    );
    assert_eq!(floats(&outcome.plots[0].values), [Some(1.0); 3]);
    assert_eq!(
        outcome.drawings.linefills[0].fields["color"],
        Value::Color {
            r: 0x29,
            g: 0x62,
            b: 0xFF,
            t: 0.0,
            input_origin: None
        }
    );
}
