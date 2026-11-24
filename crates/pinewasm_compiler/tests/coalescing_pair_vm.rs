//! Differential replay of a scalar MIR fixture and the current styled lowering pair.
//! The scalar profile excludes paint opcodes. Its fixture removes presentation
//! operands before lowering, so it is not the full current compiler emission.
//! Styled emission has separate fail-closed and VM checks below. Neither path
//! is a proof of Rust semantics.

use pinewasm_compiler::{MarketData, RunOptions, RuntimeErrorKind, Value};
use pinewasm_csil::{
    lower_mir_with_pass_record, ProofBoundScalarView, ProofScalarView, ProofScalarViewError,
};

const SOURCE: &str = "//@version=6\nindicator(\"Coalescing pair\")\nx = close\nplot(x)\n";

fn programs(styled: bool) -> (pinewasm_csil::CsilProgram, pinewasm_csil::CsilProgram) {
    let parsed = pinewasm_parse::parse(SOURCE);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let analysis = pinewasm_semantics::analyze(&parsed.program);
    assert!(
        analysis.diagnostics.is_empty(),
        "{:?}",
        analysis.diagnostics
    );
    let mut mir = pinewasm_mir::lower(&analysis.hir);
    if !styled {
        // A scalar-only MIR test fixture. It does not cover the current styled
        // artifact, which has its own checks below.
        let mut removed_colors = Vec::new();
        for block in &mut mir.blocks {
            for instruction in &mut block.instrs {
                if let pinewasm_mir::MirInstr::Plot { colors, .. } = instruction {
                    removed_colors.extend(colors.iter().map(|(_, id)| *id));
                    colors.clear();
                }
            }
        }
        for block in &mut mir.blocks {
            block.instrs.retain(|instruction| {
                !matches!(instruction, pinewasm_mir::MirInstr::Const { dst, .. } if removed_colors.contains(dst))
            });
        }
        for plot in &mut mir.metadata.plots {
            plot.color_roles.clear();
            plot.style.color_palette.clear();
        }
    }
    let captured = lower_mir_with_pass_record(&mir).expect("lowering pair should lower");
    let (source, target) = captured.coalescing_programs();
    assert_eq!(source.main().code.len(), if styled { 6 } else { 5 });
    assert_eq!(target.main().code.len(), if styled { 4 } else { 3 });
    pinewasm_csil::validate(&source).expect("pre-pass CSIL should validate");
    pinewasm_csil::validate(&target).expect("post-pass CSIL should validate");
    (source, target)
}

fn captured_programs() -> (pinewasm_csil::CsilProgram, pinewasm_csil::CsilProgram) {
    programs(false)
}

#[test]
fn current_styled_capture_fails_closed_in_scalar_views_and_preserves_vm_outputs() {
    let (source, target) = programs(true);
    for program in [&source, &target] {
        assert!(matches!(
            ProofScalarView::from_program(program),
            Err(ProofScalarViewError::UnsupportedOpcode(_))
        ));
        assert!(matches!(
            ProofBoundScalarView::from_program(program),
            Err(ProofScalarViewError::UnsupportedOpcode(_))
        ));
    }
    let prices = [-0.0, 0.0, -2.5, 1_000_000_000.25];
    let data = market(&prices);
    let options = RunOptions {
        profile: true,
        ..RunOptions::default()
    };
    let before = pinewasm_vm::run(&source, &data, &options);
    let after = pinewasm_vm::run(&target, &data, &options);
    assert!(before.errors.is_empty());
    assert!(after.errors.is_empty());
    assert_eq!(before.bars_executed as usize, prices.len());
    assert_eq!(after.bars_executed, before.bars_executed);
    assert_eq!(before.plots, after.plots);
    for (actual, expected) in before.plots[0].values.iter().zip(prices) {
        let Value::Float(actual) = actual else {
            panic!("expected float plot")
        };
        assert_eq!(actual.to_bits(), expected.to_bits());
    }
    assert_eq!(
        before.profile.unwrap().count.iter().sum::<f64>(),
        6.0 * prices.len() as f64
    );
    assert_eq!(
        after.profile.unwrap().count.iter().sum::<f64>(),
        4.0 * prices.len() as f64
    );
    let limits = RunOptions {
        max_instructions_per_bar: 4,
        ..RunOptions::default()
    };
    let before = pinewasm_vm::run(&source, &data, &limits);
    let after = pinewasm_vm::run(&target, &data, &limits);
    assert_eq!(before.bars_executed, 0);
    assert_eq!(
        before.errors[0].kind,
        RuntimeErrorKind::InstructionLimitExceeded
    );
    assert_eq!(after.bars_executed as usize, prices.len());
    assert!(after.errors.is_empty());
}

