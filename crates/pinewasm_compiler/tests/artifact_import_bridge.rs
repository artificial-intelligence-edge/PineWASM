#[cfg(test)]
mod tests {
    use pinewasm_compiler::{
        run_program, BuiltinSeries, MarketData, Program, RunOptions, RuntimeErrorKind, Value,
    };
    use pinewasm_csil::{validate, CsilInstr};
    use pinewasm_span::{SourceId, Span};
    use sha2::{Digest, Sha256};

    const ARTIFACT_BYTES: &[u8] = br#"{"header":{"magic":"CSIL","il_version":2,"language_version":6,"runtime_abi_version":0},"constants":[],"series_slots":[{"name":"x","persistent":false,"varip":false}],"plots":[{"title":null,"style":{"kind":"plot","plotStyle":"line","lineStyle":"solid","lineWidth":1,"display":"all","histogramBase":0.0,"offset":0,"showLast":null,"forceOverlay":false,"trackPrice":false,"join":false,"editable":true,"format":null,"precision":null,"group":null,"ohlcField":null,"markerShape":null,"markerLocation":null,"markerSize":null,"character":null,"text":null,"minHeight":null,"maxHeight":null,"unsupported":[]}}],"visuals":[],"fills":[],"ta_states":[],"inputs":[],"object_types":[{"name":"chart.point","field_count":3},{"name":"line","field_count":10},{"name":"label","field_count":14},{"name":"box","field_count":19},{"name":"table","field_count":9},{"name":"polyline","field_count":9},{"name":"linefill","field_count":3},{"name":"footprint","field_count":8},{"name":"volume_row","field_count":8}],"functions":[{"name":"main","code":[{"ReadBuiltin":{"source":"Close","offset":0}},{"StoreUserSlotCurrent":{"slot":0}},{"ReadUserSlot":{"slot":0,"offset":0}},{"PlotF64":{"plot_slot":0}},"Return"],"source_map":[{"source":0,"start":38,"end":43},{"source":0,"start":34,"end":43},{"source":0,"start":49,"end":50},{"source":0,"start":44,"end":51},{"source":0,"start":44,"end":51}],"max_stack":1}],"metadata":{"title":"Imported","overlay":false,"max_bars_back":500}}"#;
    const ARTIFACT_SHA256: &str =
        "de3f773dbaa49aa457ce08d30b5b94bfcc4596c9cd8c6d3dc433d7dcd60b8a05";
    const ARTIFACT_BYTE_LEN: usize = 1444;

    fn sha256_hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn explicit_market() -> MarketData {
        MarketData {
            open: vec![100.0, 98.0, 103.0],
            high: vec![102.0, 101.0, 105.0],
            low: vec![97.0, 96.0, 100.0],
            close: vec![101.25, 99.5, 104.75],
            volume: vec![10.0, 20.0, 30.0],
            ..MarketData::default()
        }
    }

    fn program_json() -> serde_json::Value {
        serde_json::from_slice(ARTIFACT_BYTES).expect("fixture is valid JSON")
    }

    #[test]
    fn artifact_import_validates_and_executes_close_trace() {
        assert_eq!(ARTIFACT_BYTES.len(), ARTIFACT_BYTE_LEN);
        assert_eq!(sha256_hex(ARTIFACT_BYTES), ARTIFACT_SHA256);

        // This is a serialized CSIL program imported through Serde, not
        // a Rust-side reconstruction of its instruction structs.
        let program: Program = serde_json::from_slice(ARTIFACT_BYTES).expect("Rust Serde import");
        assert_eq!(program.functions.len(), 1);
        let main = &program.functions[0];
        assert_eq!(main.name, "main");
        assert_eq!(main.max_stack, 1);
        assert_eq!(
            main.code,
            vec![
                CsilInstr::ReadBuiltin {
                    source: BuiltinSeries::Close,
                    offset: 0,
                },
                CsilInstr::StoreUserSlotCurrent { slot: 0 },
                CsilInstr::ReadUserSlot { slot: 0, offset: 0 },
                CsilInstr::PlotF64 { plot_slot: 0 },
                CsilInstr::Return,
            ]
        );
        assert_eq!(
            main.source_map,
            vec![
                Span::new(SourceId::MAIN, 38, 43),
                Span::new(SourceId::MAIN, 34, 43),
                Span::new(SourceId::MAIN, 49, 50),
                Span::new(SourceId::MAIN, 44, 51),
                Span::new(SourceId::MAIN, 44, 51),
            ]
        );
        assert_eq!(main.source_map.len(), main.code.len());

        validate(&program).expect("pinned Rust CSIL validator accepts fixture");
        let market = explicit_market();
        let result = run_program(&program, &market, &RunOptions::default());

        assert!(
            result.runtime_errors.is_empty(),
            "{:#?}",
            result.runtime_errors
        );
        assert!(result.diagnostics.is_empty());
        assert!(result.compile.is_none());
        assert_eq!(result.bars_executed, 3);
        assert_eq!(result.plots.len(), 1);
        assert_eq!(result.plots[0].slot, 0);
        assert_eq!(
            result.plots[0].values,
            vec![
                Value::Float(101.25),
                Value::Float(99.5),
                Value::Float(104.75),
            ]
        );
    }

