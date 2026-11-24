//! Pinned fixture bytes and current Rust emission have separate identities.
//! Explicit default paint changes the current artifact and adds an instruction,
//! so the fixture's identity does not carry over to it. Finite VM comparisons
//! cover each pair, not a serializer theorem or general proof.

use pinewasm_compiler::{
    compile, run_program, CompileOptions, MarketData, RunOptions, RuntimeErrorKind, Value,
};
use pinewasm_csil::{validate, CsilInstr, CsilProgram};
use sha2::{Digest, Sha256};

const SOURCE: &str = "//@version=6\nindicator(\"budget admission\")\nx = close\nplot(x)\n";
const PINNED_LEN: usize = 1705;
const PINNED_SHA256: &str = "9e939a4a4e1da4f531ac5eb9172b5345bf32e89f0915624275683d31c791e693";
const PINNED_BUDGET_JSON: &str = r#"{"header":{"magic":"CSIL","il_version":2,"language_version":6,"runtime_abi_version":0},"constants":[],"series_slots":[{"name":"x","persistent":false,"varip":false}],"plots":[{"title":null,"style":{"kind":"plot","plotStyle":"line","lineStyle":"solid","lineWidth":1,"display":"all","histogramBase":0.0,"offset":0,"showLast":null,"forceOverlay":false,"trackPrice":false,"join":false,"editable":true,"format":null,"precision":null,"group":null,"ohlcField":null,"markerShape":null,"markerLocation":null,"markerSize":null,"character":null,"text":null,"minHeight":null,"maxHeight":null,"unsupported":[]}}],"visuals":[],"fills":[],"ta_states":[],"inputs":[],"object_types":[{"name":"chart.point","field_count":3},{"name":"line","field_count":10},{"name":"label","field_count":14},{"name":"box","field_count":19},{"name":"table","field_count":9},{"name":"polyline","field_count":9},{"name":"linefill","field_count":3},{"name":"footprint","field_count":8},{"name":"volume_row","field_count":8}],"functions":[{"name":"main","code":[{"ReadBuiltin":{"source":"Close","offset":0}},{"PlotF64":{"plot_slot":0}},"Return"],"source_map":[{"source":0,"start":47,"end":52},{"source":0,"start":53,"end":60},{"source":0,"start":53,"end":60}],"budget_reference":{"version":1,"code":[{"ReadBuiltin":{"source":"Close","offset":0}},{"StoreUserSlotCurrent":{"slot":0}},{"ReadUserSlot":{"slot":0,"offset":0}},{"PlotF64":{"plot_slot":0}},"Return"],"source_map":[{"source":0,"start":47,"end":52},{"source":0,"start":43,"end":52},{"source":0,"start":58,"end":59},{"source":0,"start":53,"end":60},{"source":0,"start":53,"end":60}],"max_stack":1},"max_stack":1}],"metadata":{"title":"budget admission","overlay":false,"max_bars_back":500}}"#;
const CURRENT_LEN: usize = 2445;
const CURRENT_SHA256: &str = "b71c429309a0ca428ddb70f3d1d51e96f53dbe9afbba70734175a4aefd7d7b6e";

fn market(close: &[f64]) -> MarketData {
    MarketData {
        open: close.to_vec(),
        high: close.to_vec(),
        low: close.to_vec(),
        close: close.to_vec(),
        volume: vec![1.0; close.len()],
        time: (1..=close.len() as i64).collect(),
        ..MarketData::default()
    }
}

fn emitted_pair() -> (CsilProgram, CsilProgram) {
    let compiled = compile(SOURCE, CompileOptions::default());
    assert!(compiled.success, "{:?}", compiled.diagnostics);
    let optimized = compiled.program.expect("source compiles to CSIL");
    let bytes = serde_json::to_vec(&optimized).expect("CSIL serializes");
    assert_eq!(
        bytes.len(),
        CURRENT_LEN,
        "Current Rust emission length drifted"
    );
    let digest = Sha256::digest(&bytes);
    let digest_hex = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert_eq!(
        digest_hex, CURRENT_SHA256,
        "Current Rust emission identity drifted; this is not the pinned fixture identity"
    );
    let decoded: CsilProgram =
        serde_json::from_slice(&bytes).expect("emitted Rust artifact decodes");
    assert_eq!(decoded, optimized);
    validate(&optimized).expect("optimized artifact validates");

    reconstruct_pair(optimized, true)
}