fn market(close: &[f64]) -> MarketData {
    MarketData {
        open: close.to_vec(),
        high: close.to_vec(),
        low: close.to_vec(),
        close: close.to_vec(),
        volume: vec![1.0; close.len()],
        ..MarketData::default()
    }
}

#[test]
fn typed_scalar_view_covers_full_unchanged_frame_and_fails_closed() {
    let (source, target) = captured_programs();
    let before = ProofScalarView::from_program(&source).expect("admitted source");
    let after = ProofScalarView::from_program(&target).expect("admitted target");
    assert_eq!(before.frame_hex, after.frame_hex);
    assert_eq!(before.function.code.len(), 5);
    assert_eq!(after.function.code.len(), 3);
    assert_eq!(before.function.source_map.len(), 5);
    assert_eq!(after.function.source_map.len(), 3);

    let mut changed_frame = target.clone();
    changed_frame.metadata.max_bars_back += 1;
    assert_ne!(
        before.frame_hex,
        ProofScalarView::from_program(&changed_frame)
            .unwrap()
            .frame_hex
    );
    let mut negative_zero_style = target.clone();
    negative_zero_style.plots[0].style.histogram_base = -0.0;
    assert_ne!(
        before.frame_hex,
        ProofScalarView::from_program(&negative_zero_style)
            .unwrap()
            .frame_hex,
        "full-frame bytes must preserve the sign of a style zero"
    );
    let mut unsupported = target.clone();
    unsupported.functions[0].code[0] = pinewasm_csil::CsilInstr::PushNa;
    assert!(matches!(
        ProofScalarView::from_program(&unsupported),
        Err(ProofScalarViewError::UnsupportedOpcode(0))
    ));
}

#[test]
fn typed_bound_view_reconstructs_exact_ordinary_artifact_bytes() {
    let (source, target) = captured_programs();
    let before = ProofBoundScalarView::from_program(&source).expect("admitted source");
    let after = ProofBoundScalarView::from_program(&target).expect("admitted target");
    let hex = |bytes: &[u8]| -> String { bytes.iter().map(|byte| format!("{byte:02x}")).collect() };
    for (bound, program) in [(&before, &source), (&after, &target)] {
        assert_eq!(
            format!(
                "{}{}{}",
                bound.prefix_hex, bound.function_hex, bound.suffix_hex
            ),
            hex(program.to_json().as_bytes())
        );
        assert_eq!(
            format!("{}{}{}", bound.prefix_hex, hex(b"[]"), bound.suffix_hex),
            bound.scalar_view.frame_hex
        );
    }
    assert_eq!(before.prefix_hex, after.prefix_hex);
    assert_eq!(before.suffix_hex, after.suffix_hex);
    assert_ne!(before.function_hex, after.function_hex);
}

#[test]
fn captured_pair_preserves_exact_plot_bits_and_reduces_vm_dispatches() {
    let (source, target) = captured_programs();
    let prices = [-0.0, 0.0, 0.000_000_001, 1_000_000_000.25];
    let options = RunOptions {
        profile: true,
        ..RunOptions::default()
    };
    let pre = pinewasm_vm::run(&source, &market(&prices), &options);
    let post = pinewasm_vm::run(&target, &market(&prices), &options);
    assert!(pre.errors.is_empty(), "pre-pass errors: {:?}", pre.errors);
    assert!(
        post.errors.is_empty(),
        "post-pass errors: {:?}",
        post.errors
    );
    assert_eq!(pre.bars_executed, prices.len() as u32);
    assert_eq!(post.bars_executed, prices.len() as u32);
    assert_eq!(pre.plots.len(), 1);
    assert_eq!(post.plots.len(), 1);
    for ((pre_value, post_value), expected) in pre.plots[0]
        .values
        .iter()
        .zip(&post.plots[0].values)
        .zip(prices)
    {
        match (pre_value, post_value) {
            (Value::Float(a), Value::Float(b)) => {
                assert_eq!(a.to_bits(), expected.to_bits());
                assert_eq!(b.to_bits(), expected.to_bits());
            }
            other => panic!("expected two float plots, got {other:?}"),
        }
    }
    let pre_dispatches: f64 = pre.profile.expect("source profile").count.iter().sum();
    let post_dispatches: f64 = post.profile.expect("target profile").count.iter().sum();
    assert_eq!(pre_dispatches, 5.0 * prices.len() as f64);
    assert_eq!(post_dispatches, 3.0 * prices.len() as f64);
}

