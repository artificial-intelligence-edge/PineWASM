//! These tests check how ta.sma handles missing inputs during and after warm-up.
//! The first two cases compare PineWASM’s output bar by bar with the expected values below.
use pinewasm_compiler::{compile, run_program, CompileOptions, MarketData, RunOptions, Value};

fn feed(bars: usize) -> MarketData {
    let close = (1..=bars).map(|bar| 10.0 * bar as f64).collect::<Vec<_>>();
    MarketData {
        open: close.clone(),
        high: close.clone(),
        low: close.clone(),
        volume: vec![1.0; bars],
        time: (0..bars)
            .map(|bar| 1_704_067_200_000 + 900_000 * bar as i64)
            .collect(),
        timeframe: "15".into(),
        close,
        ..MarketData::default()
    }
}

fn assert_check(body: &str, expected: &[Option<f64>]) {
    assert_check_with_shared_sma_slot(body, expected, false);
}

fn assert_check_with_shared_sma_slot(body: &str, expected: &[Option<f64>], shared: bool) {
    for options in [CompileOptions::default(), CompileOptions::all()] {
        let compiled = compile(
            &format!("//@version=6\nindicator(\"SMA missing observations\")\n{body}\n"),
            options,
        );
        assert!(compiled.success, "{body}: {:?}", compiled.diagnostics);
        let mut program = compiled.program.unwrap();
        if shared {
            // The frontend currently rejects stateful TA inside loops. A
            // validated shared-slot CSIL program exercises the repeated-call
            // lifecycle without widening source admission.
            use pinewasm_csil::CsilInstr;
            let slots = program
                .functions
                .iter()
                .flat_map(|f| &f.code)
                .filter_map(|i| {
                    if let CsilInstr::WindowMean { slot, .. } = i {
                        Some(*slot)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            for instruction in program.functions.iter_mut().flat_map(|f| &mut f.code) {
                match instruction {
                    CsilInstr::WindowMean { slot, .. } => *slot = slots[0],
                    CsilInstr::StoreUserSlotCurrent { slot } if slots.contains(slot) => {
                        *slot = slots[0];
                    }
                    _ => {}
                }
            }
            for function in &mut program.functions {
                function.budget_reference = None;
            }
            pinewasm_csil::validate(&program).expect("shared numeric SMA slot is valid CSIL");
        }
        let result = run_program(&program, &feed(expected.len()), &RunOptions::default());
        assert!(
            result.runtime_errors.is_empty(),
            "{:?}",
            result.runtime_errors
        );
        let values = expected
            .iter()
            .map(|value| value.map_or(Value::Na, Value::Float))
            .collect::<Vec<_>>();
        assert_eq!(result.plots[0].values, values, "{body}");
    }
}

#[test]
fn first_party_sma_skips_a_missing_observation_during_warmup() {
    // Bar 1 is missing. The first complete two-value window uses 10 and 30,
    // so bar 2 must return 20.
    assert_check(
        "float x=10.0*(bar_index+1)\nfloat gapped=bar_index==1?na:x\nplot(ta.sma(gapped,2))",
        &[
            None,
            None,
            Some(20.0),
            Some(35.0),
            Some(45.0),
            Some(55.0),
            Some(65.0),
            Some(75.0),
        ],
    );
}

#[test]
fn first_party_sma_ignores_consecutive_and_post_seed_missing_observations() {
    // Bars 1, 2 and 5 are missing. Bar 5 holds the previous mean of 45.
    // Bar 6 averages the values from bars 4 and 6, giving 60.
    assert_check(
        "float x=10.0*(bar_index+1)\nfloat gapped=bar_index==1 or bar_index==2 or bar_index==5?na:x\nplot(ta.sma(gapped,2))",
        &[None, None, None, Some(25.0), Some(45.0), Some(45.0), Some(60.0), Some(75.0), Some(85.0), Some(95.0)],
    );
}

#[test]
fn dense_sma_control_keeps_the_existing_arithmetic_order() {
    assert_check(
        "plot(ta.sma(10.0*(bar_index+1),2))",
        &[
            None,
            Some(15.0),
            Some(25.0),
            Some(35.0),
            Some(45.0),
            Some(55.0),
            Some(65.0),
            Some(75.0),
        ],
    );
}

#[test]
fn first_party_sma_retains_observations_across_long_gaps_and_growing_lengths() {
    // The length-four route needs observations that precede the initial
    // length-two window and several elapsed-bar rings.
    let source =
        "float g=bar_index==0?10.0:bar_index==5?20.0:bar_index==9?30.0:bar_index==10?40.0:na\n";
    assert_check(
        &format!("{source}plot(ta.sma(g,2))"),
        &[
            None,
            None,
            None,
            None,
            None,
            Some(15.0),
            Some(15.0),
            Some(15.0),
            Some(15.0),
            Some(25.0),
            Some(35.0),
            Some(35.0),
        ],
    );
    assert_check(
        &format!("{source}int n=bar_index<9?2:4\nplot(ta.sma(g,n))"),
        &[
            None,
            None,
            None,
            None,
            None,
            Some(15.0),
            Some(15.0),
            Some(15.0),
            Some(15.0),
            None,
            Some(25.0),
            Some(25.0),
        ],
    );
}

#[test]
fn first_party_sma_replaces_repeated_observations_on_the_same_bar() {
    for final_output in [false, true] {
        let body = format!(
            "float first=ta.sma(float(bar_index*10),2)\nfloat middle=ta.sma(float(bar_index*10+1),2)\nfloat last=ta.sma(float(bar_index*10+2),2)\nplot({})",
            if final_output { "last" } else { "first" },
        );
        let expected = std::iter::once(None)
            .chain(
                (0..11)
                    .map(|bar| Some((if final_output { 7 } else { 6 }) as f64 + 10.0 * bar as f64)),
            )
            .collect::<Vec<_>>();
        assert_check_with_shared_sma_slot(&body, &expected, true);
    }
}

#[test]
fn input_alias_helper_and_computed_lengths_follow_the_live_direct_source_rule() {
    // Derived length routes: the cases cover direct literal and series
    // lengths. These ensure compiler lowering does not alter that rule.
    let expected = [
        None,
        None,
        Some(20.0),
        Some(35.0),
        Some(45.0),
        Some(55.0),
        Some(65.0),
        Some(75.0),
    ];
    for body in [
        "n=input.int(2)\nplot(ta.sma(g,n))",
        "n=input.int(2)\na=n\nplot(ta.sma(g,a))",
        "f(simple int n)=>ta.sma(g,n)\nplot(f(input.int(2)))",
        "n=input.int(1)+1\nplot(ta.sma(g,n))",
    ] {
        assert_check(
            &format!("float g=bar_index==1?na:10.0*(bar_index+1)\n{body}"),
            &expected,
        );
    }
}

#[test]
fn requested_context_sma_uses_its_own_present_observations() {
    // Derived from the SMA rule; no higher-timeframe claim is made.
    // Thirty-minute closes are 20, 40, 60, 80. The source close/(close-40)
    // is -1, na, 3, 2. Preserve the existing
    // requested-context clock and lookahead-on mapping to two chart bars.
    assert_check(
        "plot(request.security(syminfo.tickerid,\"30\",ta.sma(close/(close-40),2),lookahead=barmerge.lookahead_on))",
        &[None,None,None,None,Some(1.0),Some(1.0),Some(2.5),Some(2.5)],
    );
}
