#![allow(dead_code)]
//! Read-only scalar admission projection. Unsupported rows retain their position.
//! This packet is deliberately not a serialization or full payload identity.
use crate::{CsilInstr, CsilProgram};
use pinewasm_hir::BuiltinSeries;

#[derive(Debug, PartialEq, Eq, serde::Serialize)]
pub(crate) enum ScalarRow {
    ReadClose(u32),
    ReadSlot(u32, u32),
    StoreSlot(u32),
    Plot(u32),
    Return,
    Unsupported,
}
#[derive(Debug, PartialEq, Eq, serde::Serialize)]
pub(crate) struct FunctionFields {
    pub name: Vec<u8>,
    pub rows: Vec<ScalarRow>,
    pub spans: Vec<(u32, u32, u32)>,
    pub max_stack: u32,
    pub budget_present: bool,
}
#[derive(Debug, PartialEq, Eq, serde::Serialize)]
pub(crate) struct ProgramFields {
    pub magic: Vec<u8>,
    pub versions: (u32, u32, u32),
    pub slots: Vec<(Vec<u8>, bool, bool)>,
    pub plot_count: usize,
    pub functions: Vec<FunctionFields>,
    pub max_bars_back: u32,
    // Not projected: constants, visuals, fills, TA states, TA bindings, inputs, object types.
    // Object types need not be empty for the admitted profile.
    pub table_counts: (usize, usize, usize, usize, usize, usize, usize),
    pub strategy_present: bool,
}

pub(crate) fn scalar_row(instr: &CsilInstr) -> ScalarRow {
    match instr {
        CsilInstr::ReadBuiltin {
            source: BuiltinSeries::Close,
            offset,
        } => ScalarRow::ReadClose(*offset),
        CsilInstr::ReadUserSlot { slot, offset } => ScalarRow::ReadSlot(*slot, *offset),
        CsilInstr::StoreUserSlotCurrent { slot } => ScalarRow::StoreSlot(*slot),
        CsilInstr::PlotF64 { plot_slot } => ScalarRow::Plot(*plot_slot),
        CsilInstr::Return => ScalarRow::Return,
        _ => ScalarRow::Unsupported,
    }
}

fn observe_rows(code: &[CsilInstr]) -> Vec<ScalarRow> {
    let mut rows = Vec::new();
    let mut index = 0;
    while index < code.len() {
        rows.push(scalar_row(&code[index]));
        index += 1;
    }
    rows
}
fn observe_spans(source_map: &[pinewasm_span::Span]) -> Vec<(u32, u32, u32)> {
    let mut spans = Vec::new();
    let mut index = 0;
    while index < source_map.len() {
        let span = &source_map[index];
        spans.push((span.source.0, span.start, span.end));
        index += 1;
    }
    spans
}
fn observe_function(function: &crate::CsilFunction) -> FunctionFields {
    FunctionFields {
        name: function.name.as_bytes().to_vec(),
        rows: observe_rows(&function.code),
        spans: observe_spans(&function.source_map),
        max_stack: function.max_stack,
        budget_present: function.budget_reference.is_some(),
    }
}