#[test]
fn captured_pair_replays_finite_binary64_edge_corpus() {
    let (source, target) = captured_programs();
    let mut prices = vec![
        -0.0,
        0.0,
        f64::MIN_POSITIVE,
        -f64::MIN_POSITIVE,
        f64::from_bits(1),
        f64::from_bits((1_u64 << 63) | 1),
        f64::MAX,
        -f64::MAX,
    ];
    let mut bits = 0xD1B5_4A32_D192_ED03_u64;
    for _ in 0..128 {
        bits ^= bits << 13;
        bits ^= bits >> 7;
        bits ^= bits << 17;
        let finite_bits = if bits & 0x7ff0_0000_0000_0000 == 0x7ff0_0000_0000_0000 {
            bits ^ 0x0010_0000_0000_0000
        } else {
            bits
        };
        prices.push(f64::from_bits(finite_bits));
    }
    let data = market(&prices);
    let pre = pinewasm_vm::run(&source, &data, &RunOptions::default());
    let post = pinewasm_vm::run(&target, &data, &RunOptions::default());
    assert!(pre.errors.is_empty(), "pre-pass errors: {:?}", pre.errors);
    assert!(
        post.errors.is_empty(),
        "post-pass errors: {:?}",
        post.errors
    );
    assert_eq!(pre.bars_executed, prices.len() as u32);
    assert_eq!(post.bars_executed, prices.len() as u32);
    for ((pre_value, post_value), expected) in pre.plots[0]
        .values
        .iter()
        .zip(&post.plots[0].values)
        .zip(&prices)
    {
        match (pre_value, post_value) {
            (Value::Float(a), Value::Float(b)) => {
                assert_eq!(a.to_bits(), expected.to_bits());
                assert_eq!(b.to_bits(), expected.to_bits());
            }
            other => panic!("expected two float plots, got {other:?}"),
        }
    }
}

#[test]
fn branch_to_removed_store_replays_at_retained_continuation() {
    let (mut source, mut target) = captured_programs();
    let branch = pinewasm_csil::CsilInstr::Jump { target: 3 };
    let skipped = pinewasm_csil::CsilInstr::PushF64(pinewasm_csil::F64Bits::new(999.0));
    let span = pinewasm_span::Span::dummy();
    source.functions[0].code.insert(1, branch.clone());
    source.functions[0].source_map.insert(1, span);
    target.functions[0].code.insert(1, branch);
    target.functions[0].source_map.insert(1, span);
    source.functions[0].code.insert(2, skipped.clone());
    source.functions[0].source_map.insert(2, span);
    target.functions[0].code.insert(2, skipped);
    target.functions[0].source_map.insert(2, span);
    pinewasm_csil::validate(&source).expect("source branch profile validates");
    pinewasm_csil::validate(&target).expect("relocated target branch validates");

    let prices = [-0.0, f64::from_bits(1), f64::MAX];
    let data = market(&prices);
    let options = RunOptions {
        profile: true,
        ..RunOptions::default()
    };
    let pre = pinewasm_vm::run(&source, &data, &options);
    let post = pinewasm_vm::run(&target, &data, &options);
    assert!(pre.errors.is_empty(), "pre-pass errors: {:?}", pre.errors);
    assert!(
        post.errors.is_empty(),
        "post-pass errors: {:?}",
        post.errors
    );
    for ((pre_value, post_value), expected) in pre.plots[0]
        .values
        .iter()
        .zip(&post.plots[0].values)
        .zip(prices)
    {
        match (pre_value, post_value) {
            (Value::Float(a), Value::Float(b)) => {
                assert_eq!(a.to_bits(), expected.to_bits());
                assert_eq!(b.to_bits(), expected.to_bits());
            }
            other => panic!("expected two float plots, got {other:?}"),
        }
    }
    let pre_dispatches: f64 = pre.profile.expect("source profile").count.iter().sum();
    let post_dispatches: f64 = post.profile.expect("target profile").count.iter().sum();
    assert_eq!(pre_dispatches, 6.0 * prices.len() as f64);
    assert_eq!(post_dispatches, 4.0 * prices.len() as f64);
}

