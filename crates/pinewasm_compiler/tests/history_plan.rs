//! Opt-in export of a source-to-CSIL history plan.
//! The request carries resolved run inputs and a claimed capacity; a separate
//! checker recomputes both arithmetic claims from the ordinary bytes.

use pinewasm_compiler::{MarketData, RunOptions, Value};
use pinewasm_csil::{CsilInstr, InputValue, ValidationError};
use pinewasm_hir::TaInputTarget;

const SOURCE: &str = "//@version=6\n\
indicator(\"History proof\", max_bars_back=2)\n\
n = input.int(2)\n\
x = close\n\
plot(x[2])\n\
plot(ta.sma(x, n))\n\
plot(ta.change(x, n))\n";

fn history_entries(resolved: &[(TaInputTarget, i64)]) -> Vec<serde_json::Value> {
    resolved
        .iter()
        .filter_map(|(target, value)| {
            let (name, slot) = match target {
                TaInputTarget::Change { slot } => ("change", *slot),
                TaInputTarget::Sma { slot } => ("sma", *slot),
                TaInputTarget::State { .. } => return None,
            };
            Some(serde_json::json!({"target": name, "slot": slot, "value": value}))
        })
        .collect()
}

#[test]
fn resolved_history_plan_exports_actual_source_program() {
    let parsed = pinewasm_parse::parse(SOURCE);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let analysis = pinewasm_semantics::analyze(&parsed.program);
    assert!(
        analysis.diagnostics.is_empty(),
        "{:?}",
        analysis.diagnostics
    );
    let mir = pinewasm_mir::lower(&analysis.hir);
    let program = pinewasm_csil::lower_mir(&mir).expect("lower source to CSIL");
    pinewasm_csil::validate(&program).expect("compiler-produced CSIL validates");
    assert!(program
        .functions
        .iter()
        .flat_map(|f| &f.code)
        .any(|instruction| { matches!(instruction, CsilInstr::ReadUserSlot { offset: 2, .. }) }));
    assert!(program
        .functions
        .iter()
        .flat_map(|f| &f.code)
        .any(|instruction| { matches!(instruction, CsilInstr::WindowMean { .. }) }));

    let mut options = RunOptions::default();
    options.inputs.insert(0, InputValue::Int(6));
    let resolved: Vec<_> = program
        .ta_input_bindings
        .iter()
        .map(|binding| {
            let value = program
                .resolve_ta_input(binding, options.inputs.get(&binding.input))
                .expect("host value is within the declared TA interval");
            (binding.target, value)
        })
        .collect();
    let values: std::collections::HashMap<_, _> = resolved.iter().copied().collect();
    let depth = program.history_depth_with_ta_inputs(&values);
    assert!(
        depth > program.history_depth(),
        "override must expand lookback"
    );
    let entries = history_entries(&resolved);
    assert!(
        !entries.is_empty(),
        "fixture must have history input bindings"
    );

    let closes: Vec<f64> = (1..=9).map(f64::from).collect();
    let data = MarketData {
        open: closes.clone(),
        high: closes.clone(),
        low: closes.clone(),
        close: closes.clone(),
        volume: vec![1.0; closes.len()],
        ..MarketData::default()
    };
    let outcome = pinewasm_vm::run(&program, &data, &options);
    assert!(
        outcome.errors.is_empty(),
        "history execution: {:?}",
        outcome.errors
    );
    assert_eq!(outcome.bars_executed as usize, closes.len());
    assert_eq!(outcome.plots[0].values[0], Value::Na);
    assert_eq!(outcome.plots[0].values[1], Value::Na);
    assert_eq!(outcome.plots[0].values[2], Value::Float(1.0));
    assert_eq!(outcome.plots[1].values[5], Value::Float(3.5));
    assert_eq!(outcome.plots[2].values[6], Value::Float(6.0));

    let default_resolved: Vec<_> = program
        .ta_input_bindings
        .iter()
        .map(|binding| {
            (
                binding.target,
                program.resolve_ta_input(binding, None).unwrap(),
            )
        })
        .collect();
    let default_values: std::collections::HashMap<_, _> =
        default_resolved.iter().copied().collect();
    let default_depth = program.history_depth_with_ta_inputs(&default_values);
    assert_eq!(default_depth, 2);
    let default_outcome = pinewasm_vm::run(&program, &data, &RunOptions::default());
    assert!(
        default_outcome.errors.is_empty(),
        "default history execution: {:?}",
        default_outcome.errors
    );
    assert_eq!(default_outcome.plots[1].values[1], Value::Float(1.5));
    assert_eq!(default_outcome.plots[2].values[2], Value::Float(2.0));
}