pub(crate) fn observe_program(program: &CsilProgram) -> ProgramFields {
    let mut slots = Vec::new();
    let mut slot_index = 0;
    while slot_index < program.series_slots.len() {
        let slot = &program.series_slots[slot_index];
        slots.push((slot.name.as_bytes().to_vec(), slot.persistent, slot.varip));
        slot_index += 1;
    }
    let mut functions = Vec::new();
    let mut function_index = 0;
    while function_index < program.functions.len() {
        let function = &program.functions[function_index];
        functions.push(observe_function(function));
        function_index += 1;
    }
    ProgramFields {
        magic: program.header.magic.as_bytes().to_vec(),
        versions: (
            program.header.csil_version,
            program.header.language_version,
            program.header.runtime_abi_version,
        ),
        slots,
        plot_count: program.plots.len(),
        functions,
        max_bars_back: program.metadata.max_bars_back,
        table_counts: (
            program.constants.len(),
            program.visuals.len(),
            program.fills.len(),
            program.ta_states.len(),
            program.ta_input_bindings.len(),
            program.inputs.len(),
            program.object_types.len(),
        ),
        strategy_present: program.metadata.strategy.is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn compiled() -> CsilProgram {
        let parsed =
            pinewasm_parse::parse("//@version=6\nindicator(\"Observer\")\nx = close\nplot(x)\n");
        let analyzed = pinewasm_semantics::analyze(&parsed.program);
        crate::lower_mir(&pinewasm_mir::lower(&analyzed.hir)).expect("real lowering")
    }
    #[test]
    fn actual_program_projection_and_roundtrip() {
        let mut p = compiled();
        assert!(matches!(
            crate::ProofBoundScalarView::from_program(&p),
            Err(crate::ProofScalarViewError::UnsupportedOpcode(_))
        ));
        assert!(matches!(p.main().code[0], CsilInstr::ReadBuiltin { .. }));
        assert!(matches!(p.main().code[1], CsilInstr::PushColor { .. }));
        assert!(matches!(p.main().code[2], CsilInstr::PlotStyledF64 { .. }));
        // Test-only scalar fixture. The full styled plot output above is outside
        // this projection.
        let read = p.main().code[0].clone();
        let span = p.main().source_map[0];
        p.functions[0].code = vec![read, CsilInstr::PlotF64 { plot_slot: 0 }, CsilInstr::Return];
        p.functions[0].source_map = vec![span; 3];
        p.functions[0].budget_reference = None;
        p.functions[0].max_stack = 1;
        p.plots[0].color_roles.clear();
        p.plots[0].style.color_palette.clear();
        crate::validate(&p).expect("scalar fixture validates");
        let fields = observe_program(&p);
        let bound = crate::ProofBoundScalarView::from_program(&p).expect("actual bound export");
        assert_eq!(
            bound.scalar_view.function.name.as_bytes(),
            fields.functions[0].name
        );
        assert_eq!(
            bound.scalar_view.function.max_stack,
            fields.functions[0].max_stack
        );
        assert_eq!(
            bound.scalar_view.function.code.len(),
            fields.functions[0].rows.len()
        );
        assert_eq!(fields.magic, p.header.magic.as_bytes());
        assert_eq!(
            fields.versions,
            (
                p.header.csil_version,
                p.header.language_version,
                p.header.runtime_abi_version
            )
        );
        assert_eq!(fields.max_bars_back, p.metadata.max_bars_back);
        assert_eq!(fields.plot_count, p.plots.len());
        assert_eq!(
            fields.table_counts,
            (
                p.constants.len(),
                p.visuals.len(),
                p.fills.len(),
                p.ta_states.len(),
                p.ta_input_bindings.len(),
                p.inputs.len(),
                p.object_types.len()
            )
        );
        assert_eq!(fields.functions.len(), p.functions.len());
        assert_eq!(fields.slots.len(), p.series_slots.len());
        for (actual, projected) in p.functions.iter().zip(&fields.functions) {
            assert_eq!(projected.rows.len(), actual.code.len());
            assert_eq!(
                projected.spans,
                actual
                    .source_map
                    .iter()
                    .map(|s| (s.source.0, s.start, s.end))
                    .collect::<Vec<_>>()
            );
            assert_eq!(projected.name, actual.name.as_bytes());
            assert_eq!(projected.max_stack, actual.max_stack);
            assert_eq!(projected.budget_present, actual.budget_reference.is_some());
        }
        let roundtrip = CsilProgram::from_json(&p.to_json()).expect("roundtrip");
        assert_eq!(fields, observe_program(&roundtrip));
    }
    #[test]
    fn malformed_inventory_not_sanitized() {
        let mut p = compiled();
        p.header.magic = "wrong".into();
        p.metadata.max_bars_back = 0;
        p.functions[0].code = vec![CsilInstr::PushI64(7), CsilInstr::Return, CsilInstr::PushNa];
        p.functions[0].source_map.clear();
        p.functions[0].name = "not-main".into();
        p.functions.push(p.functions[0].clone());
        let packet = observe_program(&p);
        assert_eq!(packet.magic, b"wrong");
        assert_eq!(packet.max_bars_back, 0);
        assert_eq!(packet.functions.len(), 2);
        assert_eq!(
            packet.functions[0].rows,
            vec![
                ScalarRow::Unsupported,
                ScalarRow::Return,
                ScalarRow::Unsupported
            ]
        );
        assert!(packet.functions[0].spans.is_empty());
        assert_eq!(packet.functions[0].name, b"not-main");
        p.functions.clear();
        assert!(observe_program(&p).functions.is_empty());
    }
    #[test]
    fn mutated_fields_are_observed_exactly() {
        let mut p = compiled();
        p.series_slots.push(crate::SlotDef {
            name: "λ雪".into(),
            persistent: true,
            varip: true,
            lane: None,
        });
        p.header.csil_version = u32::MAX;
        p.header.language_version = 7;
        p.header.runtime_abi_version = 8;
        p.metadata.max_bars_back = u32::MAX;
        p.constants.push("extra".into());
        p.functions[0].name = "λ-main".into();
        p.functions[0].max_stack = u32::MAX;
        p.functions[0].source_map = vec![pinewasm_span::Span {
            source: pinewasm_span::SourceId(u32::MAX),
            start: u32::MAX,
            end: 0,
        }];
        p.functions[0].budget_reference = Some(crate::BudgetReference {
            version: 1,
            code: vec![],
            source_map: vec![],
            max_stack: 0,
        });
        let observed = observe_program(&p);
        assert_eq!(
            observed.slots.last(),
            Some(&("λ雪".as_bytes().to_vec(), true, true))
        );
        assert_eq!(observed.versions, (u32::MAX, 7, 8));
        assert_eq!(observed.max_bars_back, u32::MAX);
        assert_eq!(observed.table_counts.0, p.constants.len());
        assert_eq!(observed.functions[0].name, "λ-main".as_bytes());
        assert_eq!(observed.functions[0].spans, vec![(u32::MAX, u32::MAX, 0)]);
        assert_eq!(observed.functions[0].max_stack, u32::MAX);
        assert!(observed.functions[0].budget_present);
        assert_eq!(
            scalar_row(&CsilInstr::ReadBuiltin {
                source: BuiltinSeries::Open,
                offset: 0
            }),
            ScalarRow::Unsupported
        );
        assert_eq!(
            scalar_row(&CsilInstr::ReadBuiltin {
                source: BuiltinSeries::Close,
                offset: 1
            }),
            ScalarRow::ReadClose(1)
        );
    }
    #[test]
    fn payloads_not_narrowed() {
        assert_eq!(
            scalar_row(&CsilInstr::ReadUserSlot {
                slot: u32::MAX,
                offset: u32::MAX
            }),
            ScalarRow::ReadSlot(u32::MAX, u32::MAX)
        );
        assert_eq!(
            scalar_row(&CsilInstr::ReadBuiltin {
                source: BuiltinSeries::Close,
                offset: u32::MAX
            }),
            ScalarRow::ReadClose(u32::MAX)
        );
    }

    fn wire_packet(program: &CsilProgram) -> serde_json::Value {
        serde_json::json!({
            "format": "pw-scalar-observer-v1",
            "packet": observe_program(program),
        })
    }

    #[test]
    fn wire_packet_has_exact_versioned_shape_and_payloads() {
        let mut p = compiled();
        p.functions[0].code = vec![
            CsilInstr::ReadBuiltin {
                source: BuiltinSeries::Close,
                offset: u32::MAX,
            },
            CsilInstr::ReadUserSlot {
                slot: u32::MAX,
                offset: 1,
            },
            CsilInstr::StoreUserSlotCurrent { slot: 7 },
            CsilInstr::PlotF64 { plot_slot: 9 },
            CsilInstr::Return,
            CsilInstr::ReadBuiltin {
                source: BuiltinSeries::Open,
                offset: 0,
            },
        ];
        p.functions[0].name = "λ-main".into();
        p.series_slots[0].name = "λ-slot".into();
        p.series_slots[0].persistent = true;
        p.series_slots[0].varip = true;
        p.functions[0].source_map = vec![pinewasm_span::Span {
            source: pinewasm_span::SourceId(u32::MAX),
            start: u32::MAX,
            end: 0,
        }];
        let wire = wire_packet(&p);
        let envelope = wire.as_object().expect("envelope");
        assert_eq!(envelope.len(), 2);
        assert_eq!(wire["format"], "pw-scalar-observer-v1");
        let packet = wire["packet"].as_object().expect("packet");
        let mut keys: Vec<_> = packet.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "functions",
                "magic",
                "max_bars_back",
                "plot_count",
                "slots",
                "strategy_present",
                "table_counts",
                "versions"
            ]
        );
        let mut function_keys: Vec<_> = wire["packet"]["functions"][0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        function_keys.sort_unstable();
        assert_eq!(
            function_keys,
            vec!["budget_present", "max_stack", "name", "rows", "spans"]
        );
        assert_eq!(
            wire["packet"]["functions"][0]["spans"],
            serde_json::json!([[u32::MAX, u32::MAX, 0]])
        );
        assert_eq!(
            wire["packet"]["slots"][0],
            serde_json::json!(["λ-slot".as_bytes(), true, true])
        );
        assert_eq!(
            wire["packet"]["functions"][0]["budget_present"],
            p.functions[0].budget_reference.is_some()
        );
        assert_eq!(
            wire["packet"]["strategy_present"],
            p.metadata.strategy.is_some()
        );
        assert_eq!(
            wire["packet"]["table_counts"],
            serde_json::json!([
                p.constants.len(),
                p.visuals.len(),
                p.fills.len(),
                p.ta_states.len(),
                p.ta_input_bindings.len(),
                p.inputs.len(),
                p.object_types.len()
            ])
        );
        assert_eq!(wire["packet"]["versions"].as_array().unwrap().len(), 3);
        assert_eq!(wire["packet"]["table_counts"].as_array().unwrap().len(), 7);
        assert_eq!(
            wire["packet"]["functions"][0]["name"],
            serde_json::json!("λ-main".as_bytes())
        );
        assert_eq!(
            wire["packet"]["functions"][0]["rows"],
            serde_json::json!([
                {"ReadClose": u32::MAX}, {"ReadSlot": [u32::MAX, 1]},
                {"StoreSlot": 7}, {"Plot": 9}, "Return", "Unsupported"
            ])
        );
        let encoded = serde_json::to_vec(&wire).expect("JSON");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&encoded).unwrap(),
            wire
        );
    }
}