#[test]
fn branch_capture_budget_prefix_does_not_commit_and_reports_the_target_span() {
    let (mut source, mut target) = captured_programs();
    let branch = pinewasm_csil::CsilInstr::Jump { target: 3 };
    let skipped = pinewasm_csil::CsilInstr::PushF64(pinewasm_csil::F64Bits::new(999.0));
    let span = pinewasm_span::Span::dummy();
    for program in [&mut source, &mut target] {
        program.functions[0].code.insert(1, branch.clone());
        program.functions[0].source_map.insert(1, span);
        program.functions[0].code.insert(2, skipped.clone());
        program.functions[0].source_map.insert(2, span);
        pinewasm_csil::validate(program).expect("branch profile validates");
    }
    let options = RunOptions {
        max_instructions_per_bar: 3,
        ..RunOptions::default()
    };
    let data = market(&[1.0]);
    let pre = pinewasm_vm::run(&source, &data, &options);
    let post = pinewasm_vm::run(&target, &data, &options);

    // The source reaches its store and the target reaches its plot after three
    // steps. The VM does not commit either bar's plot until Return succeeds, so
    // both outcomes expose no completed bar or plot at this budget.
    assert_eq!(pre.bars_executed, 0);
    assert_eq!(post.bars_executed, 0);
    assert!(pre.plots[0].values.is_empty());
    assert!(post.plots[0].values.is_empty());
    for result in [&pre, &post] {
        assert_eq!(result.errors.len(), 1);
        assert_eq!(
            result.errors[0].kind,
            RuntimeErrorKind::InstructionLimitExceeded
        );
        assert_eq!(result.errors[0].instruction_offset, Some(4));
    }

    // The rewritten PC 4 maps to the retained Return/plot span, while source
    // PC 4 maps to the removed slot read. Exact low-budget error attribution
    // therefore is not preserved by this instruction-count optimization.
    assert_eq!(pre.errors[0].span, Some(source.main().source_map[4]));
    assert_eq!(post.errors[0].span, Some(target.main().source_map[4]));
    assert_ne!(pre.errors[0].span, post.errors[0].span);
}

#[test]
fn shorter_capture_succeeds_under_tighter_instruction_budget() {
    let (source, target) = captured_programs();
    let data = market(&[1.0, 2.0, 3.0]);
    let options = RunOptions {
        max_instructions_per_bar: 3,
        ..RunOptions::default()
    };
    let pre = pinewasm_vm::run(&source, &data, &options);
    let post = pinewasm_vm::run(&target, &data, &options);
    assert!(
        !pre.errors.is_empty(),
        "five-step source should exceed budget"
    );
    assert_eq!(pre.bars_executed, 0);
    assert!(
        post.errors.is_empty(),
        "three-step target errors: {:?}",
        post.errors
    );
    assert_eq!(post.bars_executed, 3);
    assert_eq!(
        post.plots[0].values,
        vec![Value::Float(1.0), Value::Float(2.0), Value::Float(3.0)]
    );
}