    #[test]
    fn malformed_and_unsupported_inputs_fail_at_the_import_boundary() {
        assert!(serde_json::from_slice::<Program>(b"{not-json").is_err());

        let mut unsupported = program_json();
        unsupported["functions"][0]["code"][0] = serde_json::json!("NoSuchOpcode");
        let error = serde_json::from_value::<Program>(unsupported)
            .expect_err("Serde must reject an unregistered CSIL opcode");
        assert!(error.to_string().contains("NoSuchOpcode"));
    }

    #[test]
    fn stack_underflow_is_rejected_by_validation_and_cached_program_execution() {
        let mut invalid = program_json();
        invalid["functions"][0]["code"] =
            serde_json::json!([{"PlotF64": {"plot_slot": 0}}, "Return"]);
        invalid["functions"][0]["source_map"]
            .as_array_mut()
            .expect("fixture source map is an array")
            .truncate(2);
        let program: Program = serde_json::from_value(invalid)
            .expect("stack-invalid code still has a valid Serde shape");

        let validation_errors =
            validate(&program).expect_err("PlotF64 on an empty stack must fail validation");
        assert!(validation_errors
            .iter()
            .any(|error| error.to_string().contains("stack underflow")));

        let result = run_program(&program, &explicit_market(), &RunOptions::default());
        assert_eq!(result.bars_executed, 0);
        assert!(result.plots.is_empty());
        assert_eq!(result.runtime_errors.len(), 1);
        assert_eq!(
            result.runtime_errors[0].kind,
            RuntimeErrorKind::InvalidProgram
        );
        assert!(result.runtime_errors[0]
            .message
            .contains("cached program rejected by the validator"));
        assert!(result.runtime_errors[0].message.contains("stack underflow"));
    }

    #[test]
    fn nonempty_return_stack_is_rejected_by_validation_and_cached_program_execution() {
        let mut invalid = program_json();
        invalid["functions"][0]["code"] =
            serde_json::json!([{"ReadBuiltin": {"source": "Close", "offset": 0}}, "Return"]);
        invalid["functions"][0]["source_map"]
            .as_array_mut()
            .expect("fixture source map is an array")
            .truncate(2);
        let program: Program = serde_json::from_value(invalid)
            .expect("nonempty-return-stack code still has a valid Serde shape");

        let validation_errors =
            validate(&program).expect_err("Return with one remaining operand must fail validation");
        assert!(validation_errors.iter().any(|error| {
            error
                .to_string()
                .contains("operand stack must be empty at RETURN")
        }));

        let result = run_program(&program, &explicit_market(), &RunOptions::default());
        assert_eq!(result.bars_executed, 0);
        assert!(result.plots.is_empty());
        assert_eq!(result.runtime_errors.len(), 1);
        assert_eq!(
            result.runtime_errors[0].kind,
            RuntimeErrorKind::InvalidProgram
        );
        assert!(result.runtime_errors[0]
            .message
            .contains("cached program rejected by the validator"));
        assert!(result.runtime_errors[0]
            .message
            .contains("operand stack must be empty at RETURN"));
    }
}