// Arithmetic model for the finite u32 boundary corpus: capacity doubles until
// it covers depth + 1, so the last successful depth is 2^31 - 1. This test
// checks Rust's checked primitives on the transitions.
fn doubled_history_capacity(depth: u32) -> Option<u32> {
    let requested = u64::from(depth) + 1;
    let mut capacity = 1u64;
    while capacity < requested {
        capacity *= 2;
    }
    u32::try_from(capacity).ok()
}

#[test]
fn history_ring_checked_u32_matches_doubling_at_edges() {
    let check = |depth: u32| {
        let actual = depth
            .checked_add(1)
            .and_then(u32::checked_next_power_of_two);
        assert_eq!(actual, doubled_history_capacity(depth), "depth={depth}");
        assert_eq!(actual.is_some(), depth < (1u32 << 31), "depth={depth}");
    };

    // A requested cell count of 2^k changes the result on either side of
    // depth = 2^k - 1. Include the add-overflow end of u32 as well.
    for exponent in 0..=32 {
        let transition = (1i64 << exponent) - 1;
        for delta in -2..=2 {
            let candidate = transition + delta;
            if (0..=i64::from(u32::MAX)).contains(&candidate) {
                check(candidate as u32);
            }
        }
    }
    for depth in [0, 1, (1u32 << 31) - 1, 1u32 << 31, u32::MAX] {
        check(depth);
    }

    let mut state = 0x5eb1_9da9u32;
    for _ in 0..10_000 {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        check(state);
    }
}

#[test]
fn history_binding_validator_rejects_mismatched_defaults_and_counts() {
    let parsed = pinewasm_parse::parse(SOURCE);
    assert!(parsed.diagnostics.is_empty());
    let analysis = pinewasm_semantics::analyze(&parsed.program);
    assert!(analysis.diagnostics.is_empty());
    let mir = pinewasm_mir::lower(&analysis.hir);
    let program = pinewasm_csil::lower_mir(&mir).expect("lower source to CSIL");
    pinewasm_csil::validate(&program).expect("source artifact validates");

    let sma_slot = program
        .ta_input_bindings
        .iter()
        .find_map(|binding| match binding.target {
            TaInputTarget::Sma { slot } => Some(slot),
            _ => None,
        })
        .expect("SMA binding");
    let change_slot = program
        .ta_input_bindings
        .iter()
        .find_map(|binding| match binding.target {
            TaInputTarget::Change { slot } => Some(slot),
            _ => None,
        })
        .expect("Change binding");
    let assert_rejected = |name: &str, candidate: &pinewasm_csil::CsilProgram| {
        let errors = pinewasm_csil::validate(candidate).expect_err(name);
        assert!(
            errors.iter().any(|error| matches!(error,
                ValidationError::Header(message) if message.contains("invalid TA input binding"))),
            "{name}: {errors:?}"
        );
    };

    let mut wrong_sma = program.clone();
    for instruction in &mut wrong_sma.functions[0].code {
        if let CsilInstr::WindowMean { slot, length } = instruction {
            if *slot == sma_slot {
                *length = 3;
                break;
            }
        }
    }
    assert_rejected("mismatched SMA default", &wrong_sma);

    let mut wrong_change = program.clone();
    for instruction in &mut wrong_change.functions[0].code {
        if let CsilInstr::ReadUserSlot { slot, offset } = instruction {
            if *slot == change_slot && *offset != 0 {
                *offset = 3;
                break;
            }
        }
    }
    assert_rejected("mismatched Change default", &wrong_change);

    let mut repeated_sma = program.clone();
    repeated_sma.functions[0].code.push(CsilInstr::WindowMean {
        slot: sma_slot,
        length: 2,
    });
    assert_rejected("repeated SMA observer", &repeated_sma);

    let mut invalid_default = program.clone();
    invalid_default.inputs[0].defval = InputValue::Int(0);
    assert_rejected("invalid input default", &invalid_default);

    let mut wrong_abi = program.clone();
    wrong_abi.header.runtime_abi_version = 0;
    assert_rejected("history binding under old runtime ABI", &wrong_abi);

    let mut short_slots = program;
    short_slots.series_slots.pop();
    assert_rejected("binding past series-slot table", &short_slots);
}