fn reconstruct_pair(optimized: CsilProgram, styled: bool) -> (CsilProgram, CsilProgram) {
    let paint = usize::from(styled);
    if styled {
        assert!(
            matches!(optimized.main().code[1], CsilInstr::PushColor { r: 33, g: 150, b: 243, t } if t == pinewasm_csil::F64Bits::new(0.0))
        );
        assert!(matches!(
            optimized.main().code[2],
            CsilInstr::PlotStyledF64 {
                plot_slot: 0,
                color_count: 1
            }
        ));
    }

    let mut reference_program = optimized.clone();
    let reference = reference_program.functions[0]
        .budget_reference
        .take()
        .expect("the compiler emitted a pre-deletion reference");
    assert_eq!(reference.version, 1);
    assert_eq!(reference.code.len(), 5 + paint);
    assert_eq!(optimized.main().code.len(), 3 + paint);
    assert_eq!(reference.max_stack, 1 + paint as u32);
    assert_eq!(optimized.main().max_stack, 1 + paint as u32);
    assert_eq!(optimized.main().code[0], reference.code[0]);
    for target_pc in 1..3 + paint {
        assert_eq!(
            optimized.main().code[target_pc],
            reference.code[target_pc + 2]
        );
    }
    for (target_pc, reference_pc) in
        std::iter::once((0, 0)).chain((1..3 + paint).map(|pc| (pc, pc + 2)))
    {
        assert_eq!(
            optimized.main().source_map[target_pc],
            reference.source_map[reference_pc],
            "surviving opcode span changed"
        );
    }
    reference_program.functions[0].code = reference.code;
    reference_program.functions[0].source_map = reference.source_map;
    reference_program.functions[0].max_stack = reference.max_stack;
    validate(&reference_program).expect("reconstructed reference validates");
    (reference_program, optimized)
}

#[test]
fn current_budget_reference_pair_matches_pinned_rust_artifact_and_successful_vm_outputs() {
    let (reference, optimized) = emitted_pair();
    for closes in [&[1.0][..], &[-2.5, 0.0, 3.25, 8.0][..]] {
        let feed = market(closes);
        let options = RunOptions {
            max_instructions_per_bar: 16,
            ..RunOptions::default()
        };
        let before = run_program(&reference, &feed, &options);
        let after = run_program(&optimized, &feed, &options);
        assert!(
            before.runtime_errors.is_empty(),
            "{:?}",
            before.runtime_errors
        );
        assert!(
            after.runtime_errors.is_empty(),
            "{:?}",
            after.runtime_errors
        );
        assert_eq!(before.bars_executed as usize, closes.len());
        assert_eq!(after.bars_executed, before.bars_executed);
        assert_eq!(before.plots.len(), 1, "the source fixture must plot");
        assert_eq!(
            before.plots[0].values,
            closes.iter().copied().map(Value::Float).collect::<Vec<_>>(),
            "the reference must plot the close feed"
        );
        assert_eq!(after.plots, before.plots);
        assert_eq!(after.visuals, before.visuals);
        assert_eq!(after.logs, before.logs);
        assert_eq!(after.alerts, before.alerts);
        assert_eq!(after.strategy, before.strategy);
    }
}

#[test]
fn real_budget_reference_pair_exposes_the_resource_boundary() {
    let (reference, optimized) = emitted_pair();
    let options = RunOptions {
        max_instructions_per_bar: 4,
        ..RunOptions::default()
    };
    let feed = market(&[1.0]);
    let before = run_program(&reference, &feed, &options);
    let after = run_program(&optimized, &feed, &options);
    assert_eq!(before.bars_executed, 0);
    assert_eq!(before.runtime_errors.len(), 1);
    assert_eq!(
        before.runtime_errors[0].kind,
        RuntimeErrorKind::InstructionLimitExceeded
    );
    assert_eq!(after.bars_executed, 1);
    assert!(after.runtime_errors.is_empty());
}

#[test]
fn forged_reference_opcode_is_rejected_by_rust_validator() {
    let (_, mut optimized) = emitted_pair();
    optimized.functions[0]
        .budget_reference
        .as_mut()
        .unwrap()
        .code[0] = CsilInstr::Return;
    assert!(validate(&optimized).is_err());
}

#[test]
fn pinned_artifact_keeps_its_exact_identity_and_vm_behavior() {
    let bytes = PINNED_BUDGET_JSON.as_bytes();
    assert_eq!(bytes.len(), PINNED_LEN);
    assert_eq!(
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
        PINNED_SHA256
    );
    let optimized: CsilProgram = serde_json::from_slice(bytes).unwrap();
    validate(&optimized).expect("pinned fixture artifact remains valid");
    let (reference, optimized) = reconstruct_pair(optimized, false);
    let closes = [1.0, -2.5, 0.0, 3.25];
    let feed = market(&closes);
    let options = RunOptions::default();
    let before = run_program(&reference, &feed, &options);
    let after = run_program(&optimized, &feed, &options);
    assert!(before.runtime_errors.is_empty());
    assert!(after.runtime_errors.is_empty());
    assert_eq!(before.bars_executed as usize, closes.len());
    assert_eq!(after.bars_executed, before.bars_executed);
    assert_eq!(
        before.plots[0].values,
        closes.into_iter().map(Value::Float).collect::<Vec<_>>()
    );
    assert_eq!(after.plots, before.plots);
}