#[test]
fn captured_pair_per_bar_budget_boundary_matches_dispatch_counts() {
    let (source, target) = captured_programs();
    let data = market(&[-0.0]);
    for budget in 0_u64..=6 {
        let options = RunOptions {
            max_instructions_per_bar: budget,
            ..RunOptions::default()
        };
        for (name, program, required) in [("source", &source, 5_u64), ("target", &target, 3_u64)] {
            let result = pinewasm_vm::run(program, &data, &options);
            if budget < required {
                assert_eq!(result.bars_executed, 0, "{name} at budget {budget}");
                assert_eq!(result.errors.len(), 1, "{name} at budget {budget}");
                assert_eq!(
                    result.errors[0].kind,
                    RuntimeErrorKind::InstructionLimitExceeded,
                    "{name} at budget {budget}"
                );
                assert_eq!(
                    result.errors[0].instruction_offset,
                    Some(budget as u32),
                    "charging should reject before the next dispatch for {name}"
                );
            } else {
                assert!(
                    result.errors.is_empty(),
                    "{name} at budget {budget}: {:?}",
                    result.errors
                );
                assert_eq!(result.bars_executed, 1, "{name} at budget {budget}");
                match result.plots[0].values.as_slice() {
                    [Value::Float(value)] => assert_eq!(value.to_bits(), (-0.0_f64).to_bits()),
                    values => panic!("unexpected {name} plot at budget {budget}: {values:?}"),
                }
            }
        }
    }
}

#[test]
fn captured_pair_dual_budget_matrix_matches_model() {
    let (source, target) = captured_programs();
    let data = market(&[-0.0]);
    for per_bar_budget in 0_u64..=6 {
        for run_budget in 0_u64..=6 {
            let options = RunOptions {
                max_instructions_per_bar: per_bar_budget,
                max_instructions_per_run: run_budget,
                ..RunOptions::default()
            };
            for (name, program, required) in
                [("source", &source, 5_u64), ("target", &target, 3_u64)]
            {
                let result = pinewasm_vm::run(program, &data, &options);
                let context = format!("{name}, per-bar {per_bar_budget}, run {run_budget}");
                if per_bar_budget >= required && run_budget >= required {
                    assert!(result.errors.is_empty(), "{context}: {:?}", result.errors);
                    assert_eq!(result.bars_executed, 1, "{context}");
                    match result.plots[0].values.as_slice() {
                        [Value::Float(value)] => {
                            assert_eq!(value.to_bits(), (-0.0_f64).to_bits(), "{context}");
                        }
                        values => panic!("unexpected plot for {context}: {values:?}"),
                    }
                } else {
                    assert_eq!(result.bars_executed, 0, "{context}");
                    assert_eq!(result.errors.len(), 1, "{context}");
                    let per_bar_first = per_bar_budget <= run_budget;
                    assert_eq!(
                        result.errors[0].kind,
                        if per_bar_first {
                            RuntimeErrorKind::InstructionLimitExceeded
                        } else {
                            RuntimeErrorKind::RunInstructionLimitExceeded
                        },
                        "{context}"
                    );
                    assert_eq!(
                        result.errors[0].instruction_offset,
                        Some(per_bar_budget.min(run_budget) as u32),
                        "the next dispatch must fail before its effect for {context}"
                    );
                }
            }
        }
    }
}

#[test]
fn captured_pair_cumulative_budget_boundary_matches_dispatch_counts() {
    let (source, target) = captured_programs();
    let data = market(&[1.0, 2.0, 3.0]);
    for budget in 0_u64..=15 {
        let options = RunOptions {
            max_instructions_per_bar: 100,
            max_instructions_per_run: budget,
            ..RunOptions::default()
        };
        for (name, program, required) in [("source", &source, 5_u64), ("target", &target, 3_u64)] {
            let result = pinewasm_vm::run(program, &data, &options);
            let completed = (budget / required).min(3) as u32;
            assert_eq!(result.bars_executed, completed, "{name} at budget {budget}");
            if completed < 3 {
                assert_eq!(result.errors.len(), 1, "{name} at budget {budget}");
                assert_eq!(
                    result.errors[0].kind,
                    RuntimeErrorKind::RunInstructionLimitExceeded,
                    "{name} at budget {budget}"
                );
                assert_eq!(result.errors[0].bar_index, Some(completed));
                assert_eq!(
                    result.errors[0].instruction_offset,
                    Some((budget % required) as u32),
                    "cumulative budget should reject before the next dispatch"
                );
            } else {
                assert!(
                    result.errors.is_empty(),
                    "{name} at budget {budget}: {:?}",
                    result.errors
                );
            }
        }
    }
}
