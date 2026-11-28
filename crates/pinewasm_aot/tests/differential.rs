//! End-to-end differential tests: compile Pine → CSIL → Wasm, execute the
//! module with `wasmi` (host imports backed by `pinewasm_vm::aot_host`), and
//! require bit-level agreement with the interpreter, na (NaN in
//! lanes) included.

use pinewasm_aot::analysis::{analyze, IntMinGuard, VTy};
use pinewasm_aot::{
    compile_program, compile_program_to_wasm, compile_standalone_wasm_program, header, status,
    AotError, AotModule, ExecutionMode, RefusalClass, StandaloneWasmModuleType,
    StandaloneWasmOptions,
};
use pinewasm_compiler::{compile, CompileOptions};
use pinewasm_hir::InputValue;
use pinewasm_mir::Value;
use pinewasm_vm::{run, MarketData, RunOptions};
use proptest::prelude::*;
use proptest::test_runner::{Config, RngAlgorithm, TestCaseError, TestRng, TestRunner};

mod support;
use support::{read_u32, standalone_wasm_wire, synthetic_data, StandaloneWasmHost};

/// The typed matrix's tables and enumeration.
#[path = "typed_matrix/cells.rs"]
mod matrix;

#[path = "support/kept_quotients.rs"]
mod kept_quotients;
use kept_quotients::{
    KEPT_INT_DIVISION_BODIES, KEPT_INT_DIVISION_BODIES_ON_MAIN, LARGE_INTS, LARGE_INT_BODIES,
};

fn compile_csil(source: &str) -> pinewasm_csil::CsilProgram {
    let result = compile(source, CompileOptions::default());
    if let Some(d) = result.diagnostics.iter().find(|d| d.is_error()) {
        panic!("compile error: {:?} {}", d.code, d.message);
    }
    result.program.expect("compile must produce a program")
}

#[test]
fn native_text_format_value_lanes_preserve_dynamic_union_and_drawings() {
    for body in [
        "var d=label.new(0,10,\"Format\",text_formatting=text.format_none)\nlabel.set_text_formatting(d,f)\n",
        "var d=box.new(0,12,1,9,text=\"Format\",text_formatting=text.format_none)\nbox.set_text_formatting(d,f)\n",
        "var t=table.new(position.top_right,1,1)\ntable.cell(t,0,0,\"Format\",text_formatting=text.format_none)\ntable.cell_set_text_formatting(t,0,0,f)\n",
    ] {
        let source = format!(
            "//@version=6\nindicator(\"Native formatting\",overlay=true)\na=bar_index%2==0?text.format_bold:text.format_none\nb=bar_index%3==0?text.format_italic:text.format_none\nf=a+b+a\n{body}plot(f==text.format_bold+text.format_italic?1:0)\n"
        );
        let program = compile_csil(&source);
        let module = compile_program_to_wasm(&program).expect("formatting remains executable");
        assert_eq!(module.layout.execution_mode, ExecutionMode::Native);
        for bars in [0, 1, 17, 1025] {
            let data = synthetic_data(bars);
            let options = RunOptions::default();
            let reference = run(&program, &data, &options);
            assert!(reference.errors.is_empty(), "{body}: {:?}", reference.errors);
            let actual = run_aot(&module, &program, &data, &options);
            assert_eq!(actual.status, status::OK, "{body}: {bars}");
            assert_outcomes_match(&reference, &actual, bars, body);
            assert_eq!(Some(reference.drawings), actual.vm_drawings, "{body}: {bars}");
            for (bar, value) in actual.plots[0].iter().enumerate() {
                assert_eq!(*value, if bar % 6 == 0 { 1.0 } else { 0.0 }, "{body}: {bar}");
            }
        }
    }
}

#[test]
fn native_v6_color_accessors_return_float_channels_and_preserve_na() {
    let program = compile_csil("//@version=6\nindicator(\"Float color channels\")\nc = bar_index % 2 == 0 ? color.rgb(18,52,86,40) : na\nplot(color.r(c))\nplot(color.g(c))\nplot(color.b(c))\nplot(color.t(c))\n");
    let module = compile_program(&program).expect("color channels remain native");
    for bars in [0, 1, 17, 1025] {
        let data = synthetic_data(bars);
        let options = RunOptions::default();
        let reference = run(&program, &data, &options);
        assert!(reference.errors.is_empty(), "{:?}", reference.errors);
        let native = run_aot(&module, &program, &data, &options);
        assert_eq!(native.status, status::OK);
        for (slot, expected) in [18.0, 52.0, 86.0, 40.0].into_iter().enumerate() {
            for bar in 0..bars {
                if bar % 2 == 0 {
                    assert_eq!(reference.plots[slot].values[bar], Value::Float(expected));
                    assert_eq!(native.plots[slot][bar], expected);
                } else {
                    assert_eq!(reference.plots[slot].values[bar], Value::Na);
                    assert!(native.plots[slot][bar].is_nan());
                }
            }
        }
    }
}

#[test]
fn fractional_color_producers_use_hosted_wasm_without_losing_precision() {
    // Raw color storage and numeric constant accessors are separate domains.
    // Current successful numeric defaults qualify whole-percent const results;
    // hosting must still preserve fractional color payloads and nonconst values.
    for (body, reason, raw_transparency, accessor) in [
        ("c = #12345680", "fractional color transparency", 100.0 * (1.0 - 128.0 / 255.0), 50.0),
        ("c = color.rgb(18,52,86,47.6)", "fractional color transparency", 47.6, 47.0),
        ("c = color.new(#123456,input.float(47.6))", "unproven color transparency", 47.6, 47.6),
        ("c = color.new(#123456,input.int(3)/2)", "unproven color transparency", 1.5, 1.5),
        ("c = color.from_gradient(input.float(0.25),0.0,1.0,color.new(#123456,20),color.new(#123456,50))", "unproven color transparency", 27.5, 27.5),
        ("c = ta.valuewhen(bar_index % 3 == 0,color.new(#123456,47.6),0)", "fractional color transparency", 47.6, 47.6),
    ] {
        let source = format!("//@version=6\nindicator(\"Exact alpha wrapper\")\n{body}\nplot(close,color=c)\nplot(color.t(c))\n");
        let program = compile_csil(&source);
        assert_eq!(compile_program(&program).unwrap_err(), AotError::UnsupportedOpcode(reason.into()), "{body}");
        let module = compile_program_to_wasm(&program).unwrap();
        assert_eq!(module.layout.execution_mode, ExecutionMode::InterpreterHosted);
        wasmparser::validate(&module.wasm).unwrap();
        for bars in [0, 1, 65, 1025] {
            let data = synthetic_data(bars);
            let options = RunOptions::default();
            let reference = run(&program, &data, &options);
            assert!(reference.errors.is_empty(), "{body}: {:?}", reference.errors);
            let actual = run_aot(&module, &program, &data, &options);
            assert_eq!(actual.status, status::OK, "{body}");
            assert_outcomes_match(&reference, &actual, bars, body);
            for bar in 0..bars {
                assert_eq!(reference.plots[1].values[bar], Value::Float(accessor), "{body}: bar {bar}");
                assert_eq!(actual.plots[1][bar], accessor, "{body}: bar {bar}");
                assert_eq!(actual.colors[0][bar], Value::Color { r:18, g:52, b:86, t:raw_transparency , input_origin: None }, "{body}: bar {bar}");
            }
        }
    }
}

#[test]
fn runtime_color_overrides_use_hosted_wasm_without_truncation() {
    let program = compile_csil("//@version=6\nindicator(\"Input alpha guard\")\nc=input.color(#123456)\nplot(close,color=c)\nplot(color.t(c))\n");
    assert_eq!(
        compile_program(&program).unwrap_err(),
        AotError::UnsupportedOpcode("color input provenance".into())
    );
    let artifact = compile_program_to_wasm(&program).unwrap();
    let data = synthetic_data(17);
    let mut options = RunOptions::default();
    options.inputs.insert(
        0,
        InputValue::Color {
            r: 18,
            g: 52,
            b: 86,
            t: 47.6,
        },
    );
    let actual = execute_hosted_wrapper(&artifact, &program, &data, &options);
    let reference = run(&program, &data, &options);
    assert_eq!(actual, reference);
    assert!(actual.errors.is_empty());
    assert_eq!(actual.plots[1].values, vec![Value::Float(47.6); 17]);
    assert!(actual.plots[0].colors[0]
        .values
        .iter()
        .all(|color| color.render() == "#12345686"));
    assert_eq!(actual.plots[0].colors[0].palette_indices, vec![Some(0); 17]);
}

#[test]
fn proved_whole_color_producers_still_run_natively() {
    for body in [
        "c = color.new(#123456,input.int(40))",
        "c = color.rgb(18,52,86,math.round(input.float(40.4)))",
        "c = color.from_gradient(close,low,high,color.new(#123456,40),color.new(#234567,40))",
        "c = color.from_gradient(0.5,0.0,1.0,color.new(#123456,20),color.new(#123456,60))",
    ] {
        let program = compile_csil(&format!("//@version=6\nindicator(\"Whole alpha\")\n{body}\nplot(close,color=c)\nplot(color.t(c))\n"));
        let module = compile_program(&program).unwrap_or_else(|error| panic!("{body}: {error}"));
        assert_eq!(module.layout.execution_mode, ExecutionMode::Native);
        for bars in [0, 1, 65, 1025] {
            let data = synthetic_data(bars);
            let options = RunOptions::default();
            let reference = run(&program, &data, &options);
            let actual = run_aot(&module, &program, &data, &options);
            assert_eq!(actual.status, status::OK, "{body}");
            assert_outcomes_match(&reference, &actual, bars, body);
            assert!(
                reference.plots[1]
                    .values
                    .iter()
                    .all(|value| *value == Value::Float(40.0)),
                "{body}"
            );
        }
    }
}

#[test]
fn native_fixed_style_cells_match_engine_snapshots_including_strings_and_ring_wrap() {
    for source in [
        "//@version=6\nindicator(\"fixed numeric styles\")\nn = input.int(7)\nplot(close, linewidth=int(math.pow(n, 2)))\n",
        "//@version=6\nindicator(\"fixed string styles\")\nn = input.string(\"x\")\na = input.int(7)\np = plot(close, offset=str.length(str.upper(n)))\nq = plot(open)\nplotchar(true, char=n)\nbgcolor(color.blue, offset=str.length(n))\nfill(p, q, color.blue, show_last=int(math.pow(a, 2)))\n",
    ] {
        let program = compile_csil(source);
        let module = compile_program(&program).expect("fixed styles remain native");
        assert!(!module.layout.style_slots.is_empty());
        for bars in [0, 1, 17, 1025] {
            let data = synthetic_data(bars);
            let options = RunOptions::default();
            let expected = run(&program, &data, &options);
            assert!(expected.errors.is_empty(), "{:?}", expected.errors);
            let actual = run_aot(&module, &program, &data, &options);
            assert_eq!(actual.status, status::OK);
            assert_eq!(actual.fixed_styles, Some(pinewasm_vm::NativeDisplayStyles {
                plots: expected.plots.iter().map(|plot| plot.style.clone()).collect(),
                visuals: expected.visuals.iter().map(|visual| visual.style.clone()).collect(),
                fills: expected.fills.iter().map(|fill| fill.style.clone()).collect(),
            }), "{source} bars={bars}");
        }
    }
}

/// Pine source for a display destination set (pane 1, price scale 2, status
/// line 4, Data Window 8), written with constants only.
fn pine_display_set(mask: u8) -> String {
    let members: Vec<&str> = [
        "display.pane",
        "display.price_scale",
        "display.status_line",
        "display.data_window",
    ]
    .into_iter()
    .enumerate()
    .filter(|(bit, _)| mask & (1 << bit) != 0)
    .map(|(_, name)| name)
    .collect();
    if members.is_empty() {
        "display.none".into()
    } else {
        members.join(" + ")
    }
}

#[test]
fn runtime_display_arithmetic_matches_interpreter_bit_for_bit() {
    // `+` and `-` over every pair of the 16 destination sets, chosen per bar,
    // each result plotted as its set number.
    let pick = |var: &str, index: &str| {
        let mut out = format!("{var} = switch {index}\n");
        for mask in 0..15u8 {
            out.push_str(&format!("    {mask} => {}\n", pine_display_set(mask)));
        }
        out.push_str(&format!("    => {}\n", pine_display_set(15)));
        out
    };
    let decode = |expr: &str| {
        let mut out = String::new();
        for mask in 0..15u8 {
            out.push_str(&format!("{expr} == {} ? {mask} : ", pine_display_set(mask)));
        }
        out + "15"
    };
    let source = format!(
        "//@version=6\nindicator(\"Display pairs\")\ni = bar_index % 16\nj = math.floor(bar_index / 16) % 16\n{}{}U = X + Y\nD = X - Y\nplot({})\nplot({})\n",
        pick("X", "i"),
        pick("Y", "j"),
        decode("U"),
        decode("D"),
    );
    let program = compile_csil(&source);
    let module = compile_program_to_wasm(&program).expect("display arithmetic remains executable");
    assert_eq!(module.layout.execution_mode, ExecutionMode::Native);
    for bars in [0, 1, 17, 256, 1025] {
        let data = synthetic_data(bars);
        let options = RunOptions::default();
        let reference = run(&program, &data, &options);
        assert!(reference.errors.is_empty(), "{:?}", reference.errors);
        let actual = run_aot(&module, &program, &data, &options);
        assert_eq!(actual.status, status::OK, "bars={bars}");
        assert_outcomes_match(&reference, &actual, bars, "display pairs");
        for bar in 0..bars {
            let (x, y) = ((bar % 16) as u8, ((bar / 16) % 16) as u8);
            assert_eq!(actual.plots[0][bar], f64::from(x | y), "{x} + {y}");
            assert_eq!(actual.plots[1][bar], f64::from(x & !y), "{x} - {y}");
        }
    }

    // Input-chosen display arguments: the styles the native module resolves
    // equal the interpreter's, for each input value. The last script is
    // resolved after the run (a reassigned operand); the others bind an
    // input expression.
    for source in [
        "//@version=6\nindicator(\"Input display sum\")\nX = input.bool(true) ? display.pane : display.none\nplot(close, display = X + display.price_scale)\n",
        "//@version=6\nindicator(\"Display difference\")\nsel = input.bool(true)\nX = sel ? display.pane : display.none\nplot(close, display = display.all - X)\n",
        "//@version=6\nindicator(\"Reassigned display sum\")\nsel = input.bool(true)\nX = display.pane\nif not sel\n    X := display.none\nplot(close, display = X + display.price_scale)\n",
    ] {
        let program = compile_csil(source);
        let module = compile_program(&program).expect("display styles remain executable");
        for selected in [true, false] {
            let data = synthetic_data(17);
            let options = RunOptions {
                inputs: [(0, InputValue::Bool(selected))].into_iter().collect(),
                ..RunOptions::default()
            };
            let expected = run(&program, &data, &options);
            assert!(expected.errors.is_empty(), "{:?}", expected.errors);
            let actual = run_aot(&module, &program, &data, &options);
            assert_eq!(actual.status, status::OK);
            assert_outcomes_match(&expected, &actual, 17, source);
            // Only the reassigned operand reaches the host through a native
            // style slot; an input expression is bound before the run.
            assert_eq!(
                !module.layout.style_slots.is_empty(),
                source.contains(":="),
                "{source}"
            );
            if !module.layout.style_slots.is_empty() {
                assert_eq!(
                    actual.fixed_styles,
                    Some(pinewasm_vm::NativeDisplayStyles {
                        plots: expected.plots.iter().map(|plot| plot.style.clone()).collect(),
                        visuals: expected
                            .visuals
                            .iter()
                            .map(|visual| visual.style.clone())
                            .collect(),
                        fills: expected.fills.iter().map(|fill| fill.style.clone()).collect(),
                    }),
                    "{source} selected={selected}"
                );
            }
        }
    }
}

#[test]
fn nested_runtime_display_arithmetic_with_history_matches_set_algebra_natively() {
    // Random nested `+` / `-` trees over three per-bar display sets, their
    // one-bar history and constants. The expected set is computed from the
    // union/difference rule here, not by either engine; `X[1]` on the first
    // bar is `na`, and an `na` operand makes the whole result `na` (-1).
    enum Tree {
        Const(u8),
        Var(usize),
        Prev(usize),
        Op(bool, Box<Tree>, Box<Tree>),
    }
    fn generate(next: &mut dyn FnMut(u64) -> u64, depth: u32) -> Tree {
        if depth == 0 || next(3) == 0 {
            match next(5) {
                0 => Tree::Const(next(16) as u8),
                1 => Tree::Prev(next(3) as usize),
                _ => Tree::Var(next(3) as usize),
            }
        } else {
            Tree::Op(
                next(2) == 0,
                Box::new(generate(next, depth - 1)),
                Box::new(generate(next, depth - 1)),
            )
        }
    }
    fn source(tree: &Tree) -> String {
        match tree {
            Tree::Const(mask) => format!("({})", pine_display_set(*mask)),
            Tree::Var(var) => format!("X{var}"),
            Tree::Prev(var) => format!("X{var}[1]"),
            Tree::Op(union, lhs, rhs) => format!(
                "({} {} {})",
                source(lhs),
                if *union { "+" } else { "-" },
                source(rhs)
            ),
        }
    }
    fn expected(tree: &Tree, var: &dyn Fn(usize, usize) -> u8, bar: usize) -> Option<u8> {
        match tree {
            Tree::Const(mask) => Some(*mask),
            Tree::Var(index) => Some(var(*index, bar)),
            Tree::Prev(index) => (bar > 0).then(|| var(*index, bar - 1)),
            Tree::Op(union, lhs, rhs) => {
                let (left, right) = (expected(lhs, var, bar)?, expected(rhs, var, bar)?);
                Some(if *union { left | right } else { left & !right })
            }
        }
    }
    let mut seed: u64 = 0x434;
    let mut next = move |n: u64| {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (seed >> 33) % n
    };
    let steps = [(1usize, 0usize), (3, 5), (7, 2)];
    let var = |index: usize, bar: usize| ((bar * steps[index].0 + steps[index].1) % 16) as u8;
    for _ in 0..40 {
        let tree = loop {
            let tree = generate(&mut next, 4);
            if matches!(tree, Tree::Op(..)) {
                break tree;
            }
        };
        let mut script = String::from("//@version=6\nindicator(\"Nested display arithmetic\")\n");
        for (index, (step, start)) in steps.iter().enumerate() {
            script += &format!("X{index} = switch (bar_index * {step} + {start}) % 16\n");
            for mask in 0..15u8 {
                script += &format!("    {mask} => {}\n", pine_display_set(mask));
            }
            script += &format!("    => {}\n", pine_display_set(15));
        }
        script += &format!("D = {}\n", source(&tree));
        let mut decode = String::from("na(D) ? -1 : ");
        for mask in 0..15u8 {
            decode += &format!("D == ({}) ? {mask} : ", pine_display_set(mask));
        }
        script += &format!("plot({decode}15)\n");
        let program = compile_csil(&script);
        let module = compile_program_to_wasm(&program).expect("display arithmetic stays native");
        assert_eq!(module.layout.execution_mode, ExecutionMode::Native);
        for bars in [0, 1, 40] {
            let data = synthetic_data(bars);
            let options = RunOptions::default();
            let reference = run(&program, &data, &options);
            assert!(reference.errors.is_empty(), "{:?}", reference.errors);
            let actual = run_aot(&module, &program, &data, &options);
            assert_eq!(actual.status, status::OK);
            assert_outcomes_match(&reference, &actual, bars, &script);
            for bar in 0..bars {
                let want = expected(&tree, &var, bar).map_or(-1.0, f64::from);
                assert_eq!(actual.plots[0][bar], want, "bar {bar}: {}", source(&tree));
            }
        }
    }
}

#[test]
fn engine_resolved_display_arithmetic_has_the_same_native_style() {
    // Two shapes the compiler cannot bind before the run: an `if` expression
    // operand, and a sum of seven independent input choices (128 sets, past
    // the 64-choice binding cap). Both must reach the host through the
    // native style slot with the interpreter's member union, which equals the
    // set computed by hand.
    let mut wide = String::from("//@version=6\nindicator(\"Wide display sum\")\n");
    let members = [
        "display.pane",
        "display.price_scale",
        "display.pine_screener",
        "display.data_window",
    ];
    for k in 0..7 {
        wide += &format!(
            "b{k} = input.bool(false)\nX{k} = b{k} ? {} : display.none\n",
            members[k % 4]
        );
    }
    wide += "plot(close, display = X0 + X1 + X2 + X3 + X4 + X5 + X6)\n";
    let cases: [(String, [(bool, u8); 2]); 3] = [
        (
            "//@version=6\nindicator(\"If display sum\")\nsel = input.bool(true)\nX = if sel\n    display.pane\nelse\n    display.status_line\nplot(close, display = X + display.price_scale)\n".into(),
            // input 0 on: pane + price scale; off: price scale + status line
            [(true, 1 | 2), (false, 2 | 4)],
        ),
        // input 0 (pane) and input 6 (screener destination, 6 % 4 == 2) on, or only 6.
        (wide.replace("plot(close,", "hline(70,"), [(true, 0), (false, 0)]),
        (wide, [(true, 1 | 16), (false, 16)]),
    ];
    for (source, runs) in &cases {
        let baseline_hline = source.contains("hline(70,");
        let mut program = compile_csil(&source.replace("hline(70,", "plot(70,"));
        // Recreate a broad display payload that uses the hline kind for plot 0.
        if baseline_hline {
            program.plots[0].style.kind = pinewasm_hir::DisplayKind::Hline;
            program.plots[0].style.plot_color_mode = None;
        }
        let display = |styles: &[pinewasm_hir::DisplayStyle]| {
            styles[0]
                .bound_scalars
                .iter()
                .find(|binding| binding.field == "display")
                .map(|binding| binding.expression.clone())
        };
        let compiled = program.plots[0].style.clone();
        assert!(
            compiled
                .runtime_scalars
                .iter()
                .any(|b| b.field == "display")
                && display(std::slice::from_ref(&compiled)).is_none(),
            "{source}: resolved after the run, never bound to a fixed value"
        );
        let module = compile_program(&program).expect("display styles remain executable");
        assert!(!module.layout.style_slots.is_empty(), "{source}");
        for (selected, mask) in runs {
            let options = RunOptions {
                inputs: [
                    (0, InputValue::Bool(*selected)),
                    (6, InputValue::Bool(true)),
                ]
                .into_iter()
                .filter(|(id, _)| (*id as usize) < program.inputs.len())
                .collect(),
                ..RunOptions::default()
            };
            let data = synthetic_data(17);
            let expected = run(&program, &data, &options);
            assert!(expected.errors.is_empty(), "{:?}", expected.errors);
            let actual = run_aot(&module, &program, &data, &options);
            assert_eq!(actual.status, status::OK);
            assert_outcomes_match(&expected, &actual, 17, source);
            let styles: Vec<_> = expected
                .plots
                .iter()
                .map(|plot| plot.style.clone())
                .collect();
            assert_eq!(
                display(&styles),
                pinewasm_hir::display_set::binding_expr(*mask),
                "{source} selected={selected}"
            );
            if baseline_hline {
                assert_eq!(styles[0].display, pinewasm_hir::PlotDisplay::None);
                assert!(styles[0].unsupported.iter().any(|option| {
                    option.field == "display" && option.reason.contains("screener")
                }));
            }
            let native = actual.fixed_styles.expect("native style slot");
            assert_eq!(native.plots, styles, "{source} selected={selected}");
        }
    }
}

#[test]
fn native_boolean_markers_preserve_zero_in_numeric_lanes() {
    let program = compile_csil("//@version=6\nindicator(\"Boolean readouts\")\nplotshape(bar_index % 2 == 0)\nplotchar(bar_index % 2 == 0)\n");
    let module = compile_program(&program).expect("boolean markers remain native");
    for bars in [0, 1, 18, 1025] {
        let data = synthetic_data(bars);
        let options = RunOptions::default();
        let expected = run(&program, &data, &options);
        assert!(expected.errors.is_empty());
        let actual = run_aot(&module, &program, &data, &options);
        assert_eq!(actual.status, status::OK);
        for (slot, lane) in actual.plots.iter().enumerate() {
            assert_eq!(lane.len(), bars);
            for (bar, value) in lane.iter().enumerate() {
                let numeric = if bar % 2 == 0 { 1.0 } else { 0.0 };
                assert_eq!(*value, numeric, "slot={slot} bar={bar}");
                assert_eq!(expected.plots[slot].values[bar], Value::Float(numeric));
            }
        }
    }
}

#[test]
fn fill_mode_snapshots_survive_native_and_hosted_wasm() {
    for gradient in [false, true] {
        let source = format!("//@version=6\nindicator(\"Fill mode snapshot\")\na=plot(20)\nb=plot(0)\nc=color.blue\nfill(a,b,c)\nfill(a,b,bar_index>=0 ? color.blue : color.blue)\n{}", if gradient { "fill(a,b,top_value=bar_index,bottom_value=0,top_color=color.blue,bottom_color=na)\n" } else { "" });
        let program = compile_csil(&source);
        let module = compile_program_to_wasm(&program).expect("portable fill channels");
        assert_eq!(
            module.layout.execution_mode,
            if gradient {
                ExecutionMode::InterpreterHosted
            } else {
                ExecutionMode::Native
            }
        );
        let mut modes = vec![
            Some(pinewasm_hir::FillColorMode::Fixed),
            Some(pinewasm_hir::FillColorMode::Series),
        ];
        if gradient {
            modes.push(Some(pinewasm_hir::FillColorMode::Series));
        }
        for bars in [0, 1, 17, 1025] {
            let data = synthetic_data(bars);
            let options = RunOptions::default();
            let reference = run(&program, &data, &options);
            assert!(reference.errors.is_empty());
            let actual = run_aot(&module, &program, &data, &options);
            assert_eq!(actual.status, status::OK);
            assert_outcomes_match(&reference, &actual, bars, "fill modes");
            assert_eq!(reference.fills[0].colors, reference.fills[1].colors);
            let styles = actual.fixed_styles.as_ref().expect("engine style snapshot");
            assert_eq!(
                styles
                    .fills
                    .iter()
                    .map(|style| style.fill_color_mode)
                    .collect::<Vec<_>>(),
                modes
            );
            assert_eq!(
                styles.fills,
                reference
                    .fills
                    .iter()
                    .map(|fill| fill.style.clone())
                    .collect::<Vec<_>>()
            );
        }
    }
}

#[test]
fn omitted_plot_colors_are_blue_on_native_and_reference_paths() {
    let program = compile_csil("//@version=6\nindicator(\"Default plot colors\")\nplot(close,\"First\")\nplot(open,\"Second\")\nplot(high,\"Explicit missing\",color=na)\n");
    let module = compile_program(&program).expect("default colours remain native");
    for bars in [0, 1, 17, 1025] {
        let data = synthetic_data(bars);
        let options = RunOptions::default();
        let expected = run(&program, &data, &options);
        assert!(expected.errors.is_empty());
        let actual = run_aot(&module, &program, &data, &options);
        assert_eq!(actual.status, status::OK);
        assert_outcomes_match(&expected, &actual, bars, "default plot colours");
        assert_eq!(actual.colors.len(), 3);
        assert_eq!(
            actual.colors[0],
            vec![
                Value::Color {
                    r: 33,
                    g: 150,
                    b: 243,
                    t: 0.0,
                    input_origin: None
                };
                bars
            ]
        );
        assert_eq!(
            actual.colors[1],
            vec![
                Value::Color {
                    r: 33,
                    g: 150,
                    b: 243,
                    t: 0.0,
                    input_origin: None
                };
                bars
            ]
        );
        assert_eq!(actual.colors[2], vec![Value::Na; bars]);
        assert_eq!(
            actual.fixed_styles,
            Some(pinewasm_vm::NativeDisplayStyles {
                plots: expected
                    .plots
                    .iter()
                    .map(|plot| plot.style.clone())
                    .collect(),
                visuals: vec![],
                fills: vec![],
            })
        );
    }
}

#[test]
fn hosted_palette_snapshots_use_exact_input_colors_without_fixed_style_slots() {
    let program = compile_csil("//@version=6\nindicator(\"Native palettes\")\nc=input.color(color.teal)\np=plot(close,color=close > open ? c : color.red)\nq=plot(open,color=color.blue)\nbgcolor(c)\nfill(p,q,c)\n");
    let module = compile_program_to_wasm(&program).expect("palette identity remains executable");
    assert_eq!(
        module.layout.execution_mode,
        ExecutionMode::InterpreterHosted
    );
    assert!(module.layout.style_slots.is_empty());
    for bars in [0, 1, 17, 1025] {
        for color in [
            InputValue::Color {
                r: 17,
                g: 51,
                b: 85,
                t: 50.0,
            },
            InputValue::Bool(false),
        ] {
            let data = synthetic_data(bars);
            let options = RunOptions {
                inputs: [(0, color)].into(),
                ..RunOptions::default()
            };
            let expected = run(&program, &data, &options);
            assert!(expected.errors.is_empty(), "{:?}", expected.errors);
            let actual = run_aot(&module, &program, &data, &options);
            assert_eq!(actual.status, status::OK);
            assert_outcomes_match(&expected, &actual, bars, "native input palettes");
            assert_eq!(
                actual.fixed_styles,
                Some(pinewasm_vm::NativeDisplayStyles {
                    plots: expected
                        .plots
                        .iter()
                        .map(|plot| plot.style.clone())
                        .collect(),
                    visuals: expected
                        .visuals
                        .iter()
                        .map(|visual| visual.style.clone())
                        .collect(),
                    fills: expected
                        .fills
                        .iter()
                        .map(|fill| fill.style.clone())
                        .collect(),
                }),
                "bars={bars}"
            );
        }
    }
}

struct AotRun {
    status: i32,
    err_bar: u32,
    err_pc: u32,
    /// One f64 lane per plot slot (NaN = na).
    plots: Vec<Vec<f64>>,
    /// Exact decomposed colors in flat order. Hosted results retain fractional
    /// transparency rather than being repacked into the narrower native ABI.
    colors: Vec<Vec<Value>>,
    /// `alertcondition` firings: (bar, const-pool message id or -1).
    alerts: Vec<(u32, i32)>,
    /// Host-recorded streams when the module imports `host.vm` (the full
    /// alert/log/drawing surface of the VM).
    vm_alerts: Option<Vec<pinewasm_vm::AlertEvent>>,
    vm_logs: Vec<pinewasm_vm::LogEntry>,
    vm_drawings: Option<pinewasm_vm::Drawings>,
    fixed_styles: Option<pinewasm_vm::NativeDisplayStyles>,
    /// The structured error behind a `HOST_ERROR` status.
    host_error: Option<pinewasm_vm::RuntimeError>,
    /// The reason behind an `ABI_MISMATCH` status: a dynamically-typed value
    /// escaped its static cell; the request must rerun on the interpreter.
    mismatch: Option<String>,
    /// A strategy's results: the host broker's drain (drained before the
    /// alerts, which it joins), or the interpreter's for a hosted module.
    strategy: Option<pinewasm_vm::StrategyOutcome>,
}

fn read_u64(mem: &[u8], off: u32) -> u64 {
    u64::from_le_bytes(mem[off as usize..off as usize + 8].try_into().unwrap())
}

/// The host state behind the wasmi imports — exactly what the JS loader
/// builds from the wasm-pkg `AotHost`.
struct HostState {
    vm: Option<pinewasm_vm::aot_host::VmHost>,
    scratch_cells: u32,
    error: Option<String>,
}

/// Resolve the input table + source lanes the way the loader does:
/// override-or-default with the interpreter's kind-matching rule (tests run
/// with defaults only, matching `RunOptions::default()`).
fn input_table_bytes(value: &InputValue) -> [u8; 8] {
    match value {
        InputValue::Int(v) => v.to_le_bytes(),
        InputValue::Float(v) => v.to_le_bytes(),
        InputValue::Bool(b) => {
            let value: f64 = if *b { 1.0 } else { 0.0 };
            value.to_le_bytes()
        }
        InputValue::Enum { index, .. } => i64::from(*index).to_le_bytes(),
        // Source/string/color values never land in the numeric table.
        _ => f64::NAN.to_le_bytes(),
    }
}

fn source_series_values(
    data: &MarketData,
    series: pinewasm_hir::BuiltinSeries,
    bars: usize,
) -> Vec<f64> {
    use pinewasm_hir::BuiltinSeries as B;
    let norm = |v: f64| if v.is_finite() { v } else { f64::NAN };
    (0..bars)
        .map(|i| {
            let (o, h, l, c) = (data.open[i], data.high[i], data.low[i], data.close[i]);
            norm(match series {
                B::Open => o,
                B::High => h,
                B::Low => l,
                B::Close => c,
                B::Volume => data.volume[i],
                B::Hl2 => h.midpoint(l),
                B::Hlc3 => (h + l + c) / 3.0,
                B::Ohlc4 => (o + h + l + c) / 4.0,
                B::Hlcc4 => (h + l + c + c) / 4.0,
                other => panic!("unsupported source input series {other:?}"),
            })
        })
        .collect()
}

#[test]
fn emitted_prepare_initializes_the_reference_per_bar_budget() {
    let program = compile_csil("indicator(\"Budget default\")\nplot(close)\n");
    let aot = compile_program(&program).expect("compile native AOT module");
    assert!(!aot.layout.imports_vm, "fixture must remain importless");

    let engine = wasmi::Engine::default();
    let module = wasmi::Module::new(&engine, &aot.wasm[..]).expect("wasmi accepts the module");
    let mut store = wasmi::Store::new(&engine, ());
    let linker = wasmi::Linker::<()>::new(&engine);
    let instance = linker
        .instantiate_and_start(&mut store, &module)
        .expect("instantiate importless module");
    let prepare = instance
        .get_typed_func::<i32, i32>(&store, "prepare")
        .expect("prepare export");
    assert_eq!(prepare.call(&mut store, 1).unwrap(), status::OK);

    let memory = instance
        .get_memory(&store, "memory")
        .expect("memory export");
    assert_eq!(
        read_u64(memory.data(&store), header::OFF_BUDGET_PER_BAR),
        RunOptions::default().max_instructions_per_bar
    );
}

#[test]
fn emitted_prepare_rejects_layout_that_would_wrap_plot_lanes() {
    let safe_program = compile_csil("indicator(\"Layout guard control\")\nplot(close)\n");
    let safe_aot = compile_program(&safe_program).expect("compile safe program");
    let engine = wasmi::Engine::default();
    let module = wasmi::Module::new(&engine, &safe_aot.wasm[..]).expect("wasmi accepts the module");
    let mut store = wasmi::Store::new(&engine, ());
    let instance = wasmi::Linker::<()>::new(&engine)
        .instantiate_and_start(&mut store, &module)
        .expect("instantiate safe module");
    let prepare = instance
        .get_typed_func::<i32, i32>(&store, "prepare")
        .expect("prepare export");
    assert_eq!(prepare.call(&mut store, 100).unwrap(), status::OK);
    let run = instance
        .get_typed_func::<(), i32>(&store, "run")
        .expect("run export");
    assert_eq!(run.call(&mut store, ()).unwrap(), status::OK);
    assert_eq!(prepare.call(&mut store, -1).unwrap(), status::TOO_MANY_BARS);

    // This is the byte-level check. Without the guard, the fixed 16,000,000
    // limit would admit four million bars and emit a wrapped plot base; a
    // Wasmi `run` would then trap while preinitializing a plot lane. Every plot remains distinct by its constant offset. The
    // semantics checker allows at most 64 plot calls (`MAX_PLOT_COUNTS`), and
    // `hline` lanes do not count, so 64 plots and 71 hlines make the same
    // 135 lanes.
    let plots = (0..64)
        .map(|i| format!("plot(close + {i})\n"))
        .chain((64..135).map(|i| format!("hline({i})\n")))
        .collect::<String>();
    let source = format!("indicator(\"Layout wrap witness\")\n{plots}");
    let witness_program = compile_csil(&source);
    let witness_aot = compile_program(&witness_program).expect("compile wrap witness");
    assert_eq!(witness_aot.layout.plot_count, 135);
    let engine = wasmi::Engine::default();
    let module =
        wasmi::Module::new(&engine, &witness_aot.wasm[..]).expect("wasmi accepts wrap witness");
    let mut store = wasmi::Store::new(&engine, ());
    let instance = wasmi::Linker::<()>::new(&engine)
        .instantiate_and_start(&mut store, &module)
        .expect("instantiate wrap witness");
    let prepare = instance
        .get_typed_func::<i32, i32>(&store, "prepare")
        .expect("prepare export");
    assert_eq!(
        prepare.call(&mut store, 4_000_000).unwrap(),
        status::TOO_MANY_BARS,
        "reject the exact repro input before accepting its wrapped layout"
    );
}

#[test]
fn emitted_prepare_admits_exact_single_plot_memory_boundary_only() {
    let program = compile_csil("indicator(\"Layout capacity boundary\")\nplot(close)\n");
    let aot = compile_program(&program).expect("compile boundary fixture");
    let engine = wasmi::Engine::default();
    let module =
        wasmi::Module::new(&engine, &aot.wasm[..]).expect("wasmi accepts boundary fixture");
    let mut store = wasmi::Store::new(&engine, ());
    let instance = wasmi::Linker::<()>::new(&engine)
        .instantiate_and_start(&mut store, &module)
        .expect("instantiate boundary fixture");
    let prepare = instance
        .get_typed_func::<i32, i32>(&store, "prepare")
        .expect("prepare export");

    // For this exact fixture: 128 fixed bytes + 48 builtin bytes/bar +
    // 8 plot bytes/bar + 4 default-colour bytes/bar. The maximum is now
    // 4,473,922 bars: 268,435,448 bytes after the eight-byte alignment,
    // eight bytes below the unchanged 4096-page (256 MiB) memory ceiling.
    // The next bar must be rejected before i32 layout math.
    assert_eq!(prepare.call(&mut store, 4_473_922).unwrap(), status::OK);
    let memory = instance
        .get_memory(&store, "memory")
        .expect("memory export");
    assert_eq!(memory.data(&store).len(), 4096 * 65_536);
    assert_eq!(
        read_u32(memory.data(&store), header::OFF_HEAP_END),
        4096 * 65_536 - 8
    );
    assert_eq!(
        prepare.call(&mut store, 4_473_923).unwrap(),
        status::TOO_MANY_BARS
    );
}

/// Instantiate and drive an AOT module the way the JS loader will.
fn run_aot(
    aot: &AotModule,
    program: &pinewasm_csil::CsilProgram,
    data: &MarketData,
    options: &RunOptions,
) -> AotRun {
    if aot.layout.imports_interpreter {
        return run_hosted_artifact(aot, program, data, options);
    }
    let vm = fresh_vm_host(aot, program, data, options);
    run_aot_with_host(aot, program, data, options, vm).0
}

/// The `host.vm` backing a native module needs, built as the loader builds
/// it; `None` for a module without `host.vm` sites.
fn fresh_vm_host(
    aot: &AotModule,
    program: &pinewasm_csil::CsilProgram,
    data: &MarketData,
    options: &RunOptions,
) -> Option<pinewasm_vm::aot_host::VmHost> {
    let bars_for_host = options.max_bars.map_or(data.bars(), |m| m.min(data.bars()));
    let has_color_palette = program
        .plots
        .iter()
        .map(|plot| &plot.style)
        .chain(program.visuals.iter().map(|visual| &visual.style))
        .chain(program.fills.iter().map(|fill| &fill.style))
        .any(|style| !style.color_palette.is_empty());
    (aot.layout.imports_vm || !aot.layout.style_slots.is_empty() || has_color_palette)
        .then(|| vm_host_for_bars(aot, program, data, options, bars_for_host))
}

/// A `host.vm` backing built for the module's first `bars` bars.
fn vm_host_for_bars(
    aot: &AotModule,
    program: &pinewasm_csil::CsilProgram,
    data: &MarketData,
    options: &RunOptions,
    bars: usize,
) -> pinewasm_vm::aot_host::VmHost {
    pinewasm_vm::aot_host::VmHost::new(
        program.clone(),
        data.clone(),
        options.clone(),
        bars,
        aot.layout
            .host_sites
            .iter()
            .map(|s| pinewasm_vm::aot_host::SiteSpec {
                pc: s.pc,
                args: s.args.clone(),
                ret: s.ret,
            })
            .collect(),
    )
}

/// Run a native module against `vm` (fresh, or reset after an earlier run)
/// and hand the host back once its streams are drained.
fn run_aot_with_host(
    aot: &AotModule,
    program: &pinewasm_csil::CsilProgram,
    data: &MarketData,
    options: &RunOptions,
    vm: Option<pinewasm_vm::aot_host::VmHost>,
) -> (AotRun, Option<pinewasm_vm::aot_host::VmHost>) {
    let engine = wasmi::Engine::default();
    let module = wasmi::Module::new(&engine, &aot.wasm[..]).expect("wasmi accepts the module");
    let has_color_palette = program
        .plots
        .iter()
        .map(|plot| &plot.style)
        .chain(program.visuals.iter().map(|visual| &visual.style))
        .chain(program.fills.iter().map(|fill| &fill.style))
        .any(|style| !style.color_palette.is_empty());
    let host = HostState {
        vm,
        scratch_cells: aot.layout.scratch_cells,
        error: None,
    };
    let mut store = wasmi::Store::new(&engine, host);
    let mut linker = <wasmi::Linker<HostState>>::new(&engine);
    // No `host.ta`, `host.pure` or `host.alert` entries, deliberately. The ta
    // engine and the transcendentals are linked into the module and fired
    // alertconditions land in an output lane, so a module still asking for any
    // of them fails instantiation here rather than quietly getting served.
    // That is the acceptance test for these 45 differential cases
    // cover the ta.* functions bit-exactly, and they run against a module with
    // no engine import at all.
    linker
        .func_wrap(
            "host",
            "vm",
            |mut caller: wasmi::Caller<'_, HostState>, site: i32, bar: i32| -> i32 {
                let memory = caller
                    .get_export("memory")
                    .and_then(wasmi::Extern::into_memory)
                    .expect("exported memory");
                let cells_len = caller.data().scratch_cells as usize;
                let scratch = {
                    let mem = memory.data(&caller);
                    read_u32(mem, header::OFF_SCRATCH_BASE) as usize
                };
                let mut cells = vec![0i64; cells_len];
                {
                    let mem = memory.data(&caller);
                    for (i, cell) in cells.iter_mut().enumerate() {
                        *cell = i64::from_le_bytes(
                            mem[scratch + i * 8..scratch + i * 8 + 8]
                                .try_into()
                                .unwrap(),
                        );
                    }
                }
                let status = caller
                    .data_mut()
                    .vm
                    .as_mut()
                    .expect("host.vm import without a VmHost")
                    .vm_call(site as u32, bar as u32, &mut cells);
                let mem = memory.data_mut(&mut caller);
                for (i, cell) in cells.iter().enumerate() {
                    mem[scratch + i * 8..scratch + i * 8 + 8].copy_from_slice(&cell.to_le_bytes());
                }
                status
            },
        )
        .expect("define host.vm");

    let instance = linker
        .instantiate_and_start(&mut store, &module)
        .expect("instantiate");

    let bars = options.max_bars.map_or(data.bars(), |m| m.min(data.bars()));
    let prepare = instance
        .get_typed_func::<i32, i32>(&store, "prepare")
        .expect("prepare export");
    assert_eq!(prepare.call(&mut store, bars as i32).unwrap(), status::OK);

    let memory = instance
        .get_memory(&store, "memory")
        .expect("memory export");
    {
        let mem = memory.data_mut(&mut store);
        assert_eq!(read_u32(mem, header::OFF_MAGIC), header::MAGIC);
        assert_eq!(read_u32(mem, header::OFF_ABI_VERSION), header::ABI_VERSION);
        let builtin_base = read_u32(mem, header::OFF_BUILTIN_BASE) as usize;
        // Builtin lanes, non-finite normalized to NaN (from_f64 parity).
        let norm = |v: f64| if v.is_finite() { v } else { f64::NAN };
        let lanes: [&[f64]; 5] = [&data.open, &data.high, &data.low, &data.close, &data.volume];
        for (lane_idx, values) in lanes.iter().enumerate() {
            let base = builtin_base + lane_idx * 8 * bars;
            for (i, v) in values.iter().take(bars).enumerate() {
                mem[base + i * 8..base + i * 8 + 8].copy_from_slice(&norm(*v).to_le_bytes());
            }
        }
        // Time lane: supplied, or the interpreter's synthetic daily axis.
        let time_base = builtin_base + 5 * 8 * bars;
        for i in 0..bars {
            let t = if data.time.is_empty() {
                1_577_836_800_000i64 + (i as i64) * 86_400_000
            } else {
                data.time[i]
            } as f64;
            mem[time_base + i * 8..time_base + i * 8 + 8].copy_from_slice(&t.to_le_bytes());
        }
        // Input table (typed overrides or defaults) + source lanes.
        for (idx, decl) in program.inputs.iter().enumerate() {
            let off = header::SIZE as usize + idx * 8;
            mem[off..off + 8].copy_from_slice(&input_table_bytes(
                options
                    .inputs
                    .get(&decl.id)
                    .filter(|v| std::mem::discriminant(*v) == std::mem::discriminant(&decl.defval))
                    .unwrap_or(&decl.defval),
            ));
        }
        for (j, &input_id) in aot.layout.source_inputs.iter().enumerate() {
            let decl = program
                .inputs
                .iter()
                .find(|d| d.id == input_id)
                .expect("source input decl");
            let series = match &decl.defval {
                InputValue::Source(series) => *series,
                other => panic!("source input default {other:?}"),
            };
            let lane = source_series_values(data, series, bars);
            let base = builtin_base + (6 + j) * 8 * bars;
            for (i, v) in lane.iter().enumerate() {
                mem[base + i * 8..base + i * 8 + 8].copy_from_slice(&v.to_le_bytes());
            }
        }
        // Budget + mintick.
        let b = header::OFF_BUDGET_PER_BAR as usize;
        mem[b..b + 8].copy_from_slice(&options.max_instructions_per_bar.to_le_bytes());
        let scratch = read_u32(mem, header::OFF_SCRATCH_BASE) as usize
            + header::SCRATCH_OFF_BUDGET_PER_RUN as usize;
        mem[scratch..scratch + 8].copy_from_slice(&options.max_instructions_per_run.to_le_bytes());
        let m = header::OFF_MINTICK as usize;
        mem[m..m + 8].copy_from_slice(&data.mintick.to_le_bytes());
        // Native heap budgets (loader parity: element cap from RunOptions,
        // object cap at the VM default).
        let e = header::OFF_MAX_ELEMS as usize;
        mem[e..e + 8].copy_from_slice(&(options.max_collection_elements as u64).to_le_bytes());
        let o = header::OFF_MAX_OBJS as usize;
        mem[o..o + 8].copy_from_slice(&pinewasm_aot::heap_defaults::MAX_OBJECTS.to_le_bytes());
    }

    let run_fn = instance
        .get_typed_func::<(), i32>(&store, "run")
        .expect("run export");
    let code = run_fn.call(&mut store, ()).unwrap();
    if let Some(err) = &store.data().error {
        panic!("host.vm contract violation: {err}");
    }

    let mem = memory.data(&store);
    assert_eq!(
        read_u32(mem, header::OFF_ERR_CODE) as i32,
        code,
        "run return status and header status diverged: return={code}, header={}",
        read_u32(mem, header::OFF_ERR_CODE)
    );
    let plots_base = read_u32(mem, header::OFF_PLOTS_BASE) as usize;
    let plots = (0..aot.layout.plot_count as usize)
        .map(|p| {
            let base = plots_base + p * 8 * bars;
            (0..bars)
                .map(|i| {
                    f64::from_le_bytes(mem[base + i * 8..base + i * 8 + 8].try_into().unwrap())
                })
                .collect()
        })
        .collect();
    let colors_base = read_u32(mem, header::OFF_COLORS_BASE) as usize;
    let color_lane_count: u32 = aot.layout.plot_color_roles.iter().sum::<u32>()
        + aot.layout.visual_color_roles.iter().sum::<u32>()
        + aot.layout.fill_color_roles.iter().sum::<u32>();
    let colors = (0..color_lane_count as usize)
        .map(|c| {
            let base = colors_base + c * 4 * bars;
            (0..bars)
                .map(|i| {
                    let packed =
                        i32::from_le_bytes(mem[base + i * 4..base + i * 4 + 4].try_into().unwrap());
                    if packed == -1 {
                        Value::Na
                    } else {
                        let packed = packed as u32;
                        Value::Color {
                            r: (packed >> 24) as u8,
                            g: (packed >> 16) as u8,
                            b: (packed >> 8) as u8,
                            t: f64::from(packed as u8),
                            input_origin: None,
                        }
                    }
                })
                .collect()
        })
        .collect();
    // The alert output lane: `(bar, message_id)` pairs in execution order.
    let alerts_base = read_u32(mem, header::OFF_ALERTS_BASE) as usize;
    let alerts = (0..read_u32(mem, header::OFF_ALERT_COUNT) as usize)
        .map(|i| {
            let entry = alerts_base + i * header::ALERT_ENTRY as usize;
            (
                read_u32(mem, entry as u32),
                i32::from_le_bytes(mem[entry + 4..entry + 8].try_into().unwrap()),
            )
        })
        .collect();
    let err_bar = read_u32(mem, header::OFF_ERR_BAR);
    let err_pc = read_u32(mem, header::OFF_ERR_PC);
    let slots_base = read_u32(mem, header::OFF_SLOTS_BASE) as usize;
    let visible_bars = if code == status::OK {
        bars
    } else {
        err_bar as usize
    };
    let style_cells: Vec<i64> = aot
        .layout
        .style_slots
        .iter()
        .map(|slot| {
            let index = visible_bars.saturating_sub(1) as u32 & slot.mask;
            let at = slots_base + slot.offset as usize + index as usize * 8;
            if slot.tag >= 2 {
                i64::from(i32::from_le_bytes(mem[at..at + 4].try_into().unwrap()))
            } else {
                i64::from_le_bytes(mem[at..at + 8].try_into().unwrap())
            }
        })
        .collect();
    let fixed_styles = store
        .data()
        .vm
        .as_ref()
        .filter(|_| !aot.layout.style_slots.is_empty() || has_color_palette)
        .map(|vm| {
            vm.resolve_display_styles(
                &aot.layout
                    .style_slots
                    .iter()
                    .map(|slot| slot.slot)
                    .collect::<Vec<_>>(),
                &aot.layout
                    .style_slots
                    .iter()
                    .map(|slot| slot.tag)
                    .collect::<Vec<_>>(),
                &style_cells,
                visible_bars,
            )
            .expect("native fixed style snapshot")
        });
    // A run that stopped early stopped on its error bar.
    let bars_executed = if code == status::OK {
        bars as u32
    } else {
        err_bar
    };
    let (vm_alerts, vm_logs, vm_drawings, host_error, mismatch, strategy) = {
        let state = store.data_mut();
        match state.vm.as_mut().filter(|_| aot.layout.imports_vm) {
            Some(vm) => {
                // Before the alerts: the broker's fill alerts join them. A
                // refusal leaves none, as on an interpreter rerun's request.
                let strategy = vm.drain_strategy(code, bars_executed).ok().flatten();
                (
                    Some(vm.drain_alerts()),
                    vm.drain_logs(),
                    Some(vm.drain_drawings()),
                    vm.take_error(),
                    vm.abi_mismatch().map(str::to_string),
                    strategy,
                )
            }
            None => (None, Vec::new(), None, None, None, None),
        }
    };
    let run = AotRun {
        status: code,
        err_bar,
        err_pc,
        plots,
        colors,
        alerts,
        vm_alerts,
        vm_logs,
        vm_drawings,
        fixed_styles,
        host_error,
        mismatch,
        strategy,
    };
    (run, store.into_data().vm)
}

fn run_hosted_artifact(
    aot: &AotModule,
    program: &pinewasm_csil::CsilProgram,
    data: &MarketData,
    options: &RunOptions,
) -> AotRun {
    struct InterpreterHost {
        program: pinewasm_csil::CsilProgram,
        data: MarketData,
        options: RunOptions,
        result: Option<AotRun>,
        calls: usize,
    }
    let engine = wasmi::Engine::default();
    let module = wasmi::Module::new(&engine, &aot.wasm[..]).expect("actual hosted module");
    let mut store = wasmi::Store::new(
        &engine,
        InterpreterHost {
            program: program.clone(),
            data: data.clone(),
            options: options.clone(),
            result: None,
            calls: 0,
        },
    );
    let mut linker = wasmi::Linker::new(&engine);
    linker
        .func_wrap(
            "host",
            "interpret",
            |mut caller: wasmi::Caller<'_, InterpreterHost>| -> i32 {
                let host = caller.data_mut();
                host.calls += 1;
                let result =
                    run_reference_vm_for_hosted_aot(&host.program, &host.data, &host.options);
                let status = result.status;
                host.result = Some(result);
                status
            },
        )
        .unwrap();
    let instance = linker.instantiate_and_start(&mut store, &module).unwrap();
    let prepare = instance
        .get_typed_func::<i32, i32>(&store, "prepare")
        .unwrap();
    let bars = options
        .max_bars
        .map_or(data.bars(), |max| max.min(data.bars()));
    assert_eq!(
        prepare
            .call(&mut store, i32::try_from(bars).unwrap())
            .unwrap(),
        status::OK
    );
    let run = instance.get_typed_func::<(), i32>(&store, "run").unwrap();
    let returned = run.call(&mut store, ()).unwrap();
    assert_eq!(
        store.data().calls,
        1,
        "each hosted run invokes the interpreter exactly once"
    );
    let result = store
        .data_mut()
        .result
        .take()
        .expect("Wasm delegated the run");
    assert_eq!(
        returned, result.status,
        "host status crosses the actual Wasm entry point"
    );
    result
}

fn run_reference_vm_for_hosted_aot(
    program: &pinewasm_csil::CsilProgram,
    data: &MarketData,
    options: &RunOptions,
) -> AotRun {
    let outcome = run(program, data, options);
    let pack_colors = |series: &[pinewasm_vm::PlotColorSeries]| {
        series
            .iter()
            .map(|role| role.values.clone())
            .collect::<Vec<_>>()
    };
    let plots = outcome
        .plots
        .iter()
        .map(|plot| {
            plot.values
                .iter()
                .map(|value| match value {
                    Value::Na => f64::NAN,
                    Value::Float(value) => *value,
                    Value::Int(value) => *value as f64,
                    other => panic!("unexpected hosted plot value {other:?}"),
                })
                .collect()
        })
        .collect();
    let mut colors = Vec::new();
    for plot in &outcome.plots {
        colors.extend(pack_colors(&plot.colors));
    }
    for visual in &outcome.visuals {
        colors.extend(pack_colors(&visual.colors));
    }
    for fill in &outcome.fills {
        colors.extend(pack_colors(&fill.colors));
    }
    let error = outcome.errors.first().cloned();
    let status = error.as_ref().map_or(status::OK, |error| match error.kind {
        pinewasm_vm::RuntimeErrorKind::InstructionLimitExceeded => status::BUDGET_EXCEEDED,
        pinewasm_vm::RuntimeErrorKind::RunInstructionLimitExceeded => status::RUN_BUDGET_EXCEEDED,
        pinewasm_vm::RuntimeErrorKind::HistoryBufferLimitExceeded => status::HISTORY_LIMIT,
        _ => status::HEAP_REROUTE,
    });
    AotRun {
        status,
        err_bar: error
            .as_ref()
            .and_then(|error| error.bar_index)
            .unwrap_or_default(),
        err_pc: error
            .as_ref()
            .and_then(|error| error.instruction_offset)
            .unwrap_or_default(),
        plots,
        colors,
        alerts: Vec::new(),
        vm_alerts: Some(outcome.alerts),
        vm_logs: outcome.logs,
        fixed_styles: Some(pinewasm_vm::NativeDisplayStyles {
            plots: outcome
                .plots
                .iter()
                .map(|plot| plot.style.clone())
                .collect(),
            visuals: outcome
                .visuals
                .iter()
                .map(|visual| visual.style.clone())
                .collect(),
            fills: outcome
                .fills
                .iter()
                .map(|fill| fill.style.clone())
                .collect(),
        }),
        vm_drawings: Some(outcome.drawings),
        host_error: error,
        mismatch: None,
        strategy: outcome.strategy,
    }
}

/// Bit-level comparison of every plot lane, plus wire-format comparison of
/// every color channel.
fn assert_outcomes_match(
    outcome: &pinewasm_vm::RunOutcome,
    aot: &AotRun,
    bars: usize,
    context: &str,
) {
    assert_eq!(
        outcome.plots.len(),
        aot.plots.len(),
        "{context}: plot count"
    );
    for (plot, lane) in outcome.plots.iter().zip(&aot.plots) {
        assert_eq!(plot.values.len(), bars, "{context}: bars");
        for (bar, (expected, got)) in plot.values.iter().zip(lane).enumerate() {
            match expected {
                Value::Na => assert!(
                    got.is_nan(),
                    "{context}: plot {} bar {bar}: interpreter na, AOT {got}",
                    plot.slot
                ),
                Value::Float(x) => assert!(
                    x.to_bits() == got.to_bits(),
                    "{context}: plot {} bar {bar}: interpreter {x} ({:016x}), AOT {got} ({:016x})",
                    plot.slot,
                    x.to_bits(),
                    got.to_bits()
                ),
                other => panic!(
                    "{context}: plot {} bar {bar}: unexpected interpreter value {other:?}",
                    plot.slot
                ),
            }
        }
    }
    // Colors: flat lane order is plot roles, then visual roles, then fill
    // roles.
    let mut lane_iter = aot.colors.iter();
    let mut check_colors = |series: &[pinewasm_vm::PlotColorSeries], what: &str| {
        for role in series {
            let lane = lane_iter
                .next()
                .unwrap_or_else(|| panic!("{context}: missing {what} lane"));
            for (bar, expected) in role.values.iter().enumerate() {
                assert_eq!(&lane[bar], expected, "{context}: {what} bar {bar}");
            }
        }
    };
    for plot in &outcome.plots {
        check_colors(&plot.colors, "plot color");
    }
    for visual in &outcome.visuals {
        check_colors(&visual.colors, "visual color");
    }
    for fill in &outcome.fills {
        check_colors(&fill.colors, "fill color");
    }
    // Every lane the module wrote must have been claimed by a declaration:
    // an unread lane means the emitter and the reader disagree on the walk.
    assert!(
        lane_iter.next().is_none(),
        "{context}: unread color lane (emitter/reader lane order drift)"
    );
}

/// `alertcondition` stream parity: same firing bars, same messages.
fn assert_alerts_match(
    outcome: &pinewasm_vm::RunOutcome,
    aot: &AotRun,
    program: &pinewasm_csil::CsilProgram,
    context: &str,
) {
    // A module with `host.vm` records the complete alert stream host-side
    // (alert() and alertcondition, execution order, dedup included).
    if let Some(vm_alerts) = &aot.vm_alerts {
        assert_eq!(&outcome.alerts, vm_alerts, "{context}: alert stream");
        return;
    }
    assert_eq!(
        outcome.alerts.len(),
        aot.alerts.len(),
        "{context}: alert count"
    );
    for (expected, (bar, msg_id)) in outcome.alerts.iter().zip(&aot.alerts) {
        assert_eq!(expected.bar, *bar, "{context}: alert bar");
        assert_eq!(
            expected.freq, "alert.freq_once_per_bar_close",
            "{context}: alert freq"
        );
        let message = if *msg_id < 0 {
            String::new()
        } else {
            program.constants[*msg_id as usize].clone()
        };
        assert_eq!(expected.message, message, "{context}: alert message");
    }
}

fn differential_with(source: &str, data: &MarketData, context: &str) {
    differential_program(&compile_csil(source), data, context);
}

fn differential_with_outcome(
    source: &str,
    data: &MarketData,
    context: &str,
) -> pinewasm_vm::RunOutcome {
    differential_program(&compile_csil(source), data, context)
}

fn differential_program(
    program: &pinewasm_csil::CsilProgram,
    data: &MarketData,
    context: &str,
) -> pinewasm_vm::RunOutcome {
    let program = program.clone();
    let aot = compile_program_to_wasm(&program)
        .unwrap_or_else(|e| panic!("{context}: program must produce a valid Wasm tier: {e}"));
    wasmparser::validate(&aot.wasm).unwrap_or_else(|e| panic!("{context}: invalid module: {e}"));

    let options = RunOptions::default();
    let outcome = run(&program, data, &options);
    assert!(
        outcome.errors.is_empty(),
        "{context}: interpreter errors: {:?}",
        outcome.errors
    );
    let aot_run = run_aot(&aot, &program, data, &options);
    assert_eq!(aot_run.status, status::OK, "{context}");
    assert_outcomes_match(&outcome, &aot_run, data.bars(), context);
    assert_alerts_match(&outcome, &aot_run, &program, context);
    assert_host_streams_match(&outcome, &aot_run, context);
    assert_strategy_match(&outcome, &aot_run, context);
    outcome
}

#[test]
fn same_position_table_replacement_uses_interpreter_hosted_aot_and_returns_drawings() {
    let source = r#"//@version=6
indicator("Hosted table replacement")
var table first = table.new(position.top_left, 1, 1)
var table alias = first
var array<table> saved = array.new_table()
array.push(saved, first)
if barstate.isfirst
    table.cell(first, 0, 0, "old")
table replacement = table.new(position.top_left, 2, 1, force_overlay = true)
table.cell(replacement, 1, 0, "new")
plot(na(first) ? 1 : 0)
plot(na(alias) ? 1 : 0)
plot(na(array.get(saved, 0)) ? 1 : 0)
plot(array.size(table.all))
"#;
    let program = compile_csil(source);
    let module = compile_program_to_wasm(&program)
        .expect("valid programs get an interpreter-hosted Wasm tier");
    assert_eq!(
        module.layout.execution_mode,
        ExecutionMode::InterpreterHosted
    );
    assert!(module.layout.imports_interpreter);

    let data = synthetic_data(2);
    let options = RunOptions::default();
    let expected = run(&program, &data, &options);
    let actual = run_aot(&module, &program, &data, &options);
    assert_eq!(actual.status, status::OK);
    assert_outcomes_match(
        &expected,
        &actual,
        data.bars(),
        "table replacement fallback",
    );
    assert_host_streams_match(&expected, &actual, "table replacement fallback");
    assert_alerts_match(&expected, &actual, &program, "table replacement fallback");
    assert_strategy_match(&expected, &actual, "table replacement fallback");
    assert_eq!(expected.drawings.tables.len(), 1);
    assert_eq!(
        expected.drawings.tables[0].fields.get("columns"),
        Some(&Value::Int(2))
    );
    assert_eq!(expected.drawings.tables[0].cells.len(), 1);
    assert_eq!(
        expected.drawings.tables[0].cells[0].attrs.get("text"),
        Some(&Value::String("new".into()))
    );
}

#[test]
fn repeatable_single_site_table_constructor_runs_hosted_and_returns_latest_dto() {
    let source = r#"//@version=6
indicator("Repeated table site")
for i = 1 to 60
    table t = table.new(position.top_left, 1, 1)
    table.cell(t, 0, 0, str.tostring(i))
plot(array.size(table.all))
"#;
    let program = compile_csil(source);
    let module = compile_program_to_wasm(&program)
        .expect("repeatable table site gets an interpreter-hosted Wasm tier");
    assert_eq!(
        module.layout.execution_mode,
        ExecutionMode::InterpreterHosted
    );
    assert!(module.layout.imports_interpreter);

    let data = synthetic_data(1);
    let options = RunOptions::default();
    let expected = run(&program, &data, &options);
    let actual = run_aot(&module, &program, &data, &options);
    assert_eq!(actual.status, status::OK);
    assert_outcomes_match(&expected, &actual, data.bars(), "repeated table site");
    assert_host_streams_match(&expected, &actual, "repeated table site");
    assert_eq!(expected.drawings.tables.len(), 1);
    assert_eq!(expected.plots[0].values, vec![Value::Float(1.0)]);
    assert_eq!(expected.drawings.tables[0].cells.len(), 1);
    assert_eq!(
        expected.drawings.tables[0].cells[0].attrs.get("text"),
        Some(&Value::String("60".into()))
    );
}

/// The host-recorded streams a module with `host.vm` sites shares with the
/// interpreter: the log stream and the final drawings, ids and references
/// included. A module without the import has neither surface, so the
/// interpreter must have produced none.
fn assert_host_streams_match(outcome: &pinewasm_vm::RunOutcome, aot: &AotRun, context: &str) {
    match &aot.vm_drawings {
        Some(drawings) => {
            assert_eq!(&outcome.drawings, drawings, "{context}: drawings");
            assert_eq!(outcome.logs, aot.vm_logs, "{context}: log stream");
        }
        None => {
            assert!(
                outcome.drawings.is_empty(),
                "{context}: an importless module cannot carry the interpreter's drawings"
            );
            assert!(
                outcome.logs.is_empty(),
                "{context}: an importless module cannot carry the interpreter's logs"
            );
        }
    }
}

/// Strategy parity: the module's strategy results are the interpreter's,
/// field for field. `Debug` spells every float by its value, so equal text
/// is equal bits up to the NaN payload (and `-0.0` stays distinct).
fn assert_strategy_match(outcome: &pinewasm_vm::RunOutcome, aot: &AotRun, context: &str) {
    assert_eq!(
        format!("{:?}", outcome.strategy),
        format!("{:?}", aot.strategy),
        "{context}: strategy results"
    );
}

fn differential(source: &str, data: &MarketData) {
    differential_with(source, data, "differential");
}

/// The runtime guards the analysis placed on the sites whose `I64` result the
/// static facts could not clear of a present `i64::MIN`, by instruction.
fn int_min_guards(
    program: &pinewasm_csil::CsilProgram,
) -> std::collections::BTreeMap<usize, IntMinGuard> {
    analyze(program)
        .expect("the program lowers natively")
        .int_min_guards
}

/// A program with at least one such site: it is not refused, and it is not
/// sent to the interpreter-hosted tier. It lowers natively, and each
/// ambiguous site carries a guard.
fn guarded_native(program: &pinewasm_csil::CsilProgram, context: &str) -> AotModule {
    let aot = compile_program(program)
        .unwrap_or_else(|error| panic!("{context}: guarded sites lower natively: {error}"));
    assert!(
        !int_min_guards(program).is_empty(),
        "{context}: expected at least one guarded site"
    );
    let tiered = compile_program_to_wasm(program).expect("universal entry point");
    assert_eq!(
        tiered.layout.execution_mode,
        ExecutionMode::Native,
        "{context}"
    );
    assert_eq!(tiered.wasm, aot.wasm, "{context}: one native module");
    aot
}

/// A run on which the interpreter reaches a present `i64::MIN` at a guarded
/// site: the module must stop with `ABI_MISMATCH` at a guarded instruction on
/// `bar`, never continue with the minimum read back as `na`.
fn assert_present_min_stops(
    aot: &AotModule,
    program: &pinewasm_csil::CsilProgram,
    data: &MarketData,
    options: &RunOptions,
    bar: u32,
    context: &str,
) -> AotRun {
    let run = run_aot(aot, program, data, options);
    assert_eq!(run.status, status::ABI_MISMATCH, "{context}");
    assert_eq!(run.err_bar, bar, "{context}: failing bar");
    assert!(
        int_min_guards(program).contains_key(&(run.err_pc as usize)),
        "{context}: stopped at instruction {}, which carries no guard",
        run.err_pc
    );
    run
}

#[test]
fn chart_point_named_arguments_match_hosted_wasm_values_and_drawings() {
    let source =
        "//@version=6\nindicator(\"Point argument order\", overlay=true, max_polylines_count=50)\n\
                  p1 = chart.point.new(price=close, index=bar_index, time=time)\n\
                  p2 = chart.point.copy(id=p1)\n\
                  pMethod = p1.copy()\n\
                  p3 = chart.point.from_index(price=high, index=bar_index)\n\
                  p4 = chart.point.from_time(price=low, time=time)\n\
                  points = array.new<chart.point>()\n\
                  array.push(points, p2)\n\
                  array.push(points, pMethod)\n\
                  array.push(points, p3)\n\
                  array.push(points, p4)\n\
                  polyline.new(points=points, line_color=color.blue, line_width=2)\n\
                  plot(p1.time + p1.index + p1.price + p2.price)\n";
    differential_with(source, &synthetic_data(4), "chart.point named args");
}

/// A Pine expression of the parameter's type for the drawing evaluation-order
/// check below. Style tokens come from the member's own namespace.
fn drawing_order_check_value(callee: &str, name: &str, ty: &pinewasm_hir::Type) -> String {
    use pinewasm_hir::Type;
    let token = match name {
        "xloc" => Some("xloc.bar_index"),
        "yloc" => Some("yloc.price"),
        "extend" => Some("extend.none"),
        "style" if callee.starts_with("label.") => Some("label.style_label_down"),
        "style" | "border_style" | "line_style" => Some("line.style_solid"),
        "size" => Some("size.normal"),
        "textalign" | "text_halign" | "text_valign" => Some("text.align_center"),
        "text_wrap" => Some("text.wrap_none"),
        "text_font_family" => Some("font.family_default"),
        "text_formatting" => Some("text.format_none"),
        "text_size" if *ty == Type::String => Some("size.normal"),
        _ => None,
    };
    if let Some(token) = token {
        return token.to_string();
    }
    match ty {
        Type::Int => "bar_index".into(),
        Type::Float => "close".into(),
        Type::String => "\"ok\"".into(),
        Type::Color => "color.red".into(),
        Type::Bool => "false".into(),
        Type::Object(kind) => match kind.as_str() {
            "line" => "ln".into(),
            "label" => "lb".into(),
            "box" => "bx".into(),
            "polyline" => "pl".into(),
            "linefill" => "lf".into(),
            "chart.point" => "chart.point.from_index(bar_index, close)".into(),
            other => panic!("`{callee}` parameter `{name}`: no generated value for {other}"),
        },
        Type::Array(elem) if **elem == Type::Object("chart.point".into()) => "pts".into(),
        other => panic!("`{callee}` parameter `{name}`: no generated value for {other:?}"),
    }
}

#[test]
fn drawing_members_evaluate_reversed_named_arguments_in_parameter_order_in_vm_and_wasm() {
    // The test checks that PineWASM evaluates named arguments in parameter
    // order (pinewasm_vm's table.cell test pins 1234). For every
    // overload of every line, label, box, polyline and linefill member with
    // two or more expressions, each argument comes from a helper that pushes
    // its parameter position, and the call names every argument in REVERSE
    // parameter order. The interpreter and the module must plot 1, 2, ..., n. `force_overlay`
    // is left out: it accepts only a `const` argument.
    let data = synthetic_data(3);
    let mut probed = Vec::new();
    for spec in pinewasm_hir::drawing::drawing_function_surface() {
        let namespace = spec.name.split('.').next().unwrap_or_default();
        if !matches!(
            namespace,
            "line" | "label" | "box" | "polyline" | "linefill"
        ) {
            continue;
        }
        for (overload, params) in spec.overloads.iter().enumerate() {
            let params: Vec<_> = params
                .iter()
                .filter(|param| param.required_qualifier.is_none())
                .collect();
            if params.len() < 2 {
                continue;
            }
            let context = format!("{}#{overload}", spec.name);
            let mut src = String::from(
                "//@version=6\n\
                 indicator(\"Named drawing evaluation order check\", overlay = true)\n\
                 var int[] order = array.new_int()\n\
                 ln = line.new(bar_index, close, bar_index, close)\n\
                 ln2 = line.new(bar_index, close, bar_index, close)\n\
                 lb = label.new(bar_index, close)\n\
                 bx = box.new(bar_index, close, bar_index, close)\n\
                 pts = array.from(chart.point.from_index(bar_index, close), chart.point.from_index(bar_index, close))\n\
                 pl = polyline.new(pts)\n\
                 lf = linefill.new(ln, ln2, color.red)\n",
            );
            for (position, param) in params.iter().enumerate() {
                src.push_str(&format!(
                    "f_{}() =>\n    array.push(order, {})\n    {}\n",
                    param.name,
                    position + 1,
                    drawing_order_check_value(&spec.name, &param.name, &param.ty)
                ));
            }
            src.push_str("array.clear(order)\n");
            let reversed: Vec<String> = params
                .iter()
                .rev()
                .map(|param| format!("{0} = f_{0}()", param.name))
                .collect();
            src.push_str(&format!("{}({})\n", spec.name, reversed.join(", ")));
            for position in 0..params.len() {
                src.push_str(&format!(
                    "plot(array.size(order) > {position} ? array.get(order, {position}) : -1, \"Position {position}\")\n"
                ));
            }
            // The module's plots must equal the interpreter's, bit for bit.
            let outcome = differential_with_outcome(&src, &data, &context);
            let observed: Vec<&Value> = outcome
                .plots
                .iter()
                .map(|plot| plot.values.last().expect("bars ran"))
                .collect();
            let expected: Vec<Value> = (1..=params.len())
                .map(|position| Value::Float(position as f64))
                .collect();
            assert_eq!(
                observed,
                expected.iter().collect::<Vec<_>>(),
                "{context}: written-order evaluation"
            );
            probed.push(context);
        }
    }
    for name in [
        "line.new#0",
        "line.new#1",
        "label.new#0",
        "label.new#1",
        "box.new#0",
        "box.new#1",
        "polyline.new#0",
        "linefill.new#0",
        "line.set_xy1#0",
        "label.set_xy#0",
        "box.set_lefttop#0",
        "linefill.set_color#0",
    ] {
        assert!(
            probed.iter().any(|probe| probe == name),
            "case `{name}` missing"
        );
    }
}

#[test]
fn for_in_over_matrix_and_map_and_enum_collections_match_interpreter() {
    // The test checks that for...in over a matrix visits rows in order and
    // over a map visits entries in insertion order, and that enum element and
    // key types are accepted. The interpreter's values are
    // pinned in pinewasm_vm (`for_in_over_matrix_visits_rows_in_order`,
    // `for_in_over_map_visits_entries_in_insertion_order`,
    // `enum_collections_store_and_return_enum_members`); the module must
    // reproduce them.
    let cases = [
        (
            "matrix for...in",
            "//@version=6\nindicator(\"F\")\n\
             m = matrix.new<float>(2, 2, 0.0)\n\
             m.set(0, 0, 1.0)\n\
             m.set(0, 1, 2.0)\n\
             m.set(1, 0, 3.0)\n\
             m.set(1, 1, 4.0)\n\
             firsts = 0.0\n\
             for row in m\n\
             \tfirsts := firsts * 10 + row.get(0)\n\
             weighted = 0.0\n\
             for [i, row] in m\n\
             \tweighted += (i + 1) * row.get(1)\n\
             plot(firsts * 100 + weighted)\n",
            1310.0,
        ),
        (
            "map for...in",
            "//@version=6\nindicator(\"F\")\n\
             m = map.new<string, float>()\n\
             m.put(\"b\", 2.0)\n\
             m.put(\"a\", 1.0)\n\
             m.put(\"c\", 3.0)\n\
             keys = \"\"\n\
             values = 0.0\n\
             for [key, value] in m\n\
             \tkeys += key\n\
             \tvalues := values * 10 + value\n\
             plot(values + (keys == \"bac\" ? 1000 : 0))\n",
            1213.0,
        ),
        (
            // Keys 2 then 1: values 2 then 1, plus 100 times each key.
            "int-keyed map for...in",
            "//@version=6\nindicator(\"F\")\n\
             m = map.new<int, float>()\n\
             m.put(2, 2.0)\n\
             m.put(1, 1.0)\n\
             values = 0.0\n\
             for [key, value] in m\n\
             \tvalues := values * 10 + value + key * 100\n\
             plot(values)\n",
            2121.0,
        ),
        (
            "enum collections",
            "//@version=6\nindicator(\"U\")\n\
             enum Side\n    buy\n    sell\n\
             a = array.new<Side>(2, Side.sell)\n\
             a.push(Side.buy)\n\
             m = matrix.new<Side>(1, 1, Side.buy)\n\
             byKey = map.new<Side, float>()\n\
             byKey.put(Side.sell, 3.0)\n\
             byValue = map.new<string, Side>()\n\
             byValue.put(\"x\", Side.sell)\n\
             map<Side, float> annotated = byKey\n\
             pairs = map.new<Side, Side>()\n\
             pairs.put(Side.buy, Side.sell)\n\
             matched = 0\n\
             for [key, value] in pairs\n\
             \tmatched += (key == Side.buy ? 1 : 0) + (value == Side.sell ? 1 : 0)\n\
             plot((a.get(0) == Side.sell and a.get(2) == Side.buy ? 1 : 0) + \
             (m.get(0, 0) == Side.buy ? 10 : 0) + annotated.get(Side.sell) * 100 + \
             (byValue.get(\"x\") == Side.sell ? 1000 : 0) + matched * 10000)\n",
            21311.0,
        ),
    ];
    for (context, source, expected) in cases {
        let outcome = differential_with_outcome(source, &synthetic_data(3), context);
        assert_eq!(
            outcome.plots[0].values,
            vec![Value::Float(expected); 3],
            "{context}"
        );
    }
}

#[test]
fn method_calls_and_chart_points_evaluate_named_arguments_in_parameter_order() {
    // The same rule for the forms the generated drawing checks do not write: a
    // method call (the receiver is parameter 1, `id`) with reversed named
    // arguments, and the chart.point constructors. Each helper pushes its
    // position; the plot reads the positions as digits, so parameter order
    // plots 12..n and written order would plot them reversed.
    let header = "//@version=6\nindicator(\"P\", overlay = true)\n\
                  var int[] order = array.new_int()\n\
                  ln = line.new(bar_index, close, bar_index, close)\n\
                  lb = label.new(bar_index, close)\n\
                  bx = box.new(bar_index, close, bar_index, close)\n\
                  fl(int k) =>\n    array.push(order, k)\n    ln\n\
                  flb(int k) =>\n    array.push(order, k)\n    lb\n\
                  fb(int k) =>\n    array.push(order, k)\n    bx\n\
                  fi(int k) =>\n    array.push(order, k)\n    bar_index\n\
                  ff(int k) =>\n    array.push(order, k)\n    close\n\
                  ft(int k) =>\n    array.push(order, k)\n    time\n\
                  array.clear(order)\n";
    let digits = "r = 0\nfor x in order\n    r := r * 10 + x\nplot(r)\n";
    for (call, expected) in [
        ("fl(1).set_xy1(y = ff(3), x = fi(2))", 123.0),
        ("flb(1).set_xy(y = ff(3), x = fi(2))", 123.0),
        ("fb(1).set_lefttop(top = ff(3), left = fi(2))", 123.0),
        ("fl(1).set_xy2(fi(2), y = ff(3))", 123.0),
        (
            "fl(1).set_first_point(point = chart.point.from_index(fi(2), ff(3)))",
            123.0,
        ),
        (
            "p = chart.point.new(price = ff(3), index = fi(2), time = ft(1))",
            123.0,
        ),
        (
            "p = chart.point.from_index(price = ff(2), index = fi(1))",
            12.0,
        ),
        (
            "p = chart.point.from_time(price = ff(2), time = ft(1))",
            12.0,
        ),
    ] {
        let source = format!("{header}{call}\n{digits}");
        let outcome = differential_with_outcome(&source, &synthetic_data(3), call);
        assert_eq!(
            outcome.plots[0].values,
            vec![Value::Float(expected); 3],
            "{call}"
        );
    }
}

#[test]
fn for_in_over_a_map_the_body_resizes_stops_in_vm_and_wasm() {
    // Adding or removing a map's entries inside a `for [key, value] in` loop
    // must stop the run with the same structured error in both engines.
    // pinewasm_vm pins the interpreter's error
    // (`for_in_over_map_stops_when_the_body_adds_or_removes_entries`); the
    // module must stop on the same bar with the same structured error.
    let source = "//@version=6\nindicator(\"F\")\n\
                  m = map.new<int, float>()\n\
                  m.put(1, 1.0)\n\
                  m.put(2, 2.0)\n\
                  for [k, v] in m\n\
                  \tm.put(k + 10, v)\n\
                  plot(close)\n";
    let program = compile_csil(source);
    let data = synthetic_data(3);
    let options = RunOptions::default();
    let outcome = run(&program, &data, &options);
    assert_eq!(outcome.errors.len(), 1, "{:?}", outcome.errors);
    assert_eq!(
        outcome.errors[0].kind,
        pinewasm_vm::RuntimeErrorKind::UserError
    );
    let aot = compile_program_to_wasm(&program).expect("module");
    let aot_run = run_aot(&aot, &program, &data, &options);
    assert_eq!(aot_run.err_bar, 0);
    let error = aot_run.host_error.expect("structured host error");
    assert_eq!(error.kind, pinewasm_vm::RuntimeErrorKind::UserError);
    assert_eq!(error.message, outcome.errors[0].message);
}

#[test]
fn for_in_over_a_matrix_fits_one_row_in_the_budget_in_vm_and_wasm() {
    // A 4 x 2 matrix (8 elements) walked by a body that keeps each row in
    // its iteration needs one row at a time: 8 + 2 = 10 elements. On one bar
    // the interpreter and the native module must both finish under a budget
    // of 10 with the hand-computed row sums 3 + 7 + 11 + 15 = 36. Under 9
    // the interpreter stops with `CollectionLimitExceeded` and the module
    // hands the request back to it (`HEAP_REROUTE`). A new row per iteration
    // needed 16, so the module rerouted and the interpreter stopped at 10.
    let source = "//@version=6\nindicator(\"F\")\n\
                  m = matrix.new<float>(4, 2, 0.0)\n\
                  for r = 0 to 3\n\
                  \tm.set(r, 0, 2 * r + 1)\n\
                  \tm.set(r, 1, 2 * r + 2)\n\
                  total = 0.0\n\
                  for row in m\n\
                  \ttotal += row.sum()\n\
                  plot(total)\n";
    let program = compile_csil(source);
    let data = synthetic_data(1);
    let aot = compile_program_to_wasm(&program).expect("module");
    for (limit, fits) in [(10, true), (9, false)] {
        let options = RunOptions {
            max_collection_elements: limit,
            ..RunOptions::default()
        };
        let outcome = run(&program, &data, &options);
        let aot_run = run_aot(&aot, &program, &data, &options);
        if fits {
            assert!(outcome.errors.is_empty(), "{limit}: {:?}", outcome.errors);
            assert_eq!(outcome.plots[0].values, vec![Value::Float(36.0)]);
            assert_eq!(aot_run.status, status::OK, "{limit}");
            assert_eq!(aot_run.plots[0], vec![36.0], "{limit}");
        } else {
            assert_eq!(
                outcome.errors.iter().map(|e| e.kind).collect::<Vec<_>>(),
                vec![pinewasm_vm::RuntimeErrorKind::CollectionLimitExceeded]
            );
            assert_eq!(aot_run.status, status::HEAP_REROUTE, "{limit}");
        }
    }
}

/// In the interpreter and the native module, a row saved on the first iteration through `escape` (with `row` and an
/// empty `spare` array) must still read the first row's value after the
/// loop. `array.concat` returns its receiver and `array.slice` is a view of
/// it, so a loop that reused one hidden row array for these bodies would
/// refill the saved array with the second row (2). A bare alias (`x = row`)
/// is the baseline. Expected 1 from rows [1] and [2], by hand.
fn aot_matrix_row_escape_source(escape: &str) -> String {
    // Only the baseline case declares the bare alias `x`.
    let alias = if escape == "x" { "\tx = row\n" } else { "" };
    format!(
        "//@version=6\nindicator(\"F\")\n\
         m = matrix.new<float>(2, 1, 0.0)\n\
         m.set(0, 0, 1.0)\n\
         m.set(1, 0, 2.0)\n\
         spare = array.new_float(0)\n\
         saved = array.new_float(0)\n\
         for [i, row] in m\n\
         {alias}\tif i == 0\n\
         \t\tsaved := {escape}\n\
         plot(saved.get(0))\n"
    )
}

#[test]
fn for_in_over_a_matrix_row_a_builtin_returns_or_views_matches_in_vm_and_wasm() {
    for escape in [
        "array.concat(row, spare)",
        "row.concat(spare)",
        "array.slice(row, 0, 1)",
        "row.slice(0, 1)",
        "array.copy(array.concat(row, spare))",
    ]
    .into_iter()
    .chain(["x"])
    {
        let outcome = differential_with_outcome(
            &aot_matrix_row_escape_source(escape),
            &synthetic_data(1),
            escape,
        );
        assert_eq!(outcome.plots[0].values, vec![Value::Float(1.0)], "{escape}");
    }
}

#[test]
fn for_in_over_a_map_holds_one_snapshot_against_the_budget_in_vm_and_wasm() {
    // A 4-entry map (4 elements) and the loop's key snapshot (4) fit a
    // budget of 8 on one bar in the interpreter and the native module, with
    // keys 10 + 20 + 30 + 40 and values 1 + 2 + 3 + 4: 110. Under 7 the
    // interpreter stops with `CollectionLimitExceeded` and the module hands
    // the request back to it. A second snapshot of the values needed 12.
    // Int, enum and string keys each pass through `map.get` natively.
    for (name, decl, key) in [
        ("int", "m = map.new<int, float>()", "i * 10"),
        (
            "enum",
            "enum Side\n    a\n    b\n    c\n    d\nm = map.new<Side, float>()",
            "i == 1 ? Side.a : i == 2 ? Side.b : i == 3 ? Side.c : Side.d",
        ),
        (
            "string",
            "m = map.new<string, float>()",
            "str.tostring(i * 10)",
        ),
    ] {
        let read_key = match name {
            "int" => "k",
            "enum" => "(k == Side.a ? 10 : k == Side.b ? 20 : k == Side.c ? 30 : 40)",
            _ => "str.tonumber(k)",
        };
        let source = format!(
            "//@version=6\nindicator(\"F\")\n{decl}\nfor i = 1 to 4\n\tm.put({key}, i)\n\
             total = 0.0\nfor [k, v] in m\n\ttotal += {read_key} + v\nplot(total)\n"
        );
        let program = compile_csil(&source);
        let data = synthetic_data(1);
        let aot = compile_program_to_wasm(&program).expect("module");
        for (limit, fits) in [(8, true), (7, false)] {
            let options = RunOptions {
                max_collection_elements: limit,
                ..RunOptions::default()
            };
            let outcome = run(&program, &data, &options);
            let aot_run = run_aot(&aot, &program, &data, &options);
            if fits {
                assert!(
                    outcome.errors.is_empty(),
                    "{name} {limit}: {:?}",
                    outcome.errors
                );
                assert_eq!(outcome.plots[0].values, vec![Value::Float(110.0)], "{name}");
                // A string-keyed map loop is not native yet: the module
                // returns `ABI_MISMATCH` and the loader reruns it on the
                // interpreter, whose result is checked above.
                if aot_run.status != status::ABI_MISMATCH || name != "string" {
                    assert_eq!(aot_run.status, status::OK, "{name} {limit}");
                    assert_eq!(aot_run.plots[0], vec![110.0], "{name} {limit}");
                }
            } else {
                assert_eq!(
                    outcome.errors.iter().map(|e| e.kind).collect::<Vec<_>>(),
                    vec![pinewasm_vm::RuntimeErrorKind::CollectionLimitExceeded],
                    "{name}"
                );
                assert_ne!(aot_run.status, status::OK, "{name} {limit}");
            }
        }
    }
}

#[test]
fn for_in_evaluates_its_iterable_once_in_vm_and_wasm() {
    // The interpreter and the module run a user function or method in a
    // `for … in` iterable once (`array.size(log)` is 1 on bar 0) and visit
    // the right values: the plot is `total * 100 + array.size(log)`.
    for (name, body, total) in [
        // Array, single and pair bindings: 1 + 2 + 3 = 6.
        ("array", "total = 0.0\nfor x in fa()\n\ttotal += x", 6.0),
        (
            "array pair",
            "total = 0.0\nfor [i, x] in fa()\n\ttotal += x * (i + 1)",
            14.0,
        ),
        // Matrix rows [1] and [2], single and pair bindings.
        (
            "matrix",
            "total = 0.0\nfor row in fm()\n\ttotal += row.get(0)",
            3.0,
        ),
        (
            "matrix pair",
            "total = 0.0\nfor [i, row] in fm()\n\ttotal += row.get(0) * (i + 1)",
            5.0,
        ),
        // Map a = 1, b = 2.
        ("map", "total = 0.0\nfor [k, v] in fp()\n\ttotal += v", 3.0),
        // A user method on a UDT: items 4 and 5.
        (
            "udt method",
            "total = 0.0\nfor x in bag.take()\n\ttotal += x",
            9.0,
        ),
    ] {
        let source = "//@version=6\nindicator(\"F\")\n\
               type Bag\n    array<float> items\n\
               var log = array.new_float(0)\n\
               var q = array.from(1.0, 2.0, 3.0)\n\
               fa() =>\n    array.push(log, 1.0)\n    array.from(1.0, 2.0, 3.0)\n\
               fm() =>\n    array.push(log, 1.0)\n    m = matrix.new<float>(2, 1, 0.0)\n    m.set(0, 0, 1.0)\n    m.set(1, 0, 2.0)\n    m\n\
               fp() =>\n    array.push(log, 1.0)\n    p = map.new<string, float>()\n    p.put(\"a\", 1.0)\n    p.put(\"b\", 2.0)\n    p\n\
               method take(Bag b) =>\n    array.push(log, 1.0)\n    b.items\n\
               bag = Bag.new(array.from(4.0, 5.0))\n\
               LOOP\n\
               plot(total * 100 + array.size(log))\n".replace("LOOP\n", &format!("{body}\n"));
        let outcome = differential_with_outcome(&source, &synthetic_data(1), name);
        assert_eq!(
            outcome.plots[0].values,
            vec![Value::Float(total * 100.0 + 1.0)],
            "{name}"
        );
    }
}

#[test]
fn for_in_over_a_map_binds_snapshot_values_in_vm_and_wasm() {
    // The interpreter and the native module bind each value from the entry
    // snapshot: replacing a later key's value in the body is not seen by its
    // iteration (1 + 2 = 3, and the map holds 20 afterwards: + 2000), while
    // a loop over `m.keys()` that reads `m.get(k)` sees it (1 + 20 = 21).
    // Removing a later key stops both engines with the same error
    // (`for_in_over_a_map_the_body_resizes_stops_in_vm_and_wasm` adds one).
    for (source, expected) in [
        (
            "//@version=6\nindicator(\"F\")\n\
               m = map.new<string, float>()\n\
               m.put(\"a\", 1.0)\n\
               m.put(\"b\", 2.0)\n\
               total = 0.0\n\
               for [k, v] in m\n\
               \ttotal += v\n\
               \tif k == \"a\"\n\
               \t\tm.put(\"b\", 20.0)\n\
               plot(total + m.get(\"b\") * 100)\n",
            2003.0,
        ),
        (
            "//@version=6\nindicator(\"F\")\n\
               m = map.new<string, float>()\n\
               m.put(\"a\", 1.0)\n\
               m.put(\"b\", 2.0)\n\
               total = 0.0\n\
               for k in m.keys()\n\
               \ttotal += m.get(k)\n\
               \tif k == \"a\"\n\
               \t\tm.put(\"b\", 20.0)\n\
               plot(total)\n",
            21.0,
        ),
    ] {
        let outcome = differential_with_outcome(source, &synthetic_data(1), source);
        assert_eq!(
            outcome.plots[0].values,
            vec![Value::Float(expected)],
            "{source}"
        );
    }
    let remove = "//@version=6\nindicator(\"F\")\n\
                  m = map.new<string, float>()\n\
                  m.put(\"a\", 1.0)\n\
                  m.put(\"b\", 2.0)\n\
                  for [k, v] in m\n\
                  \tif k == \"a\"\n\
                  \t\tm.remove(\"b\")\n\
                  plot(close)\n";
    let program = compile_csil(remove);
    let data = synthetic_data(1);
    let options = RunOptions::default();
    let outcome = run(&program, &data, &options);
    assert_eq!(outcome.errors.len(), 1, "{:?}", outcome.errors);
    assert_eq!(
        outcome.errors[0].kind,
        pinewasm_vm::RuntimeErrorKind::UserError
    );
    let aot = compile_program_to_wasm(&program).expect("module");
    let aot_run = run_aot(&aot, &program, &data, &options);
    if aot_run.status != status::ABI_MISMATCH {
        let error = aot_run.host_error.expect("structured host error");
        assert_eq!(error.kind, pinewasm_vm::RuntimeErrorKind::UserError);
        assert_eq!(error.message, outcome.errors[0].message);
    }
}

#[test]
fn for_in_map_keys_feed_map_get_in_vm_and_wasm() {
    // `for [key, value] in m` binds keys from the entry snapshot; passing
    // that key back to `m.get` must keep the map's key type in the native
    // module (int, enum and string keys). Entries (1, 1.0) and (2, 2.0)
    // give (1 * 10 + 1) + (2 * 10 + 2) = 33.
    for (name, decl, put) in [
        (
            "int",
            "m = map.new<int, float>()",
            "m.put(1, 1.0)\nm.put(2, 2.0)\n",
        ),
        (
            "enum",
            "enum Side\n    buy\n    sell\nm = map.new<Side, float>()",
            "m.put(Side.buy, 1.0)\nm.put(Side.sell, 2.0)\n",
        ),
        (
            "string",
            "m = map.new<string, float>()",
            "m.put(\"a\", 1.0)\nm.put(\"b\", 2.0)\n",
        ),
    ] {
        let source = format!(
            "//@version=6\nindicator(\"F\")\n{decl}\n{put}s = 0.0\nfor [k, v] in m\n\ts += m.get(k) * 10 + v\nplot(s)\n"
        );
        let outcome = differential_with_outcome(&source, &synthetic_data(3), name);
        assert_eq!(
            outcome.plots[0].values,
            vec![Value::Float(33.0); 3],
            "{name}"
        );
    }
}

#[test]
fn deleting_na_drawings_is_a_no_op_in_vm_and_wasm() {
    // The test checks that deleting an na drawing finishes without error and
    // leaves the surviving drawings equal in both engines.
    let source = "//@version=6\nindicator(\"na delete\", overlay=true)\n\
                  l = line.new(bar_index, high, bar_index[10], low[10], width = 1)\n\
                  line.delete(l[1])\n\
                  lb = label.new(bar_index, high, \"x\")\n\
                  label.delete(lb[1])\n\
                  var box vb = na\n\
                  box.delete(vb)\n\
                  var polyline vp = na\n\
                  polyline.delete(vp)\n\
                  var linefill vf = na\n\
                  linefill.delete(vf)\n\
                  var table vt = na\n\
                  table.delete(vt)\n\
                  plot(close)\n";
    let data = synthetic_data(6);
    let program = compile_csil(source);
    let outcome = differential_program(&program, &data, "na delete");
    assert_eq!(outcome.drawings.lines.len(), 1);
    assert_eq!(outcome.drawings.labels.len(), 1);
    let aot = compile_program_to_wasm(&program).expect("wasm tier");
    let aot_run = run_aot(&aot, &program, &data, &RunOptions::default());
    if let Some(drawings) = &aot_run.vm_drawings {
        assert_eq!(&outcome.drawings, drawings, "na delete: drawings");
    }
}

#[test]
fn lazy_ternary_arms_match_interpreter() {
    // Non-literal ternary arms lower through a temp series slot and
    // a join block; the guarded out-of-bounds arm must not run in either
    // engine, and live outer operands must survive the branch.
    let source = "//@version=6\nindicator(\"lazy ternary\")\n\
                  a = array.from(1.0, 2.0, 3.0)\n\
                  idx = bar_index % 5\n\
                  g = idx < array.size(a) ? array.get(a, idx) : na\n\
                  x = close + 1.0\n\
                  plot(g)\n\
                  plot(x * 2.0 + (close > open ? (x > 50.0 ? close - x : x) : open * 0.5))\n";
    differential_with(source, &synthetic_data(32), "lazy ternary");
}

/// Compile a script that keeps int quotients through the universal entry
/// point, require the native chart tier, and compare the module with the
/// interpreter on every plot. Whether the typed pass lowers each quotient
/// itself or the fraction carry runs it as float code is not asserted here:
/// that boundary moves as the typed pass learns shapes.
fn fractional_int_vm_and_native_wasm(source: &str, data: &MarketData) -> pinewasm_vm::RunOutcome {
    let program = compile_csil(source);
    pinewasm_csil::validate(&program).expect("fractional int CSIL validates");
    let module = compile_program_to_wasm(&program).expect("the universal entry point");
    assert_eq!(module.layout.execution_mode, ExecutionMode::Native);
    assert!(!module.layout.imports_interpreter);
    assert_eq!(module.layout.native_blocker, None);
    wasmparser::validate(&module.wasm).expect("valid WebAssembly");
    let outcome = run(&program, data, &RunOptions::default());
    assert!(outcome.errors.is_empty(), "VM errors: {:?}", outcome.errors);
    let aot_run = run_aot(&module, &program, data, &RunOptions::default());
    assert_eq!(aot_run.status, status::OK, "{:?}", aot_run.host_error);
    assert_outcomes_match(&outcome, &aot_run, data.bars(), "fractional int");
    outcome
}

// ---- plot regressions ------------------------------------------------------

#[test]
fn plot_close_matches_interpreter() {
    differential("indicator(\"C\")\nplot(close)\n", &synthetic_data(64));
}

#[test]
fn sma14_matches_interpreter_synthetic() {
    differential(
        "indicator(\"SMA\")\nplot(ta.sma(close, 14))\n",
        &synthetic_data(300),
    );
}

#[test]
fn valuewhen_bool_and_color_match_interpreter() {
    let source = r#"//@version=6
indicator("valuewhen bool and color")
bool remembered_bool = ta.valuewhen(close > open, close > 2, 0)
color remembered_color = ta.valuewhen(close > open, close > 2 ? color.red : color.blue, 0)
plot(remembered_bool ? 1 : 0)
plot(close, color=remembered_color)
"#;
    let data = MarketData {
        open: vec![0.0, 4.0, 4.0, 3.0],
        high: vec![2.0, 5.0, 6.0, 4.0],
        low: vec![0.0, 2.0, 3.0, 1.0],
        close: vec![1.0, 3.0, 5.0, 2.0],
        volume: vec![1.0; 4],
        ..MarketData::default()
    };
    differential_with(source, &data, "ta.valuewhen bool/color source overloads");
}

#[test]
fn sma14_matches_interpreter_on_a_long_feed() {
    // Thousands of bars, not hundreds: enough accumulation for f64 rounding
    // to drift if the two engines ever evaluate the window differently.
    differential(
        "indicator(\"SMA\")\nplot(ta.sma(close, 14))\n",
        &synthetic_data(6000),
    );
}

#[test]
fn float_expression_matches_interpreter() {
    differential(
        "indicator(\"X\")\nplot((close - open) / 2.0 + ta.sma(close, 5) * 1.5)\nplot(close[3] - close)\n",
        &synthetic_data(128),
    );
}

#[test]
fn int_division_keeps_its_fraction_in_vm_and_native_wasm() {
    // The test checks that `int / int` keeps its fraction for every qualifier;
    // `int()` truncates toward zero and a zero divisor is na. Every quotient
    // here is widened, rounded or cast at once, so no cell has to hold either
    // an int or a float and the program lowers natively.
    let program = compile_csil(
        "indicator(\"DIV\")\nn = input.int(9)\nplot(n * 3 / 4)\nplot(math.round(n * 3 / 4))\nplot(bar_index / 4)\nplot(int(-bar_index / 4))\nplot(1 / (bar_index % 3))\n",
    );
    let artifact = compile_program_to_wasm(&program).unwrap();
    assert_eq!(artifact.layout.execution_mode, ExecutionMode::Native);
    let outcome = differential_program(&program, &synthetic_data(64), "fractional int division");
    assert_eq!(outcome.plots[0].values[0], Value::Float(6.75));
    assert_eq!(outcome.plots[1].values[0], Value::Float(7.0));
    assert_eq!(outcome.plots[2].values[1], Value::Float(0.25));
    assert_eq!(outcome.plots[3].values[5], Value::Float(-1.0));
    assert_eq!(outcome.plots[3].values[3], Value::Float(0.0));
    assert_eq!(outcome.plots[4].values[2], Value::Float(0.5));
}

#[test]
fn fractional_int_division_agrees_in_vm_and_native_wasm() {
    // The test checks that these forms compile and that both engines plot
    // 2.50, 3.50, 5.00, 2.00 and 30.00.
    let source = "indicator(\"DIV\")\nint initialized = 5 / 2\nf(int a) =>\n    int reassigned = a\n    reassigned /= 2\n    reassigned\na = array.from(10, 20, 30, 40)\nplot(initialized)\nplot(f(5))\nplot(initialized + 1)\nplot(initialized * 2)\nplot(int(initialized))\nplot(array.get(a, initialized))\n";
    let outcome = fractional_int_vm_and_native_wasm(source, &synthetic_data(8));
    for (plot, expected) in [(0, 2.5), (1, 2.5), (2, 3.5), (3, 5.0), (4, 2.0), (5, 30.0)] {
        assert_eq!(outcome.plots[plot].values[0], Value::Float(expected));
    }
}

#[test]
fn legacy_int_div_opcode_truncates_in_both_engines() {
    // The compiler never emits INT_DIV; the opcode is still decoded so that
    // serialized programs keep working. Pin it by swapping each compiled INT_REM
    // for INT_DIV: it truncates toward zero, a zero divisor is na, and the
    // standalone module agrees with the interpreter.
    let mut program = compile_csil(
        "indicator(\"ID\")\nplot(bar_index % 4)\nplot((bar_index - 10) % 4)\nplot(bar_index % (bar_index - bar_index))\n",
    );
    let mut swapped = 0;
    for function in &mut program.functions {
        for instr in &mut function.code {
            if matches!(instr, pinewasm_csil::CsilInstr::IntRem) {
                *instr = pinewasm_csil::CsilInstr::IntDiv;
                swapped += 1;
            }
        }
    }
    assert_eq!(swapped, 3, "one INT_REM per plot");
    pinewasm_csil::validate(&program).expect("INT_DIV program must validate");
    let outcome = differential_program(&program, &synthetic_data(16), "INT_DIV opcode");
    assert_eq!(outcome.plots[0].values[7], Value::Float(1.0), "7 / 4");
    assert_eq!(
        outcome.plots[1].values[3],
        Value::Float(-1.0),
        "-7 / 4 truncates toward zero"
    );
    assert_eq!(outcome.plots[2].values[5], Value::Na, "5 / 0 is na");
}

#[test]
fn warmup_bars_are_na_in_both_engines() {
    let program = compile_csil("indicator(\"W\")\nplot(ta.sma(close, 14))\n");
    let data = synthetic_data(20);
    let aot = compile_program(&program).unwrap();
    let outcome = run(&program, &data, &RunOptions::default());
    let aot_run = run_aot(&aot, &program, &data, &RunOptions::default());
    for bar in 0..13 {
        assert_eq!(outcome.plots[0].values[bar], Value::Na);
        assert!(aot_run.plots[0][bar].is_nan());
    }
    assert!(matches!(outcome.plots[0].values[13], Value::Float(_)));
    assert!(!aot_run.plots[0][13].is_nan());
}

#[test]
fn budget_exhaustion_matches_interpreter() {
    let source = "indicator(\"B\")\nplot(ta.sma(close, 14))\n";
    let program = compile_csil(source);
    let data = synthetic_data(32);
    let options = RunOptions {
        max_instructions_per_bar: 10,
        ..RunOptions::default()
    };

    let outcome = run(&program, &data, &options);
    assert_eq!(outcome.errors.len(), 1, "interpreter must abort");
    assert_eq!(outcome.bars_executed, 0);

    let aot = compile_program(&program).unwrap();
    let aot_run = run_aot(&aot, &program, &data, &options);
    assert_eq!(aot_run.status, status::BUDGET_EXCEEDED);
    assert_eq!(aot_run.err_bar, 0);
}

#[test]
fn cumulative_budget_exhaustion_matches_native_instruction_and_bar() {
    let source = "indicator(\"B\")\nplot(close)\n";
    let program = compile_csil(source);
    let data = synthetic_data(2);
    let total = program.main().code.len() as u64 * data.bars() as u64;

    let exact = RunOptions {
        max_instructions_per_run: total,
        ..RunOptions::default()
    };
    assert!(run(&program, &data, &exact).errors.is_empty());
    let aot = compile_program(&program).unwrap();
    assert_eq!(run_aot(&aot, &program, &data, &exact).status, status::OK);

    let over = RunOptions {
        max_instructions_per_run: total - 1,
        ..RunOptions::default()
    };
    let outcome = run(&program, &data, &over);
    assert_eq!(
        outcome.errors[0].kind,
        pinewasm_vm::RuntimeErrorKind::RunInstructionLimitExceeded
    );
    let aot_run = run_aot(&aot, &program, &data, &over);
    assert_eq!(aot_run.status, status::RUN_BUDGET_EXCEEDED);
    assert_eq!(aot_run.err_bar, outcome.errors[0].bar_index.unwrap());
    assert_eq!(
        aot_run.err_pc,
        outcome.errors[0].instruction_offset.unwrap()
    );
}

#[test]
fn host_vm_cumulative_budget_uses_explicit_interpreter_reroute() {
    let source = "indicator(\"S\")\ns = str.repeat(\"x\", 8)\nplot(str.length(s))\n";
    let program = compile_csil(source);
    let data = synthetic_data(2);
    let options = RunOptions {
        max_instructions_per_run: 100,
        ..RunOptions::default()
    };
    let aot = compile_program(&program).expect("string calls use host.vm");
    assert!(aot.layout.imports_vm);
    let aot_run = run_aot(&aot, &program, &data, &options);
    assert_eq!(aot_run.status, status::RUN_BUDGET_REROUTE);
    assert!(
        aot_run.host_error.is_none(),
        "reroute happens before effects"
    );
    let interpreter = run(&program, &data, &options);
    assert!(interpreter.errors.is_empty(), "fallback succeeds exactly");
}

/// During `host.vm`, scratch cell 0 carries the remaining per-bar **u64**
/// budget. A per-bar budget of `2^63` or more therefore reaches the host as a
/// negative `i64` cell; the host must read it back as the same u64.
#[test]
fn host_vm_full_u64_budget_matches_interpreter() {
    let source = "indicator(\"S\")\ns = str.repeat(\"x\", 8)\nplot(str.length(s))\n";
    let program = compile_csil(source);
    let data = synthetic_data(3);
    let aot = compile_program(&program).expect("string calls use host.vm");
    assert!(aot.layout.imports_vm);
    // `u64::MAX` itself would reroute (the run counter must cover the
    // per-bar one), so the top case leaves room for the run.
    for per_bar in [1_u64 << 63, u64::MAX - 1_000_000] {
        let options = RunOptions {
            max_instructions_per_bar: per_bar,
            max_instructions_per_run: u64::MAX,
            ..RunOptions::default()
        };
        let outcome = run(&program, &data, &options);
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        let aot_run = run_aot(&aot, &program, &data, &options);
        assert_eq!(aot_run.status, status::OK, "per_bar={per_bar}");
        assert_outcomes_match(&outcome, &aot_run, data.bars(), "u64 host budget");
    }
}

/// `str.repeat` has no native lowering: a compiled module reaches it only
/// through `host.vm`, which runs the VM's own implementation. A
/// repeat of nothing must therefore answer at once there too, whatever the
/// count, and a standalone module (which may import nothing) refuses the
/// script rather than carrying a copy of the loop.
#[test]
fn string_repeat_of_nothing_is_constant_time_through_host_vm() {
    let source = "indicator(\"S\")\n\
                  s = str.repeat(\"\", 9223372036854775807)\n\
                  plot(str.length(s))\n";
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let program = compile_csil(source);
        let data = synthetic_data(2);
        let options = RunOptions::default();
        let aot = compile_program(&program).expect("string calls use host.vm");
        assert!(aot.layout.imports_vm);
        let aot_run = run_aot(&aot, &program, &data, &options);
        let standalone = compile_standalone_wasm_program(
            &program,
            &StandaloneWasmOptions {
                signal_plot: None,
                module_type: StandaloneWasmModuleType::Indicator,
            },
        );
        let _ = sender.send((aot_run.status, aot_run.plots, standalone.is_err()));
    });
    // A wall-clock bound so a regression to a per-repeat loop fails this test
    // instead of hanging the suite.
    let (run_status, plots, standalone_refused) = receiver
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("host.vm must not loop once per requested repeat");
    assert_eq!(run_status, status::OK);
    assert_eq!(plots[0], vec![0.0, 0.0]);
    assert!(
        standalone_refused,
        "a standalone module cannot call host.vm"
    );
}

#[test]
fn string_repeat_work_budget_matches_interpreter_host_path() {
    let source = "indicator(\"S\")\ns = str.repeat(\"x\", 900000)\nplot(str.length(s))\n";
    let program = compile_csil(source);
    let data = synthetic_data(1);
    let options = RunOptions {
        max_instructions_per_bar: 9,
        ..RunOptions::default()
    };

    let outcome = run(&program, &data, &options);
    assert_eq!(
        outcome.errors[0].kind,
        pinewasm_vm::RuntimeErrorKind::InstructionLimitExceeded
    );

    let aot = compile_program(&program).expect("string calls use host.vm");
    let aot_run = run_aot(&aot, &program, &data, &options);
    assert_eq!(
        aot_run.host_error.expect("structured host error").kind,
        pinewasm_vm::RuntimeErrorKind::InstructionLimitExceeded
    );
}

#[test]
fn substring_boundaries_preserve_structured_errors_through_compiled_wasm() {
    for expression in [
        "str.substring(\"abc\", -1)",
        "str.substring(\"abc\", 4)",
        "str.substring(\"abc\", 2, 1)",
        "str.substring(\"abc\", 0, -1)",
    ] {
        let source = format!("//@version=6\nindicator(\"Substring\")\nif bar_index == 1\n    log.info({expression})\nplot(close)\n");
        let program = compile_csil(&source);
        let data = synthetic_data(3);
        let options = RunOptions::default();
        let vm = run(&program, &data, &options);
        let module = compile_program(&program).expect("substring uses host.vm");
        let compiled = run_aot(&module, &program, &data, &options);
        let error = compiled.host_error.expect("compiled substring must fail");
        assert_eq!(
            error.kind,
            pinewasm_vm::RuntimeErrorKind::StringIndexOutOfBounds
        );
        assert_eq!(error.bar_index, Some(1));
        assert_eq!(error.message, vm.errors[0].message);
    }
}

#[test]
fn cumulative_string_budget_persists_across_aot_host_calls() {
    let source = "indicator(\"S\")\ns = str.repeat(\"x\", 8)\nplot(str.length(s))\n";
    let program = compile_csil(source);
    let data = synthetic_data(2);
    let options = RunOptions {
        max_string_bytes_per_run: 12,
        ..RunOptions::default()
    };

    let outcome = run(&program, &data, &options);
    assert_eq!(outcome.bars_executed, 1);
    assert_eq!(
        outcome.errors[0].kind,
        pinewasm_vm::RuntimeErrorKind::StringLimitExceeded
    );

    let aot = compile_program(&program).expect("string calls use host.vm");
    let aot_run = run_aot(&aot, &program, &data, &options);
    assert_eq!(
        aot_run.host_error.expect("structured host error").kind,
        pinewasm_vm::RuntimeErrorKind::StringLimitExceeded
    );
}

#[test]
fn matrix_structural_array_results_cross_host_vm_without_type_loss() {
    let source = "indicator(\"M\")\n\
                  ints = matrix.new<int>(2, 2, 9007199254740993)\n\
                  int_row = ints.row(0)\n\
                  int_removed = matrix.remove_col(ints, 0)\n\
                  int_ok = int_row.get(0) == 9007199254740993 and int_removed.get(1) == 9007199254740993 and matrix.columns(ints) == 1\n\
                  strings = matrix.new<string>(1, 2, \"kept\")\n\
                  string_col = matrix.col(strings, 1)\n\
                  string_removed = strings.remove_row(0)\n\
                  string_ok = string_col.get(0) == \"kept\" and string_removed.get(1) == \"kept\" and strings.rows() == 0\n\
                  plot(int_ok and string_ok ? 1.0 : 0.0)\n";
    let program = compile_csil(source);
    // Full-domain integer elements come back through `host.vm` result cells.
    // The host answers a present minimum with `ABI_MISMATCH` itself, so these
    // sites are guarded without any emitted check and the program is native.
    let aot = guarded_native(&program, "matrix structural arrays");
    assert!(int_min_guards(&program)
        .values()
        .any(|guard| *guard == IntMinGuard::HostCell));

    let data = synthetic_data(4);
    let options = RunOptions::default();
    let outcome = run(&program, &data, &options);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    let aot_run = run_aot(&aot, &program, &data, &options);
    assert_eq!(aot_run.status, status::OK, "{:?}", aot_run.host_error);
    assert!(aot_run.mismatch.is_none(), "{:?}", aot_run.mismatch);
    assert_outcomes_match(&outcome, &aot_run, data.bars(), "matrix structural arrays");
}

#[test]
fn budget_metering_preserves_partial_bar_effects_and_exact_pc() {
    let program = compile_csil(
        "indicator(\"BM\")\nplot(close)\nalertcondition(true, \"A\", \"fired\")\nplot(open)\n",
    );
    let data = synthetic_data(1);
    let aot = compile_program(&program).unwrap();

    for limit in 0..=program.main().code.len() as u64 {
        let options = RunOptions {
            max_instructions_per_bar: limit,
            ..RunOptions::default()
        };
        let outcome = run(&program, &data, &options);
        let aot_run = run_aot(&aot, &program, &data, &options);
        assert_alerts_match(&outcome, &aot_run, &program, &format!("budget {limit}"));
        match outcome.errors.first() {
            Some(error) => {
                assert_eq!(aot_run.status, status::BUDGET_EXCEEDED, "budget {limit}");
                assert_eq!(
                    Some(aot_run.err_pc),
                    error.instruction_offset,
                    "budget {limit}"
                );
            }
            None => assert_eq!(aot_run.status, status::OK, "budget {limit}"),
        }
    }
}

/// Expose only the existing private prepare/run functions in a standalone WebAssembly test
/// artifact. This lets the low-fuel harness exercise the actual standalone WebAssembly emitter
/// with arbitrary u64 counters without changing the public C ABI. These
/// fixtures have no generated heap helpers, so prepare/run immediately
/// precede the final evaluate wrapper; typed wasmi lookups verify their ABI.
fn standalone_wasm_fuel_test_module(program: &pinewasm_csil::CsilProgram) -> (AotModule, Vec<i64>) {
    let mut aot = compile_program(program).unwrap();
    assert!(!aot.layout.imports_vm);
    assert!(!program.main().code.iter().any(|op| matches!(
        op,
        pinewasm_csil::CsilInstr::CallCollection { .. }
            | pinewasm_csil::CsilInstr::NewObject { .. }
            | pinewasm_csil::CsilInstr::CopyObject { .. }
    )));
    let (wasm, groups) = standalone_wasm_with_prepare_and_run(program);
    aot.wasm = wasm;
    (aot, groups)
}

/// [`standalone_wasm_fuel_test_module`] for a program whose drawing sites the
/// standalone WebAssembly projection erases. The ordinary tier keeps those
/// sites on `host.vm`, so its layout is reused with the host plan dropped:
/// the standalone module has no imports and no host sites.
fn erased_drawing_fuel_test_module(program: &pinewasm_csil::CsilProgram) -> AotModule {
    let mut aot = compile_program(program).unwrap();
    assert!(
        aot.layout.imports_vm,
        "the ordinary tier must host the drawing sites"
    );
    let (wasm, _) = standalone_wasm_with_prepare_and_run(program);
    aot.wasm = wasm;
    aot.layout.imports_vm = false;
    aot.layout.host_sites.clear();
    aot
}

/// The standalone WebAssembly artifact for `program` with its internal
/// `prepare`/`run` exported, and the fuel-window costs found in its
/// `bar_step` and its pieces, before the entry point.
fn standalone_wasm_with_prepare_and_run(
    program: &pinewasm_csil::CsilProgram,
) -> (Vec<u8>, Vec<i64>) {
    use wasm_encoder::{ExportKind, ExportSection, Module, RawSection};
    use wasmparser::{Operator, Parser, Payload};

    let wasm = compile_standalone_wasm_program(
        program,
        &StandaloneWasmOptions {
            signal_plot: None,
            module_type: StandaloneWasmModuleType::Indicator,
        },
    )
    .unwrap();
    pinewasm_aot::audit_standalone_wasm(&wasm).unwrap();
    let mut functions = 0;
    for payload in Parser::new(0).parse_all(&wasm) {
        if let Payload::FunctionSection(section) = payload.unwrap() {
            functions = section.count();
        }
    }
    // `evaluate_indicator_cabi` (the last function) calls `prepare` with
    // exactly `local.get slice_end; call prepare`, and `run` is emitted right
    // after `prepare`. Native-heap helpers, when present, sit between `run`
    // and the entry point.
    let mut prepare = None;
    let mut body_count = 0;
    for payload in Parser::new(0).parse_all(&wasm) {
        if let Payload::CodeSectionEntry(body) = payload.unwrap() {
            if body_count == functions - 1 {
                let ops: Vec<_> = body
                    .get_operators_reader()
                    .unwrap()
                    .into_iter()
                    .map(Result::unwrap)
                    .collect();
                prepare = ops.windows(2).find_map(|pair| match pair {
                    [Operator::LocalGet { local_index: 2 }, Operator::Call { function_index }] => {
                        Some(*function_index)
                    }
                    _ => None,
                });
            }
            body_count += 1;
        }
    }
    let prepare = prepare.expect("evaluate_indicator_cabi calls prepare(slice_end)");
    let run = prepare + 1;
    let mut module = Module::new();
    let mut body_index = 0;
    let mut groups = Vec::new();
    for payload in Parser::new(0).parse_all(&wasm) {
        let payload = payload.unwrap();
        if let Payload::ExportSection(section) = payload {
            let mut exports = ExportSection::new();
            for export in section {
                let export = export.unwrap();
                let kind = match export.kind {
                    wasmparser::ExternalKind::Func => ExportKind::Func,
                    wasmparser::ExternalKind::Memory => ExportKind::Memory,
                    other => panic!("unexpected standalone WebAssembly export {other:?}"),
                };
                if export.name == "evaluate_indicator_cabi" {
                    assert_eq!(export.index, functions - 1);
                }
                exports.export(export.name, kind, export.index);
            }
            exports.export("prepare", ExportKind::Func, prepare);
            exports.export("run", ExportKind::Func, run);
            module.section(&exports);
        } else {
            if let Some((id, range)) = payload.as_section() {
                module.section(&RawSection {
                    id,
                    data: &wasm[range.start as usize..range.end as usize],
                });
            }
            if let Payload::CodeSectionEntry(body) = payload {
                // Split bodies put the same fuel windows in their pieces.
                // Scan every generated body after run; linked-runtime
                // bodies precede it, and helpers cannot match this exact
                // two-counter guard/debit template.
                if body_index > run && body_index <= functions - 2 {
                    let ops: Vec<_> = body
                        .get_operators_reader()
                        .unwrap()
                        .into_iter()
                        .map(Result::unwrap)
                        .collect();
                    // Match the full guard/debit prelude of the UInt64
                    // template, not merely the
                    // two comparisons. A changed operand, local, signedness
                    // or subtraction must invalidate these fuel-window
                    // integration fixtures.
                    groups.extend(ops
                        .windows(16)
                        .filter_map(|ops| match ops {
                            [Operator::LocalGet { local_index: bar_guard },
                             Operator::I64Const { value: bar_cost },
                             Operator::I64GeU,
                             Operator::LocalGet { local_index: run_guard },
                             Operator::I64Const { value: run_cost },
                             Operator::I64GeU,
                             Operator::I32And,
                             Operator::If { blockty: wasmparser::BlockType::Empty },
                             Operator::LocalGet { local_index: bar_debit },
                             Operator::I64Const { value: debit_bar_cost },
                             Operator::I64Sub,
                             Operator::LocalSet { local_index: bar_set },
                             Operator::LocalGet { local_index: run_debit },
                             Operator::I64Const { value: debit_run_cost },
                             Operator::I64Sub,
                             Operator::LocalSet { local_index: run_set }]
                                if bar_cost == run_cost
                                    && bar_cost == debit_bar_cost
                                    && bar_cost == debit_run_cost
                                    && *bar_cost > 0
                                    && bar_guard == bar_debit
                                    && bar_guard == bar_set
                                    && run_guard == run_debit
                                    && run_guard == run_set
                                    && bar_guard != run_guard =>
                            {
                                Some(*bar_cost)
                            }
                            _ => None,
                        })
                    );
                }
                body_index += 1;
            }
        }
    }
    let wasm = module.finish();
    wasmparser::Validator::new().validate_all(&wasm).unwrap();
    (wasm, groups)
}

fn assert_standalone_wasm_fuel_matches(
    program: &pinewasm_csil::CsilProgram,
    aot: &AotModule,
    original: &AotModule,
    data: &MarketData,
    per_bar: u64,
    per_run: u64,
) {
    let options = RunOptions {
        max_instructions_per_bar: per_bar,
        max_instructions_per_run: per_run,
        ..RunOptions::default()
    };
    let expected = run(program, data, &options);
    let actual = run_aot(aot, program, data, &options);
    let original = run_aot(original, program, data, &options);
    let context = format!("standalone WebAssembly grouped fuel bar={per_bar}, run={per_run}");
    // The interpreter truncates plot vectors on an aborted bar. Compare the
    // complete partially written lanes against the original per-PC emitter,
    // and independently compare the interpreter's status/PC and alerts.
    assert_eq!(actual.status, original.status, "{context}");
    assert_eq!(actual.err_bar, original.err_bar, "{context}");
    assert_eq!(actual.err_pc, original.err_pc, "{context}");
    let bits = |plots: &[Vec<f64>]| {
        plots
            .iter()
            .map(|lane| lane.iter().map(|v| v.to_bits()).collect::<Vec<_>>())
            .collect::<Vec<_>>()
    };
    assert_eq!(bits(&actual.plots), bits(&original.plots), "{context}");
    assert_eq!(actual.colors, original.colors, "{context}");
    assert_eq!(actual.alerts, original.alerts, "{context}");
    assert_alerts_match(&expected, &actual, program, &context);
    if let Some(error) = expected.errors.first() {
        let status = match error.kind {
            pinewasm_vm::RuntimeErrorKind::InstructionLimitExceeded => status::BUDGET_EXCEEDED,
            pinewasm_vm::RuntimeErrorKind::RunInstructionLimitExceeded => {
                status::RUN_BUDGET_EXCEEDED
            }
            other => panic!("unexpected interpreter error {other:?}"),
        };
        assert_eq!(actual.status, status, "{context}");
        assert_eq!(Some(actual.err_bar), error.bar_index, "{context}");
        assert_eq!(Some(actual.err_pc), error.instruction_offset, "{context}");
    } else {
        assert_eq!(actual.status, status::OK, "{context}");
        assert_outcomes_match(&expected, &actual, data.bars(), &context);
    }
}

#[test]
fn standalone_wasm_fuel_groups_preserve_every_budget_cut_and_partial_effect() {
    let program = compile_csil(concat!(
        "indicator('Grouped fuel')\na = close\nb = open\nplot(a)\n",
        "x = ((a + b) * 2.0 - 1.0) / 3.0 + 4.0\nplot(x)\nplot(x[1])\n",
        "alertcondition(true, 'Done', 'after group')\n",
    ));
    let (aot, groups) = standalone_wasm_fuel_test_module(&program);
    let original = compile_program(&program).unwrap();
    assert!(
        !groups.is_empty(),
        "fixture must exercise the grouped standalone WebAssembly fast path"
    );
    let data = synthetic_data(1);
    let maximum = program.main().code.len() as u64 + 1;
    for per_bar in 0..=maximum {
        for per_run in 0..=maximum {
            assert_standalone_wasm_fuel_matches(&program, &aot, &original, &data, per_bar, per_run);
        }
    }
}

#[test]
fn standalone_wasm_fuel_groups_preserve_branches_loops_na_and_full_u64_counters() {
    let sources = [
        concat!(
            "indicator('Fuel branches')\na = close\nb = open\n",
            "float x = na\nif a > b\n    x := (a + b) * 2.0 - 1.0\n",
            "else\n    x := (a - b) * 3.0 + 1.0\nplot(x)\n",
        ),
        concat!(
            "indicator('Fuel loop')\na = close\nb = open\nfloat x = 0.0\n",
            "for i = 0 to 2\n    x := (a + b) * 2.0 - 1.0\nplot(x)\n",
        ),
        concat!(
            "indicator('Fuel joins')\na = close\nb = open\n",
            "plot((a + b) * 2.0 - 1.0, color=a > b ? color.red : color.blue)\n",
            "plot((a - b) / 0.0 + -0.0)\n",
        ),
    ];
    let mut data = synthetic_data(3);
    data.close[0] = f64::NAN;
    data.open[1] = -0.0;
    data.close[1] = 0.0;
    for source in sources {
        let program = compile_csil(source);
        // The counting loop's `IntAdd` crosses a loop join, so it carries a
        // minimum guard. It is not refused: it runs the same fuel
        // comparison as the other sources.
        let (aot, groups) = standalone_wasm_fuel_test_module(&program);
        let original = compile_program(&program).unwrap();
        assert!(!groups.is_empty());
        let limit = program.main().code.len() as u64 * 4;
        for remaining in 0..=limit {
            for (per_bar, per_run) in [
                (remaining, u64::MAX),
                (u64::MAX, remaining),
                (remaining, remaining),
            ] {
                assert_standalone_wasm_fuel_matches(
                    &program, &aot, &original, &data, per_bar, per_run,
                );
            }
        }
        for counter in [1_u64 << 63, u64::MAX - 1, u64::MAX] {
            assert_standalone_wasm_fuel_matches(&program, &aot, &original, &data, counter, counter);
        }
    }
}

/// Assemble exactly eight admitted scalar instructions so neither frontend
/// constant folding nor a short ungrouped suffix can hide the tested opcode.
fn scalar_fuel_sequence_program(
    expression: [pinewasm_csil::CsilInstr; 8],
) -> pinewasm_csil::CsilProgram {
    use pinewasm_csil::CsilInstr as C;
    let mut program = compile_csil("indicator('Scalar fuel')\nplot(close)\n");
    let main = &mut program.functions[0];
    let span = main.source_map[0];
    main.code = expression.to_vec();
    main.code.extend([C::PlotF64 { plot_slot: 0 }, C::Return]);
    main.source_map = vec![span; main.code.len()];
    let mut depth: u32 = 0;
    main.max_stack = 0;
    for op in &main.code {
        let effect = op
            .resolve_stack_effect(&program.ta_states)
            .expect("fixture stack rule resolves");
        depth = depth
            .checked_sub(effect.pops)
            .expect("fixture stack underflow")
            + effect.pushes;
        main.max_stack = main.max_stack.max(depth);
    }
    assert_eq!(depth, 0);
    pinewasm_csil::validate(&program).unwrap();
    assert_eq!(&program.main().code[..8], &expression);
    program
}

fn assert_standalone_wasm_scalar_fuel_sequence(expression: [pinewasm_csil::CsilInstr; 8]) {
    let program = scalar_fuel_sequence_program(expression);
    let (aot, groups) = standalone_wasm_fuel_test_module(&program);
    // The whole bar is one window, so every exact opcode above is in the
    // guarded fast arm.
    assert_eq!(groups.len(), 1, "all eight fixture PCs must be grouped");
    assert!(groups[0] >= 8, "{groups:?}");
    let original = compile_program(&program).unwrap();
    let data = synthetic_data(1);
    let maximum = program.main().code.len() as u64 + 1;
    for per_bar in 0..=maximum {
        for per_run in 0..=maximum {
            assert_standalone_wasm_fuel_matches(&program, &aot, &original, &data, per_bar, per_run);
        }
    }
}

#[test]
fn standalone_wasm_float_negation_group_preserves_exact_bits_and_budget_cuts() {
    use pinewasm_csil::CsilInstr as C;
    for value in [
        0.0,
        -0.0,
        f64::from_bits(1),
        f64::from_bits((1_u64 << 63) | 1),
        f64::MIN_POSITIVE,
        f64::MAX,
    ] {
        let expression = [
            C::PushF64(pinewasm_csil::F64Bits::new(value)),
            C::Dup,
            C::Pop,
            C::Dup,
            C::Pop,
            C::Dup,
            C::Pop,
            C::F64Neg,
        ];
        assert_standalone_wasm_scalar_fuel_sequence(expression.clone());
        let program = scalar_fuel_sequence_program(expression);
        let (aot, groups) = standalone_wasm_fuel_test_module(&program);
        assert_eq!(groups.len(), 1);
        assert!(groups[0] >= 8, "{groups:?}");
        let data = synthetic_data(1);
        let options = RunOptions::default();
        let expected = run(&program, &data, &options);
        assert!(expected.errors.is_empty(), "{:?}", expected.errors);
        let actual = run_aot(&aot, &program, &data, &options);
        assert_eq!(actual.status, status::OK, "{:?}", actual.host_error);
        let want = value.to_bits() ^ (1_u64 << 63);
        let reference = match expected.plots[0].values.first() {
            Some(Value::Float(number)) => number.to_bits(),
            other => panic!("unexpected interpreter float result: {other:?}"),
        };
        assert_eq!(reference, want, "interpreter negation of {value:?}");
        assert_eq!(
            actual.plots[0][0].to_bits(),
            want,
            "standalone WASM negation of {value:?}"
        );
    }
}

#[test]
fn standalone_wasm_float_negation_group_preserves_every_finite_exponent_class() {
    use pinewasm_csil::CsilInstr as C;
    use pinewasm_hir::BuiltinSeries;

    let mut program = compile_csil("indicator('Float input fuel')\nplot(close)\n");
    assert!(program.series_slots.is_empty());
    program.series_slots.push(pinewasm_csil::SlotDef {
        name: "sample".to_string(),
        persistent: false,
        varip: false,
        lane: None,
    });
    let main = &mut program.functions[0];
    let span = main.source_map[0];
    // The whole bar, the eight scalar instructions among it, is one guarded
    // standalone WASM window, this time reading data rather than embedding
    // one constant in each module.
    main.code = vec![
        C::ReadBuiltin {
            source: BuiltinSeries::Close,
            offset: 0,
        },
        C::StoreUserSlotCurrent { slot: 0 },
        C::ReadUserSlot { slot: 0, offset: 0 },
        C::Dup,
        C::Pop,
        C::Dup,
        C::Pop,
        C::Dup,
        C::Pop,
        C::F64Neg,
        C::PlotF64 { plot_slot: 0 },
        C::Return,
    ];
    main.source_map = vec![span; main.code.len()];
    main.max_stack = 2;
    pinewasm_csil::validate(&program).unwrap();

    let (aot, groups) = standalone_wasm_fuel_test_module(&program);
    assert_eq!(groups, [12], "input-driven float sequence must be grouped");

    let mut samples = Vec::with_capacity(2047 * 2);
    for exponent in 0_u64..2047 {
        // Exponent 0 includes zero/subnormals. Exponent 2047 is deliberately
        // excluded because the VM maps infinities and NaNs to `na`.
        let mantissa = match exponent % 3 {
            0 => 0,
            1 => 1,
            _ => (1_u64 << 52) - 1,
        };
        for sign in [0_u64, 1_u64 << 63] {
            samples.push(f64::from_bits(sign | (exponent << 52) | mantissa));
        }
    }
    let mut data = synthetic_data(samples.len());
    data.close.clone_from(&samples);
    let options = RunOptions::default();
    let expected = run(&program, &data, &options);
    let actual = run_aot(&aot, &program, &data, &options);
    assert!(expected.errors.is_empty(), "{:?}", expected.errors);
    assert_eq!(actual.status, status::OK, "{:?}", actual.host_error);
    assert_eq!(expected.plots[0].values.len(), samples.len());
    assert_eq!(actual.plots[0].len(), samples.len());
    for (index, value) in samples.iter().enumerate() {
        let want = value.to_bits() ^ (1_u64 << 63);
        let reference = match &expected.plots[0].values[index] {
            Value::Float(number) => number.to_bits(),
            other => panic!("unexpected interpreter value at {index}: {other:?}"),
        };
        assert_eq!(reference, want, "VM exponent sample {index}");
        assert_eq!(
            actual.plots[0][index].to_bits(),
            want,
            "standalone WASM exponent sample {index}"
        );
    }
}

#[test]
fn standalone_wasm_fuel_groups_preserve_integer_and_boolean_scalar_ops() {
    use pinewasm_csil::CsilInstr as C;
    for value in [
        i64::MIN + 1,
        -9_007_199_254_740_993,
        -1,
        0,
        1,
        9_007_199_254_740_993,
        i64::MAX - 1,
        i64::MAX,
    ] {
        assert_standalone_wasm_scalar_fuel_sequence([
            C::PushI64(value),
            C::IntNeg,
            C::IntToF64,
            C::F64Neg,
            C::Dup,
            C::Pop,
            C::Dup,
            C::Pop,
        ]);
    }
    // Complete three-valued truth tables, including the integer na sentinel
    // above and Boolean na here; Select makes every result observable.
    for lhs in [C::PushBool(false), C::PushBool(true), C::PushNa] {
        for rhs in [C::PushBool(false), C::PushBool(true), C::PushNa] {
            for combine in [C::BoolAnd, C::BoolOr] {
                assert_standalone_wasm_scalar_fuel_sequence([
                    lhs.clone(),
                    rhs.clone(),
                    combine,
                    C::BoolNot,
                    C::PushF64(pinewasm_csil::F64Bits::new(1.0)),
                    C::PushF64(pinewasm_csil::F64Bits::new(2.0)),
                    C::Select,
                    C::F64Neg,
                ]);
            }
        }
    }
}

#[test]
fn standalone_wasm_fuel_groups_preserve_extreme_and_mixed_numeric_comparisons() {
    use pinewasm_csil::CsilInstr as C;
    // MIN + 1 is the lowest integer in the admitted native subset; PushNa
    // exercises missing values independently of the integer value domain.
    let pairs = [
        (C::PushI64(i64::MAX), C::PushI64(i64::MAX - 1)),
        (C::PushI64(i64::MAX), C::PushI64(i64::MAX)),
        (
            C::PushF64(pinewasm_csil::F64Bits::new(9_007_199_254_740_992.0)),
            C::PushF64(pinewasm_csil::F64Bits::new(9_007_199_254_740_992.0)),
        ),
        (C::PushI64(i64::MIN + 1), C::PushI64(i64::MIN + 2)),
        (
            C::PushI64(9_007_199_254_740_993),
            C::PushF64(pinewasm_csil::F64Bits::new(9_007_199_254_740_992.0)),
        ),
        (
            C::PushI64(i64::MAX),
            C::PushF64(pinewasm_csil::F64Bits::new(i64::MAX as f64)),
        ),
        (
            C::PushI64(i64::MIN + 1),
            C::PushF64(pinewasm_csil::F64Bits::new(i64::MIN as f64)),
        ),
        (C::PushI64(0), C::PushF64(pinewasm_csil::F64Bits::new(-0.0))),
        (C::PushI64(1), C::PushNa),
        (C::PushNa, C::PushF64(pinewasm_csil::F64Bits::new(1.0))),
        (
            C::PushF64(pinewasm_csil::F64Bits::new(f64::MAX)),
            C::PushI64(i64::MAX),
        ),
        (
            C::PushF64(pinewasm_csil::F64Bits::new(f64::MIN_POSITIVE)),
            C::PushI64(0),
        ),
    ];
    for (lhs, rhs) in pairs {
        // Operand order matters: each mixed comparison exercises both typed
        // stack orders and their integer-to-float conversion paths.
        for (lhs, rhs) in [(lhs.clone(), rhs.clone()), (rhs, lhs)] {
            for compare in [C::CmpGt, C::CmpGe, C::CmpLt, C::CmpLe, C::CmpEq, C::CmpNe] {
                // The validator permits mixed ordering, but equality requires
                // matching types. Do not manufacture an unaccepted program.
                if matches!(compare, C::CmpEq | C::CmpNe)
                    && matches!(
                        (&lhs, &rhs),
                        (C::PushI64(_), C::PushF64(_)) | (C::PushF64(_), C::PushI64(_))
                    )
                {
                    continue;
                }
                assert_standalone_wasm_scalar_fuel_sequence([
                    lhs.clone(),
                    rhs.clone(),
                    compare,
                    C::PushF64(pinewasm_csil::F64Bits::new(1.0)),
                    C::PushF64(pinewasm_csil::F64Bits::new(2.0)),
                    C::Select,
                    C::Dup,
                    C::Pop,
                ]);
            }
        }
    }
}

#[test]
fn budget_metering_preserves_the_full_u64_domain() {
    let program = compile_csil("indicator(\"U64\")\nplot(close)\n");
    let data = synthetic_data(4);
    let options = RunOptions {
        max_instructions_per_bar: u64::MAX,
        ..RunOptions::default()
    };
    let outcome = run(&program, &data, &options);
    assert!(outcome.errors.is_empty());

    let aot = compile_program(&program).unwrap();
    let aot_run = run_aot(&aot, &program, &data, &options);
    assert_eq!(aot_run.status, status::OK);
    assert_outcomes_match(&outcome, &aot_run, data.bars(), "u64 budget");
}

// ---- control flow + typed domains -------------------------------------------

#[test]
fn if_else_matches_interpreter() {
    differential(
        "indicator(\"IF\")\nvalue = 0.0\nif close > open\n    value := close\nelse\n    value := open\nplot(value)\n",
        &synthetic_data(128),
    );
}

#[test]
fn var_counter_and_slot_written_guard_match() {
    // `var` lowers to JumpIfSlotWritten; the counter is an int slot with
    // checked arithmetic and IntToF64 at the plot boundary.
    differential(
        "indicator(\"VAR\")\nvar count = 0\nif close > open\n    count := count + 1\nplot(count)\n",
        &synthetic_data(200),
    );
}

#[test]
fn for_loop_matches_interpreter() {
    differential(
        "indicator(\"L\")\nsum = 0.0\nfor i = 0 to 9\n    sum := sum + close[i]\nplot(sum / 10)\n",
        &synthetic_data(96),
    );
}

#[test]
fn positive_descending_explicit_step_matches_interpreter() {
    differential(
        "indicator(\"D\")\nsum = 0\nfor i = 10 to 2 by 2\n    sum += i\nplot(sum)\n",
        &synthetic_data(16),
    );
}

#[test]
fn ternary_and_kleene_logic_match() {
    differential(
        "indicator(\"T\")\nup = close > open\nstrong = up and close > close[1]\nweak = up or close < open\nplot(strong ? 2.0 : weak ? 1.0 : 0.0)\nplot(not up ? 1.0 : 0.0)\n",
        &synthetic_data(160),
    );
}

#[test]
fn dynamic_history_offset_matches_interpreter() {
    differential(
        "indicator(\"DYN\")\nn = math.min(bar_index, 5)\nplot(close[n])\n",
        &synthetic_data(64),
    );
}

#[test]
fn engine_ta_functions_match_interpreter() {
    // ta.ema/rma/atr/stdev/highest all advance VM-owned state through the
    // host.ta import.
    differential(
        "indicator(\"TA\")\nplot(ta.ema(close, 12))\nplot(ta.rma(close, 7))\nplot(ta.atr(14))\nplot(ta.stdev(close, 10))\nplot(ta.highest(high, 20))\n",
        &synthetic_data(1024),
    );
}

#[test]
fn extrema_windows_match_through_history_conditional_calls_and_na() {
    differential(
        concat!(
            "indicator(\"extrema\")\n",
            "x = bar_index % 101 == 0 ? float(na) : float(bar_index % 13 - 6)\n",
            "plot(ta.highest(x, 37))\nplot(ta.lowest(x, 37))\n",
            "plot(ta.highestbars(x, 37))\nplot(ta.lowestbars(x, 37))\n",
            "plot(ta.highest(x[2], 3)[1])\n",
            "float conditional = na\n",
            "if bar_index % 3 != 0\n",
            "    conditional := ta.lowest(x, 37)\nplot(conditional)\n",
        ),
        &synthetic_data(512),
    );
}

#[test]
fn extrema_cache_refills_match_in_linked_wasm() {
    differential(
        concat!(
            "indicator(\"cache refill\")\n",
            "x = bar_index % 169 == 0 ? float(na) : float(bar_index)\n",
            "x := bar_index % 87 == 0 ? 3.0 : x\n",
            "plot(ta.highest(-x, 65))\nplot(ta.lowest(x, 65))\n",
            "plot(ta.highestbars(-x, 65))\nplot(ta.lowestbars(x, 65))\n",
        ),
        &synthetic_data(512),
    );
}

/// A feed whose high/low/close are signed zeros (with a few `±1` and na
/// bars), so the market-data `max`/`min` sites (`ta.tr`, `ta.atr`,
/// `ta.wpr`, `ta.sar`) meet `+0.0` against `-0.0`.
fn signed_zero_data(n: usize) -> MarketData {
    const ALPHABET: [f64; 8] = [0.0, -0.0, 0.0, -0.0, -0.0, 1.0, -1.0, f64::NAN];
    let mut data = MarketData::default();
    let mut state = 0x5EED_0000_0000_0A07u64;
    let mut next = move || {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    for i in 0..n {
        // Long stretches of pure zeros, so that windows of every tested
        // length fill with zeros of both signs.
        let span = if (i / 96) % 2 == 0 { 5 } else { 8 };
        let mut pick = || ALPHABET[(next() % span) as usize];
        data.open.push(pick());
        data.high.push(pick());
        data.low.push(pick());
        data.close.push(pick());
        data.volume.push(1000.0 + (i % 97) as f64 * 10.0);
        data.time.push(1_000_000_000_000 + (i as i64) * 86_400_000);
    }
    data
}

/// Signed-zero ties end to end: `f64::max`/`f64::min` leave the `+0.0`
/// vs `-0.0` tie to the target, and the interpreter (native) and the linked
/// wasm engine must not return different zeros. Every plot is compared bit
/// for bit, on window lengths either side of the long-window switch at 32,
/// over script-made signed-zero series and signed-zero market data.
#[test]
fn signed_zero_ties_match_in_linked_wasm_at_every_window_length() {
    let data = signed_zero_data(480);
    let mut zero_signs = [0usize; 2];
    for length in [2usize, 3, 9, 31, 32, 33, 40] {
        let source = format!(
            concat!(
                "indicator(\"signed zeros {n}\")\n",
                // `-0.0`, `+0.0`, and an occasional na, from the script.
                "z = bar_index % 53 == 0 ? float(na) : (bar_index * 7 % 5 < 2 ? -1.0 : 1.0) * 0.0\n",
                "plot(ta.highest(z, {n}))\nplot(ta.lowest(z, {n}))\n",
                "plot(ta.highest(close, {n}))\nplot(ta.lowest(close, {n}))\n",
                "plot(ta.highestbars(close, {n}))\nplot(ta.lowestbars(close, {n}))\n",
                "plot(ta.max(z))\nplot(ta.min(z))\n",
                "plot(ta.max(close))\nplot(ta.min(close))\n",
                "plot(ta.range(close, {n}))\n",
                "plot(ta.stoch(close, high, low, {n}))\n",
                "plot(ta.wpr({n}))\n",
                "plot(ta.cmo(close, {n}))\nplot(ta.rsi(close, {n}))\n",
                "plot(ta.tr(true))\nplot(ta.atr({n}))\n",
                "plot(ta.sar(0.02, 0.02, 0.2))\n",
                "[kc_mid, kc_up, kc_lo] = ta.kc(close, {n}, 2.0)\nplot(kc_up)\nplot(kc_lo)\n",
            ),
            n = length
        );
        let context = format!("signed zeros, length {length}");
        let outcome = differential_program(&compile_csil(&source), &data, &context);
        // Non-vacuity: the first four plots (`ta.highest`/`ta.lowest`)
        // return zeros, and never `-0.0` (window extrema are canonical).
        for plot in outcome.plots.iter().take(4) {
            for value in &plot.values {
                if let Value::Float(x) = value {
                    if *x == 0.0 {
                        zero_signs[usize::from(x.is_sign_negative())] += 1;
                    }
                }
            }
        }
    }
    assert!(
        zero_signs[0] > 100 && zero_signs[1] == 0,
        "zero extrema, none of them -0.0: {zero_signs:?}"
    );
}

#[test]
fn extrema_cache_preserves_aot_region_admission() {
    // 48 * 100,000 values fit the existing 64-MiB TA region limit;
    // doubling that storage for candidate indices rejected this program.
    let mut source = String::from("indicator(\"region budget\")\nn = 100000\n");
    for i in 0..48 {
        let func = ["highest", "lowest", "highestbars", "lowestbars"][i % 4];
        source.push_str(&format!("plot(ta.{func}(close + {i}, n))\n"));
    }
    let program = compile_csil(&source);
    let aot = compile_program(&program).expect("previously admitted region must still compile");
    assert_eq!(aot.layout.ta_decls.len(), 48);
    assert_eq!(aot.layout.ta_layout.cells, 4_800_000);
    let result = run_aot(
        &aot,
        &program,
        &synthetic_data(1),
        &RunOptions {
            max_instructions_per_bar: 10_000_000,
            ..RunOptions::default()
        },
    );
    assert_eq!(result.status, status::OK);
    assert_eq!(result.plots.len(), 48);
    assert!(result
        .plots
        .iter()
        .all(|plot| plot.len() == 1 && plot[0].is_nan()));
}

#[test]
fn tuple_ta_macd_matches_interpreter() {
    differential(
        "indicator(\"MACD\")\n[macdLine, signalLine, histLine] = ta.macd(close, 12, 26, 9)\nplot(macdLine)\nplot(signalLine)\nplot(histLine)\n",
        &synthetic_data(1024),
    );
}

#[test]
fn crossover_bool_result_matches_interpreter() {
    differential(
        "indicator(\"X\")\nfast = ta.sma(close, 5)\nslow = ta.sma(close, 20)\nplot(ta.crossover(fast, slow) ? 1.0 : 0.0)\nplot(ta.barssince(close > open))\n",
        &synthetic_data(512),
    );
}

#[test]
fn pure_math_matches_interpreter() {
    differential(
        "indicator(\"M\")\nplot(math.exp(math.log(math.abs(close))))\nplot(math.pow(close / 100, 2.5))\nplot(math.round(close, 3))\nplot(math.max(open, close, hl2))\nplot(math.sign(close - open))\nplot(math.floor(close) + math.ceil(open))\nplot(math.avg(open, high, low, close))\nplot(math.sqrt(math.abs(close - open)))\n",
        &synthetic_data(512),
    );
}

#[test]
fn scalar_casts_that_can_saturate_to_min_run_natively_behind_the_guard() {
    let source = concat!(
        "indicator(\"CAST\")\n",
        "i_from_float = int((close - open) * 2.5)\n",
        "i_from_int = int(bar_index)\n",
        "i_with_na = int(close[1])\n",
        "f_from_int = float(bar_index)\n",
        "f_from_float = float(close - open)\n",
        "f_with_na = float(i_with_na)\n",
        "b_from_float = bool(close - close[1])\n",
        "b_from_int = bool(bar_index - 1)\n",
        "b_from_bool = bool(close > close[1])\n",
        "plot(i_from_float)\n",
        "plot(i_from_int)\n",
        "plot(i_with_na)\n",
        "plot(f_from_int)\n",
        "plot(f_from_float)\n",
        "plot(f_with_na)\n",
        "plot(b_from_float ? 1.0 : 0.0)\n",
        "plot(b_from_int ? 1.0 : 0.0)\n",
        "plot(b_from_bool ? 1.0 : 0.0)\n",
    );
    let program = compile_csil(source);
    // An unrestricted finite float can truncate to `i64::MIN`, so each such
    // cast is guarded instead of sending the program to the interpreter.
    guarded_native(&program, "scalar casts");
    assert!(int_min_guards(&program)
        .values()
        .any(|guard| *guard == IntMinGuard::FloatToInt));
    differential(source, &synthetic_data(40));
}

#[test]
fn f64_remainder_extremes_match_interpreter() {
    // `x - trunc(x / y) * y` is not an IEEE remainder implementation for
    // large quotients: the intermediate division/multiplication can round
    // away the low remainder bits. Keep this differential at an operand
    // magnitude where Rust's `%` and the naive expansion demonstrably differ.
    differential(
        "indicator(\"REM\")\nplot(1e300 % 3.0)\nplot(-1e300 % 7.0)\n",
        &synthetic_data(4),
    );
}

#[test]
fn large_int_input_stays_exact() {
    // The value is one above JavaScript's exact-integer range. Checking its
    // parity through `% 2` catches an input table that silently round-trips
    // it through f64 before loading it as i64.
    differential(
        "indicator(\"BIGINT\")\nn = input.int(9007199254740993, \"n\")\nplot(n % 2)\n",
        &synthetic_data(4),
    );
}

#[test]
fn time_input_uses_the_exact_integer_cell_encoding() {
    differential(
        "indicator(\"TIME\")\nt = input.time(1700000000001, \"t\")\nplot(t % 2)\n",
        &synthetic_data(4),
    );
}

#[test]
fn round_to_mintick_matches_interpreter() {
    let mut data = synthetic_data(64);
    data.mintick = 0.25;
    differential(
        "indicator(\"RM\")\nplot(math.round_to_mintick(close))\n",
        &data,
    );
}

#[test]
fn derived_builtins_and_barstate_match() {
    differential(
        "indicator(\"D\")\nplot(hl2)\nplot(hlc3)\nplot(ohlc4)\nplot(hlcc4)\nplot(barstate.isfirst ? 1.0 : 0.0)\nplot(barstate.islast ? 1.0 : 0.0)\nplot(bar_index)\n",
        &synthetic_data(48),
    );
}

#[test]
fn terminal_confirmed_history_matches_native_and_standalone_without_realtime() {
    let source = "//@version=6\nindicator(\"Confirmed history\")\nflag = barstate.islastconfirmedhistory\nplot(flag ? 1.0 : 0.0)\nplot(flag[1] ? 1.0 : 0.0)\nplot(barstate.islast ? 1.0 : 0.0)\n";
    let program = compile_csil(source);
    let artifact = compile_program_to_wasm(&program).expect("native history flags");
    assert_eq!(artifact.layout.execution_mode, ExecutionMode::Native);
    let wasm = compile_standalone_wasm_program(
        &program,
        &StandaloneWasmOptions {
            signal_plot: None,
            module_type: StandaloneWasmModuleType::Indicator,
        },
    )
    .expect("standalone history flags");
    for bars in [1, 3, 48] {
        let data = synthetic_data(bars);
        let result = differential_program(&program, &data, "terminal confirmed history");
        let expected: Vec<_> = (0..bars)
            .map(|bar| Value::Float(if bar + 1 == bars { 1.0 } else { 0.0 }))
            .collect();
        assert_eq!(result.plots[0].values, expected);
        assert_eq!(result.plots[1].values, vec![Value::Float(0.0); bars]);
        assert_eq!(result.plots[2].values, expected);
        assert_standalone_wasm_plot_matches_vm(
            &program,
            &wasm,
            &data,
            "terminal confirmed history standalone",
        );
    }
}

#[test]
fn int_arithmetic_with_widened_division_uses_native_wasm() {
    let source = "indicator(\"I\")\na = bar_index + 3\nb = bar_index * 7 - 2\nplot(a % 5)\nplot(b / 3)\nplot(-a)\n";
    let program = compile_csil(source);
    let artifact = compile_program_to_wasm(&program).unwrap();
    assert_eq!(artifact.layout.execution_mode, ExecutionMode::Native);
    let outcome = differential_program(&program, &synthetic_data(80), "widened quotient");
    assert_eq!(outcome.plots[0].values[0], Value::Float(3.0));
    assert_eq!(outcome.plots[1].values[0], Value::Float(-2.0 / 3.0));
    assert_eq!(outcome.plots[2].values[0], Value::Float(-3.0));
}

#[test]
fn native_tagged_binary_adapters_preserve_overflow_missing_and_signed_results() {
    use pinewasm_csil::CsilInstr as C;
    for (lhs, rhs, op, expected) in [
        (C::PushI64(i64::MAX), C::PushI64(2), C::IntMul, None),
        (C::PushI64(-i64::MAX), C::PushI64(2), C::IntMul, None),
        (
            C::PushI64(-i64::MAX),
            C::PushI64(-1),
            C::IntMul,
            Some(i64::MAX as f64),
        ),
        (C::PushI64(-i64::MAX), C::PushI64(0), C::IntMul, Some(0.0)),
        (C::PushNa, C::PushI64(0), C::IntMul, None),
        (C::PushI64(7), C::PushI64(-4), C::IntRem, Some(3.0)),
        (C::PushI64(-7), C::PushI64(4), C::IntRem, Some(-3.0)),
        (C::PushI64(7), C::PushI64(0), C::IntRem, None),
        (C::PushI64(7), C::PushNa, C::IntRem, None),
        (C::PushNa, C::PushI64(-1), C::IntDiv, None),
    ] {
        let mut program = compile_csil("indicator('Tagged binary adapter')\nplot(close)\n");
        let main = &mut program.functions[0];
        let span = main.source_map[0];
        main.code = vec![
            lhs,
            rhs,
            op,
            C::IntToF64,
            C::PlotF64 { plot_slot: 0 },
            C::Return,
        ];
        main.source_map = vec![span; main.code.len()];
        main.max_stack = 2;
        pinewasm_csil::validate(&program).unwrap();
        compile_program(&program).expect("adapter fixture must use native lowering");
        let outcome = differential_program(&program, &synthetic_data(3), "tagged binary adapter");
        for value in &outcome.plots[0].values {
            match expected {
                Some(expected) => assert_eq!(*value, Value::Float(expected)),
                None => assert_eq!(*value, Value::Na),
            }
        }
    }
}

#[test]
fn valid_i64_min_producers_stop_the_native_run_instead_of_reading_as_na() {
    let source = concat!(
        "indicator(\"Minimum integer\")\n",
        "x = bar_index - 9223372036854775807 - 1\n",
        "var int held = na\n",
        "held := x\n",
        "plot(held == x ? 1.0 : 0.0)\n",
        "plot(held[1] < x ? 1.0 : 0.0)\n",
    );
    let program = compile_csil(source);
    let artifact = guarded_native(&program, "MIN-capable lane");

    let outcome = run(&program, &synthetic_data(2), &RunOptions::default());
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(
        outcome.plots[0].values,
        vec![Value::Float(1.0), Value::Float(1.0)],
        "same-bar comparisons and branches must preserve valid MIN values"
    );
    assert_eq!(
        outcome.plots[1].values,
        vec![Value::Float(0.0), Value::Float(1.0)],
        "history must preserve MIN as an integer rather than missing"
    );
    // Bar 0 computes the minimum. The module reports the mismatch there, so
    // the consumer reruns the request on the interpreter, whose answer is the
    // one checked above.
    assert_present_min_stops(
        &artifact,
        &program,
        &synthetic_data(2),
        &RunOptions::default(),
        0,
        "MIN-capable lane",
    );
}

#[test]
fn integer_valuewhen_runs_natively_and_preserves_history_offsets() {
    let program = compile_csil(
        "//@version=6\nindicator(\"valuewhen int\")\nint n = ta.valuewhen(bar_index > 0, bar_index, 0)\nplot(n)\nplot(close[bar_index - n])\n",
    );
    // `bar_index - n` subtracts a TA result of unknown range: guarded.
    guarded_native(&program, "integer valuewhen");
    assert!(int_min_guards(&program)
        .values()
        .any(|guard| *guard == IntMinGuard::Arith));
    differential_program(&program, &synthetic_data(8), "integer valuewhen");
}

#[test]
fn dynamic_sma_length_uses_explicit_hosted_wasm_fallback() {
    let program = compile_csil("//@version=6\nindicator(\"Dynamic SMA\")\nlength = bar_index < 3 ? 2 : 3\nplot(ta.sma(close, length))\n");
    assert!(matches!(
        compile_program(&program),
        Err(AotError::UnsupportedOpcode(ref opcode)) if opcode == "WindowMeanDynamic"
    ));
    let artifact = compile_program_to_wasm(&program).expect("valid dynamic SMA uses hosted VM");
    assert_eq!(
        artifact.layout.execution_mode,
        ExecutionMode::InterpreterHosted
    );
    assert!(artifact.layout.imports_interpreter);
    let data = synthetic_data(5);
    let expected = run(&program, &data, &RunOptions::default());
    assert!(expected.errors.is_empty(), "{:?}", expected.errors);
    let actual = execute_hosted_wrapper(&artifact, &program, &data, &RunOptions::default());
    assert!(actual.errors.is_empty(), "{:?}", actual.errors);
    assert_eq!(actual.plots[0].values, expected.plots[0].values);
}

#[test]
fn dynamic_extremum_length_uses_explicit_hosted_wasm_fallback() {
    let program = compile_csil("//@version=6\nindicator(\"Dynamic highest\")\nlength = bar_index < 3 ? 2 : 3\nplot(ta.highest(close, length))\n");
    assert!(matches!(
        compile_program(&program),
        Err(AotError::UnsupportedOpcode(ref opcode)) if opcode == "WindowExtremumDynamic"
    ));
    let artifact =
        compile_program_to_wasm(&program).expect("valid dynamic extremum uses hosted VM");
    assert_eq!(
        artifact.layout.execution_mode,
        ExecutionMode::InterpreterHosted
    );
    let data = synthetic_data(5);
    let expected = run(&program, &data, &RunOptions::default());
    let actual = execute_hosted_wrapper(&artifact, &program, &data, &RunOptions::default());
    assert!(expected.errors.is_empty(), "{:?}", expected.errors);
    assert_eq!(actual.plots[0].values, expected.plots[0].values);
}

#[test]
fn simple_expression_ema_uses_explicit_hosted_wasm_fallback() {
    let program = compile_csil("//@version=6\nindicator(\"Simple EMA\")\nlength = input.int(3, \"Length\")\nresolved = timeframe.isdaily ? length : length + 1\nplot(ta.ema(close, resolved))\n");
    assert!(matches!(
        compile_program(&program),
        Err(AotError::UnsupportedOpcode(ref opcode)) if opcode == "EmaSimpleDynamic"
    ));
    let artifact = compile_program_to_wasm(&program).expect("simple EMA uses hosted VM");
    assert_eq!(
        artifact.layout.execution_mode,
        ExecutionMode::InterpreterHosted
    );
    let data = synthetic_data(8);
    let expected = run(&program, &data, &RunOptions::default());
    let actual = execute_hosted_wrapper(&artifact, &program, &data, &RunOptions::default());
    assert!(expected.errors.is_empty(), "{:?}", expected.errors);
    assert_eq!(actual.plots[0].values, expected.plots[0].values);
}

#[test]
fn dynamic_ta_median_mode_and_range_use_hosted_wasm_with_matching_values() {
    let program = compile_csil(
        "//@version=6\nindicator(\"Dynamic statistics\")\nlength = bar_index % 3 + 1\nplot(ta.median(close, length))\nplot(ta.mode(bar_index, length))\nplot(ta.range(bar_index, length))\n",
    );
    assert!(matches!(
        compile_program(&program),
        Err(AotError::UnsupportedOpcode(ref opcode)) if opcode == "WindowStatisticDynamic"
    ));
    let artifact = compile_program_to_wasm(&program).expect("dynamic statistics use hosted VM");
    assert_eq!(
        artifact.layout.execution_mode,
        ExecutionMode::InterpreterHosted
    );
    let data = synthetic_data(8);
    let expected = run(&program, &data, &RunOptions::default());
    let actual = execute_hosted_wrapper(&artifact, &program, &data, &RunOptions::default());
    assert!(expected.errors.is_empty(), "{:?}", expected.errors);
    assert_eq!(actual.plots, expected.plots);
}

#[test]
fn full_domain_integer_inputs_run_natively_and_min_stops_at_the_input_read() {
    let program = compile_csil(
        "indicator(\"Minimum input\")\nn = input.int(0, \"N\")\nplot(n == 0 ? 1.0 : 0.0)\n",
    );
    let artifact = guarded_native(&program, "full-domain input");
    let read = program
        .main()
        .code
        .iter()
        .position(|instr| matches!(instr, pinewasm_csil::CsilInstr::ReadInput { .. }))
        .expect("input read");
    assert_eq!(
        int_min_guards(&program).get(&read),
        Some(&IntMinGuard::InputRead)
    );
    let data = synthetic_data(1);
    differential_program(&program, &data, "full-domain input default");

    let mut options = RunOptions::default();
    options.inputs.insert(0, InputValue::Int(i64::MIN));
    let outcome = run(&program, &data, &options);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(outcome.plots[0].values, vec![Value::Float(0.0)]);

    // The interpreter compares the minimum as an ordinary integer. The
    // module would read the same cell as `na`, so it stops at the read.
    let stopped = assert_present_min_stops(&artifact, &program, &data, &options, 0, "MIN input");
    assert_eq!(stopped.err_pc as usize, read);

    options.inputs.insert(0, InputValue::Int(i64::MIN + 1));
    let adjacent = run(&program, &data, &options);
    let actual = run_aot(&artifact, &program, &data, &options);
    assert_eq!(actual.status, status::OK);
    assert_outcomes_match(&adjacent, &actual, data.bars(), "MIN + 1 input");
}

/// Execute the emitted wrapper, including its import and exported entry points.
/// The host must bind the exact validated program out of band.
fn execute_hosted_wrapper(
    artifact: &AotModule,
    program: &pinewasm_csil::CsilProgram,
    data: &MarketData,
    options: &RunOptions,
) -> pinewasm_vm::RunOutcome {
    assert_eq!(
        artifact.layout.execution_mode,
        ExecutionMode::InterpreterHosted
    );
    let engine = wasmi::Engine::default();
    let module = wasmi::Module::new(&engine, &artifact.wasm[..]).expect("valid hosted Wasm");
    let mut store = wasmi::Store::new(&engine, None::<pinewasm_vm::RunOutcome>);
    let mut linker = wasmi::Linker::new(&engine);
    let host_program = program.clone();
    let host_data = data.clone();
    let host_options = options.clone();
    linker
        .func_wrap(
            "host",
            "interpret",
            move |mut caller: wasmi::Caller<'_, Option<pinewasm_vm::RunOutcome>>| -> i32 {
                *caller.data_mut() = Some(run(&host_program, &host_data, &host_options));
                status::OK
            },
        )
        .expect("bind interpreter");
    let instance = linker
        .instantiate_and_start(&mut store, &module)
        .expect("instantiate hosted wrapper");
    let prepare = instance
        .get_typed_func::<i32, i32>(&store, "prepare")
        .expect("prepare export");
    assert_eq!(
        prepare.call(&mut store, data.bars() as i32).unwrap(),
        status::OK
    );
    let run_export = instance
        .get_typed_func::<(), i32>(&store, "run")
        .expect("run export");
    assert_eq!(run_export.call(&mut store, ()).unwrap(), status::OK);
    store.into_data().expect("host.interpret was called")
}

#[test]
fn exact_minimum_bytecode_stops_the_native_run_and_survives_in_the_interpreter() {
    use pinewasm_csil::CsilInstr as C;
    // Start with nonconstant arithmetic so the frontend cannot fold away the
    // opcode under test. Replacements preserve instruction count and jumps.
    for (lhs, rhs, arithmetic) in [
        (-9223372036854775807, -1, C::IntAdd),
        (-9223372036854775807, 1, C::IntSub),
        (-(1_i64 << 62), 2, C::IntMul),
        (i64::MIN, 0, C::IntAdd),
    ] {
        let mut program = compile_csil(concat!(
            "indicator(\"Exact minimum\")\n",
            "x = bar_index + 42\n",
            "var int held = na\n",
            "held := x\n",
            "float branch = 0.0\n",
            "if not na(held) and held < 0\n",
            "    branch := 1.0\n",
            "plot(branch)\n",
            "plot(not na(held[1]) and held[1] == held ? 1.0 : 0.0)\n",
            "plot(na(held) ? 1.0 : 0.0)\n",
        ));
        let code = &mut program.functions[0].code;
        let producer = code
            .windows(3)
            .position(|ops| {
                matches!(ops[0], C::ReadBuiltin { .. })
                    && matches!(ops[1], C::PushI64(42))
                    && matches!(ops[2], C::IntAdd)
            })
            .expect("nonconstant integer producer survives compilation");
        code[producer] = C::PushI64(lhs);
        code[producer + 1] = C::PushI64(rhs);
        code[producer + 2] = arithmetic;
        // The edit invalidates the cached budget metadata, so it is cleared here.
        program.functions[0].budget_reference = None;
        pinewasm_csil::validate(&program).expect("valid exact-opcode fixture");
        let artifact = guarded_native(&program, "exact minimum");
        // The module stops on the first bar, at the guarded producer: the
        // arithmetic, or the minimum literal that feeds it.
        let stopped = assert_present_min_stops(
            &artifact,
            &program,
            &synthetic_data(3),
            &RunOptions::default(),
            0,
            "exact minimum",
        );
        assert!(
            (producer..=producer + 2).contains(&(stopped.err_pc as usize)),
            "stopped at {} instead of the producer at {producer}",
            stopped.err_pc
        );
        // The interpreter, which the consumer reruns the request on, keeps
        // the minimum through slots, history and branches.
        let outcome = run(&program, &synthetic_data(3), &RunOptions::default());
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert_eq!(outcome.plots[0].values, vec![Value::Float(1.0); 3]);
        assert_eq!(
            outcome.plots[1].values,
            vec![Value::Float(0.0), Value::Float(1.0), Value::Float(1.0)]
        );
        assert_eq!(outcome.plots[2].values, vec![Value::Float(0.0); 3]);
    }
}

#[test]
fn straight_line_integer_boundary_gate_distinguishes_min_from_overflow_and_na() {
    use pinewasm_csil::CsilInstr as C;
    for (expression, expected, present_min) in [
        (vec![C::PushI64(i64::MIN)], Some(i64::MIN as f64), true),
        (
            vec![C::PushI64(-9223372036854775807), C::PushI64(-1), C::IntAdd],
            Some(i64::MIN as f64),
            true,
        ),
        (
            vec![C::PushI64(-9223372036854775807), C::PushI64(1), C::IntSub],
            Some(i64::MIN as f64),
            true,
        ),
        (
            vec![C::PushI64(-(1_i64 << 62)), C::PushI64(2), C::IntMul],
            Some(i64::MIN as f64),
            true,
        ),
        (vec![C::PushI64(i64::MAX)], Some(i64::MAX as f64), false),
        (vec![C::PushNa, C::PushI64(1), C::IntAdd], None, false),
        (
            vec![C::PushI64(i64::MAX), C::PushI64(1), C::IntAdd],
            None,
            false,
        ),
        (
            vec![C::PushI64(-9223372036854775807), C::PushI64(2), C::IntSub],
            None,
            false,
        ),
    ] {
        let mut program = compile_csil("indicator('Boundary')\nplot(close)\n");
        let main = &mut program.functions[0];
        let span = main.source_map[0];
        main.code = expression;
        main.code
            .extend([C::IntToF64, C::PlotF64 { plot_slot: 0 }, C::Return]);
        main.source_map = vec![span; main.code.len()];
        main.max_stack = 2;
        pinewasm_csil::validate(&program).unwrap();
        // Every case is native. Overflow and `na` are statically distinct
        // from a present minimum at these constant sites, so only the four
        // minimum producers carry a guard.
        let artifact = compile_program_to_wasm(&program).unwrap();
        assert_eq!(artifact.layout.execution_mode, ExecutionMode::Native);
        assert_eq!(int_min_guards(&program).is_empty(), !present_min);
        let data = synthetic_data(1);
        let reference = run(&program, &data, &RunOptions::default());
        assert!(reference.errors.is_empty(), "{:?}", reference.errors);
        let actual = if present_min {
            // The module stops; the value is the interpreter's.
            assert_present_min_stops(
                &artifact,
                &program,
                &data,
                &RunOptions::default(),
                0,
                "boundary",
            );
            match reference.plots[0].values.first() {
                Some(Value::Float(value)) => *value,
                value => panic!("expected float plot, got {value:?}"),
            }
        } else {
            let actual = run_aot(&artifact, &program, &data, &RunOptions::default());
            assert_eq!(actual.status, status::OK);
            assert_outcomes_match(&reference, &actual, data.bars(), "boundary");
            actual.plots[0][0]
        };
        match expected {
            Some(value) => assert_eq!(actual, value),
            None => assert!(actual.is_nan(), "overflow and na must stay missing"),
        }
    }
}

#[test]
fn safe_i64_max_literals_and_na_remain_native() {
    for source in [
        "indicator(\"Maximum integer\")\nplot(9223372036854775807)\n",
        "indicator(\"Integer na\")\nplot(na)\n",
    ] {
        let program = compile_csil(source);
        assert!(
            compile_program(&program).is_ok(),
            "safe admitted fixture was rejected: {source}"
        );
    }
}

#[test]
fn inputs_resolve_defaults_like_interpreter() {
    differential(
        "indicator(\"IN\")\nlen = input.int(14, \"Length\")\nsrc = input.source(close, \"Source\")\nmult = input.float(2.0, \"Mult\")\nuse_high = input.bool(false, \"High?\")\nbase = use_high ? high : src\nplot(ta.sma(base, len) * mult)\n",
        &synthetic_data(512),
    );
}

#[test]
fn styled_plot_colors_match_interpreter() {
    differential(
        "indicator(\"COL\")\nplot(close, color = close > open ? color.new(color.green, 20) : color.rgb(200, 30, 30, 10))\n",
        &synthetic_data(96),
    );
}

#[test]
fn out_of_range_color_channel_clamps_like_interpreter() {
    // A large-negative FINITE channel saturates to i64::MIN in the cast; the
    // interpreter clamps it to a valid channel (0), so the color must NOT
    // become na from the sentinel collision. Also exercise a huge-positive
    // channel (saturates high → clamps to 255/100).
    differential(
        "indicator(\"CLAMP\")\nplot(close, color = color.rgb(-1e30, 1e30, 128, -5))\n",
        &synthetic_data(48),
    );
}

#[test]
fn fill_band_colors_match_interpreter() {
    // A `fill(...)` band is color-only: its geometry is the two plots it
    // spans, so the whole per-bar output is the color channel. A gradient
    // (series color) is the case that made FillColor worth lowering.
    differential(
        "indicator(\"FILL\")\ntop = plot(high, \"Top\")\nbot = plot(low, \"Bot\")\nfill(top, bot, color = close > open ? color.new(color.green, 60) : color.new(color.red, 60))\n",
        &synthetic_data(96),
    );
}

#[test]
fn fill_lanes_follow_plot_and_visual_lanes() {
    // Lane-order regression: plot roles, then visual roles, then fill roles.
    // Mixing all three in one script is what catches an emitter/reader walk
    // that drifted: every family's channel has to land in its own lane.
    differential(
        concat!(
            "indicator(\"MIX\")\n",
            "top = plot(high, \"Top\", color = close > open ? color.green : color.red)\n",
            "bot = plot(low, \"Bot\")\n",
            "bgcolor(close > open ? color.new(color.blue, 80) : color.new(color.orange, 80))\n",
            "fill(top, bot, color = close > ta.sma(close, 5) ? color.new(color.teal, 70) : color.new(color.maroon, 70))\n",
        ),
        &synthetic_data(96),
    );
}

#[test]
fn multiple_fill_bands_keep_distinct_lanes() {
    // Two bands over three plots: slot 0's channel must not leak into slot 1.
    differential(
        concat!(
            "indicator(\"BANDS\")\n",
            "base = plot(0.0, \"Base\")\n",
            "a = plot(close - open, \"A\")\n",
            "b = plot(high - low, \"B\")\n",
            "fill(base, a, color = close > open ? color.new(color.green, 50) : color.new(color.red, 50))\n",
            "fill(base, b, color = high > close[1] ? color.new(color.aqua, 90) : color.new(color.fuchsia, 90))\n",
        ),
        &synthetic_data(64),
    );
}

#[test]
fn fill_band_with_na_color_matches_interpreter() {
    // An `na` color must reach the lane as the -1 sentinel, not a black
    // pixel: bars where the condition is false record no color at all.
    differential(
        concat!(
            "indicator(\"NAFILL\")\n",
            "top = plot(high, \"Top\")\n",
            "bot = plot(low, \"Bot\")\n",
            "fill(top, bot, color = close > open ? color.new(color.green, 40) : na)\n",
        ),
        &synthetic_data(48),
    );
}

#[test]
fn nz_and_na_match_interpreter() {
    differential(
        "indicator(\"NZ\")\nplot(nz(close[500], -1.0))\nplot(na(close[500]) ? 1.0 : 0.0)\nplot(fixnan(close[500]))\n",
        &synthetic_data(40),
    );
}

#[test]
fn integer_fixnan_matches_interpreter() {
    differential(
        "indicator(\"fixnan int\")\nint x = fixnan(bar_index % 3 == 0 ? na : bar_index)\nplot(x)\n",
        &synthetic_data(40),
    );
}

#[test]
fn history_limit_error_matches_interpreter() {
    // A dynamic offset beyond max_bars_back must abort both engines with
    // the same structured error on the same bar.
    let source = "indicator(\"H\")\nn = bar_index\nplot(close[n])\n";
    let program = compile_csil(source);
    let mbb = program.metadata.max_bars_back as usize;
    let data = synthetic_data(mbb + 8);

    let outcome = run(&program, &data, &RunOptions::default());
    assert_eq!(outcome.errors.len(), 1, "interpreter must abort past mbb");
    let interp_bar = outcome.errors[0].bar_index.expect("bar index");

    let aot = compile_program(&program).unwrap();
    let aot_run = run_aot(&aot, &program, &data, &RunOptions::default());
    assert_eq!(aot_run.status, status::HISTORY_LIMIT);
    assert_eq!(aot_run.err_bar, interp_bar);
    // Bars before the abort keep their recorded output.
    for bar in 0..interp_bar as usize {
        let expected = outcome
            .plots
            .first()
            .and_then(|plot| plot.values.get(bar))
            .expect("interpreter plot contains every completed bar");
        match expected {
            Value::Float(x) => assert_eq!(x.to_bits(), aot_run.plots[0][bar].to_bits()),
            Value::Na => assert!(aot_run.plots[0][bar].is_nan()),
            other => panic!("unexpected {other:?}"),
        }
    }
}

// ---- fallback classification -------------------------------------------------

#[test]
fn unsupported_programs_fall_back_by_name() {
    // Typed collections live in linear memory: a pure-array program needs no
    // VM host at all. Strings and drawings still
    // compile natively with `host.vm` sites.
    let arrays = compile_csil(
        "indicator(\"ARR\")\nvar prices = array.new_float(0)\narray.push(prices, close)\nplot(array.size(prices))\n",
    );
    let arrays = compile_program(&arrays).expect("collections lower natively");
    assert!(arrays.layout.native_heap);
    assert!(!arrays.layout.imports_vm);
    let strings = compile_csil(
        "indicator(\"S\")\nplot(close, title = \"x\")\nlabel.new(bar_index, close, \"hi\")\n",
    );
    assert!(compile_program(&strings).is_ok());

    // A strategy's results come back from the broker on the VM host. One that
    // never reaches the host has no broker to report from, and
    // a declaration that recalculates within a bar needs the interpreter's
    // bar loop; both name the fallback so neither silently loses its report.
    let strategy = compile_csil("strategy(\"ST\")\nplot(close)\n");
    assert!(matches!(
        compile_program(&strategy),
        Err(AotError::StrategyRequiresInterpreter)
    ));
    let strategy = compile_csil(
        "strategy(\"ST\", calc_on_order_fills = true)\nif bar_index == 0\n    strategy.entry(\"L\", strategy.long)\nplot(close)\n",
    );
    assert!(matches!(
        compile_program(&strategy),
        Err(AotError::StrategyRequiresInterpreter)
    ));
}

#[test]
fn strategies_reaching_the_host_broker_are_native_and_the_rest_hosted() {
    // The native result ABI has no StrategyOutcome section: the results come
    // back from the broker on the VM host. A strategy whose orders,
    // getters or trade reads reach that host is native.
    let native = [
        (
            "order-bearing",
            "strategy(\"ORDER\")\nstrategy.entry(\"L\", strategy.long)\nplot(close)\n",
        ),
        (
            "strategy-getter",
            "strategy(\"GETTER\")\nplot(strategy.position_size)\n",
        ),
        (
            "trade-accessor",
            "strategy(\"TRADE\")\nplot(strategy.closedtrades.entry_bar_index(0))\n",
        ),
    ];
    for (name, source) in native {
        let program = compile_csil(source);
        let module = compile_program_to_wasm(&program)
            .unwrap_or_else(|error| panic!("{name} strategy must compile: {error}"));
        assert_eq!(
            module.layout.execution_mode,
            ExecutionMode::Native,
            "{name} strategy"
        );
        assert!(module.layout.host_broker, "{name} strategy");
        assert!(module.layout.imports_vm, "{name} strategy");
        assert!(!module.layout.links_broker, "{name} strategy");
    }

    // A plot-only strategy never reaches the host, so it would have no broker
    // to report from; recalculating within a bar or skipping early bars needs
    // the interpreter's bar loop. The declaration names the fallback, so
    // none of them silently loses its report.
    let cases = [
        ("plot-only", "strategy(\"PLOT\")\nplot(close)\n"),
        (
            "calc_on_order_fills",
            "strategy(\"FILLS\", calc_on_order_fills = true)\nstrategy.entry(\"L\", strategy.long)\nplot(close)\n",
        ),
        (
            "calc_on_every_history_tick",
            "strategy(\"TICKS\", calc_on_every_history_tick = true)\nstrategy.entry(\"L\", strategy.long)\nplot(close)\n",
        ),
        (
            "calc_bars_count",
            "strategy(\"WINDOW\", calc_bars_count = 50)\nstrategy.entry(\"L\", strategy.long)\nplot(close)\n",
        ),
    ];
    let expected_blocker = AotError::StrategyRequiresInterpreter.to_string();

    for (name, source) in cases {
        let program = compile_csil(source);
        assert!(
            matches!(
                compile_program(&program),
                Err(AotError::StrategyRequiresInterpreter)
            ),
            "{name} strategy must be rejected by the native tier"
        );

        let hosted = compile_program_to_wasm(&program)
            .unwrap_or_else(|error| panic!("{name} strategy must remain exportable: {error}"));
        assert_eq!(
            hosted.layout.execution_mode,
            ExecutionMode::InterpreterHosted,
            "{name} strategy"
        );
        assert!(hosted.layout.imports_interpreter, "{name} strategy");
        assert_eq!(
            hosted.layout.native_blocker.as_deref(),
            Some(expected_blocker.as_str()),
            "{name} strategy"
        );
        wasmparser::validate(&hosted.wasm)
            .unwrap_or_else(|error| panic!("{name} hosted artifact is invalid: {error}"));
    }
}

#[test]
fn universal_wasm_keeps_native_tier_and_hosts_unsupported_surfaces() {
    let native = compile_program_to_wasm(&compile_csil("indicator(\"N\")\nplot(close)\n")).unwrap();
    assert_eq!(native.layout.execution_mode, ExecutionMode::Native);
    assert!(!native.layout.imports_interpreter);
    assert!(native.layout.native_blocker.is_none());

    let strategy = compile_csil(
        "strategy(\"ST\", calc_on_order_fills = true)\nif bar_index == 0\n    strategy.entry(\"L\", strategy.long)\nplot(close)\n",
    );
    let hosted = compile_program_to_wasm(&strategy).expect("valid CSIL always emits wasm");
    assert_eq!(
        hosted.layout.execution_mode,
        ExecutionMode::InterpreterHosted
    );
    assert!(hosted.layout.imports_interpreter);
    let expected_blocker = AotError::StrategyRequiresInterpreter.to_string();
    assert_eq!(
        hosted.layout.native_blocker.as_deref(),
        Some(expected_blocker.as_str()),
    );
    wasmparser::validate(&hosted.wasm).expect("hosted artifact is valid WebAssembly");

    // Execute the actual Wasm entry point and prove it invokes the host once.
    let engine = wasmi::Engine::default();
    let module = wasmi::Module::new(&engine, &hosted.wasm[..]).unwrap();
    let mut store = wasmi::Store::new(&engine, 0u32);
    let mut linker = wasmi::Linker::new(&engine);
    linker
        .func_wrap(
            "host",
            "interpret",
            |mut caller: wasmi::Caller<'_, u32>| -> i32 {
                *caller.data_mut() += 1;
                status::OK
            },
        )
        .unwrap();
    let instance = linker
        .instantiate_and_start(&mut store, &module)
        .expect("hosted module instantiates");
    let prepare = instance
        .get_typed_func::<i32, i32>(&store, "prepare")
        .unwrap();
    assert_eq!(prepare.call(&mut store, 64).unwrap(), status::OK);
    let run = instance.get_typed_func::<(), i32>(&store, "run").unwrap();
    assert_eq!(run.call(&mut store, ()).unwrap(), status::OK);
    assert_eq!(*store.data(), 1, "Wasm run must cross host.interpret once");
}

fn collect_pine_files(root: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in
        std::fs::read_dir(root).unwrap_or_else(|error| panic!("{}: {error}", root.display()))
    {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            collect_pine_files(&path, out);
        } else if path
            .extension()
            .is_some_and(|extension| extension == "pine")
        {
            out.push(path);
        }
    }
}

#[test]
fn every_compiler_supported_public_fixture_produces_valid_wasm() {
    let mut files = Vec::new();
    collect_pine_files(std::path::Path::new("../../tests/compile-pass"), &mut files);
    files.sort();

    let mut compiler_supported = 0usize;
    let mut native = 0usize;
    let mut hosted = 0usize;
    for path in &files {
        let source = std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let result = compile(&source, CompileOptions::default());
        let Some(program) = result.program else {
            // A fixture that needs separately supplied modules is not
            // compiler-supported under this isolated invocation and therefore
            // does not enter the executable invariant.
            continue;
        };
        if result
            .diagnostics
            .iter()
            .any(pinewasm_span::Diagnostic::is_error)
        {
            continue;
        }
        compiler_supported += 1;
        let module = compile_program_to_wasm(&program)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        wasmparser::validate(&module.wasm)
            .unwrap_or_else(|error| panic!("{}: invalid wasm: {error}", path.display()));
        match module.layout.execution_mode {
            ExecutionMode::Native => native += 1,
            ExecutionMode::InterpreterHosted => hosted += 1,
        }
    }

    eprintln!(
        "[aot-fixtures] compiler-supported={compiler_supported} native={native} hosted={hosted}"
    );
    assert!(
        compiler_supported >= 48,
        "fixture coverage unexpectedly shrank to {compiler_supported} programs"
    );
    assert!(
        native >= 25,
        "native lowering regressed to {native}; the MIN safety gate should only \
         demote integer-domain-sensitive programs"
    );
    assert!(
        hosted > 0,
        "the universal hosted tier must remain exercised"
    );
}

/// Every `module.name` an emitted artifact asks its host for.
fn imports_of(wasm: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        if let wasmparser::Payload::ImportSection(reader) = payload.expect("well-formed") {
            for group in reader {
                for entry in group.expect("well-formed import") {
                    let (_, import) = entry.expect("well-formed import item");
                    names.push(format!("{}.{}", import.module, import.name));
                }
            }
        }
    }
    names
}

#[test]
fn emitted_module_imports_match_needs() {
    // Transcendentals and `%` link the numeric runtime, and a fired
    // alertcondition writes an output lane. Neither needs a host import.
    for (label, source) in [
        ("plain", "indicator(\"P\")\nplot(close)\n"),
        ("exp", "indicator(\"E\")\nplot(math.exp(close / 100))\n"),
        ("pow", "indicator(\"W\")\nplot(math.pow(close, 1.5))\n"),
        ("round2", "indicator(\"D\")\nplot(math.round(close, 2))\n"),
        ("remainder", "indicator(\"R\")\nplot(close % 3.0)\n"),
        (
            "alertcondition",
            "indicator(\"A\")\nalertcondition(close > open, \"Up\", \"up bar\")\nplot(close)\n",
        ),
        (
            "mixed",
            "indicator(\"M\")\nplot(math.log(close) % math.sin(high))\n",
        ),
        // A `fill(...)` band is a per-bar color sink like a styled plot: an
        // i32 store into its own lane, never a host call.
        (
            "fill",
            "indicator(\"F\")\ntop = plot(high)\nbot = plot(low)\nfill(top, bot, color = close > open ? color.new(color.green, 50) : na)\n",
        ),
    ] {
        let module = compile_program(&compile_csil(source)).unwrap();
        assert_eq!(
            imports_of(&module.wasm),
            Vec::<String>::new(),
            "{label}: artifact must be import-free"
        );
    }

    // The TA engine is linked into the emitted module, so a `ta.*` program
    // imports nothing.
    let ta = compile_program(&compile_csil("indicator(\"T\")\nplot(ta.ema(close, 9))\n")).unwrap();
    assert!(ta.layout.links_ta);
    assert_eq!(
        imports_of(&ta.wasm),
        Vec::<String>::new(),
        "a ta program must be import-free"
    );
    // The engine ships in the same blob as the transcendentals, so a ta
    // program links the runtime whether or not it calls one.
    assert!(ta.layout.links_runtime);
    let ta_and_pure = compile_program(&compile_csil(
        "indicator(\"TP\")\nplot(math.log(ta.ema(close, 9)))\n",
    ))
    .unwrap();
    assert_eq!(imports_of(&ta_and_pure.wasm), Vec::<String>::new());
    assert!(ta_and_pure.layout.links_runtime);

    // The layout flags stay honest about which mechanism a program reached for.
    let plain = compile_program(&compile_csil("indicator(\"P\")\nplot(close)\n")).unwrap();
    assert!(!plain.layout.links_ta && !plain.layout.links_runtime);
    for source in [
        "indicator(\"E\")\nplot(math.exp(close / 100))\n",
        "indicator(\"R\")\nplot(close % 3.0)\n",
    ] {
        assert!(
            compile_program(&compile_csil(source))
                .unwrap()
                .layout
                .links_runtime
        );
    }
}

// ---- compile-pass fixtures ---------------------------------------------------------

/// Compile a compile-pass fixture and, when it is inside the AOT subset,
/// require a bit-exact differential against the interpreter on the shared
/// feed. Returns whether the fixture compiled natively (for the coverage
/// floor assertion); interpreter-only fixtures are reported and skipped, as
/// is any fixture that stops compiling standalone (pipeline.rs owns that
/// guarantee).
fn fixture_differential(path: &std::path::Path, data: &MarketData) -> bool {
    let file = path.file_name().unwrap().to_string_lossy();
    let source =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let result = compile(&source, CompileOptions::default());
    if result
        .diagnostics
        .iter()
        .any(pinewasm_span::Diagnostic::is_error)
    {
        eprintln!("[aot-fixture] {file}: not standalone-compilable, skipped");
        return false;
    }
    let program = result.program.expect("program");
    let aot = compile_program_to_wasm(&program)
        .unwrap_or_else(|reason| panic!("{file}: no valid Wasm tier: {reason}"));
    wasmparser::validate(&aot.wasm).unwrap_or_else(|e| panic!("{file}: invalid module: {e}"));
    let options = RunOptions::default();
    let outcome = run(&program, data, &options);
    assert!(
        outcome.errors.is_empty(),
        "{file}: interpreter errors: {:?}",
        outcome.errors
    );
    let aot_run = run_aot(&aot, &program, data, &options);
    if let Some(reason) = &aot_run.mismatch {
        // A dynamically-typed value escaped its static cell at run time; the
        // production loader reruns the request on the interpreter (exact).
        eprintln!("[aot-sample] {file}: runtime interpreter fallback: {reason}");
        return false;
    }
    assert_eq!(
        aot_run.status,
        status::OK,
        "{file}: {:?}",
        aot_run.host_error
    );
    assert_outcomes_match(&outcome, &aot_run, data.bars(), &file);
    assert_alerts_match(&outcome, &aot_run, &program, &file);
    assert_eq!(outcome.logs, aot_run.vm_logs, "{file}: log stream");
    if let Some(drawings) = &aot_run.vm_drawings {
        assert_eq!(&outcome.drawings, drawings, "{file}: drawings");
    }
    match aot.layout.execution_mode {
        ExecutionMode::Native => eprintln!(
            "[aot-fixture] {file}: native AOT bit-exact over {} bars",
            data.bars()
        ),
        ExecutionMode::InterpreterHosted => eprintln!(
            "[aot-fixture] {file}: hosted VM exact over {} bars",
            data.bars()
        ),
    }
    true
}

#[test]
fn fixtures_differential() {
    let mut files = Vec::new();
    collect_pine_files(std::path::Path::new("../../tests/compile-pass"), &mut files);
    files.sort();
    // 500 bars, not more: dynamic_history_offset.pine reads `close[bar_index]`,
    // so any longer feed trips the max_bars_back (500) runtime limit in both
    // engines instead of producing comparable outputs.
    let data = synthetic_data(500);
    let compiled: Vec<String> = files
        .iter()
        .filter(|path| fixture_differential(path, &data))
        .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    // Coverage floor: numeric, engine-backed, collection, UDT, and
    // alert/table surfaces must remain compilable and bit-exact using fixtures
    // that ship in this repository.
    for must in [
        "bollinger.pine",
        "ta_engine.pine",
        "arrays_core.pine",
        "udt.pine",
        "table_alerts.pine",
    ] {
        assert!(
            compiled.iter().any(|file| file == must),
            "{must} regressed out of the AOT subset (compiled: {compiled:?})"
        );
    }
}

// ---- the native heap -------------------------------------------------------

/// Compile + assert the native-heap gate passed (so these tests can never
/// silently regress to `host.vm` sites), then run the standard differential.
fn native_heap_differential(source: &str, data: &MarketData, context: &str) {
    let program = compile_csil(source);
    let aot = compile_program(&program)
        .unwrap_or_else(|e| panic!("{context}: inside the AOT subset: {e}"));
    assert!(
        aot.layout.native_heap,
        "{context}: expected the native heap, blocker: {:?}",
        aot.layout.native_blocker
    );
    assert!(
        !aot.layout.vm_heap_residual,
        "{context}: residual heap sites"
    );
    differential_with(source, data, context);
}

/// Compile + assert the module keeps BOTH heaps: scalar arrays and plain
/// objects in linear memory, registered drawings on `host.vm`. This is the
/// shape where the two engines allocate different numbers of interpreter heap
/// objects, so everything a host can read about a drawing has to be
/// independent of the handle it was given. Returns the interpreter outcome
/// after the standard differential, drawings and ids included.
fn native_heap_with_drawings_differential(
    source: &str,
    data: &MarketData,
    context: &str,
) -> pinewasm_vm::RunOutcome {
    let program = compile_csil(source);
    let aot = compile_program(&program)
        .unwrap_or_else(|e| panic!("{context}: inside the AOT subset: {e}"));
    assert!(
        aot.layout.native_heap && aot.layout.imports_vm,
        "{context}: expected a native heap beside hosted drawings, blocker: {:?}",
        aot.layout.native_blocker
    );
    let outcome = differential_program(&program, data, context);
    assert!(
        !outcome.drawings.is_empty(),
        "{context}: the program must leave drawings to compare"
    );
    outcome
}

fn drawing_ids(group: &[pinewasm_vm::DrawingObject]) -> Vec<u32> {
    group.iter().map(|drawing| drawing.id).collect()
}

#[test]
fn drawing_ids_match_when_objects_live_on_the_native_heap() {
    // The interpreter gives `T.new` a heap handle on every bar; the module
    // keeps that object in linear memory. A drawing id taken from the handle
    // read 1, 3, 5 on the interpreter and 0, 1, 2 from the module.
    let source = "//@version=6\nindicator(\"ids\", overlay=true)\n\
                  type T\n    int a\n    float b\n\
                  k = bar_index * 0.5 + 1\n\
                  t = T.new(int(k), k)\n\
                  label.new(bar_index, close, str.tostring(t.a))\n\
                  plot(close)\n";
    let outcome =
        native_heap_with_drawings_differential(source, &synthetic_data(3), "udt beside labels");
    assert_eq!(drawing_ids(&outcome.drawings.labels), [0, 1, 2]);
}

#[test]
fn drawing_ids_match_when_arrays_live_on_the_native_heap() {
    // Arrays take interpreter handles too, and a linefill names its two lines
    // by id: the ids and the references must both survive native allocations
    // made between the drawings.
    let source = "//@version=6\nindicator(\"fill ids\", overlay=true)\n\
                  type T\n    float v\n\
                  a = array.new_float(0)\n\
                  array.push(a, close)\n\
                  l1 = line.new(bar_index, high, bar_index + 1, high)\n\
                  t = T.new(open)\n\
                  l2 = line.new(bar_index, low, bar_index + 1, low)\n\
                  linefill.new(l1, l2, color.new(color.blue, 80))\n\
                  plot(array.sum(a) + t.v)\n";
    let outcome =
        native_heap_with_drawings_differential(source, &synthetic_data(3), "arrays beside lines");
    assert_eq!(drawing_ids(&outcome.drawings.lines), [0, 1, 3, 4, 6, 7]);
    assert_eq!(drawing_ids(&outcome.drawings.linefills), [2, 5, 8]);
    let refs: Vec<_> = outcome
        .drawings
        .linefills
        .iter()
        .map(|fill| fill.line_refs)
        .collect();
    assert_eq!(
        refs,
        [
            Some([Some(0), Some(1)]),
            Some([Some(3), Some(4)]),
            Some([Some(6), Some(7)])
        ]
    );
}

#[test]
fn drawing_reads_deletes_and_eviction_match_beside_a_native_heap() {
    // What a script can do with a drawing reference while the native heap
    // stays planned: test an earlier bar's for na, delete this bar's or an
    // earlier one, and let the live cap evict the oldest. A box and a table
    // sit in the same id sequence. (Reading a drawing back with a getter
    // moves the whole program's heap to the host, where handles agree.)
    let source = "//@version=6\nindicator(\"refs\", overlay=true, max_labels_count=3)\n\
                  type T\n    float v\n\
                  t = T.new(close)\n\
                  a = array.new_float(2, high)\n\
                  lb = label.new(bar_index, t.v, \"x\")\n\
                  extra = label.new(bar_index, low, \"extra\")\n\
                  if bar_index % 2 == 0\n    label.delete(extra)\n\
                  box.new(bar_index, high, bar_index + 1, low)\n\
                  var table tb = table.new(position.top_right, 1, 1)\n\
                  table.cell(tb, 0, 0, str.tostring(t.v))\n\
                  label.delete(lb[1])\n\
                  plot(array.sum(a) + t.v)\n\
                  plot(na(lb[1]) ? 1 : 0)\n";
    let outcome =
        native_heap_with_drawings_differential(source, &synthetic_data(5), "label references");
    // Bar 0: labels 0 and 1, box 2, table 3. Later bars: two labels and a box.
    // Bar 4 registered 13, then 14 (which evicted 10 at the cap), deleted 14.
    assert_eq!(drawing_ids(&outcome.drawings.labels), [11, 13]);
    assert_eq!(drawing_ids(&outcome.drawings.boxes), [2, 6, 9, 12, 15]);
    assert_eq!(drawing_ids(&outcome.drawings.tables), [3]);
}

#[test]
fn linefill_pair_replacement_matches_hosted_wasm_beside_native_allocations() {
    for pair in ["a, b", "b, a"] {
        // Native UDT/array allocations change heap handles but must not alter
        // the drawing ids, pair identity or latest fill's reference order.
        let source = format!(
            r#"//@version=6
indicator("Native fill replacement")
type Sample
    float value
sample = Sample.new(close)
values = array.new_float(2, sample.value)
a = line.new(bar_index, high, bar_index + 1, high)
b = line.new(bar_index, low, bar_index + 1, low)
linefill.new(a, b, color.red)
linefill.new({pair}, color.blue)
plot(array.sum(values))
"#
        );
        let outcome = native_heap_with_drawings_differential(&source, &synthetic_data(3), pair);
        assert_eq!(drawing_ids(&outcome.drawings.linefills), [3, 7, 11]);
        for (index, fill) in outcome.drawings.linefills.iter().enumerate() {
            let first = (index * 4) as u32;
            let refs = if pair == "a, b" {
                [Some(first), Some(first + 1)]
            } else {
                [Some(first + 1), Some(first)]
            };
            assert_eq!(fill.line_refs, Some(refs));
        }
    }
}

#[test]
fn linefill_boundary_deletion_and_eviction_match_hosted_wasm_beside_native_allocations() {
    for deletion in ["", "line.delete(a)", "line.delete(b)"] {
        let source = format!(
            r#"//@version=6
indicator("Native fill deletion", max_lines_count=3)
values = array.new_float(2, close)
a = line.new(bar_index, high, bar_index + 1, high)
b = line.new(bar_index, low, bar_index + 1, low)
linefill.new(a, b, color.blue)
if bar_index == 2
    {deletion}
    values.set(0, high)
plot(array.sum(values))
"#
        );
        let outcome = native_heap_with_drawings_differential(&source, &synthetic_data(3), deletion);
        if deletion.is_empty() {
            assert_eq!(drawing_ids(&outcome.drawings.linefills), [8]);
            assert_eq!(
                outcome.drawings.linefills[0].line_refs,
                Some([Some(6), Some(7)])
            );
        } else {
            assert!(outcome.drawings.linefills.is_empty(), "{deletion}");
        }
    }
}

#[test]
fn linefill_script_visible_live_array_and_stale_delete_match_hosted_wasm() {
    let source = r#"//@version=6
indicator("Live fill array")
var a = line.new(0, 1, 1, 2)
var b = line.new(0, 3, 1, 4)
old = linefill.new(a, b, color.red)
latest = linefill.new(b, a, color.blue)
plot(array.size(linefill.all))
linefill.delete(old)
plot(array.size(linefill.all))
plot(line.get_y1(linefill.get_line1(latest)))
"#;
    let outcome = differential_program(
        &compile_csil(source),
        &synthetic_data(3),
        "script-visible fill replacement",
    );
    assert_eq!(drawing_ids(&outcome.drawings.linefills), [7]);
    for plot in &outcome.plots[..2] {
        assert_eq!(plot.values, vec![Value::Float(1.0); 3]);
    }
}

#[test]
fn a_script_cannot_read_a_drawing_or_object_handle() {
    // The handle a drawing or a user object holds differs between the
    // interpreter and a module with a native heap. That is only sound while
    // no script can turn one into a value: the checker refuses every route
    // from a reference to a number, a string, or an ordering.
    let header = "//@version=6\nindicator(\"h\", overlay=true)\n\
                  type T\n    float v\n\
                  t = T.new(close)\n\
                  lb = label.new(bar_index, close, \"x\")\n\
                  all = label.all\n";
    for body in [
        "plot(int(lb))\n",
        "plot(float(lb))\n",
        "plot(lb == lb[1] ? 1 : 0)\n",
        "plot(lb != lb[1] ? 1 : 0)\n",
        "plot(lb > lb[1] ? 1 : 0)\n",
        "plot(t == t[1] ? 1 : 0)\n",
        "plot(lb + 1)\n",
        "plot(lb)\n",
        "label.set_text(lb, str.tostring(lb))\nplot(close)\n",
        "label.set_text(lb, str.tostring(t))\nplot(close)\n",
        "label.set_text(lb, str.format(\"{0}\", lb))\nplot(close)\n",
        "log.info(\"{0}\", lb)\nplot(close)\n",
        "log.info(array.join(all, \",\"))\nplot(close)\n",
        "array.sort(all)\nplot(close)\n",
        "plot(array.max(all))\n",
    ] {
        let source = format!("{header}{body}");
        let result = compile(&source, CompileOptions::default());
        assert!(
            result
                .diagnostics
                .iter()
                .any(pinewasm_span::Diagnostic::is_error),
            "a script must not be able to observe a handle:\n{body}"
        );
    }
}

/// Run `source` on both engines where the module is expected to hand the
/// request back: its status must be `HEAP_REROUTE` with no host error left
/// behind, so a loader reruns on the interpreter and reports whatever the
/// interpreter reports. Returns the artifact and the interpreter outcome.
fn heap_budget_reroute(
    source: &str,
    bars: usize,
    context: &str,
) -> (pinewasm_aot::AotModule, pinewasm_vm::RunOutcome) {
    let data = synthetic_data(bars);
    let program = compile_csil(source);
    let aot = compile_program_to_wasm(&program)
        .unwrap_or_else(|e| panic!("{context}: no valid Wasm tier: {e}"));
    assert_eq!(
        aot.layout.execution_mode,
        ExecutionMode::Native,
        "{context}"
    );
    let options = RunOptions::default();
    let outcome = run(&program, &data, &options);
    let aot_run = run_aot(&aot, &program, &data, &options);
    assert_eq!(
        aot_run.status,
        status::HEAP_REROUTE,
        "{context}: the module must hand a spent heap budget to the interpreter, host error: {:?}",
        aot_run.host_error
    );
    assert!(
        aot_run.host_error.is_none(),
        "{context}: a reroute carries no error of its own"
    );
    (aot, outcome)
}

fn heap_limit_error(outcome: &pinewasm_vm::RunOutcome) -> Option<&str> {
    outcome
        .errors
        .first()
        .filter(|error| error.kind == pinewasm_vm::RuntimeErrorKind::CollectionLimitExceeded)
        .map(|error| error.message.as_str())
}

#[test]
fn hosted_drawing_fields_count_against_the_native_element_budget() {
    // 99,990 native elements and one label's 14 fields are 100,004 elements
    // on the interpreter's single heap: over the 100,000 budget on bar 0. The
    // module keeps the array and the label on two heaps, each under budget,
    // and the run must stop with an element-limit error instead of finishing
    // with three labels.
    let source = "//@version=6\nindicator(\"split elements\", overlay=true)\n\
                  var a = array.new_float(99990, 0.0)\n\
                  label.new(bar_index, close, \"x\")\n\
                  plot(array.size(a))\n";
    let (aot, outcome) = heap_budget_reroute(source, 3, "split element budget");
    assert!(aot.layout.native_heap && aot.layout.imports_vm);
    assert!(
        heap_limit_error(&outcome).is_some_and(|message| message.contains("element limit")),
        "{:?}",
        outcome.errors
    );
}

#[test]
fn hosted_drawings_count_against_the_native_object_budget() {
    // 999,999 native arrays and two labels are 1,000,001 objects on the
    // interpreter's single heap. The module must stop the run at the object limit.
    let source = "//@version=6\nindicator(\"split objects\", overlay=true)\n\
                  if bar_index == 0\n    for i = 0 to 999998\n        array.new_float(0)\n\
                  label.new(bar_index, close, \"x\")\n\
                  label.new(bar_index, open, \"y\")\n\
                  plot(close)\n";
    let (aot, outcome) = heap_budget_reroute(source, 2, "split object budget");
    assert!(aot.layout.native_heap && aot.layout.imports_vm);
    assert!(
        heap_limit_error(&outcome).is_some_and(|message| message.contains("object limit")),
        "{:?}",
        outcome.errors
    );
}

#[test]
fn a_spent_host_heap_budget_reroutes_instead_of_failing_the_run() {
    // A label a bar. The interpreter collects each label the live cap
    // evicts, so the run is clean at any length. With the program's heap all
    // on the host (a getter keeps it there) the module has no collector and
    // its host never collects: after 7,142 labels of 14 fields its element
    // budget is spent. Without a reroute that would surface as a
    // `CollectionLimitExceeded` host error the interpreter never raises.
    let hosted = "//@version=6\nindicator(\"host budget\", overlay=true)\n\
                  lb = label.new(bar_index, close, \"x\")\n\
                  plot(label.get_x(lb))\n";
    let (aot, outcome) = heap_budget_reroute(hosted, 8000, "host heap budget");
    assert!(aot.layout.imports_vm);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(outcome.drawings.labels.len(), 50);
}

#[test]
fn a_collecting_module_keeps_its_host_heap_inside_the_budget() {
    // The same label a bar beside a native heap: the module's collector has
    // its host collect the evicted labels, as the interpreter does, so the
    // module runs every bar itself and matches the interpreter.
    let beside_native = "//@version=6\nindicator(\"host budget\", overlay=true)\n\
                         var a = array.new_float(1, 0.0)\n\
                         label.new(bar_index, close, \"x\")\n\
                         plot(array.size(a))\n";
    let program = compile_csil(beside_native);
    let aot = compile_program(&program).expect("inside the AOT subset");
    assert!(aot.layout.native_heap && aot.layout.imports_vm);
    let outcome =
        differential_program(&program, &synthetic_data(8000), "host budget beside native");
    assert_eq!(outcome.drawings.labels.len(), 50);
}

/// Run `program` on both engines at element limit `limit` and require the
/// module to count exactly what the interpreter counts. Where the interpreter
/// stops with `CollectionLimitExceeded`, the module hands the request back
/// (`HEAP_REROUTE`) at the same bar and instruction; where it finishes, the
/// module finishes natively with the same plots, drawings and logs. Returns
/// whether the interpreter finished.
fn collector_agrees_at(
    program: &pinewasm_csil::CsilProgram,
    aot: &AotModule,
    data: &MarketData,
    limit: usize,
    context: &str,
) -> bool {
    let options = RunOptions {
        max_collection_elements: limit,
        ..RunOptions::default()
    };
    let outcome = run(program, data, &options);
    let actual = run_aot(aot, program, data, &options);
    let context = format!("{context}, element limit {limit}");
    match outcome.errors.first() {
        None => {
            assert_eq!(
                actual.status,
                status::OK,
                "{context}: the interpreter finished and the module did not \
                 (bar {}, pc {}, host error {:?})",
                actual.err_bar,
                actual.err_pc,
                actual.host_error
            );
            assert_outcomes_match(&outcome, &actual, data.bars(), &context);
            assert_host_streams_match(&outcome, &actual, &context);
            true
        }
        Some(error) => {
            assert_eq!(
                error.kind,
                pinewasm_vm::RuntimeErrorKind::CollectionLimitExceeded,
                "{context}: {error:?}"
            );
            assert_eq!(
                (actual.status, actual.host_error.as_ref()),
                (status::HEAP_REROUTE, None),
                "{context}: the interpreter stopped at bar {:?}, pc {:?}",
                error.bar_index,
                error.instruction_offset
            );
            assert_eq!(
                (Some(actual.err_bar), Some(actual.err_pc)),
                (error.bar_index, error.instruction_offset),
                "{context}: the module ran out at another (bar, pc) than the interpreter"
            );
            false
        }
    }
}

/// [`collector_agrees_at`] at every element limit from 1 to `max_limit`,
/// for a native-heap module with a collector. The interpreter must finish
/// at `max_limit`; each fixture charges more than that over its run, so
/// finishing there takes collections. Returns how many limits the
/// interpreter finished at.
fn collector_agrees_at_every_limit(
    source: &str,
    bars: usize,
    max_limit: usize,
    context: &str,
) -> usize {
    collector_agrees_at_limits(source, bars, 1..=max_limit, context)
}

/// Element limits from `low` to `high` in steps of `step`, then 1,000,000,
/// which the interpreter finishes for every fixture that uses them.
fn limits_from(low: usize, high: usize, step: usize) -> impl Iterator<Item = usize> {
    (low..=high).step_by(step).chain([1_000_000])
}

/// [`collector_agrees_at`] at each of `limits`, for a native-heap module
/// with a collector. The interpreter must stop at one of them and finish at
/// the last. Returns how many limits the interpreter finished at.
fn collector_agrees_at_limits(
    source: &str,
    bars: usize,
    limits: impl IntoIterator<Item = usize>,
    context: &str,
) -> usize {
    let program = compile_csil(source);
    let aot = compile_program(&program)
        .unwrap_or_else(|e| panic!("{context}: inside the AOT subset: {e}"));
    assert!(
        aot.layout.native_heap && !aot.layout.vm_heap_residual,
        "{context}: expected the native heap, blocker: {:?}",
        aot.layout.native_blocker
    );
    let plan = analyze(&program)
        .expect("the program lowers natively")
        .heap
        .expect("a native heap plan");
    assert!(plan.gc.is_some(), "{context}: the module must collect");
    let data = synthetic_data(bars);
    let verdicts: Vec<(usize, bool)> = limits
        .into_iter()
        .map(|limit| {
            let finished = collector_agrees_at(&program, &aot, &data, limit, context);
            (limit, finished)
        })
        .collect();
    let finished = verdicts.iter().filter(|(_, finished)| *finished).count();
    assert!(
        finished < verdicts.len(),
        "{context}: the interpreter finished at every limit"
    );
    let (last, finished_last) = verdicts.last().copied().expect("at least one limit");
    assert!(
        finished_last,
        "{context}: the interpreter must finish at {last}"
    );
    finished
}

#[test]
fn per_bar_objects_are_collected_as_the_interpreter_collects_them() {
    // Build plain objects on every bar, including one holding persistent
    // feature arrays that grow by a push per bar, plus a helper's one-element
    // `array.from` temporary. Only the pushes stay live; the interpreter
    // collects the rest at bar boundaries. The native module must likewise
    // reclaim transient objects instead of exhausting its collection budget.
    let source = "//@version=6\nindicator(\"collector\", overlay=true)\n\
                  type Settings\n    float source\n    int neighbors\n    bool exits\n\
                  type Features\n    array<float> f1\n    array<float> f2\n\
                  size_of(float x) =>\n    array.size(array.from(x))\n\
                  var f1 = array.new_float()\n\
                  var f2 = array.new_float()\n\
                  settings = Settings.new(close, 8, false)\n\
                  array.push(f1, close)\n\
                  array.push(f2, open)\n\
                  features = Features.new(f1, f2)\n\
                  last = array.get(features.f1, array.size(features.f1) - 1)\n\
                  plot(settings.source + last + size_of(high))\n";
    // 7 elements charged a bar, 2 of them live: 120 bars charge 840.
    let finished = collector_agrees_at_every_limit(source, 120, 320, "per-bar objects");
    assert!(finished > 0 && finished < 320, "{finished}");
}

#[test]
fn evicted_labels_are_collected_as_the_interpreter_collects_them() {
    // The module's other per-bar allocation: a label on every bar. The
    // interpreter collects each label the registry's live cap evicts; the
    // module's host collects them too when the module asks.
    let source = "//@version=6\nindicator(\"labels\", overlay=true, max_labels_count=5)\n\
                  type Settings\n    float source\n    int neighbors\n\
                  var f1 = array.new_float()\n\
                  settings = Settings.new(close, 8)\n\
                  array.push(f1, close)\n\
                  label.new(bar_index, settings.source, \"x\")\n\
                  plot(array.sum(f1))\n";
    let finished = collector_agrees_at_every_limit(source, 60, 260, "evicted labels");
    assert!(finished > 0, "{finished}");
}

#[test]
fn drawing_references_the_module_holds_are_collection_roots() {
    // Labels referenced from the module's own memory stay live on the host
    // after the registry drops them: from a slot's history (deleted two bars
    // later), and from a field of a `var` object.
    let source = "//@version=6\nindicator(\"held labels\", overlay=true, max_labels_count=3)\n\
                  type Holder\n    label held\n    float v\n\
                  var a = array.new_float()\n\
                  array.push(a, close)\n\
                  lb = label.new(bar_index, close, \"x\")\n\
                  if bar_index % 3 == 0\n    label.delete(lb[2])\n\
                  var Holder keep = Holder.new(label.new(bar_index, open, \"k\"), 0.0)\n\
                  if bar_index % 4 == 0\n    keep := Holder.new(label.new(bar_index, open, \"h\"), close)\n\
                  plot(keep.v + array.size(a))\n";
    let finished = collector_agrees_at_every_limit(source, 48, 220, "held labels");
    assert!(finished > 0, "{finished}");
}

#[test]
fn objects_reached_through_fields_and_history_stay_live() {
    // Objects reached only through other objects' fields, two levels deep,
    // a `var` object replaced every five bars whose replacement keeps one
    // of the old object's leaves, and an object read back three bars: the
    // collector must reach exactly what the interpreter reaches.
    let source = "//@version=6\nindicator(\"graphs\")\n\
                  type Leaf\n    float v\n\
                  type Pair\n    Leaf left\n    Leaf right\n\
                  type Box\n    Pair inner\n    float w\n\
                  var Box keep = Box.new(Pair.new(Leaf.new(0.0), Leaf.new(1.0)), 0.0)\n\
                  leaf = Leaf.new(close)\n\
                  pair = Pair.new(leaf, keep.inner.left)\n\
                  if bar_index % 5 == 0\n    keep := Box.new(pair, close)\n\
                  back = bar_index >= 3 ? leaf[3].v : 0.0\n\
                  plot(keep.inner.left.v + keep.inner.right.v + keep.w + pair.right.v + back)\n";
    let finished = collector_agrees_at_every_limit(source, 90, 160, "object graphs");
    assert!(finished > 0, "{finished}");
}

/// [`collector_agrees_at`] wherever the interpreter's verdict changes, for a
/// fixture too long to run natively at every limit: the interpreter runs at
/// every limit from 1 to `max_limit`, and the module at each limit whose
/// verdict (finished, or the bar and pc it stopped at) differs from the one
/// below, at the one below it, and at both ends. A module counting
/// differently by any amount moves one of those changes.
fn collector_agrees_where_the_interpreter_changes(
    source: &str,
    bars: usize,
    max_limit: usize,
    context: &str,
) -> usize {
    let program = compile_csil(source);
    let aot = compile_program(&program)
        .unwrap_or_else(|e| panic!("{context}: inside the AOT subset: {e}"));
    let plan = analyze(&program)
        .expect("the program lowers natively")
        .heap
        .expect("a native heap plan");
    assert!(plan.gc.is_some(), "{context}: the module must collect");
    let data = synthetic_data(bars);
    let verdict = |limit: usize| {
        let options = RunOptions {
            max_collection_elements: limit,
            ..RunOptions::default()
        };
        run(&program, &data, &options)
            .errors
            .first()
            .map(|error| (error.bar_index, error.instruction_offset))
    };
    // The interpreter runs are independent: spread them over the cores.
    let threads = std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get);
    let chunk = max_limit.div_ceil(threads);
    let verdicts: Vec<Option<(Option<u32>, Option<u32>)>> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|t| {
                let verdict = &verdict;
                scope.spawn(move || {
                    let first = 1 + t * chunk;
                    let last = ((t + 1) * chunk).min(max_limit);
                    (first..=last).map(verdict).collect::<Vec<_>>()
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| worker.join().expect("an interpreter sweep thread"))
            .collect()
    });
    assert_eq!(verdicts.len(), max_limit);
    let mut limits = std::collections::BTreeSet::from([1, max_limit]);
    for limit in 2..=max_limit {
        if verdicts[limit - 1] != verdicts[limit - 2] {
            limits.extend([limit - 1, limit]);
        }
    }
    for &limit in &limits {
        collector_agrees_at(&program, &aot, &data, limit, context);
    }
    assert!(
        verdicts[max_limit - 1].is_none(),
        "{context}: the interpreter must finish at {max_limit}"
    );
    limits.len()
}

#[test]
fn arena_pressure_collects_where_the_interpreter_does() {
    // Forty empty arrays a bar charge no elements, only objects, so the
    // interpreter's arena threshold (4,096 objects) starts its first
    // collection, near bar 98, before its element threshold can. Every
    // allocation counts toward that bar: the arrays, the object and the
    // label. A 3,000-element temporary on bar 110 then fits or not
    // depending on exactly which bar that collection ran on.
    let crossing = "//@version=6\nindicator(\"arena\", overlay=true, max_labels_count=2)\n\
                    type P\n    float x\n\
                    var keep = array.new_float()\n\
                    array.push(keep, close)\n\
                    for i = 0 to 39\n    array.new_float(0)\n\
                    p = P.new(close)\n\
                    label.new(bar_index, p.x, \"a\")\n\
                    if bar_index == 110\n    big = array.new_float(3000, 0.0)\n    array.push(keep, array.sum(big))\n\
                    plot(array.size(keep))\n";
    let checked =
        collector_agrees_where_the_interpreter_changes(crossing, 115, 3400, "arena crossing");
    assert!(checked > 2, "{checked}");
}

#[test]
fn a_large_module_collects_where_the_interpreter_does() {
    // Every bar's start runs the collection check, however the module
    // arranges the rest of its bar. A body of over a thousand instructions
    // (a larger body is several thousand instructions) around the per-bar objects and
    // labels must collect like a small one. Its collections also survive
    // across bars only through other cells: a `var` and a `varip` array
    // replaced every few bars, a `var` object whose field holds an array,
    // and a function's own `var` array, one per call site. The kept ones
    // must stay live and the replaced ones be collected.
    let mut source = String::from(
        "//@version=6\nindicator(\"large\", overlay=true, max_labels_count=5)\n\
         type Settings\n    float source\n    int neighbors\n\
         type Basket\n    array<float> items\n    float w\n\
         keeper(float x) =>\n    var float[] acc = array.new_float()\n    array.push(acc, x)\n    array.size(acc)\n\
         var f1 = array.new_float()\n\
         var float[] held = array.new_float(0)\n\
         if bar_index % 3 == 0\n    held := array.new_float(4, close)\n\
         varip float[] carried = array.new_float(0)\n\
         if bar_index % 5 == 0\n    carried := array.new_float(2, open)\n\
         var Basket basket = Basket.new(array.new_float(1, 0.0), 0.0)\n\
         if bar_index % 4 == 0\n    basket := Basket.new(array.new_float(3, high), close)\n\
         settings = Settings.new(close, 8)\n\
         array.push(f1, close)\n\
         label.new(bar_index, settings.source, \"x\")\n\
         kept = keeper(close) + keeper(open)\n\
         t0 = close\n",
    );
    for i in 1..=300 {
        source.push_str(&format!(
            "t{i} = t{} * 0.5 + high * 0.25 + low * 0.25\n",
            i - 1
        ));
    }
    source.push_str(
        "plot(array.sum(f1) + array.sum(held) + array.sum(carried) + array.sum(basket.items) \
         + basket.w + kept + t300)\n",
    );
    let program = compile_csil(&source);
    assert!(program.main().code.len() > 1_000);
    // The body runs as several pieces, so the check must be part of every
    // bar's start, not only of an unsplit bar.
    let aot = compile_program(&program).expect("inside the AOT subset");
    assert!(
        bar_bodies(&aot.wasm).len() > 1,
        "{:?}",
        bar_bodies(&aot.wasm)
    );
    let checked = collector_agrees_where_the_interpreter_changes(&source, 40, 700, "large module");
    assert!(checked > 2, "{checked}");
}

#[test]
fn a_collection_returns_the_memory_above_the_last_live_record() {
    // A large temporary on every bar must release its backing memory, not
    // merely its collection-budget charge. Return the memory above the
    // highest still-reachable record to the bump allocator at each collection
    // so the module can reuse it throughout the run. A `var` array replaced
    // after the temporary every seven bars sits
    // above that bar's temporary, so trimming must stop at it, and the
    // values read from it after later collections must be its own.
    let source = "//@version=6\nindicator(\"churn\")\n\
                  var keep = array.new_float()\n\
                  array.push(keep, close)\n\
                  tmp = array.new_float(120000, close)\n\
                  var float[] late = array.new_float(1, 0.0)\n\
                  if bar_index % 7 == 0\n    late := array.new_float(3, close)\n\
                  plot(array.sum(tmp) + array.size(keep) + array.sum(late))\n";
    let program = compile_csil(source);
    let aot = compile_program(&program).expect("inside the AOT subset");
    let plan = analyze(&program)
        .expect("the program lowers natively")
        .heap
        .expect("a native heap plan");
    assert!(plan.gc.is_some(), "the module must collect");
    let data = synthetic_data(300);
    let options = RunOptions {
        max_collection_elements: 1_000_000,
        ..RunOptions::default()
    };
    let outcome = run(&program, &data, &options);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    let actual = run_aot(&aot, &program, &data, &options);
    assert_eq!(
        actual.status,
        status::OK,
        "the module stopped at bar {} (pc {})",
        actual.err_bar,
        actual.err_pc
    );
    assert_outcomes_match(&outcome, &actual, data.bars(), "churn");
}

#[test]
fn a_collection_that_reaches_nothing_returns_the_whole_heap() {
    // Only temporaries: no record survives a bar boundary, so a collection
    // reaches nothing and the next bar allocates from the start of the heap
    // again, every bar, without running out of memory or budget.
    let source = "//@version=6\nindicator(\"temporaries\")\n\
                  tmp = array.new_float(120000, close)\n\
                  first = array.from(close, open)\n\
                  plot(array.sum(tmp) + array.sum(first))\n";
    let program = compile_csil(source);
    let aot = compile_program(&program).expect("inside the AOT subset");
    let plan = analyze(&program)
        .expect("the program lowers natively")
        .heap
        .expect("a native heap plan");
    assert!(
        plan.gc.as_ref().is_some_and(|gc| gc.roots.is_empty()),
        "{:?}",
        plan.gc
    );
    let data = synthetic_data(300);
    let options = RunOptions {
        max_collection_elements: 1_000_000,
        ..RunOptions::default()
    };
    let outcome = run(&program, &data, &options);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    let actual = run_aot(&aot, &program, &data, &options);
    assert_eq!(
        actual.status,
        status::OK,
        "the module stopped at bar {} (pc {})",
        actual.err_bar,
        actual.err_pc
    );
    assert_outcomes_match(&outcome, &actual, data.bars(), "temporaries");
}

#[test]
fn the_collection_sites_are_the_hosts() {
    assert_eq!(
        pinewasm_aot::host_gc_site::ROOTS,
        pinewasm_vm::aot_host::gc_site::ROOTS
    );
    assert_eq!(
        pinewasm_aot::host_gc_site::COLLECT,
        pinewasm_vm::aot_host::gc_site::COLLECT
    );
}

/// A linked VM host answers for the stops a module makes in its own
/// code (`VmHost::native_stop`), which it knows by the module's statuses.
#[test]
fn the_stops_a_linked_host_answers_for_are_the_modules() {
    use pinewasm_vm::aot_host::native_status;
    assert_eq!(native_status::BUDGET_EXCEEDED, status::BUDGET_EXCEEDED);
    assert_eq!(native_status::HISTORY_LIMIT, status::HISTORY_LIMIT);
    assert_eq!(
        native_status::RUN_BUDGET_EXCEEDED,
        status::RUN_BUDGET_EXCEEDED
    );
}

#[test]
fn a_split_module_roots_objects_read_back_through_history() {
    // A body split into pieces, with an object holding a 50-element array
    // built every bar and read back three bars. At a bar's start, after the
    // preamble, the object from three bars back is still a root and the one
    // from four bars back is garbage. A collection anywhere later in the bar
    // would still count the older one, so the module would stop at another
    // limit than the interpreter.
    let mut source = String::from(
        "//@version=6\nindicator(\"split history\")\n\
         type Basket\n    array<float> items\n    float w\n\
         o = Basket.new(array.new_float(50, close), close)\n\
         back = bar_index >= 3 ? array.sum(o[3].items) + o[3].w : 0.0\n\
         t0 = close\n",
    );
    for i in 1..=300 {
        source.push_str(&format!(
            "t{i} = t{} * 0.5 + high * 0.25 + low * 0.25\n",
            i - 1
        ));
    }
    source.push_str("plot(back + t300)\n");
    let program = compile_csil(&source);
    let aot = compile_program(&program).expect("inside the AOT subset");
    assert!(
        bar_bodies(&aot.wasm).len() > 1,
        "{:?}",
        bar_bodies(&aot.wasm)
    );
    let checked = collector_agrees_where_the_interpreter_changes(&source, 40, 900, "split history");
    assert!(checked > 2, "{checked}");
}

#[test]
fn an_array_in_a_var_objects_field_stays_live_as_it_grows() {
    // A `var` object's array field grows by two elements a bar, so its buffer
    // is reallocated above that bar's 2,000-element temporary, and the object
    // is replaced every 37 bars. The array is reached only through the field.
    // A collector that missed it would count it as garbage, and its trim
    // would hand the array's buffer to the next temporary.
    let source = "//@version=6\nindicator(\"field array\")\n\
                  type Basket\n    array<float> items\n    float w\n\
                  var Basket b = Basket.new(array.new_float(), 0.0)\n\
                  tmp = array.new_float(2000, close)\n\
                  array.push(b.items, close)\n\
                  array.push(b.items, open)\n\
                  if bar_index % 37 == 0\n    b := Basket.new(array.new_float(1, high), close)\n\
                  plot(array.sum(b.items) + b.w + array.size(tmp))\n";
    let finished =
        collector_agrees_at_limits(source, 300, limits_from(2000, 30000, 701), "field array");
    assert!(finished > 1, "{finished}");
}

#[test]
fn a_copied_object_shares_its_arrays_with_the_original() {
    // `Basket.copy` copies the object, not its array: the copy and the
    // original share one array until the original is replaced every 11 bars,
    // and the copy keeps pushing to it. The shared array is live through
    // either object and counts once.
    let source = "//@version=6\nindicator(\"copy\")\n\
                  type Basket\n    array<float> items\n    float w\n\
                  var Basket orig = Basket.new(array.new_float(), 0.0)\n\
                  var Basket cp = Basket.copy(orig)\n\
                  tmp = array.new_float(1500, open)\n\
                  if bar_index % 11 == 0\n    cp := Basket.copy(orig)\n    orig := Basket.new(array.new_float(2, close), close)\n\
                  array.push(cp.items, close)\n\
                  plot(array.sum(cp.items) + array.sum(orig.items) + array.size(tmp))\n";
    let finished = collector_agrees_at_limits(source, 300, limits_from(1500, 30000, 401), "copy");
    assert!(finished > 1, "{finished}");
}

#[test]
fn a_functions_own_var_object_is_kept_per_call_site() {
    // A function's `var` object lives in one slot per call site, and each
    // holds an array that grows every bar. Both stay live, though none of
    // the script's own variables reaches either.
    let source = "//@version=6\nindicator(\"call sites\")\n\
                  type Acc\n    array<float> xs\n    float n\n\
                  acc(float x) =>\n    var Acc a = Acc.new(array.new_float(), 0.0)\n    array.push(a.xs, x)\n    a.n += 1\n    array.sum(a.xs) + a.n\n\
                  tmp = array.new_float(1800, open)\n\
                  plot(acc(close) + acc(open) * 2 + array.size(tmp))\n";
    let finished =
        collector_agrees_at_limits(source, 300, limits_from(1800, 30000, 457), "call sites");
    assert!(finished > 1, "{finished}");
}

#[test]
fn objects_linked_through_fields_stay_live() {
    // A `var` holder of two links, each holding two leaves and an array, is
    // rebuilt every five bars from parts of the old one: the second link
    // moves to the first place, and one leaf of the first link moves into a
    // new second link. What the new holder reaches through two levels of
    // fields stays live; the rest of the old holder is garbage.
    let source = "//@version=6\nindicator(\"links\")\n\
                  type Leaf\n    float v\n\
                  type Link\n    Leaf a\n    Leaf b\n    array<float> xs\n\
                  type Holder\n    Link l1\n    Link l2\n\
                  var Holder h = Holder.new(Link.new(Leaf.new(0.0), Leaf.new(0.0), array.new_float()), Link.new(Leaf.new(0.0), Leaf.new(0.0), array.new_float()))\n\
                  tmp = array.new_float(1000, open)\n\
                  if bar_index % 5 == 0\n    h := Holder.new(h.l2, Link.new(Leaf.new(close), h.l1.a, array.new_float(2, close)))\n\
                  array.push(h.l2.xs, close)\n\
                  array.push(h.l1.xs, open)\n\
                  plot(h.l1.a.v + h.l2.b.v + array.sum(h.l1.xs) + array.sum(h.l2.xs) + array.size(tmp))\n";
    let finished = collector_agrees_at_limits(source, 300, limits_from(1000, 30000, 311), "links");
    assert!(finished > 1, "{finished}");
}

#[test]
fn labels_held_in_nested_and_historical_fields_stay_live() {
    // Labels the drawing registry has dropped stay live while the module
    // holds them: one two fields down in a `var` object, replaced every five
    // bars, and one in a field of an object read back three bars. The host
    // collects only what the module says it holds, so a label missed in a
    // field undercounts by its 14 fields, which every limit catches.
    let source = "//@version=6\nindicator(\"nested labels\", overlay=true, max_labels_count=2)\n\
                  type Tag\n    label l\n    float v\n\
                  type Box\n    Tag inner\n    float w\n\
                  var a = array.new_float()\n\
                  array.push(a, close)\n\
                  lb = label.new(bar_index, close, \"x\")\n\
                  var Box keep = Box.new(Tag.new(label.new(bar_index, open, \"k\"), 0.0), 0.0)\n\
                  if bar_index % 5 == 0\n    keep := Box.new(Tag.new(lb, close), high)\n\
                  t = Tag.new(lb, low)\n\
                  back = bar_index >= 3 ? t[3].v : 0.0\n\
                  plot(keep.w + keep.inner.v + back + array.size(a))\n";
    let finished = collector_agrees_at_every_limit(source, 40, 420, "nested labels");
    assert!(finished > 0, "{finished}");
}

#[test]
fn references_inside_collections_keep_the_heap_on_the_host() {
    // The collector traces references only through object fields: a native
    // array holds scalars, and it counts an array's elements without reading
    // them. An array of objects or a map is not native, so it keeps the
    // whole heap on the host, with no collector. A change that brings either
    // to the native heap must teach the collector to trace it first. Both are
    // refused before any heap plan exists today; `gc_plan`'s own refusal of a
    // lane that is not a scalar is pinned in `analysis/gc_tests.rs`.
    for (shape, body) in [
        (
            "array of objects",
            "var holders = array.new<Basket>()\n\
             array.push(holders, Basket.new(array.new_float(2, close), close))\n\
             plot(array.size(holders))\n",
        ),
        (
            "map of floats",
            "var m = map.new<string, float>()\n\
             map.put(m, str.tostring(bar_index % 3), close)\n\
             plot(map.size(m))\n",
        ),
    ] {
        let source = format!(
            "//@version=6\nindicator(\"held\")\n\
             type Basket\n    array<float> items\n    float w\n{body}"
        );
        let program = compile_csil(&source);
        // A program that does not lower natively has nothing to collect.
        let heap = analyze(&program).ok().and_then(|analysis| analysis.heap);
        assert!(
            heap.as_ref().is_none_or(|plan| plan.gc.is_none()),
            "{shape}: a reference inside a collection reached a collector plan"
        );
    }
}

#[test]
fn array_credits_and_temporaries_match_the_interpreter_under_collection() {
    // Shifts, pops and clears return elements as they happen; a sized
    // temporary array and a pushed-to one become garbage at the bar's end.
    let source = "//@version=6\nindicator(\"array credits\")\n\
                  var q = array.new_float()\n\
                  array.push(q, close)\n\
                  if array.size(q) > 6\n    array.shift(q)\n\
                  tmp = array.new_float(5, close)\n\
                  array.push(tmp, open)\n\
                  last = array.pop(tmp)\n\
                  if bar_index % 25 == 0\n    array.clear(q)\n\
                  plot(array.sum(tmp) + last + array.size(q))\n";
    let finished = collector_agrees_at_every_limit(source, 80, 120, "array credits");
    assert!(finished > 0, "{finished}");
}

/// A full-domain integer producer does not send the program to the
/// interpreter-hosted tier: it lowers natively behind a minimum guard and
/// agrees with the interpreter on data that never reaches the minimum.
fn guarded_integer_differential(source: &str, data: &MarketData, context: &str) {
    let program = compile_csil(source);
    guarded_native(&program, context);
    differential_program(&program, data, context);
}

#[test]
fn native_heap_float_array_pipeline() {
    native_heap_differential(
        "indicator(\"A\")\n\
         var prices = array.new_float()\n\
         array.push(prices, close)\n\
         if array.size(prices) > 8\n\
         \tarray.shift(prices)\n\
         plot(array.sum(prices))\n\
         plot(array.avg(prices))\n\
         plot(array.get(prices, 0))\n",
        &synthetic_data(128),
        "float_array_pipeline",
    );
}

#[test]
fn negative_array_indices_count_from_the_end_natively() {
    native_heap_differential(
        "indicator(\"N\")\n\
         var prices = array.new_float(4, 1.0)\n\
         array.set(prices, bar_index % 4, close)\n\
         array.set(prices, -1, close * 2)\n\
         array.set(prices, -4, close * 3)\n\
         plot(array.get(prices, -1))\n\
         plot(array.get(prices, -4))\n\
         plot(array.get(prices, -(bar_index % 4) - 1))\n",
        &synthetic_data(64),
        "negative_array_indices",
    );
}

#[test]
fn integer_heap_arrays_run_natively_behind_the_minimum_guard() {
    guarded_integer_differential(
        "indicator(\"IB\")\n\
         var counts = array.new_int(4, 0)\n\
         var flags = array.new_bool(2, false)\n\
         i = bar_index % 4\n\
         array.set(counts, i, array.get(counts, i) + 1)\n\
         array.set(flags, bar_index % 2, close > open)\n\
         plot(array.get(counts, i))\n\
         plot(array.first(flags) ? 1 : 0)\n\
         plot(array.get(flags, 1) ? 1 : 0)\n",
        &synthetic_data(96),
        "int_bool_arrays",
    );
}

#[test]
fn native_heap_growth_and_aggregates() {
    // Push far past the initial capacity so the buffer doubles repeatedly,
    // with na elements exercising the aggregate skip rules.
    native_heap_differential(
        "indicator(\"G\")\n\
         var xs = array.new_float()\n\
         array.push(xs, close)\n\
         array.push(xs, bar_index % 7 == 0 ? na : open)\n\
         plot(array.sum(xs))\n\
         plot(array.min(xs))\n\
         plot(array.max(xs))\n\
         plot(array.size(xs))\n\
         plot(array.last(xs))\n",
        &synthetic_data(300),
        "growth_and_aggregates",
    );
}

/// Every integer pushed here is bounded (`bar_index`, `bar_index * 2`), and
/// native array pushes/pops only move those values or push an `na`
/// placeholder, so the producer gate keeps this program native.
#[test]
fn native_heap_pop_clear_and_int_aggregates() {
    native_heap_differential(
        "indicator(\"PC\")\n\
         var ys = array.new_int(0)\n\
         array.push(ys, bar_index)\n\
         array.push(ys, bar_index * 2)\n\
         if array.size(ys) > 6\n\
         \tarray.pop(ys)\n\
         if bar_index % 50 == 49\n\
         \tarray.clear(ys)\n\
         plot(array.size(ys))\n\
         plot(array.size(ys) > 0 ? array.avg(ys) : na)\n",
        &synthetic_data(200),
        "pop_clear_int_aggregates",
    );
}

/// Color arrays live on the native heap: `array.from` arrays take the lane of
/// their arguments (here color, through a merge with an `array.new_color`
/// array), and a color read from them reaches `barcolor` and a plot color.
#[test]
fn native_heap_color_arrays() {
    native_heap_differential(
        "indicator(\"C\", overlay=true)\n\
         greens = array.from(color.new(color.green, 80), color.new(color.green, 40), color.green)\n\
         reds = array.new_color(3, color.red)\n\
         array.set(reds, 1, na)\n\
         array.push(reds, color.new(color.red, 50))\n\
         k = bar_index % 4\n\
         shades = close > open ? greens : reds\n\
         c = k < array.size(shades) ? array.get(shades, k) : array.last(reds)\n\
         barcolor(c)\n\
         plot(close, color=c)\n\
         plot(array.size(reds))\n",
        &synthetic_data(64),
        "color_arrays",
    );
}

/// An `int` array from `array.from` pops natively; before its lane was known,
/// only `size` and `clear` could reach it and the program kept its heap on
/// `host.vm`.
#[test]
fn native_heap_int_array_from_pops() {
    native_heap_differential(
        "indicator(\"Changing bound\")\n\
         a = array.from(1, 2, 3)\n\
         s = 0\n\
         last = array.size(a) - 1\n\
         for i = 0 to last by 1\n\
         \ts := s + array.pop(a)\n\
         plot(s)\n",
        &synthetic_data(16),
        "int_array_from_pops",
    );
}

/// `array.stdev` and `array.variance` run natively over float and int lanes,
/// biased and not, with a run-time `biased` flag, na elements, a single
/// element (the sample form divides by zero: na), an empty array, and values
/// whose sum depends on the order it is added in.
#[test]
fn native_heap_variance_and_stdev() {
    native_heap_differential(
        "indicator(\"V\")\n\
         var xs = array.new_float()\n\
         array.push(xs, bar_index % 9 == 0 ? na : close - open)\n\
         if array.size(xs) > 12\n\
         \tarray.shift(xs)\n\
         var ns = array.new_int()\n\
         array.push(ns, bar_index % 5)\n\
         if array.size(ns) > 7\n\
         \tarray.shift(ns)\n\
         plot(array.stdev(xs))\n\
         plot(array.variance(xs))\n\
         plot(array.stdev(xs, false))\n\
         plot(array.variance(xs, false))\n\
         plot(array.stdev(ns))\n\
         plot(array.variance(ns, false))\n\
         plot(array.stdev(xs, bar_index % 2 == 0))\n\
         one = array.from(close)\n\
         plot(array.variance(one, false))\n\
         plot(array.stdev(one))\n\
         plot(array.stdev(array.new_float()))\n\
         wide = array.from(1e16, 1.0, -1e16, 3.5, -0.0, close)\n\
         plot(array.variance(wide))\n\
         plot(array.stdev(wide, false))\n",
        &synthetic_data(160),
        "variance_and_stdev",
    );
}

#[test]
fn integer_rounding_and_cast_identities_remain_exact_and_native() {
    for value in [
        i64::MIN + 1,
        -9_007_199_254_740_993,
        -3,
        0,
        3,
        9_007_199_254_740_993,
        i64::MAX,
    ] {
        let source = format!(
            "indicator('Integer identities')\nx = input.int({value}, minval=-9223372036854775807, maxval=9223372036854775807)\nplot(math.floor(x) == x ? 1 : 0)\nplot(math.ceil(x) == x ? 1 : 0)\nplot(math.round(x) == x ? 1 : 0)\nplot(int(x) == x ? 1 : 0)\n",
        );
        let program = compile_csil(&source);
        compile_program(&program).expect("integer identities cannot introduce valid MIN");
        let outcome =
            differential_program(&program, &synthetic_data(2), "exact integer identities");
        for plot in &outcome.plots {
            assert_eq!(plot.values, vec![Value::Float(1.0); 2]);
        }
    }
    let program = compile_csil(
        "indicator('Missing integer identities')\nplot(na(math.floor(bar_index[1])) ? 1 : 0)\nplot(na(math.ceil(bar_index[1])) ? 1 : 0)\nplot(na(math.round(bar_index[1])) ? 1 : 0)\nplot(na(int(bar_index[1])) ? 1 : 0)\n",
    );
    compile_program(&program).expect("integer missing history stays missing");
    let outcome = differential_program(&program, &synthetic_data(3), "missing integer identities");
    for plot in &outcome.plots {
        assert_eq!(
            plot.values,
            vec![Value::Float(1.0), Value::Float(0.0), Value::Float(0.0)]
        );
    }
}

#[test]
fn integer_identity_calls_preserve_bounded_field_arithmetic_facts() {
    for function in ["math.floor", "math.ceil", "math.round", "int"] {
        let program = compile_csil(&format!(
            "indicator('Identity field range')\ntype Threshold\n    int value\nx = Threshold.new(input.int(20, minval=0, maxval=100))\nplot({function}(x.value) + 1)\n",
        ));
        compile_program(&program).expect("identity must retain the proved field range");
        let outcome = differential_program(&program, &synthetic_data(3), "identity field range");
        assert_eq!(outcome.plots[0].values, vec![Value::Float(21.0); 3]);
    }
}

#[test]
fn immediately_widened_float_rounding_and_casts_preserve_min_bounds_and_zero_bits() {
    let program = compile_csil(
        "indicator('Widened float integer results')\nplot(math.floor(close))\nplot(math.ceil(close))\nplot(math.round(close))\nplot(int(close))\n",
    );
    compile_program(&program).expect("an immediate widening need not encode an integer sentinel");
    let samples = [
        i64::MIN as f64,
        (i64::MIN as f64) - 2048.0,
        (i64::MIN as f64) + 1024.0,
        (i64::MAX as f64) - 1024.0,
        i64::MAX as f64,
        f64::MAX,
        -f64::MAX,
        -2.5,
        -1.5,
        -0.5,
        -0.25,
        -0.0,
        0.0,
        0.25,
        0.5,
        1.5,
        2.5,
        f64::from_bits(1),
        -f64::from_bits(1),
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
    ];
    let mut data = synthetic_data(samples.len());
    data.close = samples.to_vec();
    let outcome = differential_program(&program, &data, "widened float integer result bits");
    for plot in &outcome.plots {
        assert_eq!(plot.values[0], Value::Float(i64::MIN as f64));
        assert_eq!(plot.values[1], Value::Na);
        assert_eq!(plot.values[4], Value::Na);
        for bar in [11, 12] {
            let Value::Float(value) = plot.values[bar] else {
                panic!("zero result must be a float")
            };
            assert_eq!(value.to_bits(), 0.0_f64.to_bits());
        }
    }
}

#[test]
fn float_rounding_and_casts_carry_the_minimum_integer_guard() {
    for expression in [
        "math.floor(close)",
        "math.ceil(close)",
        "math.round(close)",
        "int(close)",
    ] {
        let program = compile_csil(&format!(
            "indicator('Float integer producer')\nx = {expression}\nplot(x == 0 ? 1 : 0)\n"
        ));
        guarded_native(&program, expression);
        assert_eq!(
            int_min_guards(&program)
                .values()
                .copied()
                .collect::<Vec<_>>(),
            vec![IntMinGuard::FloatToInt],
            "{expression}"
        );
        differential_program(&program, &synthetic_data(8), expression);
    }
    let program = compile_csil(
        "indicator('MIN integer identities')\nx = -9223372036854775807 - 1\nplot(math.floor(x) == x ? 1 : 0)\nplot(math.ceil(x) == x ? 1 : 0)\nplot(math.round(x) == x ? 1 : 0)\nplot(int(x) == x ? 1 : 0)\n",
    );
    let artifact = guarded_native(&program, "MIN integer identities");
    assert_present_min_stops(
        &artifact,
        &program,
        &synthetic_data(2),
        &RunOptions::default(),
        0,
        "MIN integer identities",
    );
    let outcome = run(&program, &synthetic_data(2), &RunOptions::default());
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    for plot in &outcome.plots {
        assert_eq!(plot.values, vec![Value::Float(1.0); 2]);
    }
}

#[test]
fn bounded_integer_object_fields_and_copies_use_native_storage() {
    for source in [
        "indicator('Bounded field')\ntype Threshold\n    int value\na = input.int(20, minval=0, maxval=100)\nx = Threshold.new(a)\ny = Threshold.copy(x)\nplot(y.value + 7)\n",
        "indicator('Bounded writes')\ntype Threshold\n    int value\nx = Threshold.new(20)\ny = x\nif bar_index % 2 == 0\n    y.value := 30\nelse\n    x.value := 40\nplot(x.value - 1)\n",
    ] {
        let program = compile_csil(source);
        let artifact = compile_program(&program).expect("all integer field writes have proved bounds");
        assert!(artifact.layout.native_heap);
        differential_program(&program, &synthetic_data(4), "bounded integer object fields");
    }
}

#[test]
fn integer_array_mutators_return_only_missing_and_remain_native() {
    let program = compile_csil(
        "indicator('Mutator missing result')\nx = array.new_int(1, 2)\narray.push(x, 3)\narray.set(x, 0, 4)\nplot(array.size(x))\narray.clear(x)\nplot(array.size(x))\n",
    );
    let artifact = compile_program(&program).expect("mutators cannot produce valid MIN");
    assert!(artifact.layout.native_heap);
    let outcome = differential_program(
        &program,
        &synthetic_data(3),
        "integer mutator missing result",
    );
    assert_eq!(outcome.plots[0].values, vec![Value::Float(2.0); 3]);
    assert_eq!(outcome.plots[1].values, vec![Value::Float(0.0); 3]);
}

#[test]
fn unknown_integer_object_writes_carry_the_minimum_guard() {
    for source in [
        "indicator('Unknown write')\ntype Threshold\n    int value\nx = Threshold.new(20)\ny = x\ny.value := int(close)\nplot(x.value)\n",
        "indicator('Unknown constructor')\ntype Threshold\n    int value\nx = Threshold.new(20)\ny = Threshold.new(int(close))\nplot(x.value)\n",
        "indicator('Unknown alias range')\ntype Threshold\n    int value\nvar int incoming = 0\nincoming := bar_index\nx = Threshold.new(20)\ny = x\ny.value := incoming\nplot(x.value + 1)\n",
    ] {
        let program = compile_csil(source);
        guarded_native(&program, source);
        differential_program(&program, &synthetic_data(6), source);
    }
}

#[test]
fn integer_udt_fields_run_natively_behind_the_minimum_guard() {
    guarded_integer_differential(
        "indicator(\"U\")\n\
         type Pair\n\
         \tfloat total\n\
         \tint hits\n\
         var Pair acc = Pair.new(0.0, 0)\n\
         acc.total := acc.total + close\n\
         acc.hits := acc.hits + 1\n\
         snap = Pair.copy(acc)\n\
         snap.total := snap.total * 2\n\
         plot(acc.total)\n\
         plot(snap.total)\n\
         plot(acc.hits)\n",
        &synthetic_data(64),
        "udt_fields_and_copy",
    );
}

#[test]
fn loop_index_in_udt_array_processing_runs_natively_behind_the_minimum_guard() {
    // A representative data-processing shape: a UDT holding array handles,
    // read back through GetField into collection receivers.
    guarded_integer_differential(
        "indicator(\"FA\")\n\
         type Features\n\
         \tarray<float> f1\n\
         \tarray<float> f2\n\
         var Features fs = Features.new(array.new_float(), array.new_float())\n\
         array.push(fs.f1, close)\n\
         array.push(fs.f2, open)\n\
         d = 0.0\n\
         for i = 0 to array.size(fs.f1) - 1\n\
         \td := d + math.abs(array.get(fs.f1, i) - array.get(fs.f2, i))\n\
         plot(d)\n",
        &synthetic_data(48),
        "arrays_inside_udt_fields",
    );
}

#[test]
fn native_heap_oob_reroutes_to_interpreter() {
    let source = "indicator(\"E\")\n\
        var xs = array.new_float(2, 1.0)\n\
        plot(bar_index < 5 ? array.get(xs, 0) : array.get(xs, 9))\n";
    let program = compile_csil(source);
    let aot = compile_program(&program).expect("inside the AOT subset");
    assert!(aot.layout.native_heap, "gate passes");
    let data = synthetic_data(16);
    let options = RunOptions::default();
    let outcome = run(&program, &data, &options);
    assert!(
        !outcome.errors.is_empty(),
        "interpreter reports the out-of-bounds error"
    );
    let aot_run = run_aot(&aot, &program, &data, &options);
    assert_eq!(
        aot_run.status,
        status::HEAP_REROUTE,
        "native run reroutes instead of formatting the error"
    );
}

#[test]
fn native_heap_element_budget_reroutes() {
    let source = "indicator(\"L\")\n\
        var xs = array.new_float()\n\
        array.push(xs, close)\n\
        plot(array.size(xs))\n";
    let program = compile_csil(source);
    let aot = compile_program(&program).expect("inside the AOT subset");
    assert!(aot.layout.native_heap, "gate passes");
    let data = synthetic_data(64);
    let options = RunOptions {
        max_collection_elements: 10,
        ..RunOptions::default()
    };
    let outcome = run(&program, &data, &options);
    assert!(
        !outcome.errors.is_empty(),
        "interpreter reports the collection limit"
    );
    let aot_run = run_aot(&aot, &program, &data, &options);
    assert_eq!(
        aot_run.status,
        status::HEAP_REROUTE,
        "budget parity reroutes"
    );
}

#[test]
fn native_heap_array_whose_byte_size_wraps_u32_reroutes() {
    // 2^29 f64 elements is 2^32 buffer bytes: the record size
    // `16 + (cap << 3)` wraps u32. The size passes the call site's
    // `<= i32::MAX` check, so only a host that raises the element and
    // instruction budgets this far can reach it; that host must get the
    // interpreter reroute, not a wrapped 16-byte allocation whose fill loop
    // runs off the end of linear memory. The interpreter side is not run:
    // it would honestly try to materialize 2^29 values.
    let source = "indicator(\"W\")\n\
        xs = array.new_float(536870912, 0.0)\n\
        plot(array.size(xs))\n";
    let program = compile_csil(source);
    let aot = compile_program(&program).expect("inside the AOT subset");
    assert!(aot.layout.native_heap, "gate passes");
    let options = RunOptions {
        max_collection_elements: 1 << 30,
        max_instructions_per_bar: u64::MAX,
        max_instructions_per_run: u64::MAX,
        ..RunOptions::default()
    };
    let aot_run = run_aot(&aot, &program, &synthetic_data(2), &options);
    assert_eq!(
        aot_run.status,
        status::HEAP_REROUTE,
        "a wrapping array size reroutes"
    );
}

#[test]
fn native_heap_gate_demotes_string_arrays() {
    // `array<string>` stays host-side: the whole program keeps the host-site
    // behavior (host sites, interpreter runs) and still matches bit-exact.
    let source = "indicator(\"S\")\n\
        var names = array.new_string()\n\
        array.push(names, \"x\")\n\
        plot(array.size(names))\n";
    let program = compile_csil(source);
    let aot = compile_program(&program).expect("inside the AOT subset");
    assert!(!aot.layout.native_heap, "string arrays fail the gate");
    assert!(aot.layout.vm_heap_residual, "heap stays host-bound");
    differential_with(source, &synthetic_data(32), "string_array_demotes");
}

// ---- numeric-polymorphic consumers of untyped operands ----------------------
//
// An object field read has no type of its own while the heap is host-side,
// and neither has a slot that only holds such a value or na: the consumer
// states the type. `math.min`/`math.max` are int exactly when every argument
// is int, `math.abs`/`math.sign` keep their operand's type, and a comparison,
// a rounding, a cast or `bool()` takes more than one type, so none of them
// can state it. The checker does, right after: `INT_TO_F64`, an int operator,
// an int slot. Guessing at the polymorphic consumer made each of those a type
// conflict and refused a valid script.

/// How the standalone build answers one program of the matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatrixOutcome {
    /// A module was produced.
    Accepted,
    /// Declared limit: a valid `i64::MIN` would alias na in the integer lane.
    IntegerNaEncoding,
    /// The backend's own typing contradicted itself. Never acceptable.
    TypeConflict,
}

impl MatrixOutcome {
    fn symbol(self) -> char {
        match self {
            MatrixOutcome::Accepted => '.',
            MatrixOutcome::IntegerNaEncoding => 'I',
            MatrixOutcome::TypeConflict => 'T',
        }
    }
}

/// `(label, declarations, expression yielding the value, value is an int)`.
const MATRIX_SOURCES: [(&str, &str, &str, bool); 16] = [
    ("int input", "n = input.int(8, \"N\")", "n", true),
    ("int literal var", "var int n = 8", "n", true),
    (
        "int object field",
        "type S\n    float src\n    int n\nS st = S.new(close, input.int(8, \"N\"))",
        "st.n",
        true,
    ),
    (
        "int object field (var object)",
        "type S\n    float src\n    int n\nvar S st = S.new(0.0, 8)",
        "st.n",
        true,
    ),
    ("int array element", "a = array.new_int(2, 8)", "array.get(a, 0)", true),
    (
        "int var set later",
        "var int n = na\nn := bar_index > 3 ? 8 : n",
        "n",
        true,
    ),
    (
        "int ternary of field/literal",
        "type S\n    int n\nS st = S.new(input.int(8, \"N\"))",
        "(close > open ? st.n : 5)",
        true,
    ),
    ("int function result", "f(int a) => a + 1", "f(7)", true),
    (
        "int nz of field",
        "type S\n    int n\nS st = S.new(input.int(8, \"N\"))",
        "nz(st.n, 1)",
        true,
    ),
    ("float input", "xf = input.float(8.5, \"X\")", "xf", false),
    (
        "int input (bounded 1..100)",
        "n = input.int(8, \"N\", minval=1, maxval=100)",
        "n",
        true,
    ),
    (
        "int object field (bounded input)",
        "type S\n    float src\n    int n\nS st = S.new(close, input.int(8, \"N\", minval=1, maxval=100))",
        "st.n",
        true,
    ),
    ("int series bar_index", "", "bar_index", true),
    ("float series close", "", "close", false),
    (
        "float object field",
        "type S\n    float src\n    int n\nS st = S.new(close, 8)",
        "st.src",
        false,
    ),
    ("float array element", "a = array.new_float(2, 8.5)", "array.get(a, 0)", false),
];

/// `(label, template with {} for the value, result is an int for an int value)`.
const MATRIX_CONSUMERS: [(&str, &str, bool); 15] = [
    ("identity", "{}", true),
    ("math.min(v, 3)", "math.min({}, 3)", true),
    ("math.max(v, 3)", "math.max({}, 3)", true),
    ("math.abs(v)", "math.abs({})", true),
    ("math.sign(v)", "math.sign({})", true),
    ("math.round(v)", "math.round({})", true),
    ("math.floor(v)", "math.floor({})", true),
    ("math.ceil(v)", "math.ceil({})", true),
    ("int(v)", "int({})", true),
    ("v + 1", "({} + 1)", true),
    ("v * 2", "({} * 2)", true),
    ("-v", "(-{})", true),
    ("nz(v)", "nz({})", true),
    ("math.avg(v, 3)", "math.avg({}, 3)", false),
    ("float(v)", "float({})", false),
];

/// The pinned answer for every (source, consumer) pair, one row per source in
/// [`MATRIX_SOURCES`] order and one column per consumer in
/// [`MATRIX_CONSUMERS`] order. `.` is accepted and `I` is the integer
/// missing-value limit. There is no `T`: the typed pass defers its numeric
/// guesses, so the twelve cells of the three object-field rows under
/// `math.min`, `math.max`, `math.abs` and `math.sign` are not type conflicts.
/// There is no `I` either: the integer missing-value gate does not refuse the
/// 31 cells (the rounding functions and `int()` over a float, and `v + 1` and
/// `v * 2` over an integer of unknown range), which lower natively behind a
/// runtime minimum guard.
///
/// A change to any cell is a change in what the product accepts; it must be
/// deliberate and reviewed.
const MATRIX_EXPECTED: [&str; 16] = ["..............."; 16];

fn matrix_program(source: usize, consumer: usize) -> String {
    let (_, declarations, value, value_is_int) = MATRIX_SOURCES[source];
    let (label, template, keeps_int) = MATRIX_CONSUMERS[consumer];
    let expression = template.replace("{}", value);
    // An int result is bound to an `int` and then widened in float
    // arithmetic, a common shape. The rounding functions
    // and `int()` produce an int from a float too.
    let int_result = (value_is_int && keeps_int)
        || matches!(
            label,
            "math.round(v)" | "math.floor(v)" | "math.ceil(v)" | "int(v)"
        );
    let binding = if int_result { "int k" } else { "k" };
    format!(
        "//@version=6\nindicator(\"check\")\n{declarations}\n{binding} = {expression}\n\
         x = close / k * 100\ns = x > 50 ? 1 : 0\nplot(s, \"Backtest Signal\")\n\
         plot(k, \"Value\")\nplot(x, \"Widened\")\n"
    )
}

fn matrix_compile(
    source: &str,
    context: &str,
) -> (pinewasm_csil::CsilProgram, Result<Vec<u8>, AotError>) {
    let result = compile(source, CompileOptions::all());
    let errors: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|d| pinewasm_span::Diagnostic::is_error(d))
        .collect();
    assert!(
        errors.is_empty(),
        "{context}: front end rejects: {errors:?}"
    );
    let program = result.program.expect("program");
    let signal_plot = program
        .plots
        .iter()
        .position(|plot| plot.title.as_deref() == Some("Backtest Signal"))
        .unwrap_or_else(|| panic!("{context}: no Backtest Signal plot"));
    let module = compile_standalone_wasm_program(
        &program,
        &StandaloneWasmOptions {
            signal_plot: Some(signal_plot),
            module_type: StandaloneWasmModuleType::Strategy,
        },
    );
    (program, module)
}

fn matrix_classify(module: &Result<Vec<u8>, AotError>, context: &str) -> MatrixOutcome {
    match module {
        Ok(_) => MatrixOutcome::Accepted,
        Err(AotError::UnsupportedIntegerNaEncoding { .. }) => MatrixOutcome::IntegerNaEncoding,
        Err(AotError::TypeConflict(_)) => MatrixOutcome::TypeConflict,
        Err(other) => panic!("{context}: refusal outside the matrix's classes: {other:?}"),
    }
}

/// Evaluate an accepted module through its own protocol and require every
/// plot lane (the signal, the consumer's own value and its widened use) to be
/// bit-equal to the interpreter's, na included.
fn assert_standalone_wasm_plot_matches_vm(
    program: &pinewasm_csil::CsilProgram,
    wasm: &[u8],
    data: &MarketData,
    context: &str,
) {
    let outcome = run(program, data, &RunOptions::default());
    assert!(
        outcome.errors.is_empty(),
        "{context}: interpreter errors: {:?}",
        outcome.errors
    );
    let mut host = StandaloneWasmHost::new(wasm);
    host.evaluate(&standalone_wasm_wire(data, None))
        .unwrap_or_else(|e| panic!("{context}: evaluate failed: {e}"));
    assert!(!outcome.plots.is_empty(), "{context}: no plots");
    for plot in &outcome.plots {
        let lane = host.plot_lane(plot.slot as usize);
        assert_eq!(lane.len(), data.bars(), "{context}: lane length");
        for (bar, (expected, got)) in plot.values.iter().zip(&lane).enumerate() {
            match expected {
                Value::Na => assert!(
                    got.is_nan(),
                    "{context}: plot {} bar {bar}: interpreter na, module {got}",
                    plot.slot
                ),
                Value::Float(x) => assert_eq!(
                    x.to_bits(),
                    got.to_bits(),
                    "{context}: plot {} bar {bar}: interpreter {x}, module {got}",
                    plot.slot
                ),
                other => panic!(
                    "{context}: plot {} bar {bar}: unexpected {other:?}",
                    plot.slot
                ),
            }
        }
    }
}

/// 16 value sources crossed with 15 numeric consumers, 240 valid scripts.
///
/// Three claims, in order of strength: no script is refused with a type
/// conflict; every script gets exactly the pinned answer; and every module
/// produced agrees with the interpreter bit for bit on every bar.
///
/// Checked by hand, because the regression lives in the library and not
/// in this file: restoring the eager pin in `demand_numeric` (pin the unknown
/// producers to `F64` and restart, with `result_follows_untyped_args`
/// disabled) fails this test with the twelve `T` cells listed above.
#[test]
fn numeric_typing_matrix_has_no_type_conflict_and_matches_the_vm() {
    let data = synthetic_data(48);
    let mut grid: Vec<String> = Vec::new();
    let mut conflicts: Vec<String> = Vec::new();
    let mut internal: Vec<String> = Vec::new();
    let mut accepted = 0usize;
    for (row, (source_label, ..)) in MATRIX_SOURCES.iter().enumerate() {
        let mut line = String::new();
        for (column, (consumer_label, ..)) in MATRIX_CONSUMERS.iter().enumerate() {
            let context = format!("{source_label} / {consumer_label}");
            let source = matrix_program(row, column);
            let (program, module) = matrix_compile(&source, &context);
            // Every script here is front-end output, so a refusal may only be
            // a declared limit. Check the class before the variant: an
            // internal refusal is recorded whatever variant carries it, and
            // only then is the answer narrowed to the matrix's own classes.
            if let Err(error) = &module {
                if error.class() != RefusalClass::Capability {
                    internal.push(format!("{context}: {error}"));
                    if !matches!(error, AotError::TypeConflict(_)) {
                        line.push('X');
                        continue;
                    }
                }
            }
            let outcome = matrix_classify(&module, &context);
            line.push(outcome.symbol());
            match (outcome, module) {
                (MatrixOutcome::Accepted, Ok(wasm)) => {
                    accepted += 1;
                    assert_standalone_wasm_plot_matches_vm(&program, &wasm, &data, &context);
                }
                (MatrixOutcome::TypeConflict, Err(error)) => {
                    conflicts.push(format!("{context}: {error}"));
                }
                _ => {}
            }
        }
        grid.push(line);
    }
    assert!(
        conflicts.is_empty(),
        "valid scripts refused with a type conflict:\n{}",
        conflicts.join("\n")
    );
    assert!(
        internal.is_empty(),
        "valid scripts refused for an internal reason:\n{}",
        internal.join("\n")
    );
    assert_eq!(
        grid, MATRIX_EXPECTED,
        "the standalone build's answer changed for the numeric typing matrix"
    );
    let limited = grid
        .iter()
        .flat_map(|line| line.chars())
        .filter(|&c| c == 'I')
        .count();
    assert_eq!((accepted, limited), (240, 0));
}

/// The classifier's own check: a program whose typing really is
/// contradictory must still be classified as a type conflict, so the matrix
/// cannot pass by never seeing one. A float stored into a slot the program
/// already uses as an int is not producible by the front end; it is planted
/// in the compiled program.
#[test]
fn numeric_typing_matrix_classifier_still_sees_a_real_type_conflict() {
    let mut program = compile_csil(
        "//@version=6\nindicator(\"control\")\nvar int n = 0\nn := 5\nplot(close, \"Backtest Signal\")\n",
    );
    let function = &mut program.functions[0];
    let slot = function
        .code
        .iter()
        .find_map(|instr| match instr {
            pinewasm_csil::CsilInstr::StoreUserSlotCurrent { slot } => Some(*slot),
            _ => None,
        })
        .expect("the program stores its int slot");
    let at = function.code.len() - 1;
    let planted = [
        pinewasm_csil::CsilInstr::PushF64(pinewasm_csil::F64Bits::new(1.5)),
        pinewasm_csil::CsilInstr::StoreUserSlotCurrent { slot },
    ];
    let mapped = function.source_map[at];
    function.code.splice(at..at, planted);
    function.source_map.splice(at..at, [mapped, mapped]);
    // The edit invalidates the stored budget data.
    function.budget_reference = None;
    let module = compile_standalone_wasm_program(
        &program,
        &StandaloneWasmOptions {
            signal_plot: None,
            module_type: StandaloneWasmModuleType::Strategy,
        },
    );
    assert_eq!(
        matrix_classify(&module, "planted type conflict"),
        MatrixOutcome::TypeConflict,
        "{module:?}"
    );
    assert_eq!(
        module.expect_err("the planted conflict is refused").class(),
        RefusalClass::Internal
    );
}

/// The same guess at the other numeric-polymorphic consumers. Each script
/// copies an int field into an int variable, reads it through a consumer
/// that takes an int or a float alike, and only then uses it as an int. All
/// were type conflicts while the first reader pinned the variable to f64.
#[test]
fn an_int_read_polymorphically_before_its_int_use_stays_int() {
    let data = synthetic_data(48);
    let prelude = "//@version=6\nindicator(\"first\")\ntype S\n    float src\n    int n\n\
                   S st = S.new(close, input.int(8, \"N\"))\nint k = st.n\n";
    for (label, reader) in [
        ("ordered comparison, left", "r = k > 2 ? 1 : 0"),
        ("ordered comparison, right", "r = 2 < k ? 1 : 0"),
        ("math.round", "r = math.round(k)"),
        ("math.floor", "r = math.floor(k)"),
        ("math.ceil", "r = math.ceil(k)"),
        ("int()", "r = int(k)"),
        ("float()", "r = float(k)"),
        ("math.abs", "int r = math.abs(k)"),
        ("math.sign", "int r = math.sign(k)"),
        ("math.min", "int r = math.min(k, 3)"),
        ("math.max", "int r = math.max(k, 3)"),
    ] {
        let source = format!(
            "{prelude}{reader}\nx = close / k * 100\nplot(x > 50 ? 1 : 0, \"Backtest Signal\")\n\
             plot(r, \"Reader\")\nplot(k, \"Value\")\nplot(x, \"Widened\")\n"
        );
        let (program, module) = matrix_compile(&source, label);
        let wasm = module.unwrap_or_else(|e| panic!("{label}: refused: {e:?}"));
        assert_standalone_wasm_plot_matches_vm(&program, &wasm, &data, label);
    }
}

/// The host tier is where the guess is final: a string array fails the
/// native-heap gate, so object fields stay on the host and the inferred cell
/// type is what the host encodes each field value into. Values of the field
/// change sign and are na on every fifth bar, so `math.min`, `math.max`,
/// `math.abs` and `math.sign` cannot be swapped for one another unnoticed.
#[test]
fn untyped_int_fields_match_the_vm_on_the_host_heap() {
    let source = "//@version=6\nindicator(\"host\")\ntype S\n    int n\n    int m\n\
                  var S st = S.new(8, 5)\n\
                  st.n := bar_index % 5 == 0 ? na : bar_index % 2 == 0 ? 6 : -4\n\
                  st.m := bar_index % 3 == 0 ? 2 : 9\n\
                  int lo = math.min(st.n, 3)\nint hi = math.max(st.n, 3)\n\
                  int both = math.min(st.n, st.m)\n\
                  int a = math.abs(st.n)\nint g = math.sign(st.n)\n\
                  int k = st.m\nup = k > 4 ? 1 : 0\n\
                  unused = math.max(st.n, st.m)\n\
                  plot(close / lo, \"Min\")\nplot(close / hi, \"Max\")\nplot(close / both, \"Both\")\n\
                  plot(close / a, \"Abs\")\nplot(close * g, \"Sign\")\nplot(close / k + up, \"Copy\")\n\
                  plot(math.max(na, 3), \"NaMax\")\nplot(math.abs(na), \"NaAbs\")\n\
                  var tags = array.new_string()\narray.push(tags, \"x\")\n";
    let program = compile_csil(source);
    let aot = compile_program(&program).unwrap_or_else(|e| panic!("host heap: {e}"));
    assert!(
        !aot.layout.native_heap && aot.layout.imports_vm,
        "the string array must keep field reads on the host"
    );
    let typed = analyze(&program).expect("typed");
    use pinewasm_hir::PureFunc as P;
    let results: Vec<(P, VTy)> = program
        .main()
        .code
        .iter()
        .enumerate()
        .filter_map(|(pc, instr)| match instr {
            pinewasm_csil::CsilInstr::CallPure { func, .. }
                if matches!(func, P::MathMin | P::MathMax | P::MathAbs | P::MathSign) =>
            {
                Some((*func, typed.result_tys[pc][0]))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        results,
        vec![
            (P::MathMin, VTy::I64),
            (P::MathMax, VTy::I64),
            (P::MathMin, VTy::I64),
            (P::MathAbs, VTy::I64),
            (P::MathSign, VTy::I64),
            // Storage-side evidence keeps both integer field operands
            // exact even when the result is discarded.
            (P::MathMax, VTy::I64),
            (P::MathMax, VTy::F64),
            (P::MathAbs, VTy::F64),
        ]
    );
    differential_with(
        source,
        &synthetic_data(64),
        "untyped int fields on the host heap",
    );
}

/// A bool field read on the host and consumed only through history keeps its
/// bool lane: `bool()` wraps the history read and states no type, and the
/// host would refuse to encode a bool into a float cell.
#[test]
fn a_bool_field_read_only_through_history_keeps_its_bool_lane() {
    let source =
        "//@version=6\nindicator(\"hist\")\ntype S\n    bool up\nvar S st = S.new(false)\n\
                  st.up := close > open\nbool u = st.up\nw = u[1]\nplot(w ? 1 : 0)\n\
                  var tags = array.new_string()\narray.push(tags, \"x\")\n";
    let program = compile_csil(source);
    let typed = analyze(&program).expect("typed");
    let slot = program
        .series_slots
        .iter()
        .position(|slot| slot.name == "u")
        .expect("slot u");
    assert_eq!(typed.slot_ty[slot], VTy::Bool3);
    differential_with(source, &synthetic_data(32), "bool field through history");
}

/// An na-initialized int compared before its first int store was refused as
/// "slot 0 stored as I64 but pinned F64".
#[test]
fn an_na_int_compared_before_its_first_store_stays_int() {
    let source = "//@version=6\nindicator(\"count\")\nvar int count = na\nhit = count > 2\n\
                  count := bar_index > 5 ? 3 : 0\nplot(hit ? 1 : 0, \"Backtest Signal\")\n";
    let (program, module) = matrix_compile(source, "na int compared first");
    let wasm = module.unwrap_or_else(|e| panic!("refused: {e:?}"));
    assert_standalone_wasm_plot_matches_vm(&program, &wasm, &synthetic_data(48), "na int");
    differential(source, &synthetic_data(48));
}

/// `bool()` takes a number or a bool, so it states no type either. Pinning an
/// untyped operand to the bool lane on sight would turn the comparison's own
/// guess into a type conflict. Both orders must compile and `bool(na)` must
/// stay false.
#[test]
fn bool_cast_of_an_untyped_value_does_not_decide_its_type() {
    let data = synthetic_data(48);
    for (label, body) in [
        ("comparison first", "a = f > 1 ? 1 : 0\nb = bool(f)"),
        ("cast first", "b = bool(f)\na = f > 1 ? 1 : 0"),
    ] {
        for declaration in [
            "float f = na",
            "var float f = na\nf := bar_index > 5 ? close : f",
        ] {
            let source = format!(
                "//@version=6\nindicator(\"bool\")\n{declaration}\n{body}\n\
                 plot(b ? -1 : a, \"Backtest Signal\")\n"
            );
            let context = format!("{label} / {declaration}");
            let (program, module) = matrix_compile(&source, &context);
            let wasm = module.unwrap_or_else(|e| panic!("{context}: refused: {e:?}"));
            assert_standalone_wasm_plot_matches_vm(&program, &wasm, &data, &context);
            differential(&source, &data);
        }
    }
}

/// Every emitted memory declares a `maximum`.
///
/// A module that ships `maximum: None` leaves growth open-ended. A host
/// running untrusted artifacts is entitled to reject that: it cannot compare an
/// undeclared bound against its own `StoreLimits` before instantiating. This
/// pins the declaration across both tiers, since they build the memory section
/// at separate sites in `emit.rs` and only one of them was easy to notice.
///
/// The bound itself is host-advisory. `StoreLimits` remains the enforcement,
/// and the loader-written element budget still fails allocations first on any
/// realistic program.
#[test]
fn emitted_memory_declares_a_maximum() {
    fn memory_limits(wasm: &[u8], context: &str) -> (u64, Option<u64>) {
        let mut found = None;
        for payload in wasmparser::Parser::new(0).parse_all(wasm) {
            if let wasmparser::Payload::MemorySection(section) = payload.expect("well-formed") {
                for memory in section {
                    let memory = memory.expect("well-formed memory entry");
                    assert!(
                        found.is_none(),
                        "{context}: expected exactly one memory, found a second"
                    );
                    found = Some((memory.initial, memory.maximum));
                }
            }
        }
        found.unwrap_or_else(|| panic!("{context}: no memory section"))
    }

    // Native tier: the native heap grows at run time, so this is the site that
    // actually relies on `memory.grow`.
    let native = compile_program(&compile_csil("indicator(\"M\")\nplot(ta.sma(close, 14))\n"))
        .expect("sma compiles native");
    assert_eq!(native.layout.execution_mode, ExecutionMode::Native);
    let (initial, maximum) = memory_limits(&native.wasm, "native");
    let maximum = maximum.expect("native module declares a memory maximum");
    assert_eq!(maximum, 4_096, "native memory cap is exactly 256 MiB");
    assert!(
        maximum >= initial,
        "native: declared maximum {maximum} is below the initial {initial}"
    );

    // Hosted tier: a separate memory declaration in `emit_interpreter_hosted`.
    // Reached via the universal artifact entry point, which falls back for any
    // program the native walk declines.
    let hosted = compile_program_to_wasm(&compile_csil(
        "indicator(\"H\")\nvar m = map.new<string, float>()\nmap.put(m, \"k\", close)\nplot(map.get(m, \"k\"))\n",
    ))
    .expect("universal entry point always yields an artifact");
    let (_, hosted_maximum) = memory_limits(&hosted.wasm, "hosted");
    assert!(
        hosted_maximum.is_some(),
        "hosted stub declares a memory maximum"
    );

    // Both tiers agree, so a loader can apply one policy to either artifact.
    assert_eq!(
        Some(maximum),
        hosted_maximum,
        "both tiers declare the same maximum"
    );
}

// ---------------------------------------------------------------------------
// `parameter_schema` / `module_metadata` custom sections
// ---------------------------------------------------------------------------

/// One custom section's payload, parsed. Panics when absent: every artifact
/// carries both, so a missing one is a regression, not a variant.
fn metadata_section(wasm: &[u8], name: &str) -> serde_json::Value {
    let mut found = None;
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        if let wasmparser::Payload::CustomSection(section) = payload.expect("well-formed") {
            if section.name() == name {
                assert!(found.is_none(), "expected exactly one {name}");
                found = Some(section.data().to_vec());
            }
        }
    }
    let bytes = found.unwrap_or_else(|| panic!("artifact carries a {name} section"));
    serde_json::from_slice(&bytes).expect("the section payload is JSON")
}

fn parameter_schema(wasm: &[u8]) -> serde_json::Value {
    metadata_section(wasm, pinewasm_aot::schema::PARAMETER_SCHEMA_SECTION)
}

fn module_metadata(wasm: &[u8]) -> serde_json::Value {
    metadata_section(wasm, pinewasm_aot::schema::MODULE_METADATA_SECTION)
}

/// Apply the parameter-schema acceptance rules.
///
/// Every rejected entry is ignored rather than reported as an error, so the
/// round-trip tests must exercise the accepted shape explicitly.
fn accepted_parameter_entries(schema: &serde_json::Value) -> Vec<serde_json::Value> {
    let Some(entries) = schema.as_array() else {
        return Vec::new(); // a schema that is not a list has no entries
    };
    entries
        .iter()
        .filter(|entry| entry.is_object())
        .filter(|entry| entry["name"].is_string())
        .filter(|entry| entry["label"].is_string())
        .filter(|entry| matches!(entry["type"].as_str(), Some("float" | "int" | "bool")))
        .cloned()
        .collect()
}

/// A configurable script must expose at least one parameter.
#[test]
fn a_configurable_script_preserves_parameters() {
    let source = "indicator(\"Configurable Trend\")\n\
        factor = input.float(3.0, \"Factor\", minval = 0.1, maxval = 10.0)\n\
        period = input.int(10, \"Period\", minval = 1, maxval = 100)\n\
        basis = ta.sma(close, period)\n\
        plot(close > basis ? factor : -factor)\n";
    let artifact =
        compile_program_to_wasm(&compile_csil(source)).expect("valid CSIL always emits wasm");

    let schema = parameter_schema(&artifact.wasm);
    let accepted = accepted_parameter_entries(&schema);
    assert!(
        !accepted.is_empty(),
        "the parameter section must list at least one parameter, got {schema}"
    );
    assert_eq!(accepted[0]["name"], "factor");
    assert_eq!(accepted[0]["label"], "Factor");
    assert_eq!(accepted[0]["type"], "float");
}

/// A representative mix of `input.*` declarations, checked through the
/// acceptance filter rather than only against the serializer's own output.
#[test]
fn parameter_schema_round_trips_input_declarations() {
    let source = "\
indicator(\"Schema\")
enum Mode
    fast = \"Fast\"
    slow = \"Slow\"
len = input.int(14, \"Length\", minval = 1, maxval = 100, step = 1, group = \"Signal\", tooltip = \"Lookback bars\")
mult = input.float(2.0, \"Multiplier\", minval = 0.5, maxval = 10.0, step = 0.5, group = \"Signal\")
floor = input.int(3, \"Length\")
enabled = input.bool(true, \"Enabled\")
src = input.source(close, \"Source\")
style = input.string(\"SMA\", \"Style\", options = [\"SMA\", \"EMA\"])
tint = input.color(#2196F3, \"Tint\")
mode = input.enum(Mode.fast, \"Mode\")
tf = input.timeframe(\"D\", \"Resolution\")
notes = input.text_area(\"hello\", \"Notes\")
base = ta.sma(src, len)
plot(enabled ? base * mult + floor : base, color = tint)
plot(style == \"SMA\" ? 1.0 : 0.0)
plot(mode == Mode.fast ? 1.0 : 0.0)
plot(tf == \"D\" ? 1.0 : 0.0)
plot(notes == \"\" ? 1.0 : 0.0)
";
    let artifact = compile_program_to_wasm(&compile_csil(source)).expect("emits");
    let schema = parameter_schema(&artifact.wasm);

    // Everything we emit survives the acceptance filter. Emitting an entry the
    // filter would drop is the same as emitting nothing, only harder to notice.
    let params = schema.as_array().expect("a top-level array").clone();
    assert_eq!(
        accepted_parameter_entries(&schema).len(),
        params.len(),
        "every emitted entry must pass the acceptance filter: {schema}"
    );

    let by_name = |name: &str| {
        params
            .iter()
            .find(|p| p["name"] == name)
            .unwrap_or_else(|| panic!("no parameter named {name}: {schema}"))
            .clone()
    };

    // A bounded int: the case the whole section exists for.
    let length = by_name("length");
    assert_eq!(length["type"], "int");
    assert_eq!(length["label"], "Length");
    assert_eq!(length["group"], "Signal");
    assert_eq!(length["default"], 14);
    assert_eq!(length["min"], 1);
    assert_eq!(length["max"], 100);
    assert_eq!(length["step"], 1);

    let mult = by_name("multiplier");
    assert_eq!(mult["type"], "float");
    assert_eq!(mult["default"], 2.0);
    // A whole float stays a float in the serialized JSON: `2.0`, never `2`.
    // A float parameter read back as an int has the wrong type, so the
    // assertions below check the float type.
    assert!(mult["default"].is_f64(), "float default keeps its type");
    assert!(mult["max"].is_f64(), "float bounds keep their type");
    assert!(length["default"].is_i64(), "int default stays an int");
    assert_eq!(mult["min"], 0.5);
    assert_eq!(mult["max"], 10.0);

    // Two inputs titled "Length": ordinary Pine, so names are disambiguated
    // rather than assumed unique.
    let second = by_name("length_2");
    assert_eq!(second["default"], 3);
    // No declared bounds: a lone bound would leave the other end undefined.
    assert!(second.get("min").is_none(), "no invented bounds");
    assert!(second.get("max").is_none(), "no invented bounds");

    let enabled = by_name("enabled");
    assert_eq!(enabled["type"], "bool");
    assert_eq!(enabled["default"], true);

    // An enum's runtime value is its member index, so it is declared as an int
    // over the full member range. The parameter section has no enum type and
    // no options list, so this is the only representation that keeps it
    // declarable at all.
    let mode = by_name("mode");
    assert_eq!(mode["type"], "int");
    assert_eq!(mode["default"], 0);
    assert_eq!(mode["min"], 0);
    assert_eq!(mode["max"], 1);

    // Strings, colors, and sources have no representation in the parameter
    // section. Entries of unrecognized type are dropped by the acceptance
    // filter; omitting them reaches the same outcome by saying so. The
    // declarations still ship in the artifact envelope.
    for absent in ["source", "style", "tint", "resolution", "notes"] {
        assert!(
            !params.iter().any(|p| p["name"] == absent),
            "{absent} is not expressible in the parameter section: {schema}"
        );
    }

    wasmparser::validate(&artifact.wasm).expect("artifact stays valid WebAssembly");
}

/// A script with no inputs emits an explicit empty array rather than nothing
/// at all. "This program has no parameters" and "this artifact says nothing
/// about parameters" are different claims, and a reader that cannot tell
/// them apart could mistake one for the other.
#[test]
fn parameter_schema_is_explicit_when_a_script_has_no_inputs() {
    let artifact = compile_program_to_wasm(&compile_csil(
        "indicator(\"None\")\nplot(ta.sma(close, 14))\n",
    ))
    .expect("valid CSIL always emits wasm");
    assert_eq!(
        parameter_schema(&artifact.wasm),
        serde_json::json!([]),
        "an empty list, not an absent section"
    );
}

/// Both sections cover the hosted tier. A hosted artifact's inputs and
/// identity are as real as a native one's; the tier is an implementation
/// detail of how the program runs, not of what it can be asked.
#[test]
fn metadata_sections_cover_the_hosted_tier() {
    let hosted = compile_program_to_wasm(&compile_csil(
        "strategy(\"H\", calc_on_order_fills = true)\nlen = input.int(20, \"Window\", minval = 2, maxval = 50)\nif ta.crossover(close, ta.sma(close, len))\n    strategy.entry(\"L\", strategy.long)\nplot(ta.sma(close, len))\n",
    ))
    .expect("valid CSIL always emits wasm");
    assert_eq!(
        hosted.layout.execution_mode,
        ExecutionMode::InterpreterHosted
    );

    let accepted = accepted_parameter_entries(&parameter_schema(&hosted.wasm));
    assert_eq!(accepted.len(), 1);
    assert_eq!(accepted[0]["name"], "window");
    assert_eq!(accepted[0]["min"], 2);
    assert_eq!(accepted[0]["max"], 50);

    let meta = module_metadata(&hosted.wasm);
    assert_eq!(meta["type"], "strategy", "a strategy declares itself one");
    wasmparser::validate(&hosted.wasm).expect("hosted artifact stays valid WebAssembly");
}

/// `module_metadata` carries the `module_id` slug, and every field satisfies
/// the id pattern.
///
/// The id pattern is applied to the whole string, not as a prefix match: a
/// regex `$` also matches before a trailing newline, so a partial match would
/// accept `rsi\n`. A slug that can never contain a newline is the reason that
/// distinction stays uninteresting here.
#[test]
fn module_metadata_claims_a_catalog_id() {
    let artifact = compile_program_to_wasm(&compile_csil(
        "indicator(\"Example Momentum\")\nplot(close)\n",
    ))
    .expect("emits");
    let meta = module_metadata(&artifact.wasm);

    assert_eq!(meta["schema_version"], 1);
    assert_eq!(meta["module_id"], "example-momentum");
    assert_eq!(meta["display_name"], "Example Momentum");
    assert_eq!(meta["type"], "indicator");

    // Kebab, never snake: the id pattern admits no underscores.
    let id = meta["module_id"].as_str().expect("a string id");
    assert!(
        !id.contains('_'),
        "an id containing an underscore is rejected: {id}"
    );
    assert!(is_catalog_id(id), "must satisfy the id pattern: {id}");
}

/// `^[a-z0-9]([a-z0-9-]{0,62}[a-z0-9])?$`, applied whole-string.
fn is_catalog_id(id: &str) -> bool {
    let ok_char = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    if id.is_empty() || id.len() > 64 {
        return false;
    }
    let bytes: Vec<char> = id.chars().collect();
    if !ok_char(bytes[0]) || !ok_char(bytes[bytes.len() - 1]) {
        return false;
    }
    bytes.iter().all(|&c| ok_char(c) || c == '-')
}

/// Titles that a naive slug turns into something the id pattern rejects.
#[test]
fn module_id_survives_hostile_titles() {
    let id_for = |title: &str| {
        let source = format!("indicator(\"{title}\")\nplot(close)\n");
        let artifact = compile_program_to_wasm(&compile_csil(&source)).expect("emits");
        module_metadata(&artifact.wasm)["module_id"]
            .as_str()
            .expect("a string id")
            .to_string()
    };

    // Trailing punctuation would leave a trailing hyphen.
    assert_eq!(id_for("Trailing Punctuation!!!"), "trailing-punctuation");
    assert_eq!(id_for("  Spaced  Out  "), "spaced-out");
    assert_eq!(id_for("RSI (14) / Fast"), "rsi-14-fast");
    // Non-ASCII slugs to nothing, so the module type stands in rather than an
    // empty id, which is not a valid id.
    assert_eq!(id_for("длина"), "indicator");
    assert_eq!(id_for("!!!"), "indicator");

    // Over the 64-character ceiling, and the truncation lands mid-separator:
    // trimming has to happen after the clamp or the result ends in a hyphen.
    let long =
        id_for("Aaaaaaaaaa Bbbbbbbbbb Cccccccccc Dddddddddd Eeeeeeeeee Ffffffffff Gggggggggg");
    assert!(long.len() <= 64, "clamped to the pattern's ceiling: {long}");
    assert!(
        is_catalog_id(&long),
        "still a legal id after clamping: {long}"
    );

    for id in [
        "trailing-punctuation",
        "spaced-out",
        "rsi-14-fast",
        "indicator",
    ] {
        assert!(is_catalog_id(id), "{id}");
    }
}

/// `module_metadata` is the section with a real size cap: an over-size one
/// must be rejected outright rather than partially read. The parameter
/// section has no corresponding limit.
#[test]
fn module_metadata_fits_the_consumer_cap() {
    let title = "A".repeat(400);
    let artifact = compile_program_to_wasm(&compile_csil(&format!(
        "indicator(\"{title}\")\nplot(close)\n"
    )))
    .expect("emits");
    let bytes = serde_json::to_vec(&module_metadata(&artifact.wasm)).expect("re-encodes");
    assert!(
        bytes.len() <= pinewasm_aot::schema::MODULE_METADATA_MAX_BYTES,
        "module_metadata must fit the cap, got {} bytes",
        bytes.len()
    );

    let meta = module_metadata(&artifact.wasm);
    let display = meta["display_name"].as_str().expect("a display name");
    assert_eq!(
        display.chars().count(),
        100,
        "clamped to the display-name limit"
    );
}

/// Emission stays a pure function of the program, sections included.
///
/// Re-emitting a validated program must reproduce the same module hash, so
/// metadata payloads must not vary from run to run.
#[test]
fn parameter_schema_emission_is_reproducible() {
    let program = compile_csil(
        "indicator(\"R\")\na = input.int(5, \"Alpha\", minval = 1, maxval = 9)\nb = input.float(1.5, \"Beta\")\nplot(ta.sma(close, a) * b)\n",
    );
    let first = compile_program_to_wasm(&program).expect("emits").wasm;
    let second = compile_program_to_wasm(&program).expect("emits").wasm;
    assert_eq!(
        first, second,
        "re-emission reproduces the artifact byte for byte"
    );

    // And the section is what distinguishes two programs that differ only in
    // their input declarations. A hosted stub identical for every script
    // would make its hash attest nothing about the program.
    let other = compile_csil(
        "indicator(\"R\")\na = input.int(6, \"Alpha\", minval = 1, maxval = 9)\nb = input.float(1.5, \"Beta\")\nplot(ta.sma(close, a) * b)\n",
    );
    assert_ne!(
        first,
        compile_program_to_wasm(&other).expect("emits").wasm,
        "a changed default changes the artifact"
    );
}

#[test]
fn linked_runtime_keeps_only_the_bodies_the_program_reaches() {
    // The numeric runtime is ~15.4 KiB and 93% of that is its code section, so
    // linking it whole made every transcendental script pay for all of it.
    // Bodies nothing reaches are stubbed; indices are preserved, so `entry`
    // still addresses functions positionally.
    let wasm = |src: &str| {
        compile_program_to_wasm(&compile_csil(src))
            .expect("valid CSIL emits wasm")
            .wasm
            .len()
    };

    let exp_only = wasm("indicator(\"N\")\nplot(math.exp(close))\n");
    let everything = wasm(concat!(
        "indicator(\"N\")\n",
        "plot(math.exp(close) + math.log(close) + math.log10(close)\n",
        "  + math.sin(close) + math.cos(close) + math.tan(close)\n",
        "  + math.asin(close) + math.acos(close) + math.atan(close)\n",
        "  + math.pow(close, 2.0) + math.round(close, 2) + (close % 3.0))\n"
    ));

    // A one-function script keeps a small fraction of the runtime.
    assert!(
        exp_only < everything / 4,
        "exp-only artifact should be far smaller: {exp_only} vs {everything}"
    );
    // And it is not empty-shaken: the reachable chain is still there.
    assert!(exp_only > 1_000, "exp still needs its own body: {exp_only}");

    // A program using every id keeps every body, so shaking must be a no-op
    // rather than a smaller-but-broken module. This is the guard against the
    // reachability walk dropping something it should have kept.
    let all_kept = wasm("indicator(\"N\")\nplot(close)\n");
    assert!(
        all_kept < exp_only,
        "a script linking no runtime at all is smaller still: {all_kept} vs {exp_only}"
    );
}

// ---- native strategy() → standalone WebAssembly differential ----------------------------------
//
// Synthetic fixtures require each strategy artifact's posture lane and
// equity accounting to remain bit-equal to the interpreter. Focused
// cases below exercise order types, exits, reversals, pyramiding, risk rules,
// margin calls, order-on-close behavior, and fraction-truncated evaluation.
//
// The interpreter's posture is reconstructed from the closed-trade ledger plus
// the open-position tail rather than `plot(strategy.position_size)`: in
// PineWASM's runtime that getter reads the pre-body snapshot, so under
// `process_orders_on_close` a same-bar close-pass fill is not visible to it
// until the next bar, while the
// standalone WebAssembly posture lane samples the broker after the close pass,
// so the event bar is the bar on which the position actually changed.

/// One OUT signal under posture-events: `(time, direction ∈ {1,0,-1}, price)`.
type PostureSignal = (i64, i64, f64);

fn compile_strategy_standalone_wasm(source: &str, context: &str) -> Vec<u8> {
    compile_standalone_wasm_program(
        &compile_csil(source),
        &StandaloneWasmOptions {
            // A strategy source designates nothing — the broker's
            // posture lane is the signal source.
            signal_plot: None,
            module_type: StandaloneWasmModuleType::Strategy,
        },
    )
    .unwrap_or_else(|e| panic!("{context}: strategy standalone WebAssembly emit failed: {e}"))
}

/// Parse an OUT document, asserting the three-field element shape,
/// posture-events direction domain, strict ordering, close pricing, and
/// flat-event well-formedness.
fn posture_signals(
    out: &[u8],
    data: &MarketData,
    slice_end: usize,
    context: &str,
) -> Vec<PostureSignal> {
    let doc: serde_json::Value = serde_json::from_slice(out).expect("OUT is valid JSON");
    assert_eq!(doc["version"], 1, "{context}: OUT version");
    let signals: Vec<PostureSignal> = doc["signals"]
        .as_array()
        .expect("signals array")
        .iter()
        .map(|s| {
            let element = s.as_object().expect("signal object");
            assert_eq!(
                element.len(),
                3,
                "{context}: exactly {{time, direction, price}}: {s}"
            );
            let direction = s["direction"].as_i64().expect("integer direction");
            assert!(
                matches!(direction, -1..=1),
                "{context}: direction must be 1, -1, or 0, got {direction}"
            );
            (
                s["time"].as_i64().expect("integer time"),
                direction,
                s["price"].as_f64().expect("price"),
            )
        })
        .collect();

    if let Some(first) = signals.first() {
        assert_ne!(first.1, 0, "{context}: first event must be nonzero");
    }
    for pair in signals.windows(2) {
        assert!(
            !(pair[0].1 == 0 && pair[1].1 == 0),
            "{context}: 0 event after 0 event"
        );
        assert!(
            pair[0].0 < pair[1].0,
            "{context}: times strictly increasing"
        );
    }
    for (time, _, price) in &signals {
        let bar = data.time[..slice_end]
            .iter()
            .position(|t| t == time)
            .unwrap_or_else(|| panic!("{context}: signal time {time} matches no evaluated bar"));
        assert_eq!(
            price.to_bits(),
            data.close[bar].to_bits(),
            "{context}: price is the event bar's close"
        );
    }
    signals
}

/// Reconstruct the per-bar posture lane from sparse posture events — the
/// inverse of edge detection, lossless because posture is constant between
/// events.
fn signals_to_lane(signals: &[PostureSignal], data: &MarketData, slice_end: usize) -> Vec<i8> {
    let mut lane = vec![0i8; slice_end];
    let mut cur = 0i8;
    let mut next = 0usize;
    for (i, cell) in lane.iter_mut().enumerate() {
        if next < signals.len() && signals[next].0 == data.time[i] {
            cur = signals[next].1 as i8;
            next += 1;
        }
        *cell = cur;
    }
    assert_eq!(next, signals.len(), "every signal lands on exactly one bar");
    lane
}

/// The interpreter's results for one strategy source over one feed: the
/// end-of-bar posture lane, the `strategy.equity` getter lane (bit-level),
/// and the full outcome for scenario assertions.
struct StrategyReference {
    posture: Vec<i8>,
    equity_bits: Vec<u64>,
    outcome: pinewasm_vm::RunOutcome,
}

fn strategy_reference(source: &str, data: &MarketData, context: &str) -> StrategyReference {
    let with_refs = format!(
        "{source}\n\
         plot(strategy.opentrades == 0 ? na : strategy.opentrades.entry_bar_index(0), \"OpenEntryRef\")\n\
         plot(strategy.equity, \"EquityRef\")\n"
    );
    let program = compile_csil(&with_refs);
    let outcome = run(&program, data, &RunOptions::default());
    assert!(
        outcome.errors.is_empty(),
        "{context}: interpreter run failed: {:?}",
        outcome.errors
    );
    let strategy = outcome.strategy.as_ref().expect("strategy outcome");
    let n = data.bars();

    // A trade's position exists from the end of its entry (fill) bar through
    // the end of the bar before its exit (fill) bar: sign · qty over
    // [entry_bar, exit_bar). The still-open position contributes from its own
    // entry bar, read off the appended OpenEntryRef plot (na there = the lot
    // opened on the final bar's close pass, invisible to every snapshot).
    let mut net = vec![0.0f64; n];
    for trade in &strategy.trades {
        assert!(trade.sign != 0.0, "closed trade carries a direction sign");
        let signed = trade.sign * trade.qty;
        for cell in net
            .iter_mut()
            .take(trade.exit_bar as usize)
            .skip(trade.entry_bar as usize)
        {
            *cell += signed;
        }
    }
    let plots = &outcome.plots;
    assert!(plots.len() >= 2, "{context}: appended reference plots");
    let position = strategy.position.size;
    if position != 0.0 {
        let open_entry = match plots[plots.len() - 2]
            .values
            .last()
            .expect("final bar value")
        {
            Value::Float(e) => *e as usize,
            Value::Int(e) => *e as usize,
            _ => n - 1,
        };
        for cell in net.iter_mut().skip(open_entry) {
            *cell += position;
        }
    }
    let posture: Vec<i8> = net
        .iter()
        .map(|v| {
            if *v > 0.0 {
                1
            } else if *v < 0.0 {
                -1
            } else {
                0
            }
        })
        .collect();
    let expected_last = if position > 0.0 {
        1
    } else if position < 0.0 {
        -1
    } else {
        0
    };
    assert_eq!(
        posture.last().copied().unwrap_or(0),
        expected_last,
        "{context}: reference reconstruction does not reconcile with the final position"
    );

    let equity_bits: Vec<u64> = plots[plots.len() - 1]
        .values
        .iter()
        .map(|v| match v {
            Value::Float(x) => x.to_bits(),
            Value::Int(x) => (*x as f64).to_bits(),
            Value::Na => f64::NAN.to_bits(),
            other => panic!("{context}: unexpected equity value {other:?}"),
        })
        .collect();
    assert_eq!(equity_bits.len(), n, "{context}: equity lane length");

    StrategyReference {
        posture,
        equity_bits,
        outcome,
    }
}

/// Truncate a feed to its first `n` bars (the evaluated window).
fn window(data: &MarketData, n: usize) -> MarketData {
    MarketData {
        open: data.open[..n].to_vec(),
        high: data.high[..n].to_vec(),
        low: data.low[..n].to_vec(),
        close: data.close[..n].to_vec(),
        volume: data.volume[..n].to_vec(),
        time: data.time[..n].to_vec(),
        ..MarketData::default()
    }
}

/// The full differential for one strategy source over one feed:
///
/// 1. the unmodified artifact's OUT signals satisfy the documented contract and their
///    reconstructed posture lane equals the interpreter's bar for bar;
/// 2. an artifact with an appended `plot(strategy.equity)` produces the
///    identical signal series (plots never touch the broker) and its equity
///    getter lane is bit-equal to the interpreter's;
/// 3. under `fraction`, both sides evaluate exactly the truncated window.
fn assert_strategy_differential(
    context: &str,
    source: &str,
    data: &MarketData,
    fraction: Option<f64>,
) -> (Vec<PostureSignal>, pinewasm_vm::RunOutcome) {
    let n = data.bars();
    let slice_end = fraction.map_or(n, |f| (((n as f64) * f).floor() as usize).max(1));
    let evaluated = window(data, slice_end);
    let reference = strategy_reference(source, &evaluated, context);

    let wire = standalone_wasm_wire(data, fraction);
    let wasm = compile_strategy_standalone_wasm(source, context);
    let out = StandaloneWasmHost::new(&wasm)
        .evaluate(&wire)
        .unwrap_or_else(|e| panic!("{context}: evaluate failed: {e}"));
    let signals = posture_signals(&out, data, slice_end, context);
    let lane = signals_to_lane(&signals, data, slice_end);
    for (bar, (aot, interp)) in lane.iter().zip(&reference.posture).enumerate() {
        assert_eq!(
            aot, interp,
            "{context}: posture diverges at bar {bar} (AOT {aot} vs interpreter {interp})"
        );
    }

    let eq_source = format!("{source}\nplot(strategy.equity, \"EquityRefLane\")\n");
    let eq_program = compile_csil(&eq_source);
    let eq_plot = eq_program.plots.len() - 1;
    let eq_wasm = compile_standalone_wasm_program(
        &eq_program,
        &StandaloneWasmOptions {
            signal_plot: None,
            module_type: StandaloneWasmModuleType::Strategy,
        },
    )
    .unwrap_or_else(|e| panic!("{context}: equity-lane standalone WebAssembly emit failed: {e}"));
    let mut host = StandaloneWasmHost::new(&eq_wasm);
    let eq_out = host
        .evaluate(&wire)
        .unwrap_or_else(|e| panic!("{context}: equity-lane evaluate failed: {e}"));
    assert_eq!(
        posture_signals(&eq_out, data, slice_end, context),
        signals,
        "{context}: an appended plot changed the signal series"
    );
    let equity = host.plot_lane(eq_plot);
    assert_eq!(equity.len(), slice_end, "{context}: equity lane length");
    for (bar, (aot, interp)) in equity.iter().zip(&reference.equity_bits).enumerate() {
        let same = aot.to_bits() == *interp || (aot.is_nan() && f64::from_bits(*interp).is_nan());
        assert!(
            same,
            "{context}: equity diverges at bar {bar} (AOT {} vs interpreter {})",
            aot,
            f64::from_bits(*interp)
        );
    }

    (signals, reference.outcome)
}

/// ~500 daily bars with pronounced multi-bar trends and reversals so channel
/// breakouts, crossovers, stops, and mean-reversion entries all trigger
/// (the same construction as the strategy smoke feed). Times are
/// UTC-midnight-aligned so the risk-session key advances daily on both sides.
fn strategy_feed() -> MarketData {
    const BAR0_TIME: i64 = 1_641_600_000_000;
    const MS_PER_DAY: i64 = 86_400_000;
    let mut base = 130.0f64;
    let mut data = MarketData::default();
    for i in 0..500usize {
        let t = i as f64;
        base += match (i / 110) % 4 {
            0 => 0.65,
            1 => -0.55,
            2 => 0.75,
            _ => -0.5,
        };
        let wobble = 3.0 * (t / 4.5).sin();
        let close = base + wobble + 1.0 * (t * 1.3).cos();
        let open = base + wobble - 0.9 * (t * 0.7).sin();
        let high = open.max(close) + 1.5 + 0.5 * (t * 0.9).sin().abs();
        let low = open.min(close) - 1.5 - 0.5 * (t * 1.1).cos().abs();
        let close = match i % 13 {
            0 => low + 0.03 * (high - low),
            4 => high - 0.03 * (high - low),
            _ => close,
        };
        data.open.push(open);
        data.high.push(high);
        data.low.push(low);
        data.close.push(close);
        data.volume.push(1_000.0 + (i % 89) as f64 * 10.0);
        data.time.push(BAR0_TIME + (i as i64) * MS_PER_DAY);
    }
    data
}

#[test]
fn strategy_fraction_truncates_the_evaluated_window() {
    // Under `fraction`, the evaluated window is max(1, floor(n × fraction))
    // bars; every event must land inside it and the truncated run must equal
    // the interpreter over exactly that prefix.
    let data = strategy_feed();
    let source = concat!(
        "strategy(\"Fraction\", initial_capital = 10000)\n",
        "fast = ta.sma(close, 5)\n",
        "slow = ta.sma(close, 20)\n",
        "if fast > slow\n",
        "    strategy.entry(\"L\", strategy.long)\n",
        "if fast < slow\n",
        "    strategy.close(\"L\")\n",
        "plot(close)\n",
    );
    let (signals, _) = assert_strategy_differential("fraction/crossover", source, &data, Some(0.6));
    assert!(
        !signals.is_empty(),
        "the truncated window must still produce events for this gate to bite"
    );
    let last_allowed = data.time[299];
    for (time, _, _) in &signals {
        assert!(
            *time <= last_allowed,
            "event at {time} escaped the fraction-truncated window"
        );
    }
}

// ---- focused strategy edge cases -------------------------------------------
//
// Each fixture crafts a feed that forces one broker behavior, runs the full
// differential (posture + equity and signal-shape checks), then asserts the
// scenario actually fired — on the interpreter's ledger, so a fixture that
// silently stops triggering fails loudly rather than passing as two
// identical no-ops.

/// Build a fixture feed from explicit (open, high, low, close) rows —
/// tick-aligned prices, constant volume, UTC-midnight daily times.
fn fixture_data(rows: &[(f64, f64, f64, f64)]) -> MarketData {
    let mut data = MarketData::default();
    for (i, (o, h, l, c)) in rows.iter().enumerate() {
        data.open.push(*o);
        data.high.push(*h);
        data.low.push(*l);
        data.close.push(*c);
        data.volume.push(1_000.0);
        data.time.push(1_641_600_000_000 + (i as i64) * 86_400_000);
    }
    data
}

/// Signals as (bar index, direction), for fixtures that pin exact shapes.
fn event_bars(signals: &[PostureSignal], data: &MarketData) -> Vec<(usize, i64)> {
    signals
        .iter()
        .map(|(t, d, _)| {
            (
                data.time.iter().position(|x| x == t).expect("event bar"),
                *d,
            )
        })
        .collect()
}

fn drift(from: f64, n: usize) -> Vec<(f64, f64, f64, f64)> {
    (0..n)
        .map(|i| {
            let base = from + i as f64 * 0.25;
            (base, base + 1.0, base - 1.0, base + 0.25)
        })
        .collect()
}

#[test]
fn fixture_limit_entry_fills_at_the_trigger_price() {
    let mut rows = vec![
        (100.00, 101.00, 99.00, 100.00),
        (100.00, 101.00, 98.00, 99.00),
        // Open 97 > limit 95; the walk's high→low segment crosses 95, so the
        // fill is the trigger price, not the open and not the close.
        (97.00, 98.00, 94.00, 96.00),
        (96.00, 97.50, 95.50, 97.00),
        (97.00, 98.00, 96.00, 97.50),
        (97.50, 98.50, 96.50, 98.00),
        (98.00, 99.00, 97.00, 98.50),
        (98.50, 99.50, 97.50, 99.00),
    ];
    rows.extend(drift(99.0, 4));
    let data = fixture_data(&rows);
    let source = "strategy(\"LimitEntry\")\n\
                  if bar_index == 0\n\
                  \tstrategy.entry(\"L\", strategy.long, limit=95.0)\n\
                  if bar_index == 6\n\
                  \tstrategy.close(\"L\")\n\
                  plot(close)\n";
    let (signals, outcome) = assert_strategy_differential("limit-entry", source, &data, None);
    assert_eq!(
        event_bars(&signals, &data),
        vec![(2, 1), (7, 0)],
        "limit entry fills on the crossing bar; the close fills next-bar open"
    );
    let strategy = outcome.strategy.expect("strategy outcome");
    assert_eq!(strategy.trades.len(), 1);
    assert_eq!(
        strategy.trades[0].entry_price, 95.0,
        "a resting limit fills at its trigger price intrabar"
    );
}

#[test]
fn fixture_stop_entry_fills_at_the_trigger_price() {
    let mut rows = vec![
        (100.00, 101.00, 99.00, 100.50),
        (100.50, 102.00, 99.50, 101.50),
        // Walk order is this model's rule: low first when |high − open| > |open − low|
        // (here 4.5 > 1.0). The low→high segment crosses the 105 buy stop,
        // filling at max(segment open, stop) = 105.
        (101.50, 106.00, 100.50, 105.00),
        (105.00, 106.50, 104.00, 106.00),
        (106.00, 107.00, 105.00, 106.50),
        (106.50, 107.50, 105.50, 107.00),
        (107.00, 108.00, 106.00, 107.50),
        (107.50, 108.50, 106.50, 108.00),
    ];
    rows.extend(drift(108.0, 4));
    let data = fixture_data(&rows);
    let source = "strategy(\"StopEntry\")\n\
                  if bar_index == 0\n\
                  \tstrategy.entry(\"L\", strategy.long, stop=105.0)\n\
                  if bar_index == 6\n\
                  \tstrategy.close(\"L\")\n\
                  plot(close)\n";
    let (signals, outcome) = assert_strategy_differential("stop-entry", source, &data, None);
    assert_eq!(event_bars(&signals, &data), vec![(2, 1), (7, 0)]);
    let strategy = outcome.strategy.expect("strategy outcome");
    assert_eq!(strategy.trades.len(), 1);
    assert_eq!(
        strategy.trades[0].entry_price, 105.0,
        "a buy stop fills at its trigger price intrabar"
    );
}

#[test]
fn fixture_stop_limit_entry_converts_and_fills() {
    let mut rows = vec![
        (100.00, 101.00, 99.00, 100.50),
        (100.50, 102.00, 99.50, 101.50),
        (101.50, 106.00, 100.50, 105.00),
        (105.00, 106.50, 104.00, 106.00),
        (106.00, 107.00, 105.00, 106.50),
        (106.50, 107.50, 105.50, 107.00),
        (107.00, 108.00, 106.00, 107.50),
        (107.50, 108.50, 106.50, 108.00),
    ];
    rows.extend(drift(108.0, 4));
    let data = fixture_data(&rows);
    // stop= + limit= on one entry: a stateful stop-limit — the 105 stop
    // activates a 105.50 buy limit rather than a market order.
    let source = "strategy(\"StopLimit\")\n\
                  if bar_index == 0\n\
                  \tstrategy.entry(\"L\", strategy.long, stop=105.0, limit=105.5)\n\
                  if bar_index == 6\n\
                  \tstrategy.close(\"L\")\n\
                  plot(close)\n";
    let (signals, outcome) = assert_strategy_differential("stop-limit", source, &data, None);
    let strategy = outcome.strategy.expect("strategy outcome");
    assert_eq!(strategy.trades.len(), 1, "the stop-limit filled");
    assert!(
        strategy.trades[0].entry_price <= 105.5,
        "the limit leg caps the fill: {}",
        strategy.trades[0].entry_price
    );
    assert_eq!(signals.first().map(|s| s.1), Some(1));
    assert_eq!(signals.last().map(|s| s.1), Some(0));
}

#[test]
fn fixture_trailing_exit_by_trail_price() {
    let mut rows = vec![
        (100.00, 101.00, 99.00, 100.00),
        (100.00, 102.00, 99.50, 101.50), // entry fills at open 100
        (101.50, 103.50, 101.00, 103.00),
        (103.00, 105.00, 102.50, 104.50), // crosses the 104 activation
        (104.50, 108.00, 104.00, 107.50), // best 108 → stop 107
        (106.50, 106.80, 104.00, 104.50), // gap under the stop → exit at open
        (104.50, 105.50, 103.50, 104.00),
    ];
    rows.extend(drift(104.0, 4));
    let data = fixture_data(&rows);
    let source = "strategy(\"TrailPrice\")\n\
                  if bar_index == 0\n\
                  \tstrategy.entry(\"L\", strategy.long)\n\
                  strategy.exit(\"X\", from_entry=\"L\", trail_price=104.0, trail_offset=100)\n\
                  plot(close)\n";
    let (signals, outcome) = assert_strategy_differential("trail-price", source, &data, None);
    let strategy = outcome.strategy.expect("strategy outcome");
    assert_eq!(
        strategy.trades.len(),
        1,
        "the trailing stop closed the long"
    );
    assert!(
        strategy.trades[0].exit_price > 104.0,
        "the ratchet ran the stop above the activation level: {}",
        strategy.trades[0].exit_price
    );
    assert_eq!(signals.last().map(|s| s.1), Some(0), "flat after the stop");
}

#[test]
fn fixture_trailing_exit_by_trail_points() {
    let mut rows = vec![
        (100.00, 101.00, 99.00, 100.00),
        (100.00, 102.00, 99.50, 101.50),
        (101.50, 103.50, 101.00, 103.00),
        (103.00, 105.00, 102.50, 104.50),
        (104.50, 108.00, 104.00, 107.50),
        (106.50, 106.80, 104.00, 104.50),
        (104.50, 105.50, 103.50, 104.00),
    ];
    rows.extend(drift(104.0, 4));
    let data = fixture_data(&rows);
    // trail_points is ticks above the entry price (400 × 0.01 = 4.00 → the
    // same 104 activation as the trail_price twin).
    let source = "strategy(\"TrailPoints\")\n\
                  if bar_index == 0\n\
                  \tstrategy.entry(\"L\", strategy.long)\n\
                  strategy.exit(\"X\", from_entry=\"L\", trail_points=400, trail_offset=100)\n\
                  plot(close)\n";
    let (signals, outcome) = assert_strategy_differential("trail-points", source, &data, None);
    let strategy = outcome.strategy.expect("strategy outcome");
    assert_eq!(
        strategy.trades.len(),
        1,
        "the trailing stop closed the long"
    );
    assert!(strategy.trades[0].exit_price > 104.0);
    assert_eq!(signals.last().map(|s| s.1), Some(0));
}

#[test]
fn fixture_partial_exit_that_stays_long_emits_no_event() {
    let data = fixture_data(&drift(100.0, 12));
    let source = "strategy(\"Partial\", default_qty_value=10)\n\
                  if bar_index == 0\n\
                  \tstrategy.entry(\"L\", strategy.long)\n\
                  if bar_index == 4\n\
                  \tstrategy.close(\"L\", qty_percent=50)\n\
                  plot(close)\n";
    let (signals, outcome) = assert_strategy_differential("partial-exit", source, &data, None);
    assert_eq!(
        event_bars(&signals, &data),
        vec![(1, 1)],
        "a partial close that does not cross flat is not an event"
    );
    let strategy = outcome.strategy.expect("strategy outcome");
    assert_eq!(strategy.trades.len(), 1, "the closed half is a real trade");
    assert_eq!(strategy.position.size, 5.0, "half the lot remains open");
}

#[test]
fn fixture_full_exit_to_flat_is_a_zero_event() {
    let data = fixture_data(&drift(100.0, 12));
    let source = "strategy(\"FlatExit\")\n\
                  if bar_index == 0\n\
                  \tstrategy.entry(\"L\", strategy.long)\n\
                  if bar_index == 4\n\
                  \tstrategy.close(\"L\")\n\
                  plot(close)\n";
    let (signals, outcome) = assert_strategy_differential("flat-exit", source, &data, None);
    assert_eq!(event_bars(&signals, &data), vec![(1, 1), (5, 0)]);
    let strategy = outcome.strategy.expect("strategy outcome");
    assert_eq!(strategy.position.size, 0.0);
}

#[test]
fn fixture_reversal_is_a_single_event() {
    let data = fixture_data(&drift(100.0, 12));
    let source = "strategy(\"Reversal\")\n\
                  if bar_index == 0\n\
                  \tstrategy.entry(\"L\", strategy.long)\n\
                  if bar_index == 4\n\
                  \tstrategy.entry(\"S\", strategy.short)\n\
                  plot(close)\n";
    let (signals, outcome) = assert_strategy_differential("reversal", source, &data, None);
    assert_eq!(
        event_bars(&signals, &data),
        vec![(1, 1), (5, -1)],
        "long → short is one direction-change event, never a 0 sandwich"
    );
    let strategy = outcome.strategy.expect("strategy outcome");
    assert!(strategy.position.size < 0.0, "short after the reversal");
}

#[test]
fn fixture_pyramiding_adds_emit_no_events() {
    let data = fixture_data(&drift(100.0, 12));
    let source = "strategy(\"Pyramid\", pyramiding=3)\n\
                  if bar_index == 0\n\
                  \tstrategy.entry(\"P1\", strategy.long)\n\
                  if bar_index == 2\n\
                  \tstrategy.entry(\"P2\", strategy.long)\n\
                  if bar_index == 4\n\
                  \tstrategy.entry(\"P3\", strategy.long)\n\
                  plot(close)\n";
    let (signals, outcome) = assert_strategy_differential("pyramiding", source, &data, None);
    assert_eq!(
        event_bars(&signals, &data),
        vec![(1, 1)],
        "same-direction adds do not change posture"
    );
    let strategy = outcome.strategy.expect("strategy outcome");
    assert_eq!(strategy.position.size, 3.0, "all three lots are open");
}

#[test]
fn fixture_oca_cancel_removes_the_unfilled_peer() {
    let mut rows = vec![
        (100.00, 101.00, 99.00, 100.00),
        // High-first walk; the high→low segment crosses the 95 buy limit
        // while never reaching the 93 sell stop, which the OCA group must
        // then cancel rather than leave resting.
        (99.00, 100.00, 94.00, 96.00),
        (96.00, 97.00, 95.00, 96.50),
    ];
    rows.extend(drift(96.5, 9));
    let data = fixture_data(&rows);
    let source = "strategy(\"Oca\")\n\
                  if bar_index == 0\n\
                  \tstrategy.entry(\"A\", strategy.long, limit=95.0, oca_name=\"pair\", oca_type=strategy.oca.cancel)\n\
                  \tstrategy.entry(\"B\", strategy.short, stop=93.0, oca_name=\"pair\", oca_type=strategy.oca.cancel)\n\
                  plot(close)\n";
    let (signals, outcome) = assert_strategy_differential("oca-cancel", source, &data, None);
    assert_eq!(
        event_bars(&signals, &data),
        vec![(1, 1)],
        "exactly the long filled; the short leg cancelled instead of firing later"
    );
    let strategy = outcome.strategy.expect("strategy outcome");
    assert!(strategy.position.size > 0.0, "long, never reversed");
}

#[test]
fn fixture_risk_rule_flattens_and_halts() {
    let mut rows = vec![
        (100.00, 101.00, 99.00, 100.00),
        (100.00, 101.00, 99.00, 100.50), // entry fills at open 100
        (100.50, 101.50, 99.50, 101.00),
        (101.00, 102.00, 100.00, 101.50),
        // A 13% crash: peak-to-trough equity drawdown through the 10% rule.
        (101.00, 101.50, 86.00, 87.00),
        (86.00, 87.00, 84.00, 85.00), // flatten fills here
        (85.00, 86.00, 84.00, 85.50),
    ];
    rows.extend(drift(85.5, 6));
    let data = fixture_data(&rows);
    // The blocked re-entry attempts prove the HALT half of halt+flatten: a
    // flattened-but-running strategy would re-enter and emit a fresh event.
    let source = "strategy(\"RiskHalt\", initial_capital=10000, \
                  default_qty_type=strategy.percent_of_equity, default_qty_value=100)\n\
                  strategy.risk.max_drawdown(10, strategy.percent_of_equity)\n\
                  if bar_index == 0\n\
                  \tstrategy.entry(\"L\", strategy.long)\n\
                  if bar_index > 6 and strategy.position_size == 0\n\
                  \tstrategy.entry(\"Retry\", strategy.long)\n\
                  plot(close)\n";
    let (signals, outcome) = assert_strategy_differential("risk-halt", source, &data, None);
    assert!(
        signals.len() >= 2 && signals.last().map(|s| s.1) == Some(0),
        "the rule flattened (last event 0) and the halt blocked every retry: {signals:?}"
    );
    let strategy = outcome.strategy.expect("strategy outcome");
    assert_eq!(strategy.position.size, 0.0, "flat after the risk stop");
    assert!(!strategy.trades.is_empty());
}

#[test]
fn fixture_margin_call_liquidates_to_flat() {
    // The crash arrives as ONE catastrophic gap: a staged decline would let
    // the margin-cover rule can trim the position across several calls and leave a
    // long tail, which is a different (partial-liquidation) scenario.
    let mut rows = vec![
        (100.00, 101.00, 99.00, 100.00),
        (100.00, 101.00, 99.00, 100.50), // entry fills: 400 units on 20% margin
        (100.50, 101.00, 98.00, 99.00),
        // Equity goes deeply negative intrabar: the margin call's cover exceeds
        // the whole position — forced liquidation to flat.
        (85.00, 86.00, 68.00, 70.00),
        (70.00, 71.00, 68.00, 69.00),
    ];
    rows.extend(drift(69.0, 7));
    let data = fixture_data(&rows);
    let source = "strategy(\"MarginCall\", initial_capital=10000, \
                  default_qty_type=strategy.cash, default_qty_value=40000, margin_long=20)\n\
                  if bar_index == 0\n\
                  \tstrategy.entry(\"L\", strategy.long)\n\
                  plot(close)\n";
    let (signals, outcome) = assert_strategy_differential("margin-call", source, &data, None);
    assert_eq!(signals.first().map(|s| s.1), Some(1));
    assert_eq!(
        signals.last().map(|s| s.1),
        Some(0),
        "the margin engine force-liquidated to flat: {signals:?}"
    );
    let strategy = outcome.strategy.expect("strategy outcome");
    assert_eq!(strategy.position.size, 0.0);
    assert!(
        !strategy.trades.is_empty(),
        "liquidation closes real trades on the ledger"
    );
}

#[test]
fn fixture_same_bar_stop_out_is_invisible_to_the_wire() {
    // Entry fills at bar 1's open; its exit child joins the book at the fill
    // and the high→low segment of the SAME bar crosses the 97 stop. The
    // position exists only intrabar, so the bar-close posture wire carries
    // nothing at all.
    let mut rows = vec![
        (100.00, 101.00, 99.00, 100.00),
        (100.00, 101.00, 95.00, 96.00),
        (96.00, 97.00, 95.00, 96.50),
    ];
    rows.extend(drift(96.5, 6));
    let data = fixture_data(&rows);
    let source = "strategy(\"SameBarStop\")\n\
                  if bar_index == 0\n\
                  \tstrategy.entry(\"L\", strategy.long)\n\
                  \tstrategy.exit(\"X\", from_entry=\"L\", stop=97.0)\n\
                  plot(close)\n";
    let (signals, outcome) = assert_strategy_differential("same-bar-stop", source, &data, None);
    assert!(
        signals.is_empty(),
        "an intra-bar round trip emits nothing: {signals:?}"
    );
    let strategy = outcome.strategy.expect("strategy outcome");
    assert_eq!(strategy.trades.len(), 1, "the stop-out really happened");
    assert_eq!(
        (strategy.trades[0].entry_bar, strategy.trades[0].exit_bar),
        (1, 1),
        "entry and exit on the same bar"
    );
    assert_eq!(strategy.trades[0].exit_price, 97.0);
}

#[test]
fn fixture_process_orders_on_close_scores_the_exact_fill() {
    let data = fixture_data(&drift(100.0, 12));
    // Under process_orders_on_close the order bar IS the event bar and the
    // close price is the fill price this model uses — the one fill model where the wire's
    // close-of-event-bar scoring is exact.
    let source = "strategy(\"PocFill\", process_orders_on_close=true)\n\
                  if bar_index == 3\n\
                  \tstrategy.entry(\"L\", strategy.long)\n\
                  if bar_index == 7\n\
                  \tstrategy.close_all()\n\
                  plot(close)\n";
    let (signals, outcome) = assert_strategy_differential("poc-exact-fill", source, &data, None);
    assert_eq!(
        event_bars(&signals, &data),
        vec![(3, 1), (7, 0)],
        "order bars are event bars under process_orders_on_close"
    );
    let strategy = outcome.strategy.expect("strategy outcome");
    assert_eq!(strategy.trades.len(), 1);
    assert_eq!(
        strategy.trades[0].entry_price.to_bits(),
        data.close[3].to_bits(),
        "the entry price equals the close bit for bit"
    );
    assert_eq!(
        strategy.trades[0].exit_price.to_bits(),
        data.close[7].to_bits(),
    );
}

fn adversarial_cases() -> u32 {
    const DEFAULT_CASES: u32 = 64;
    const MAX_CASES: u32 = 100_000;
    let Some(raw) = std::env::var_os("PINEWASM_ADVERSARIAL_CASES") else {
        return DEFAULT_CASES;
    };
    let raw = raw
        .to_str()
        .expect("PINEWASM_ADVERSARIAL_CASES must be UTF-8");
    let cases: u32 = raw
        .parse()
        .expect("PINEWASM_ADVERSARIAL_CASES must be an integer");
    assert!(
        (1..=MAX_CASES).contains(&cases),
        "PINEWASM_ADVERSARIAL_CASES must be in 1..={MAX_CASES}"
    );
    cases
}

fn adversarial_differential_source(kind: u8, literal: i16, window: u8) -> String {
    let literal = i32::from(literal);
    let window = u32::from(window.max(1));
    match kind % 6 {
        0 => format!(
            "indicator(\"Arithmetic property\")\nplot((close - open) * {literal}.0)\n"
        ),
        1 => format!(
            "indicator(\"TA property\")\nplot(ta.sma(close, {window}))\nplot(ta.ema(open, {window}))\n"
        ),
        2 => format!(
            "indicator(\"Branch property\")\nx = close > open ? high - low : close + {literal}.0\nplot(x, color = x >= 0 ? color.green : color.red)\n"
        ),
        3 => "indicator(\"Fill property\")\ntop = plot(high, \"Top\")\nbottom = plot(low, \"Bottom\")\nfill(top, bottom, color = close > open ? color.new(color.green, 40) : color.new(color.red, 70))\n".to_string(),
        4 => format!(
            "indicator(\"Alert property\")\nup = close > open + {literal}.0\nalertcondition(up, \"Up\", \"up bar\")\nplot(up ? 1.0 : 0.0)\n"
        ),
        _ => format!(
            "indicator(\"History property\")\noffset = math.min(bar_index, {window})\nplot(close[offset])\n"
        ),
    }
}

fn require_native_adversarial_aot(aot: &AotModule) -> Result<(), TestCaseError> {
    prop_assert_eq!(aot.layout.execution_mode, ExecutionMode::Native);
    prop_assert!(
        !aot.layout.imports_vm,
        "native parity must not execute reference-VM host residuals"
    );
    prop_assert!(!aot.layout.imports_interpreter);
    prop_assert!(!aot.layout.vm_heap_residual);
    prop_assert!(aot.layout.host_sites.is_empty());
    prop_assert!(aot.layout.native_blocker.is_none());
    Ok(())
}

#[test]
fn adversarial_native_gate_accepts_every_generated_source_family() {
    for kind in 0..6 {
        let source = adversarial_differential_source(kind, -37, 8);
        let program = compile_csil(&source);
        let aot = compile_program(&program).expect("generated family must compile natively");
        require_native_adversarial_aot(&aot)
            .unwrap_or_else(|error| panic!("source family {kind} left native subset: {error}"));
    }
}

#[test]
fn adversarial_native_gate_rejects_a_passing_reference_vm_residual() {
    let program = compile_csil(
        "indicator(\"Residual control\")\ns = str.repeat(\"x\", 8)\nplot(str.length(s))\n",
    );
    let aot = compile_program(&program).expect("string calls compile through host.vm");
    assert!(
        aot.layout.imports_vm,
        "the negative case must exercise a real VM residual"
    );
    let data = synthetic_data(2);
    let options = RunOptions::default();
    let outcome = run(&program, &data, &options);
    assert!(outcome.errors.is_empty());
    let aot_run = run_aot(&aot, &program, &data, &options);
    assert_eq!(aot_run.status, status::OK);
    assert_outcomes_match(&outcome, &aot_run, 2, "residual negative case");

    let error = require_native_adversarial_aot(&aot)
        .expect_err("matching output must not conceal reference-VM execution");
    assert!(error.to_string().contains("reference-VM host residuals"));
}

/// The bounded adversarial run deliberately reuses this file's production-
/// ABI loader and complete supported-output comparison rather than growing a
/// second, fuzz-only interpretation of the emitted module.
#[test]
#[ignore = "release-gated by cargo xtask fuzz-smoke"]
fn adversarial_supported_subset_vm_aot_parity() {
    let config = Config {
        cases: adversarial_cases(),
        failure_persistence: None,
        max_shrink_iters: 4_096,
        ..Config::default()
    };
    let rng = TestRng::from_seed(RngAlgorithm::ChaCha, &[0x66; 32]);
    let mut runner = TestRunner::new_with_rng(config, rng);
    let case = (0u8..6, any::<i16>(), 1u8..=8, 1usize..=16, any::<i8>());

    runner
        .run(&case, |(kind, literal, window, bars, shift)| {
            let source = adversarial_differential_source(kind, literal, window);
            let compiled = compile(&source, CompileOptions::default());
            prop_assert!(compiled.success, "structured source failed: {source}");
            let program = compiled.program.expect("successful compile has a program");
            let aot = compile_program(&program)
                .unwrap_or_else(|error| panic!("structured source left AOT subset: {error}"));
            require_native_adversarial_aot(&aot)?;

            let mut data = synthetic_data(bars);
            let shift = f64::from(shift) / 16.0;
            for (((open, high), low), close) in data
                .open
                .iter_mut()
                .zip(&mut data.high)
                .zip(&mut data.low)
                .zip(&mut data.close)
            {
                *open += shift;
                *high += shift;
                *low += shift;
                *close += shift;
            }
            let options = RunOptions {
                max_instructions_per_bar: 50_000,
                max_instructions_per_run: 500_000,
                max_string_bytes_per_run: 64 * 1024,
                max_vm_bytes_per_run: 4 * 1024 * 1024,
                max_stack_depth: 512,
                max_bars: Some(bars),
                max_collection_elements: 4_096,
                ..RunOptions::default()
            };
            let outcome = run(&program, &data, &options);
            prop_assert!(
                outcome.errors.is_empty(),
                "interpreter rejected structured case: {:?}",
                outcome.errors
            );
            prop_assert_eq!(outcome.bars_executed, bars as u32);
            prop_assert!(outcome.logs.is_empty());
            prop_assert!(outcome.drawings.is_empty());
            prop_assert!(outcome.strategy.is_none());

            let aot_run = run_aot(&aot, &program, &data, &options);
            prop_assert_eq!(aot_run.status, status::OK);
            assert_outcomes_match(&outcome, &aot_run, bars, "adversarial differential");
            assert_alerts_match(&outcome, &aot_run, &program, "adversarial differential");
            Ok(())
        })
        .expect("deterministic VM/AOT differential campaign failed");
}

#[test]
fn runtime_ta_input_lengths_match_recompiled_defaults_and_vm() {
    let source = concat!(
        "indicator(\"runtime lengths\")\nn = input.int(14, \"Length\")\na = n\n",
        "smooth(simple int p) =>\n    ta.ema(close, p)\n",
        "plot(ta.sma(close, a))\nplot(ta.change(close, n))\nplot(smooth(a))\n",
        "plot(ta.rsi(close, n))\nplot(math.sum(close, n))\nplot(ta.hma(close, n))\n",
        "plot(ta.pivothigh(high, n, n))\nplot(ta.cmo(close, n))\n",
        "[b, u, l] = ta.bb(close, n, 2.0)\nplot(b)\nplot(u)\nplot(l)\n",
        "[m, s, h] = ta.macd(close, n, n, n)\nplot(m)\nplot(s)\nplot(h)\n",
    );
    let program = compile_csil(source);
    // The unbounded length input is read behind a minimum guard, so the
    // program is native and the override reaches the linked TA engine.
    let aot = guarded_native(&program, "runtime lengths");
    let data = synthetic_data(160);
    for length in [14, 50, 2, 14] {
        let options = RunOptions {
            inputs: std::collections::HashMap::from([(0, InputValue::Int(length))]),
            ..RunOptions::default()
        };
        let reference =
            compile_csil(&source.replace("input.int(14", &format!("input.int({length}")));
        let expected = run(&reference, &data, &RunOptions::default());
        let actual = run_aot(&aot, &program, &data, &options);
        assert_eq!(actual.status, status::OK, "length={length}");
        assert_outcomes_match(&expected, &actual, data.bars(), "runtime lengths");
        let vm = run(&program, &data, &options);
        assert_outcomes_match(&vm, &actual, data.bars(), "VM override parity");
    }
}

/// An adjustable `ta.sma` or `ta.change` length reserves the largest legal
/// window only in the slot it reads. A uniform ring multiplied that window by
/// every slot, making an adjustable average consume unnecessary memory in
/// scripts with many unrelated slots. Reserve history according to the
/// observers of each slot while preserving interpreter parity.
#[test]
fn an_adjustable_average_beside_many_slots_runs_native_and_matches_the_vm() {
    let mut source = String::from(concat!(
        "indicator(\"Adjustable average beside many slots\")\n",
        "n = input.int(20, \"Length\")\n",
        "k = input.int(3, \"Offset\", minval = 0, maxval = 40)\nt0 = close\n",
    ));
    for i in 1..=300 {
        source.push_str(&format!(
            "t{i} = t{} * 0.5 + high * 0.25 + low * 0.25\n",
            i - 1
        ));
    }
    source.push_str(
        "plot(ta.sma(close, n))\nplot(ta.change(t300, n))\nplot(t300[2])\nplot(t150[k])\n",
    );
    let program = compile_csil(&source);
    let aot = compile_program_to_wasm(&program).expect("program compiles to a Wasm tier");
    assert_eq!(
        aot.layout.execution_mode,
        ExecutionMode::Native,
        "{:?}",
        aot.layout.native_blocker
    );
    let uniform_bytes = u64::from(aot.layout.ring) * 8 * u64::from(aot.layout.slot_count);
    assert!(
        uniform_bytes > 4096 * 65_536,
        "fixture must exceed the memory maximum under a uniform ring: {uniform_bytes}"
    );
    let data = synthetic_data(400);
    for (length, offset) in [(20, 3), (250, 40), (1, 0)] {
        let options = RunOptions {
            inputs: std::collections::HashMap::from([
                (0, InputValue::Int(length)),
                (1, InputValue::Int(offset)),
            ]),
            ..RunOptions::default()
        };
        let expected = run(&program, &data, &options);
        assert!(expected.errors.is_empty(), "{:?}", expected.errors);
        let actual = run_aot(&aot, &program, &data, &options);
        assert_eq!(actual.status, status::OK, "length={length}");
        assert_outcomes_match(&expected, &actual, data.bars(), "adjustable average");
    }
}

/// The most locals any function body of `wasm` declares.
fn max_declared_locals(wasm: &[u8]) -> usize {
    wasmparser::Parser::new(0)
        .parse_all(wasm)
        .filter_map(|payload| match payload.expect("valid module") {
            wasmparser::Payload::CodeSectionEntry(body) => Some(
                body.get_locals_reader()
                    .expect("local declarations")
                    .into_iter()
                    .map(|entry| entry.expect("local entry").0 as usize)
                    .sum::<usize>(),
            ),
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

/// A chart module with more than 256 slots never keeps every current-bar
/// cell in a local. V8's baseline compiler snapshots every local at each
/// structured branch, so keeping every cell in locals amplifies compiler
/// memory use for large bodies. This fixture's body is long enough to be
/// split (`body/split.rs`), so each of
/// its functions keeps at most the 64 cells its own code uses most and the
/// rest in private globals; a shorter body keeps the 64 its loops use most
/// (`a_short_chart_module_with_many_slots_keeps_64_cells_in_locals`).
/// Branch-local na, persistent carry, histories and every scalar type must
/// still match the interpreter, on a full run and on a shorter one.
#[test]
fn a_chart_module_with_many_slots_bounds_its_locals_and_matches_the_vm() {
    let mut source = String::from(concat!(
        "indicator(\"Many current cells\")\n",
        "var float carry = na\n",
        "float total = 0.0\n",
    ));
    for i in 0..300 {
        source.push_str(&format!(
            "float value{i} = na\nif close > open\n    value{i} := close + {i}.0\n\
             total += nz(value{i}, 0.0)\n"
        ));
    }
    source.push_str(concat!(
        "float looped = 0.0\n",
        "for j = 0 to 9\n",
        "    looped += nz(value0, 1.0) * j + nz(value299[1], 0.5)\n",
        "int counter = bar_index\n",
        "bool rising = close > close[1]\n",
        "color tint = rising ? color.green : color.red\n",
        "carry := total + nz(carry[1], 0.0) * 0.01\n",
        "plot(total)\n",
        "plot(carry)\n",
        "plot(looped)\n",
        "plot(value150[2])\n",
        "plot(counter, color = tint)\n",
        "plot(rising ? 1 : 0)\n",
    ));
    let program = compile_csil(&source);
    let aot = compile_program_to_wasm(&program).expect("program compiles to a Wasm tier");
    assert_eq!(
        aot.layout.execution_mode,
        ExecutionMode::Native,
        "{:?}",
        aot.layout.native_blocker
    );
    assert!(
        aot.layout.slot_count > 256,
        "fixture must exceed the all-locals bound: {}",
        aot.layout.slot_count
    );
    // Linked runtime helpers count too; the script's current cells must not
    // all appear in one function's local declaration.
    let max_locals = max_declared_locals(&aot.wasm);
    assert!(
        max_locals < 256,
        "unbounded compiler-local state: {max_locals} locals for {} slots",
        aot.layout.slot_count
    );
    let data = synthetic_data(400);
    for bars in [data.bars(), 37] {
        let options = RunOptions {
            max_bars: Some(bars),
            ..RunOptions::default()
        };
        let expected = run(&program, &data, &options);
        assert!(expected.errors.is_empty(), "{:?}", expected.errors);
        let actual = run_aot(&aot, &program, &data, &options);
        assert_eq!(actual.status, status::OK, "bars={bars}");
        assert_outcomes_match(&expected, &actual, bars, "many current cells");
    }
}

/// The unsplit placement of a chart module with more than 256 slots: a body
/// too short to split keeps the 64 cells its loops use most in typed locals
/// of its one `bar_step`, and every other cell in a private global.
#[test]
fn a_short_chart_module_with_many_slots_keeps_64_cells_in_locals() {
    let mut source = String::from("indicator(\"Many short cells\")\n");
    for i in 0..300 {
        source.push_str(&format!("float c{i} = close + {i}.0\n"));
    }
    source.push_str(
        "float looped = 0.0\nfor j = 0 to 9\n    looped += c0 * j + c299\nplot(looped + c150)\n",
    );
    let program = compile_csil(&source);
    let aot = compile_program_to_wasm(&program).expect("program compiles to a Wasm tier");
    assert_eq!(aot.layout.execution_mode, ExecutionMode::Native);
    assert!(aot.layout.slot_count > 256, "{}", aot.layout.slot_count);
    assert_eq!(bar_bodies(&aot.wasm).len(), 1, "the body must stay whole");
    let max_locals = max_declared_locals(&aot.wasm);
    assert!(
        max_locals < 256,
        "{max_locals} locals for {} slots",
        aot.layout.slot_count
    );
    let data = synthetic_data(40);
    let options = RunOptions::default();
    let expected = run(&program, &data, &options);
    assert!(expected.errors.is_empty(), "{:?}", expected.errors);
    let actual = run_aot(&aot, &program, &data, &options);
    assert_eq!(actual.status, status::OK);
    assert_outcomes_match(&expected, &actual, data.bars(), "many short cells");
}

#[test]
fn runtime_ta_float_inputs_match_recompiled_defaults_and_vm() {
    let source = concat!(
        "indicator(\"runtime float TA inputs\")\n",
        "var float mult = input.float(2.0, \"Mult\")\n",
        "factor = input.float(3.0, \"Factor\")\n",
        "inverse_mult = input.float(1.0, \"Inverse Mult\")\n",
        "[mid, upper, lower] = ta.bb(close, 5, mult)\n",
        "[inverse_mid, inverse_upper, inverse_lower] = ta.bb(close, 5, -inverse_mult)\n",
        "[trend, direction] = ta.supertrend(factor, 5)\n",
        "plot(upper)\nplot(inverse_upper)\nplot(trend)\n",
    );
    let program = compile_csil(source);
    let aot = compile_program_to_wasm(&program).expect("float-bound TA compiles to Wasm");
    assert!(
        !aot.layout.imports_interpreter,
        "the TA case must exercise native Wasm"
    );
    let data = synthetic_data(80);
    for (mult, factor) in [(2.0, 3.0), (3.5, 1.5), (-0.5, 4.0)] {
        let options = RunOptions {
            inputs: std::collections::HashMap::from([
                (0, InputValue::Float(mult)),
                (1, InputValue::Float(factor)),
                (2, InputValue::Float(1.5)),
            ]),
            ..RunOptions::default()
        };
        let expected_source = source
            .replace(
                "input.float(2.0, \"Mult\")",
                &format!("input.float({mult}, \"Mult\")"),
            )
            .replace(
                "input.float(3.0, \"Factor\")",
                &format!("input.float({factor}, \"Factor\")"),
            )
            .replace(
                "input.float(1.0, \"Inverse Mult\")",
                "input.float(1.5, \"Inverse Mult\")",
            );
        let expected = run(
            &compile_csil(&expected_source),
            &data,
            &RunOptions::default(),
        );
        let actual = run_aot(&aot, &program, &data, &options);
        assert_eq!(actual.status, status::OK, "mult={mult}, factor={factor}");
        assert_outcomes_match(&expected, &actual, data.bars(), "runtime float TA inputs");
        let vm = run(&program, &data, &options);
        assert_outcomes_match(&vm, &actual, data.bars(), "VM float override parity");
    }
}

#[test]
fn runtime_positive_and_percent_ta_float_inputs_match_native_wasm() {
    let source = concat!(
        "indicator(\"positive and percent TA inputs\")\n",
        "start = input.float(0.02, \"Start\")\n",
        "inc = input.float(0.02, \"Increment\")\n",
        "maximum = input.float(0.2, \"Maximum\")\n",
        "pct = input.float(50.0, \"Percent\")\n",
        "plot(ta.sar(start, inc, maximum))\n",
        "plot(ta.percentile_nearest_rank(close, 5, pct))\n",
    );
    let program = compile_csil(source);
    let aot = compile_program_to_wasm(&program).expect("float-bound TA compiles to Wasm");
    assert!(!aot.layout.imports_interpreter);
    let data = synthetic_data(80);
    let options = RunOptions {
        inputs: std::collections::HashMap::from([
            (0, InputValue::Float(0.03)),
            (1, InputValue::Float(0.04)),
            (2, InputValue::Float(0.3)),
            (3, InputValue::Float(90.0)),
        ]),
        ..RunOptions::default()
    };
    let expected_source = source
        .replace(
            "input.float(0.02, \"Start\")",
            "input.float(0.03, \"Start\")",
        )
        .replace(
            "input.float(0.02, \"Increment\")",
            "input.float(0.04, \"Increment\")",
        )
        .replace(
            "input.float(0.2, \"Maximum\")",
            "input.float(0.3, \"Maximum\")",
        )
        .replace(
            "input.float(50.0, \"Percent\")",
            "input.float(90.0, \"Percent\")",
        );
    let expected = run(
        &compile_csil(&expected_source),
        &data,
        &RunOptions::default(),
    );
    let actual = run_aot(&aot, &program, &data, &options);
    assert_eq!(actual.status, status::OK);
    assert_outcomes_match(
        &expected,
        &actual,
        data.bars(),
        "positive and percent float TA inputs",
    );
    let vm = run(&program, &data, &options);
    assert_outcomes_match(&vm, &actual, data.bars(), "positive and percent VM parity");

    for (input, value) in [(0, 0.0), (0, f64::NAN), (0, f64::INFINITY), (3, 101.0)] {
        let invalid = RunOptions {
            inputs: std::collections::HashMap::from([(input, InputValue::Float(value))]),
            ..RunOptions::default()
        };
        let rejected = run_aot(&aot, &program, &data, &invalid);
        assert_eq!(rejected.status, status::INVALID_TA_LENGTH);
    }
}

#[test]
fn runtime_ta_lengths_meter_resolved_windows() {
    for expr in [
        "ta.sma(close, n)",
        "math.sum(close, n)",
        "ta.hma(close, n)",
        "ta.pivothigh(high, n, n)",
    ] {
        let program = compile_csil(&format!(
            "indicator(\"meter\")\nn = input.int(2)\nplot({expr})\n"
        ));
        let aot = guarded_native(&program, expr);
        let data = synthetic_data(64);
        for budget in [20, 60, 150] {
            let options = RunOptions {
                inputs: std::collections::HashMap::from([(0, InputValue::Int(50))]),
                max_instructions_per_bar: budget,
                ..RunOptions::default()
            };
            let vm = run(&program, &data, &options);
            let actual = run_aot(&aot, &program, &data, &options);
            assert_eq!(
                actual.status == status::OK,
                vm.errors.is_empty(),
                "{expr} budget={budget}"
            );
            if let Some(error) = vm.errors.first() {
                assert_eq!(actual.status, status::BUDGET_EXCEEDED);
                assert_eq!(Some(actual.err_pc), error.instruction_offset);
            }
        }
    }
}

#[test]
fn vertical_gradient_fill_uses_hosted_numeric_visual_lanes() {
    let program = compile_csil("indicator(\"G\")\na=plot(close)\nb=plot(0)\nfill(a,b,80,0,color.blue,color.new(color.blue,100))\n");
    let module = compile_program_to_wasm(&program).expect("gradient must retain executable output");
    assert_eq!(
        module.layout.execution_mode,
        ExecutionMode::InterpreterHosted
    );
    wasmparser::validate(&module.wasm).unwrap();
}

#[test]
fn numeric_array_join_executes_through_compiled_wasm() {
    let program = compile_csil(
        r#"//@version=6
indicator("Numeric join")
a = array.from(1, -2, 3)
b = array.from(1.25, -2.5, 3.0)
plot(array.join(separator="|", id=a) == "1|-2|3" ? 1 : 0)
plot(b.join("|") == "1.25|-2.5|3" ? 1 : 0)
"#,
    );
    let artifact = compile_program_to_wasm(&program).expect("join compiles to Wasm");
    assert_eq!(artifact.layout.execution_mode, ExecutionMode::Native);
    let data = synthetic_data(3);
    let actual = run_aot(&artifact, &program, &data, &RunOptions::default());
    assert_eq!(actual.status, status::OK, "{:?}", actual.host_error);
    assert_eq!(actual.plots.len(), 2);
    for plot in &actual.plots {
        assert_eq!(plot, &vec![1.0; 3]);
    }
}

#[test]
fn typed_user_arguments_execute_through_compiled_wasm() {
    let program = compile_csil(
        r#"//@version=6
indicator("Typed user arguments")
f(float n) => n + 0.25
g(float n=na) => na(n) ? 7.0 : n
append(array<float> a, float v) =>
    array.push(a, v)
    array.sum(a)
a = array.from(1.0)
x = append(a, 2)
plot(f(2))
plot(g())
plot(x)
plot(array.sum(a))
"#,
    );
    let artifact = compile_program_to_wasm(&program).expect("typed arguments compile to Wasm");
    assert_eq!(artifact.layout.execution_mode, ExecutionMode::Native);
    let data = synthetic_data(3);
    let actual = run_aot(&artifact, &program, &data, &RunOptions::default());
    assert_eq!(actual.status, status::OK, "{:?}", actual.host_error);
    assert_eq!(
        actual.plots,
        vec![vec![2.25; 3], vec![7.0; 3], vec![3.0; 3], vec![3.0; 3]]
    );
}

#[test]
fn literal_log_messages_match_through_compiled_wasm() {
    let program = compile_csil("indicator(\"Literal logs\")\nlog.info(message=\"{{0}} 'quoted' } {\")\nlog.warning(\"{{literal}}\")\nlog.error(\"{0}\")\nplot(close)\n");
    let artifact = compile_program_to_wasm(&program).expect("logs compile to Wasm");
    let data = synthetic_data(1);
    let actual = run_aot(&artifact, &program, &data, &RunOptions::default());
    assert_eq!(actual.status, status::OK, "{:?}", actual.host_error);
    assert!(actual.mismatch.is_none(), "{:?}", actual.mismatch);
    assert_eq!(
        actual
            .vm_logs
            .iter()
            .map(|entry| entry.message.as_str())
            .collect::<Vec<_>>(),
        ["{{0}} 'quoted' } {", "{{literal}}", "{0}"]
    );
}

#[test]
fn per_bar_string_formats_execute_through_compiled_wasm() {
    let program = compile_csil("indicator(\"Per-bar format\")\nx = bar_index == 0 ? 1.25 : 2.5\nf = bar_index == 0 ? \"#.00\" : \"#.0\"\nprefix = bar_index == 0 ? \"x={0}\" : \"y={0}\"\nlog.info(\"{0}|{1}\", str.tostring(x, f), str.format(prefix, x))\nplot(close)\n");
    let artifact = compile_program_to_wasm(&program).expect("series formatting compiles to Wasm");
    let data = synthetic_data(2);
    let actual = run_aot(&artifact, &program, &data, &RunOptions::default());
    assert_eq!(actual.status, status::OK, "{:?}", actual.host_error);
    assert!(actual.mismatch.is_none(), "{:?}", actual.mismatch);
    assert_eq!(
        actual
            .vm_logs
            .iter()
            .map(|entry| entry.message.as_str())
            .collect::<Vec<_>>(),
        ["1.25|x=1.25", "2.5|y=2.5"]
    );
}

#[test]
fn per_bar_time_formats_execute_through_compiled_wasm() {
    let program = compile_csil("indicator(\"Time format\")\nf = bar_index == 0 ? \"yyyy-MM-dd HH:mm\" : \"HH:mm\"\nz = bar_index == 0 ? \"UTC\" : \"UTC+1\"\nlog.info(str.format_time(0, f, z))\nplot(close)\n");
    let artifact = compile_program_to_wasm(&program).expect("series time formatting compiles");
    let actual = run_aot(
        &artifact,
        &program,
        &synthetic_data(2),
        &RunOptions::default(),
    );
    assert_eq!(actual.status, status::OK, "{:?}", actual.host_error);
    assert!(actual.mismatch.is_none(), "{:?}", actual.mismatch);
    assert_eq!(
        actual
            .vm_logs
            .iter()
            .map(|entry| entry.message.as_str())
            .collect::<Vec<_>>(),
        ["1970-01-01 00:00", "01:00"]
    );
}

#[test]
fn time_format_defaults_and_missing_timestamp_match_compiled_wasm() {
    let program = compile_csil("indicator(\"Time defaults\")\nlog.info(str.format_time(0))\nlog.info(str.format_time(na, \"yyyy-MM-dd HH:mm:ssZ\", \"UTC-5\"))\nlog.info(str.format_time(-1, \"yyyy-MM-dd HH:mm:ssZ\", \"UTC+5:30\"))\nplot(close)\n");
    let artifact = compile_program_to_wasm(&program).expect("time formatting compiles");
    let actual = run_aot(
        &artifact,
        &program,
        &synthetic_data(1),
        &RunOptions::default(),
    );
    assert_eq!(actual.status, status::OK, "{:?}", actual.host_error);
    assert!(actual.mismatch.is_none(), "{:?}", actual.mismatch);
    assert_eq!(
        actual
            .vm_logs
            .iter()
            .map(|entry| entry.message.as_str())
            .collect::<Vec<_>>(),
        [
            "1970-01-01T00:00:00+0000",
            "1969-12-31 19:00:00-0500",
            "1970-01-01 05:29:59+0530"
        ]
    );
}

#[test]
fn tostring_and_message_number_formats_keep_distinct_tie_rounding() {
    // The tie 1.25 with "#.0": str.tostring rounds it half up to 1.3, and the
    // str.format pattern rounds it half to even to 1.2.
    let program = compile_csil("indicator(\"Rounding\")\nlog.info(str.tostring(1.25, \"#.0\"))\nlog.info(str.format(\"{0,number,#.0}\", 1.25))\nplot(close)\n");
    let artifact = compile_program_to_wasm(&program).expect("rounding compiles");
    let actual = run_aot(
        &artifact,
        &program,
        &synthetic_data(1),
        &RunOptions::default(),
    );
    assert_eq!(actual.status, status::OK, "{:?}", actual.host_error);
    assert!(actual.mismatch.is_none(), "{:?}", actual.mismatch);
    assert_eq!(
        actual
            .vm_logs
            .iter()
            .map(|entry| entry.message.as_str())
            .collect::<Vec<_>>(),
        ["1.3", "1.2"]
    );
}

#[test]
fn outstanding_total_preserves_integer_digits_through_compiled_wasm() {
    let program = compile_csil("indicator(\"Exact shares\")\nlog.info(str.tostring(syminfo.target_price_estimates))\nlog.info(str.tostring(syminfo.shares_outstanding_total))\nplot(close)\n");
    let artifact = compile_program_to_wasm(&program).expect("integer metadata compiles");
    let mut data = synthetic_data(1);
    data.syminfo.shares_outstanding_total = Some(9_007_199_254_740_993);
    data.syminfo.target_price_estimates = Some(1.5);
    let actual = run_aot(&artifact, &program, &data, &RunOptions::default());
    assert_eq!(actual.status, status::OK, "{:?}", actual.host_error);
    assert!(actual.mismatch.is_none(), "{:?}", actual.mismatch);
    assert_eq!(actual.vm_logs[0].message, "1.5");
    assert_eq!(actual.vm_logs[1].message, "9007199254740993");
}

#[test]
fn isin_host_string_matches_compiled_wasm() {
    let program = compile_csil(
        "indicator(\"ISIN\")\nlog.info(syminfo.isin)\nplot(str.length(syminfo.isin))\n",
    );
    let artifact = compile_program_to_wasm(&program).expect("ISIN compiles");
    for isin in ["", "US0378331005"] {
        let mut data = synthetic_data(1);
        data.syminfo.isin = isin.to_string();
        let actual = run_aot(&artifact, &program, &data, &RunOptions::default());
        assert_eq!(actual.status, status::OK, "{:?}", actual.host_error);
        assert!(actual.mismatch.is_none(), "{:?}", actual.mismatch);
        assert_eq!(actual.vm_logs[0].message, isin);
    }
}

#[test]
fn empty_string_na_matches_compiled_wasm() {
    let program = compile_csil("indicator(\"Empty string availability\")\ns = bar_index == 0 ? \"\" : \"x\"\nplot(na(s) ? 1 : 0)\nlog.info(str.tostring(na(s)))\n");
    let artifact = compile_program_to_wasm(&program).expect("string na compiles");
    let actual = run_aot(
        &artifact,
        &program,
        &synthetic_data(2),
        &RunOptions::default(),
    );
    assert_eq!(actual.status, status::OK, "{:?}", actual.host_error);
    assert!(actual.mismatch.is_none(), "{:?}", actual.mismatch);
    assert_eq!(actual.vm_logs[0].message, "true");
    assert_eq!(actual.vm_logs[1].message, "false");
}

#[test]
fn explicit_bool_of_na_still_returns_false_in_native_wasm() {
    let program =
        compile_csil("//@version=6\nindicator(\"Boolean cast\")\nplot(bool(na) ? 1 : 0)\n");
    let artifact = compile_program_to_wasm(&program).expect("explicit cast compiles");
    assert_eq!(artifact.layout.execution_mode, ExecutionMode::Native);
    let actual = run_aot(
        &artifact,
        &program,
        &synthetic_data(2),
        &RunOptions::default(),
    );
    assert_eq!(actual.status, status::OK, "{:?}", actual.host_error);
    assert!(actual.mismatch.is_none(), "{:?}", actual.mismatch);
    assert_eq!(actual.plots[0], vec![0.0, 0.0]);
}

#[test]
fn missing_numeric_comparisons_return_false_in_compiled_wasm() {
    for ty in ["float", "int"] {
        for op in [">", ">=", "<", "<=", "==", "!="] {
            let source = format!("//@version=6\nindicator(\"Missing comparison\")\n{ty} missing = na\n{ty} value = 1\nplot(not (missing {op} value) ? 1 : 0)\nplot(not (value {op} missing) ? 1 : 0)\nplot(not (missing {op} missing) ? 1 : 0)\n");
            let program = compile_csil(&source);
            let artifact = compile_program_to_wasm(&program).expect("comparison compiles");
            assert_eq!(artifact.layout.execution_mode, ExecutionMode::Native);
            let actual = run_aot(
                &artifact,
                &program,
                &synthetic_data(2),
                &RunOptions::default(),
            );
            assert_eq!(actual.status, status::OK, "{:?}", actual.host_error);
            assert!(actual.mismatch.is_none(), "{:?}", actual.mismatch);
            assert_eq!(actual.plots, vec![vec![1.0, 1.0]; 3], "{ty} {op}");
        }
    }
}

#[test]
fn unavailable_boolean_history_matches_compiled_wasm() {
    for target in ["flag", "barstate.isfirst", "(bar_index == 0)"] {
        for offset in ["1", "lag"] {
            let source = format!("//@version=6\nindicator(\"Boolean history\")\nflag = bar_index == 0\nlag = bar_index >= 0 ? 1 : 2\nprior = {target}[{offset}]\nplot(not prior ? 1 : 0)\n");
            let program = compile_csil(&source);
            let artifact = compile_program_to_wasm(&program).expect("history compiles");
            let actual = run_aot(
                &artifact,
                &program,
                &synthetic_data(3),
                &RunOptions::default(),
            );
            assert_eq!(actual.status, status::OK, "{:?}", actual.host_error);
            assert!(actual.mismatch.is_none(), "{:?}", actual.mismatch);
            assert_eq!(actual.plots[0], vec![1.0, 0.0, 1.0], "{target}[{offset}]");
        }
    }
}

#[test]
fn unmatched_boolean_conditionals_match_compiled_wasm() {
    for body in [
        "result = if bar_index == 1\n    true",
        "result = switch bar_index\n    1 => true",
        "result = switch\n    bar_index == 1 => true",
    ] {
        let source = format!(
            "//@version=6\nindicator(\"Boolean branches\")\n{body}\nplot(not result ? 1 : 0)\n"
        );
        let program = compile_csil(&source);
        let artifact = compile_program_to_wasm(&program).expect("conditional compiles");
        let actual = run_aot(
            &artifact,
            &program,
            &synthetic_data(3),
            &RunOptions::default(),
        );
        assert_eq!(actual.status, status::OK, "{:?}", actual.host_error);
        assert!(actual.mismatch.is_none(), "{:?}", actual.mismatch);
        assert_eq!(actual.plots[0], vec![1.0, 0.0, 1.0], "{body}");
    }
}

#[test]
fn nz_v6_color_defaults_match_compiled_wasm() {
    let source = r#"//@version=6
indicator("NZ v6")
color missing = na
c = nz(missing)
i = nz(na, 7)
j = nz(7, na)
k = nz(na, color.red)
l = nz(color.red, na)
plot(i)
plot(j)
plot(color.r(c))
plot(color.g(c))
plot(color.b(c))
plot(color.t(c))
plot(na(c) ? 1 : 0)
plot(k == color.red and l == color.red ? 1 : 0)
"#;
    let program = compile_csil(source);
    let artifact = compile_program_to_wasm(&program).expect("nz color compiles");
    assert_eq!(
        artifact.layout.execution_mode,
        ExecutionMode::Native,
        "{:?}",
        artifact.layout.native_blocker
    );
    let actual = run_aot(
        &artifact,
        &program,
        &synthetic_data(2),
        &RunOptions::default(),
    );
    assert_eq!(actual.status, status::OK, "{:?}", actual.host_error);
    assert!(actual.mismatch.is_none(), "{:?}", actual.mismatch);
    assert_eq!(
        actual.plots,
        vec![
            vec![7.0; 2],
            vec![7.0; 2],
            vec![0.0; 2],
            vec![0.0; 2],
            vec![0.0; 2],
            vec![100.0; 2],
            vec![0.0; 2],
            vec![1.0; 2]
        ]
    );
}

#[test]
fn nz_integer_selection_keeps_precision_and_min_safety() {
    let source = "indicator(\"NZ exact integer\")\ni = nz(na, 9007199254740993)\nplot(i == 9007199254740993 ? 1 : 0)\n";
    let program = compile_csil(source);
    let artifact = compile_program_to_wasm(&program).expect("integer nz");
    assert_eq!(artifact.layout.execution_mode, ExecutionMode::Native);
    let actual = run_aot(
        &artifact,
        &program,
        &synthetic_data(2),
        &RunOptions::default(),
    );
    assert_eq!(actual.status, status::OK, "{:?}", actual.host_error);
    assert!(actual.mismatch.is_none(), "{:?}", actual.mismatch);
    assert_eq!(actual.plots, vec![vec![1.0; 2]]);
    for expression in [
        "nz(na, -9223372036854775807 - 1)",
        "nz(-9223372036854775807 - 1, 0)",
    ] {
        let source = format!("indicator(\"NZ minimum\")\nx = {expression}\nplot(x == -9223372036854775807 - 1 ? 1 : 0)\n");
        let program = compile_csil(&source);
        let artifact = guarded_native(&program, expression);
        assert_present_min_stops(
            &artifact,
            &program,
            &synthetic_data(2),
            &RunOptions::default(),
            0,
            expression,
        );
        let actual = run(&program, &synthetic_data(2), &RunOptions::default());
        assert!(actual.errors.is_empty(), "{:?}", actual.errors);
        assert_eq!(actual.plots[0].values, vec![Value::Float(1.0); 2]);
    }
}

#[test]
fn immediately_widened_integer_division_runs_natively() {
    for (left, right, expected) in [
        ("3", "4", 0.75),
        ("-7", "2", -3.5),
        ("9007199254740993", "3", 3002399751580331.0),
        ("7", "0", f64::NAN),
    ] {
        let source = format!("indicator(\"Native division\")\nplot({left} / {right})\n");
        let program = compile_csil(&source);
        let artifact = compile_program_to_wasm(&program).expect("division compiles");
        assert_eq!(
            artifact.layout.execution_mode,
            ExecutionMode::Native,
            "{:?}",
            artifact.layout.native_blocker
        );
        let actual = run_aot(
            &artifact,
            &program,
            &synthetic_data(2),
            &RunOptions::default(),
        );
        assert_eq!(actual.status, status::OK, "{:?}", actual.host_error);
        assert!(actual.mismatch.is_none(), "{:?}", actual.mismatch);
        assert!(
            actual.plots[0].iter().all(|value| if expected.is_nan() {
                value.is_nan()
            } else {
                value.to_bits() == expected.to_bits()
            }),
            "{left}/{right}: {:?}",
            actual.plots
        );
    }
    differential(
        "indicator(\"Native missing division\")\nplot(bar_index[1] / 2)\n",
        &synthetic_data(4),
    );
}

#[test]
fn pine_int_division_stored_only_in_unobserved_slot_compiles_natively() {
    let program =
        compile_csil("indicator(\"Unobserved division\")\nscratch = bar_index / 2\nplot(close)\n");
    let division_pc = program
        .main()
        .code
        .iter()
        .position(|instr| matches!(instr, pinewasm_csil::CsilInstr::PineIntDiv))
        .expect("integer division remains in CSIL");
    let pinewasm_csil::CsilInstr::StoreUserSlotCurrent { slot } =
        program.main().code[division_pc + 1]
    else {
        panic!("division writes directly to its scratch slot");
    };
    assert_eq!(program.series_slots[slot as usize].name, "scratch");
    assert!(!program.series_slots[slot as usize].persistent);

    let native = compile_program(&program).expect("unplotted scratch division is native");
    assert_eq!(native.layout.execution_mode, ExecutionMode::Native);
    let data = synthetic_data(4);
    let reference = run(&program, &data, &RunOptions::default());
    let actual = run_aot(&native, &program, &data, &RunOptions::default());
    assert_eq!(actual.status, status::OK, "{:?}", actual.host_error);
    assert!(actual.mismatch.is_none(), "{:?}", actual.mismatch);
    assert_outcomes_match(&reference, &actual, data.bars(), "unplotted division");

    let standalone = compile_standalone_wasm_program(
        &program,
        &StandaloneWasmOptions {
            signal_plot: None,
            module_type: StandaloneWasmModuleType::Indicator,
        },
    )
    .expect("unplotted scratch division is standalone eligible");
    pinewasm_aot::audit_standalone_wasm(&standalone).expect("standalone artifact audit");

    // A slot read by integer arithmetic would have to hold an int or a
    // float. The typed pass still refuses that; both tiers then carry the
    // quotient as float code (`tests/pine_int_div_carried.rs` for the
    // standalone module, the chart-tier tests below).
    let source =
        "indicator(\"Observed division\")\nscratch = bar_index / 2\nplot(scratch)\nplot(scratch + 1)\n";
    let observed = compile_csil(source);
    let error = pinewasm_aot::analysis::analyze(&observed)
        .err()
        .expect("the typed pass refuses the program as written");
    assert!(error.to_string().contains("PineIntDiv"), "{error}");
    let native = compile_program(&observed).expect("the chart tier carries the quotient");
    assert_eq!(native.layout.execution_mode, ExecutionMode::Native);
    let standalone = compile_standalone_wasm_program(
        &observed,
        &StandaloneWasmOptions {
            signal_plot: None,
            module_type: StandaloneWasmModuleType::Indicator,
        },
    )
    .expect("the standalone compile carries this quotient");
    pinewasm_aot::audit_standalone_wasm(&standalone).expect("standalone artifact audit");
}

/// A kept Pine integer quotient lowers natively on the universal tier too
/// where nothing can tell an exact quotient from an uneven one: a slot whose
/// every read is widened at once (history included), and a comparison fused
/// with the division. `tests/pine_int_div_kept.rs` covers the standalone
/// module shape by shape.
#[test]
fn kept_pine_int_division_lowers_natively_and_matches_the_interpreter() {
    for source in [
        "indicator(\"Historical division\")\nscratch = bar_index / 2\nplot(scratch[1])\n",
        "indicator(\"Widened slot\")\nint k = bar_index / 2\nvar int held = 0\nif bar_index % 3 == 1\n    held := (0 - bar_index) / 4\nplot(k)\nplot(close / k * 100)\nplot(held)\nplot(held[1])\n",
        "indicator(\"Fused comparison\")\nplot(bar_index / 2 > 3 ? 1 : 0)\nplot(3 >= bar_index / 2 ? 1 : 0)\nplot(bar_index / 2 == 2 ? 1 : 0)\nplot(bar_index[1] / 0 != 2 ? 1 : 0)\nplot(bar_index / 2 > close - 100 ? 1 : 0)\n",
    ] {
        let program = compile_csil(source);
        let artifact = compile_program_to_wasm(&program).expect("kept division compiles");
        assert_eq!(
            artifact.layout.execution_mode,
            ExecutionMode::Native,
            "{source}: {:?}",
            artifact.layout.native_blocker
        );
        differential(source, &synthetic_data(24));
    }
}

/// The standalone runtime is the only writer of its input table, and it
/// refuses an `i64::MIN` int param (the integer na cell) with a request
/// error. Unbounded `input.int` therefore lowers natively on this path: the
/// default and every other param value evaluate, and the one value that would
/// alias na fails closed instead of reading as na.
#[test]
fn standalone_unbounded_integer_inputs_refuse_the_na_sentinel_at_the_request() {
    let unbounded = compile_csil(
        "indicator(\"Unbounded int input\")\na = input.int(8, \"Length\")\nscratch = a / 2\nplot(a)\n",
    );
    let wasm = compile_standalone_wasm_program(
        &unbounded,
        &StandaloneWasmOptions {
            signal_plot: None,
            module_type: StandaloneWasmModuleType::Indicator,
        },
    )
    .expect("unbounded int inputs lower natively on the standalone path");
    let data = synthetic_data(2);
    let wire =
        |params: &str| standalone_wasm_wire(&data, None).replacen("\"params\":{}", params, 1);
    let mut host = StandaloneWasmHost::new(&wasm);
    host.evaluate(&wire("\"params\":{}"))
        .expect("the default evaluates");
    host.evaluate(&wire("\"params\":{\"length\":-9223372036854775807}"))
        .expect("MIN + 1 is an ordinary value");
    let error = host
        .evaluate(&wire("\"params\":{\"length\":-9223372036854775808}"))
        .expect_err("MIN collides with the integer na cell");
    assert!(error.contains("collides with na"), "{error}");
    host.evaluate(&wire("\"params\":{\"length\":5}"))
        .expect("a refused request leaves the instance reusable");

    // The host-loaded tiers write inputs through a PineWASM loader that is
    // trusted only for the declared bounds, so there the read itself is
    // guarded: a MIN cell stops the run instead of reading as na.
    let hosted = guarded_native(&unbounded, "unbounded input on the host-loaded tier");
    let read = unbounded
        .main()
        .code
        .iter()
        .position(|instr| matches!(instr, pinewasm_csil::CsilInstr::ReadInput { .. }))
        .expect("input read");
    assert_eq!(
        int_min_guards(&unbounded).get(&read),
        Some(&IntMinGuard::InputRead)
    );
    let mut options = RunOptions::default();
    options.inputs.insert(0, InputValue::Int(i64::MIN));
    assert_present_min_stops(&hosted, &unbounded, &data, &options, 0, "MIN input cell");
    let bounded = compile_csil(
        "indicator(\"Bounded int input\")\na = input.int(8, minval = 1, maxval = 100)\nscratch = a / 2\nplot(close)\n",
    );
    compile_program(&bounded).expect("declared input limits exclude the integer na sentinel");
}

/// The scripts whose `ta.ema` length is computed from inputs, with the two
/// inputs every body reads. Each lowers to `EMA_SIMPLE_DYNAMIC`.
const COMPUTED_EMA_LENGTH_BODIES: [(&str, &str); 14] = [
    ("sum", "plot(ta.ema(close, n + 1))"),
    ("product", "plot(ta.ema(close, n * 2))"),
    ("alias", "m = n * 2 - 1\nplot(ta.ema(close, m))"),
    ("two inputs", "plot(ta.ema(close, n + k))"),
    (
        "helper",
        "ema(simple int length) => ta.ema(close, length)\nplot(ema(n + k))",
    ),
    ("math.max", "plot(ta.ema(close, math.max(n, k)))"),
    (
        "na source",
        "src = bar_index % 7 == 3 ? na : close\nplot(ta.ema(src, n + 1))",
    ),
    (
        "conditional site",
        "float x = na\nif bar_index % 3 != 0\n    x := ta.ema(close, n + 1)\nplot(x)",
    ),
    (
        "several sites",
        "plot(ta.ema(close, n + 1))\nplot(ta.ema(open, n * 3))\nplot(ta.ema(close, n))",
    ),
    ("int source", "plot(ta.ema(bar_index, n + 1))"),
    (
        "smoothed twice",
        "plot(ta.ema(ta.ema(close, n + 1), k * 2))",
    ),
    (
        "unchanged var",
        "var int m = n + k\nplot(ta.ema(close, m))",
    ),
    (
        "source overflows",
        "src = bar_index % 11 == 5 ? close * 1e308 * 1e308 : close\nplot(ta.ema(src, n + 1))",
    ),
    (
        "source underflows",
        "src = bar_index % 13 == 4 ? -close * 1e308 * 1e308 : close * 1e-320\nplot(ta.ema(src, n + k))",
    ),
];

fn computed_ema_length_source(body: &str) -> String {
    format!(
        "//@version=6\nindicator(\"Computed EMA length\")\nn = input.int(3, \"N\")\nk = input.int(2, \"K\")\n{body}\n"
    )
}

fn computed_ema_length_inputs(n: i64, k: i64) -> RunOptions {
    RunOptions {
        inputs: std::collections::HashMap::from([(0, InputValue::Int(n)), (1, InputValue::Int(k))]),
        ..RunOptions::default()
    }
}

/// A `ta.ema` length computed from inputs is a run-time length
/// (`EMA_SIMPLE_DYNAMIC`). The standalone tier lowers it natively, so the
/// inputs it reads are parameters a host can vary: for every selection, each
/// plot lane is bit-equal to the interpreter's, na included. One resident
/// instance takes all the selections in turn, so each evaluation must seed
/// its smoothers again and take its own length.
#[test]
fn standalone_computed_ema_lengths_match_the_interpreter_for_every_selected_input() {
    let data = synthetic_data(160);
    for (label, body) in COMPUTED_EMA_LENGTH_BODIES {
        let program = compile_csil(&computed_ema_length_source(body));
        assert!(
            program.main().code.iter().any(|instruction| matches!(
                instruction,
                pinewasm_csil::CsilInstr::EmaSimpleDynamic { .. }
            )),
            "{label}: the length must be a run-time one"
        );
        let wasm = compile_standalone_wasm_program(
            &program,
            &StandaloneWasmOptions {
                signal_plot: None,
                module_type: StandaloneWasmModuleType::Indicator,
            },
        )
        .unwrap_or_else(|refusal| panic!("{label}: refused: {refusal:?}"));
        let mut host = StandaloneWasmHost::new(&wasm);
        let mut lanes_by_selection = Vec::new();
        for (n, k) in [(3, 2), (1, 1), (20, 7), (3, 2), (60, 40)] {
            let context = format!("{label}, n={n}, k={k}");
            let expected = run(&program, &data, &computed_ema_length_inputs(n, k));
            assert!(
                expected.errors.is_empty(),
                "{context}: interpreter errors: {:?}",
                expected.errors
            );
            let wire = standalone_wasm_wire(&data, None).replacen(
                "\"params\":{}",
                &format!("\"params\":{{\"n\":{n},\"k\":{k}}}"),
                1,
            );
            host.evaluate(&wire)
                .unwrap_or_else(|error| panic!("{context}: evaluate failed: {error}"));
            let mut lanes = Vec::new();
            for plot in &expected.plots {
                let lane = host.plot_lane(plot.slot as usize);
                assert_eq!(lane.len(), data.bars(), "{context}: lane length");
                for (bar, (expected, got)) in plot.values.iter().zip(&lane).enumerate() {
                    match expected {
                        Value::Na => assert!(
                            got.is_nan(),
                            "{context}: plot {} bar {bar}: interpreter na, module {got}",
                            plot.slot
                        ),
                        Value::Float(x) => assert_eq!(
                            x.to_bits(),
                            got.to_bits(),
                            "{context}: plot {} bar {bar}: interpreter {x}, module {got}",
                            plot.slot
                        ),
                        other => panic!(
                            "{context}: plot {} bar {bar}: unexpected {other:?}",
                            plot.slot
                        ),
                    }
                }
                lanes.push(lane.iter().map(|value| value.to_bits()).collect::<Vec<_>>());
            }
            lanes_by_selection.push(lanes);
        }
        // The same selection twice gives the same lanes, and a different one
        // does not: the inputs are live, and nothing carries over.
        assert_eq!(
            lanes_by_selection[0], lanes_by_selection[3],
            "{label}: the repeated selection"
        );
        assert_ne!(
            lanes_by_selection[0], lanes_by_selection[2],
            "{label}: the length must follow the inputs"
        );
    }
}

/// A computed length outside `1..=MAX_WINDOW_LENGTH` stops the interpreter
/// with `InvalidTaLength`. The standalone module reports the same condition
/// as an indicator failure and stays usable for the next request.
#[test]
fn standalone_computed_ema_length_out_of_range_stops_and_the_instance_recovers() {
    let max = i64::from(pinewasm_csil::MAX_WINDOW_LENGTH);
    let data = synthetic_data(40);
    let program = compile_csil(&computed_ema_length_source("plot(ta.ema(close, n + k))"));
    let wasm = compile_standalone_wasm_program(
        &program,
        &StandaloneWasmOptions {
            signal_plot: None,
            module_type: StandaloneWasmModuleType::Indicator,
        },
    )
    .expect("a computed EMA length lowers natively on the standalone path");
    let wire = |n: i64, k: i64| {
        standalone_wasm_wire(&data, None).replacen(
            "\"params\":{}",
            &format!("\"params\":{{\"n\":{n},\"k\":{k}}}"),
            1,
        )
    };
    let mut host = StandaloneWasmHost::new(&wasm);
    host.evaluate(&wire(3, 2)).expect("the default evaluates");
    let default_lane = host.plot_lane(0);

    for (n, k) in [
        (0, 0),
        (-5, 2),
        (max, 1),
        (i64::MAX - 1, 1),
        (max / 2 + 1, max / 2 + 1),
    ] {
        let context = format!("n={n}, k={k}");
        let expected = run(&program, &data, &computed_ema_length_inputs(n, k));
        // `i64::MAX - 1 + 1` is still far out of range; nothing here wraps.
        assert_eq!(expected.errors.len(), 1, "{context}: {:?}", expected.errors);
        assert_eq!(
            expected.errors[0].kind,
            pinewasm_vm::RuntimeErrorKind::InvalidTaLength,
            "{context}"
        );
        let error = host
            .evaluate(&wire(n, k))
            .expect_err("an out-of-range computed length must stop the run");
        assert_eq!(
            error, "Indicator failure: TA input length out of range",
            "{context}"
        );
        host.evaluate(&wire(3, 2))
            .unwrap_or_else(|error| panic!("{context}: recovery failed: {error}"));
        let recovered = host.plot_lane(0);
        assert_eq!(
            recovered.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            default_lane.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            "{context}: the next request starts clean"
        );
    }

    // Both ends of the legal range run.
    for (n, k) in [(1, 0), (max - 1, 1)] {
        let expected = run(&program, &data, &computed_ema_length_inputs(n, k));
        assert!(expected.errors.is_empty(), "{:?}", expected.errors);
        host.evaluate(&wire(n, k))
            .unwrap_or_else(|error| panic!("n={n}, k={k}: {error}"));
    }
}

/// The chart tier still hands a run-time EMA length to the interpreter: its
/// status protocol has no report for a length that goes out of range after
/// the first bar.
#[test]
fn a_computed_ema_length_is_native_only_on_the_standalone_tier() {
    let program = compile_csil(&computed_ema_length_source("plot(ta.ema(close, n + 1))"));
    assert!(matches!(
        compile_program(&program),
        Err(AotError::UnsupportedOpcode(ref opcode)) if opcode == "EmaSimpleDynamic"
    ));
    let artifact = compile_program_to_wasm(&program).expect("the hosted tier takes it");
    assert_eq!(
        artifact.layout.execution_mode,
        ExecutionMode::InterpreterHosted
    );
}

#[test]
fn integer_division_rounded_immediately_keeps_pine_numeric_semantics() {
    let source = "indicator(\"Round Pine division\")\nplot(math.round(bar_index / 2))\nplot(math.round(-3 / 4))\nplot(math.round(-7 / 2))\nplot(math.round(bar_index / 0))\nplot(math.round(bar_index[1] / 4))\nplot(math.round(9007199254740993 / 1) == 9007199254740993 ? 1 : 0)\n";
    let program = compile_csil(source);
    let artifact = compile_program_to_wasm(&program).expect("rounded division compiles");
    assert_eq!(
        artifact.layout.execution_mode,
        ExecutionMode::Native,
        "{:?}",
        artifact.layout.native_blocker
    );
    let actual = run_aot(
        &artifact,
        &program,
        &synthetic_data(2),
        &RunOptions::default(),
    );
    assert_eq!(actual.status, status::OK, "{:?}", actual.host_error);
    assert!(actual.mismatch.is_none(), "{:?}", actual.mismatch);
    assert_eq!(actual.plots[0], vec![0.0, 1.0]);
    assert_eq!(actual.plots[1], vec![-1.0; 2]);
    assert_eq!(actual.plots[2], vec![-4.0; 2]);
    assert!(actual.plots[3].iter().all(|value| value.is_nan()));
    assert!(actual.plots[4][0].is_nan());
    assert_eq!(actual.plots[4][1], 0.0);
    assert_eq!(actual.plots[5], vec![1.0; 2]);
    assert!(program.main().code.windows(2).any(|pair| {
        matches!(pair[0], pinewasm_csil::CsilInstr::PineIntDiv)
            && matches!(
                pair[1],
                pinewasm_csil::CsilInstr::CallPure {
                    func: pinewasm_hir::PureFunc::MathRound,
                    argc: 1
                }
            )
    }));
    differential(source, &synthetic_data(2));
}

/// Erased drawing sites keep the VM's fuel schedule: `line.new` still costs
/// its `field_count` units (one dispatch plus `exec_new_object`'s extra) and
/// each erased `RegisterDrawing`/`DeleteDrawing` still costs its dispatch,
/// so every per-bar and per-run budget cut lands on the same bar and pc as
/// the VM and as the ordinary module that hosts those sites. This
/// parity is part of the `NewObject` no-failure certificate.
#[test]
fn erased_drawing_sites_preserve_standalone_fuel_boundaries() {
    let program = compile_csil(concat!(
        "indicator('Drawing fuel')\n",
        "var line l = na\n",
        "line.delete(l)\n",
        "l := line.new(bar_index, close, bar_index + 1, close, extend = extend.right)\n",
        "if close > open\n",
        "    line.delete(l)\n",
        "plot(close)\n",
    ));
    let new_object = program
        .main()
        .code
        .iter()
        .find_map(|op| match op {
            pinewasm_csil::CsilInstr::NewObject { field_count, .. } => Some(*field_count),
            _ => None,
        })
        .expect("a line.new constructor");
    assert!(
        new_object > 1,
        "the fixture must charge a constructor extra"
    );
    let standalone = erased_drawing_fuel_test_module(&program);
    let data = synthetic_data(2);
    let maximum = program.main().code.len() as u64 + u64::from(new_object) + 1;
    let mut cut_at_constructor = false;
    for per_bar in 0..=maximum {
        for per_run in 0..=maximum * 2 {
            let options = RunOptions {
                max_instructions_per_bar: per_bar,
                max_instructions_per_run: per_run,
                ..RunOptions::default()
            };
            let context = format!("erased drawing fuel bar={per_bar}, run={per_run}");
            // The ordinary module reroutes a host site it cannot bound
            // (`RUN_BUDGET_REROUTE`), so the erased sites are compared with
            // the VM itself: same status, bar and pc on every cut.
            let expected = run(&program, &data, &options);
            let actual = run_aot(&standalone, &program, &data, &options);
            assert_alerts_match(&expected, &actual, &program, &context);
            if let Some(error) = expected.errors.first() {
                let status = match error.kind {
                    pinewasm_vm::RuntimeErrorKind::InstructionLimitExceeded => {
                        status::BUDGET_EXCEEDED
                    }
                    pinewasm_vm::RuntimeErrorKind::RunInstructionLimitExceeded => {
                        status::RUN_BUDGET_EXCEEDED
                    }
                    other => panic!("unexpected interpreter error {other:?}"),
                };
                assert_eq!(actual.status, status, "{context}");
                assert_eq!(Some(actual.err_bar), error.bar_index, "{context}");
                assert_eq!(Some(actual.err_pc), error.instruction_offset, "{context}");
                cut_at_constructor |= matches!(
                    error
                        .instruction_offset
                        .and_then(|pc| program.main().code.get(pc as usize)),
                    Some(pinewasm_csil::CsilInstr::NewObject { .. })
                );
            } else {
                assert_eq!(actual.status, status::OK, "{context}");
                assert_outcomes_match(&expected, &actual, data.bars(), &context);
            }
        }
    }
    assert!(
        cut_at_constructor,
        "some budget must run out inside the erased constructor's extra"
    );
}

#[test]
fn widened_integer_division_preserves_standalone_fuel_boundaries() {
    let program = compile_csil(
        "indicator('Division fuel')\nscratch = bar_index / 4\nplot(close)\nplot(bar_index / 4)\nplot(math.round(bar_index / 2))\nplot(bar_index[1] / 0)\n",
    );
    let (standalone, _) = standalone_wasm_fuel_test_module(&program);
    let original = compile_program(&program).unwrap();
    let data = synthetic_data(2);
    let maximum = program.main().code.len() as u64 + 1;
    for per_bar in 0..=maximum {
        for per_run in 0..=maximum * 2 {
            assert_standalone_wasm_fuel_matches(
                &program,
                &standalone,
                &original,
                &data,
                per_bar,
                per_run,
            );
        }
    }
}

/// UDT settings fields hold integers written only by `NewObject`/`SetField`
/// operands, so a field read is as safe as a slot read: the producer gate has
/// already checked every value that can reach the field. Bounded and
/// unbounded `input.int` fields, a declared default, an omitted (`na`) field,
/// a copy, a field write, and native `array<int>` reads all stay native.
const SETTINGS_OBJECT_SOURCE: &str = concat!(
    "//@version=6\n",
    "indicator(\"Settings object fields\")\n",
    "type Settings\n",
    "    int length\n",
    "    int maxBarsBack\n",
    "    int step = 3\n",
    "    int unset\n",
    "    float mult = 1.5\n",
    "settings = Settings.new(input.int(4, \"Length\", minval = 1, maxval = 50), MAX_BARS_BACK)\n",
    "copied = Settings.copy(settings)\n",
    "copied.step := bar_index % 5\n",
    "ints = array.new_int(1, settings.maxBarsBack)\n",
    "array.push(ints, settings.length)\n",
    "array.set(ints, 0, settings.step)\n",
    "plot(close[settings.length])\n",
    "plot(settings.maxBarsBack * settings.mult)\n",
    "plot(bar_index >= settings.length ? 1.0 : 0.0)\n",
    "plot(na(settings.unset) ? 1.0 : 0.0)\n",
    "plot(copied.step * 1.0)\n",
    "plot(array.get(ints, 0) + 0.5)\n",
    "plot(array.last(ints) * 2.0)\n",
);

fn settings_object_source(max_bars_back: &str) -> String {
    SETTINGS_OBJECT_SOURCE.replace("MAX_BARS_BACK", max_bars_back)
}

#[test]
fn udt_settings_integer_fields_lower_natively_and_match_the_vm() {
    let source =
        settings_object_source("input.int(2000, \"Max Bars Back\", minval = 1, maxval = 5000)");
    let program = compile_csil(&source);
    assert!(program
        .main()
        .code
        .iter()
        .any(|instr| matches!(instr, pinewasm_csil::CsilInstr::GetField { .. })));
    let artifact = compile_program_to_wasm(&program).expect("settings object compiles");
    assert_eq!(
        artifact.layout.execution_mode,
        ExecutionMode::Native,
        "{:?}",
        artifact.layout.native_blocker
    );
    assert!(
        artifact.layout.native_heap,
        "objects and arrays use the native heap"
    );
    differential(&source, &synthetic_data(8));
}

#[test]
fn udt_settings_unbounded_integer_field_is_guarded_at_its_input_not_its_read() {
    let source = settings_object_source("input.int(2000, \"Max Bars Back\")");
    let program = compile_csil(&source);
    let unbounded = program
        .inputs
        .iter()
        .find(|decl| decl.title.as_deref() == Some("Max Bars Back"))
        .expect("unbounded input")
        .id;
    let read = program
        .main()
        .code
        .iter()
        .position(|instr| {
            matches!(instr, pinewasm_csil::CsilInstr::ReadInput { input } if *input == unbounded)
        })
        .expect("input read");

    // Host-loaded tiers trust the loader only for declared bounds: the
    // guard sits on the unbounded input read, never on a field load.
    guarded_native(&program, "unbounded settings field");
    let guards = int_min_guards(&program);
    assert_eq!(guards.get(&read), Some(&IntMinGuard::InputRead));
    for (pc, instr) in program.main().code.iter().enumerate() {
        if matches!(instr, pinewasm_csil::CsilInstr::GetField { .. }) {
            assert!(!guards.contains_key(&pc), "field load {pc} is guarded");
        }
    }
    differential(&source, &synthetic_data(8));

    // The standalone runtime refuses a MIN param, so the same program lowers
    // natively there, field reads included, and matches the VM plot for plot.
    let wasm = compile_standalone_wasm_program(
        &program,
        &StandaloneWasmOptions {
            signal_plot: None,
            module_type: StandaloneWasmModuleType::Indicator,
        },
    )
    .expect("unbounded settings fields lower natively on the standalone path");
    let data = synthetic_data(8);
    let expected = run(&program, &data, &RunOptions::default());
    assert!(expected.errors.is_empty(), "{:?}", expected.errors);
    let mut host = StandaloneWasmHost::new(&wasm);
    host.evaluate(&standalone_wasm_wire(&data, None))
        .expect("the default evaluates");
    for (index, plot) in expected.plots.iter().enumerate() {
        let lane = host.plot_lane(index);
        assert_eq!(lane.len(), plot.values.len(), "plot {index}");
        for (bar, (actual, value)) in lane.iter().zip(&plot.values).enumerate() {
            match value {
                Value::Float(expected) => assert_eq!(
                    actual.to_bits(),
                    expected.to_bits(),
                    "plot {index} bar {bar}"
                ),
                Value::Na => assert!(actual.is_nan(), "plot {index} bar {bar}"),
                other => panic!("plot {index} bar {bar}: unexpected {other:?}"),
            }
        }
    }
    let wire = standalone_wasm_wire(&data, None).replacen(
        "\"params\":{}",
        "\"params\":{\"max_bars_back\":-9223372036854775808}",
        1,
    );
    let error = host
        .evaluate(&wire)
        .expect_err("MIN collides with the integer na cell");
    assert!(error.contains("collides with na"), "{error}");
}

/// Admitting field reads does not admit what is stored: a producer that can
/// create a valid `i64::MIN` is refused at its own pc, whether it reaches the
/// field through `NewObject` or `SetField`.
#[test]
fn min_capable_producer_stored_into_a_field_is_guarded_at_the_producer() {
    for (source, producer) in [
        (
            concat!(
                "indicator(\"Constructor producer\")\n",
                "type Holder\n",
                "    int value\n",
                "holder = Holder.new(math.round(close * 1e30))\n",
                "plot(holder.value * 1.0)\n",
            ),
            pinewasm_hir::PureFunc::MathRound,
        ),
        (
            concat!(
                "indicator(\"Setter producer\")\n",
                "type Holder\n",
                "    int value = 1\n",
                "holder = Holder.new()\n",
                "holder.value := int(close * -1e30)\n",
                "plot(holder.value * 1.0)\n",
            ),
            pinewasm_hir::PureFunc::CastInt,
        ),
    ] {
        let program = compile_csil(source);
        let code = &program.main().code;
        let producer_pc = code
            .iter()
            .position(|instr| {
                matches!(instr, pinewasm_csil::CsilInstr::CallPure { func, .. } if *func == producer)
            })
            .unwrap_or_else(|| panic!("{source}: producer present"));
        // The guard sits on the producer; the field that stores its result
        // and the field load stay unguarded.
        guarded_native(&program, source);
        assert_eq!(
            int_min_guards(&program),
            std::collections::BTreeMap::from([(producer_pc, IntMinGuard::FloatToInt)]),
            "{source}"
        );
        compile_standalone_wasm_program(
            &program,
            &StandaloneWasmOptions {
                signal_plot: None,
                module_type: StandaloneWasmModuleType::Indicator,
            },
        )
        .expect("the standalone path guards the same producer");
        differential(source, &synthetic_data(4));
    }
}

#[test]
fn bounded_nonnegative_float_integer_results_run_natively() {
    let program = compile_csil("indicator('Float bounds')\ncompression = math.max(close, 1.0)\nx = int(math.min(math.round(math.abs(close) / compression * 9.0), 9.0))\nplot((x + 1) / 10.0)\n");
    compile_program(&program).expect("nonnegative clamp excludes MIN");
    let mut data = synthetic_data(9);
    data.close = vec![
        -1e20,
        -2.5,
        -0.0,
        0.0,
        0.25,
        2.5,
        1e20,
        f64::NAN,
        f64::INFINITY,
    ];
    differential_program(&program, &data, "nonnegative rounded clamp and arithmetic");
    for function in ["math.floor", "math.ceil", "math.round", "int"] {
        let source = format!(
            "indicator('Nonnegative producer')\nplot(({function}(math.abs(close)) + 1) / 10.0)\n"
        );
        let program = compile_csil(&source);
        compile_program(&program).expect("nonnegative conversion and checked overflow exclude MIN");
        let mut edge_data = synthetic_data(5);
        edge_data.close = vec![
            i64::MIN as f64,
            (i64::MAX as f64) - 1024.0,
            f64::MAX,
            -0.25,
            f64::NAN,
        ];
        differential_program(
            &program,
            &edge_data,
            "nonnegative integer boundary and overflow",
        );
    }
    for source in [
        "x = int(math.min(close, 9.0))\n",
        "bound = close > 0 ? 1.0 : -1.0\nx = int(math.abs(close) / bound)\n",
        "bound = math.max(close, 1.0)\nbound := close\nx = int(math.abs(close) / bound)\n",
    ] {
        let program = compile_csil(&format!(
            "indicator('Unknown bounds')\n{source}plot(x == 0 ? 1 : 0)\n"
        ));
        // No proved bound: the conversion is guarded instead of refused.
        guarded_native(&program, source);
        assert!(int_min_guards(&program)
            .values()
            .all(|guard| *guard == IntMinGuard::FloatToInt));
        differential_program(&program, &synthetic_data(6), source);
    }
}

#[test]
fn last_bar_index_subtraction_boundary_and_empty_runs_match_vm() {
    let program = compile_csil("indicator('Last index domain')\ntype Settings\n    int lookback\ns = Settings.new(input.int(9223372036854775807, minval=9223372036854775807, maxval=9223372036854775807))\nx = last_bar_index - s.lookback\nplot(x == -9223372036854775807 ? 1 : 0)\nplot(last_bar_index[1])\n");
    compile_program(&program).expect("present last index cannot be -1");
    let outcome = differential_program(&program, &synthetic_data(1), "last index minus MAX");
    assert_eq!(outcome.plots[0].values, vec![Value::Float(1.0)]);
    assert!(outcome.plots[1].values[0].is_na());
    differential_program(&program, &synthetic_data(5), "last index history");
    let empty = differential_program(
        &program,
        &synthetic_data(0),
        "empty run has no last-index read",
    );
    assert!(empty.plots.iter().all(|p| p.values.is_empty()));
}

#[test]
fn native_integer_stack_tags_survive_branch_spills_and_duplication() {
    use pinewasm_csil::CsilInstr as C;
    for (lhs, rhs, expected) in [
        (C::PushNa, C::PushI64(1), Value::Na),
        (C::PushI64(-7), C::PushI64(2), Value::Float(-5.0)),
    ] {
        let mut program = compile_csil("indicator('Stack tags')\nplot(close)\nplot(close)\n");
        let main = &mut program.functions[0];
        let span = main.source_map[0];
        main.code = vec![
            lhs,
            rhs,
            C::IntAdd,
            C::ReadBuiltin {
                source: pinewasm_hir::BuiltinSeries::BarIndex,
                offset: 0,
            },
            C::PushI64(0),
            C::CmpEq,
            C::JumpIfFalse { target: 10 },
            C::Dup,
            C::IntToF64,
            C::PlotF64 { plot_slot: 0 },
            C::IntToF64,
            C::PlotF64 { plot_slot: 1 },
            C::Return,
        ];
        main.source_map = vec![span; main.code.len()];
        main.max_stack = 3;
        pinewasm_csil::validate(&program).unwrap();
        compile_program(&program).expect("branch tag fixture must be native");
        let outcome = differential_program(
            &program,
            &synthetic_data(3),
            "independent stack validity at joins",
        );
        assert_eq!(outcome.plots[0].values[0], expected);
        assert_eq!(outcome.plots[1].values, vec![expected; 3]);
    }
}

#[test]
fn native_tagged_integer_selection_negation_and_comparisons_match_vm() {
    use pinewasm_csil::CsilInstr as C;
    let run = |ops: Vec<C>, expected: Value| {
        let mut program = compile_csil("indicator('Tagged consumers')\nplot(close)\n");
        let main = &mut program.functions[0];
        let span = main.source_map[0];
        main.code = ops;
        main.code
            .extend([C::IntToF64, C::PlotF64 { plot_slot: 0 }, C::Return]);
        main.source_map = vec![span; main.code.len()];
        main.max_stack = 3;
        pinewasm_csil::validate(&program).unwrap();
        compile_program(&program).expect("tagged consumers fixture must be native");
        let outcome = differential_program(
            &program,
            &synthetic_data(2),
            "tagged select, negate, compare",
        );
        assert_eq!(outcome.plots[0].values, vec![expected; 2]);
    };
    for (cond, lhs, rhs, expected) in [
        (
            C::PushBool(true),
            C::PushI64(-i64::MAX),
            C::PushNa,
            Value::Float(i64::MAX as f64),
        ),
        (C::PushBool(false), C::PushI64(7), C::PushNa, Value::Na),
        (C::PushNa, C::PushI64(7), C::PushI64(-3), Value::Float(3.0)),
    ] {
        run(vec![cond, lhs, rhs, C::Select, C::IntNeg], expected);
    }
    // Convert the comparison boolean by selecting integer 1/0, then widen.
    for op in [C::CmpGt, C::CmpGe, C::CmpLt, C::CmpLe, C::CmpEq, C::CmpNe] {
        for (lhs, rhs) in [
            (C::PushNa, C::PushI64(0)),
            (C::PushI64(0), C::PushNa),
            (C::PushNa, C::PushNa),
        ] {
            run(
                vec![
                    lhs,
                    rhs,
                    op.clone(),
                    C::PushI64(1),
                    C::PushI64(0),
                    C::Select,
                ],
                Value::Float(0.0),
            );
        }
    }
    for (op, unequal, equal) in [
        (C::CmpGt, false, false),
        (C::CmpGe, false, true),
        (C::CmpLt, true, false),
        (C::CmpLe, true, true),
        (C::CmpEq, false, true),
        (C::CmpNe, true, false),
    ] {
        for (lhs, expected) in [(-5, unequal), (3, equal)] {
            run(
                vec![
                    C::PushI64(lhs),
                    C::PushI64(3),
                    op.clone(),
                    C::PushI64(1),
                    C::PushI64(0),
                    C::Select,
                ],
                Value::Float(if expected { 1.0 } else { 0.0 }),
            );
        }
    }
    let program = compile_csil("indicator('Mixed tagged comparisons')\nx = bar_index[1]\nplot(x == 0.0 ? 1 : 0)\nplot(0.0 != x ? 1 : 0)\n");
    compile_program(&program).expect("mixed numeric comparisons must be native");
    let outcome = differential_program(
        &program,
        &synthetic_data(3),
        "mixed tagged comparisons and missing history",
    );
    assert_eq!(
        outcome.plots[0].values,
        vec![Value::Float(0.0), Value::Float(1.0), Value::Float(0.0)]
    );
    assert_eq!(
        outcome.plots[1].values,
        vec![Value::Float(0.0), Value::Float(0.0), Value::Float(1.0)]
    );
}

// ---- the integer minimum guard ---------------------------------------------
//
// The `I64` lane spells `na` as `i64::MIN`. Where the static facts cannot
// clear a site of a present minimum, the module does not refuse the whole
// program: the site carries a runtime guard that stops the run with
// `ABI_MISMATCH` when the operation's result is the minimum and is not the
// operation's own `na`.

/// Compile `source` as an indicator-type standalone module.
fn standalone_indicator(program: &pinewasm_csil::CsilProgram, context: &str) -> Vec<u8> {
    compile_standalone_wasm_program(
        program,
        &StandaloneWasmOptions {
            signal_plot: None,
            module_type: StandaloneWasmModuleType::Indicator,
        },
    )
    .unwrap_or_else(|error| panic!("{context}: standalone emit: {error}"))
}

/// Programs that a whole-program gate would refuse. Each compiles on the
/// host-loaded tier and as a standalone module, and both agree with the
/// interpreter bit for bit on ordinary data.
#[test]
fn formerly_refused_integer_programs_compile_and_match_the_vm() {
    for (label, source) in [
        (
            "unbounded input plus one, widened",
            "indicator('P')\nint k = input.int(8) + 1\nx = close / k * 100\nplot(x)\nplot(k)\n",
        ),
        (
            "rounded close",
            "indicator('P')\nint k = math.round(close)\nx = close / k * 100\nplot(x)\nplot(k)\n",
        ),
        (
            "floor, ceil and int of a float",
            "indicator('P')\nplot(math.floor(close) + math.ceil(open) - int(high))\n",
        ),
        (
            "integer counter across bars",
            "indicator('P')\nvar int n = 0\nn := n + 1\nplot(n * 2 - 1)\n",
        ),
        (
            "integer loop",
            "indicator('P')\nint total = 0\nfor i = 0 to 9\n    total := total + i * i\nplot(total)\n",
        ),
        (
            "bar time arithmetic",
            "indicator('P')\nplot(time - time[1])\nplot(last_bar_time - time)\n",
        ),
    ] {
        let program = compile_csil(source);
        guarded_native(&program, label);
        let data = synthetic_data(48);
        differential_program(&program, &data, label);
        let wasm = standalone_indicator(&program, label);
        pinewasm_aot::audit_standalone_wasm(&wasm)
            .unwrap_or_else(|error| panic!("{label}: public audit: {error}"));
        assert_standalone_wasm_plot_matches_vm(&program, &wasm, &data, label);
    }
}

/// A checked add, subtract or multiply whose result is a present minimum.
/// The interpreter keeps it as an integer; the module stops with status 6 at
/// the guarded instruction. One step away from the minimum, and on overflow
/// (the operation's own `na`), the guard stays silent and both agree.
#[test]
fn present_minimum_from_guarded_arithmetic_stops_with_abi_mismatch() {
    use pinewasm_csil::CsilInstr as C;
    for (label, expression, opcode, to_minimum, adjacent, overflow) in [
        (
            "add",
            "a + -1",
            C::IntAdd,
            i64::MIN + 1,
            i64::MIN + 2,
            i64::MIN,
        ),
        (
            "subtract",
            "a - 1",
            C::IntSub,
            i64::MIN + 1,
            i64::MIN + 2,
            i64::MIN,
        ),
        (
            "multiply",
            "a * 2",
            C::IntMul,
            -(1_i64 << 62),
            -(1_i64 << 62) + 1,
            -(1_i64 << 62) - 1,
        ),
    ] {
        // `minval` keeps the input read itself clear, so the arithmetic is
        // the only ambiguous site. `na(x)` tells a present minimum from `na`.
        let source = format!(
            "indicator('Guarded {label}')\n\
             a = input.int(5, \"Length\", minval = -9223372036854775807)\n\
             x = {expression}\n\
             plot(na(x) ? 0.0 : 1.0)\n\
             plot(x < 0 ? 1.0 : 0.0)\n"
        );
        let program = compile_csil(&source);
        let aot = guarded_native(&program, label);
        let site = program
            .main()
            .code
            .iter()
            .position(|instr| *instr == opcode)
            .unwrap_or_else(|| panic!("{label}: opcode survives compilation"));
        assert_eq!(
            int_min_guards(&program),
            std::collections::BTreeMap::from([(site, IntMinGuard::Arith)]),
            "{label}"
        );
        let data = synthetic_data(3);
        let with_input = |value: i64| RunOptions {
            inputs: std::collections::HashMap::from([(0, InputValue::Int(value))]),
            ..RunOptions::default()
        };

        // The interpreter's value is the minimum, present and negative.
        let options = with_input(to_minimum);
        let vm = run(&program, &data, &options);
        assert!(vm.errors.is_empty(), "{label}: {:?}", vm.errors);
        assert_eq!(vm.plots[0].values, vec![Value::Float(1.0); 3], "{label}");
        assert_eq!(vm.plots[1].values, vec![Value::Float(1.0); 3], "{label}");
        // The module does not produce a value at all.
        let stopped = assert_present_min_stops(&aot, &program, &data, &options, 0, label);
        assert_eq!(stopped.err_pc as usize, site, "{label}");

        // The same request against the standalone module, which has no
        // interpreter to reroute to, is an evaluation error.
        let wasm = standalone_indicator(&program, label);
        let wire = |value: i64| {
            standalone_wasm_wire(&data, None).replacen(
                "\"params\":{}",
                &format!("\"params\":{{\"length\":{value}}}"),
                1,
            )
        };
        let mut host = StandaloneWasmHost::new(&wasm);
        let error = host
            .evaluate(&wire(to_minimum))
            .expect_err("a present minimum must not evaluate");
        assert_eq!(error, "unexpected status 6", "{label}");
        host.evaluate(&wire(adjacent))
            .unwrap_or_else(|error| panic!("{label}: MIN + 1 evaluates: {error}"));

        // One step from the minimum, and an overflow, are ordinary results.
        for (value, present) in [(adjacent, 1.0), (overflow, 0.0), (5, 1.0)] {
            let options = with_input(value);
            let vm = run(&program, &data, &options);
            assert!(vm.errors.is_empty(), "{label}: {:?}", vm.errors);
            assert_eq!(
                vm.plots[0].values,
                vec![Value::Float(present); 3],
                "{label}"
            );
            let actual = run_aot(&aot, &program, &data, &options);
            assert_eq!(actual.status, status::OK, "{label} input {value}");
            assert_outcomes_match(&vm, &actual, data.bars(), label);
        }
    }
}

/// A float-to-integer conversion whose rounded operand is exactly `-2^63`.
/// The interpreter's value is the minimum. The module stops with status 6 on
/// that bar. Every other float, in range or not, is an ordinary result.
#[test]
fn present_minimum_from_a_guarded_float_conversion_stops_with_abi_mismatch() {
    let minimum = i64::MIN as f64;
    for function in ["int", "math.round", "math.floor", "math.ceil"] {
        let source = format!(
            "indicator('Guarded conversion')\n\
             x = {function}(close)\n\
             plot(na(x) ? 0.0 : 1.0)\n\
             plot(x)\n"
        );
        let program = compile_csil(&source);
        let aot = guarded_native(&program, function);
        let site = program
            .main()
            .code
            .iter()
            .position(|instr| matches!(instr, pinewasm_csil::CsilInstr::CallPure { .. }))
            .expect("conversion call");
        assert_eq!(
            int_min_guards(&program),
            std::collections::BTreeMap::from([(site, IntMinGuard::FloatToInt)]),
            "{function}"
        );

        // Neighbours of the minimum on both sides, out-of-range values and
        // non-finite values: the conversion's own `na` or an ordinary integer.
        let mut ordinary = synthetic_data(9);
        ordinary.close = vec![
            minimum + 1024.0,
            minimum - 2048.0,
            -((1_i64 << 62) as f64),
            i64::MAX as f64,
            9_007_199_254_740_993.0,
            -0.5,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ];
        let outcome = differential_program(&program, &ordinary, function);
        assert_eq!(outcome.plots[0].values[0], Value::Float(1.0), "{function}");
        assert_eq!(outcome.plots[0].values[1], Value::Float(0.0), "{function}");

        let mut data = synthetic_data(4);
        data.close[2] = minimum;
        let options = RunOptions::default();
        let vm = run(&program, &data, &options);
        assert!(vm.errors.is_empty(), "{function}: {:?}", vm.errors);
        assert_eq!(vm.plots[0].values[2], Value::Float(1.0), "{function}");
        assert_eq!(vm.plots[1].values[2], Value::Float(minimum), "{function}");
        let stopped = assert_present_min_stops(&aot, &program, &data, &options, 2, function);
        assert_eq!(stopped.err_pc as usize, site, "{function}");

        let wasm = standalone_indicator(&program, function);
        let error = StandaloneWasmHost::new(&wasm)
            .evaluate(&standalone_wasm_wire(&data, None))
            .expect_err("a present minimum must not evaluate");
        assert_eq!(error, "unexpected status 6", "{function}");
    }
}

/// `time` rides an f64 lane. The read is guarded to the range in which that
/// lane holds an i64 exactly; a timestamp at or beyond `2^53` in magnitude,
/// the minimum included, stops the run instead of reading a rounded value.
#[test]
fn bar_time_outside_the_exact_lane_range_stops_with_abi_mismatch() {
    let program = compile_csil("indicator('Time')\nplot(time == time[1] ? 1.0 : 0.0)\n");
    let aot = guarded_native(&program, "time");
    assert!(int_min_guards(&program)
        .values()
        .all(|guard| *guard == IntMinGuard::TimeLane));
    // The interpreter requires a strictly increasing time axis, so the
    // tested timestamp sits at whichever end keeps the axis valid.
    let limit = 1_i64 << 53;
    let axis = |bar: usize, value: i64| {
        let mut data = synthetic_data(3);
        if bar == 0 {
            data.time = vec![value, value.saturating_add(1), value.saturating_add(2)];
        } else {
            data.time[2] = value;
        }
        data
    };
    for (bar, exact) in [(2, limit - 1), (0, -(limit - 1)), (0, 0)] {
        differential_program(&program, &axis(bar, exact), "exact time");
    }
    for (bar, inexact) in [
        (2, limit),
        (2, limit + 1),
        (2, i64::MAX - 86_400_000),
        (0, -limit),
        (0, i64::MIN),
    ] {
        let data = axis(bar, inexact);
        let options = RunOptions::default();
        let vm = run(&program, &data, &options);
        assert!(vm.errors.is_empty(), "{:?}", vm.errors);
        assert_present_min_stops(&aot, &program, &data, &options, bar as u32, "inexact time");
    }
}

/// `fixnan` of an integer answers through the TA engine's f64 cell. Its
/// result is guarded: `na` and ordinary integers agree with the interpreter,
/// and a held value that converts to the minimum stops the run.
#[test]
fn integer_ta_results_carry_the_minimum_guard() {
    let program = compile_csil(
        "indicator('Fixnan int')\na = input.int(3, minval = -9223372036854775807)\nint v = bar_index % 2 == 0 ? a : na\nx = fixnan(v)\nplot(na(x) ? 0.0 : 1.0)\nplot(x < 0 ? 1.0 : 0.0)\n",
    );
    let aot = guarded_native(&program, "integer fixnan");
    assert_eq!(
        int_min_guards(&program)
            .values()
            .copied()
            .collect::<Vec<_>>(),
        vec![IntMinGuard::TaResult]
    );
    let data = synthetic_data(6);
    differential_program(&program, &data, "integer fixnan");
    for ordinary in [-(1_i64 << 53), -(1_i64 << 62), i64::MAX - 1024] {
        let options = RunOptions {
            inputs: std::collections::HashMap::from([(0, InputValue::Int(ordinary))]),
            ..RunOptions::default()
        };
        let vm = run(&program, &data, &options);
        assert!(vm.errors.is_empty(), "{:?}", vm.errors);
        let actual = run_aot(&aot, &program, &data, &options);
        assert_eq!(actual.status, status::OK, "input {ordinary}");
        assert_outcomes_match(&vm, &actual, data.bars(), "integer fixnan");
    }
    // `MIN + 1` rounds to -2^63 in the engine's f64 cell, which the
    // interpreter reads back as the minimum; `MAX` rounds to 2^63, which the
    // interpreter keeps as a float. Neither fits the integer cell.
    for unrepresentable in [i64::MIN + 1, i64::MAX] {
        let options = RunOptions {
            inputs: std::collections::HashMap::from([(0, InputValue::Int(unrepresentable))]),
            ..RunOptions::default()
        };
        let vm = run(&program, &data, &options);
        assert!(vm.errors.is_empty(), "{:?}", vm.errors);
        assert_eq!(vm.plots[0].values, vec![Value::Float(1.0); 6]);
        assert_present_min_stops(&aot, &program, &data, &options, 0, "integer fixnan");
    }
}

/// Integer `ta.valuewhen` answers through the same f64 cell. Its result is
/// guarded wherever the module keeps it as an integer: a captured value that
/// rounds to `-2^63` is the present minimum in the interpreter and one that
/// rounds to `2^63` is a float there, so neither may continue as an integer.
#[test]
fn integer_valuewhen_results_carry_the_minimum_guard() {
    let program = compile_csil(
        "indicator('Valuewhen int')\na = input.int(3)\nx = ta.valuewhen(bar_index % 2 == 0, a, 0)\nplot(na(x) ? 0.0 : 1.0)\nplot(x < 0 ? 1.0 : 0.0)\nplot(x == a ? 1.0 : 0.0)\n",
    );
    let aot = guarded_native(&program, "integer valuewhen");
    let call = program
        .main()
        .code
        .iter()
        .position(|instr| matches!(instr, pinewasm_csil::CsilInstr::CallTa { .. }))
        .expect("the capture call");
    assert_eq!(
        int_min_guards(&program).get(&call),
        Some(&IntMinGuard::TaResult)
    );
    let data = synthetic_data(6);
    differential_program(&program, &data, "integer valuewhen");
    let with = |value: i64| RunOptions {
        inputs: std::collections::HashMap::from([(0, InputValue::Int(value))]),
        ..RunOptions::default()
    };
    // The nearest inputs on the representable side of each rounding boundary.
    for ordinary in [
        -(1_i64 << 53) - 1,
        (1_i64 << 53) + 1,
        i64::MIN + 513,
        i64::MAX - 512,
    ] {
        let options = with(ordinary);
        let vm = run(&program, &data, &options);
        assert!(vm.errors.is_empty(), "{:?}", vm.errors);
        let actual = run_aot(&aot, &program, &data, &options);
        assert_eq!(actual.status, status::OK, "input {ordinary}");
        assert_outcomes_match(&vm, &actual, data.bars(), "integer valuewhen");
    }
    for unrepresentable in [i64::MIN + 1, i64::MIN + 512, i64::MAX - 511, i64::MAX] {
        let options = with(unrepresentable);
        let vm = run(&program, &data, &options);
        assert!(vm.errors.is_empty(), "{:?}", vm.errors);
        // The interpreter holds a present value: the minimum, or a float.
        assert_eq!(vm.plots[0].values, vec![Value::Float(1.0); 6]);
        let stopped =
            assert_present_min_stops(&aot, &program, &data, &options, 0, "integer valuewhen");
        assert_eq!(stopped.err_pc as usize, call, "input {unrepresentable}");
    }
}

/// The guard on an integer capture also stops a fraction. The analysis
/// refuses every program in which a quotient can reach such a capture, so the
/// fraction is put there by hand: the interpreter keeps `3.5` as a float, and
/// the module must not continue with `3`.
#[test]
fn a_fraction_reaching_an_integer_capture_stops_the_run() {
    use pinewasm_csil::CsilInstr as C;
    for source in [
        "indicator('Fixnan fraction')\na = input.int(3)\nx = fixnan(a)\nplot(na(x) ? 0.0 : 1.0)\nplot(x == 3 ? 1.0 : 0.0)\n",
        "indicator('Valuewhen fraction')\na = input.int(3)\nx = ta.valuewhen(bar_index >= 0, a, 0)\nplot(na(x) ? 0.0 : 1.0)\nplot(x == 3 ? 1.0 : 0.0)\n",
    ] {
        let mut program = compile_csil(source);
        let data = synthetic_data(4);
        differential_program(&program, &data, source);
        let call = program
            .main()
            .code
            .iter()
            .position(|instr| matches!(instr, C::CallTa { .. }))
            .expect("the capture call");
        let main = &mut program.functions[0];
        assert!(matches!(main.code[call - 1], C::IntToF64), "{source}");
        let span = main.source_map[call];
        for instr in &mut main.code {
            match instr {
                C::Jump { target }
                | C::JumpIfFalse { target }
                | C::JumpIfExact { target, .. }
                | C::JumpIfSlotWritten { target, .. }
                    if *target as usize >= call =>
                {
                    *target += 2;
                }
                _ => {}
            }
        }
        main.code
            .splice(call..call, [C::PushF64(0.5_f64.into()), C::F64Add]);
        main.source_map.splice(call..call, [span, span]);
        main.max_stack += 1;
        main.budget_reference = None;
        pinewasm_csil::validate(&program).expect("the edited program validates");
        let call = call + 2;
        let aot = guarded_native(&program, source);
        assert_eq!(
            int_min_guards(&program).get(&call),
            Some(&IntMinGuard::TaResult),
            "{source}"
        );
        let options = RunOptions::default();
        let vm = run(&program, &data, &options);
        assert!(vm.errors.is_empty(), "{:?}", vm.errors);
        assert_eq!(vm.plots[0].values, vec![Value::Float(1.0); 4], "{source}");
        assert_eq!(vm.plots[1].values, vec![Value::Float(0.0); 4], "{source}");
        let stopped = assert_present_min_stops(&aot, &program, &data, &options, 0, source);
        assert_eq!(stopped.err_pc as usize, call, "{source}");
    }
}

/// A producer kind with no guard keeps the refusal: a trade time read from
/// the linked broker, whose `na` is the same bit pattern as a present minimum.
#[test]
fn unguardable_integer_producers_keep_the_refusal() {
    use pinewasm_csil::CsilInstr as C;
    let mut program = compile_csil("indicator('Unguardable')\nplot(close)\n");
    let main = &mut program.functions[0];
    let span = main.source_map[0];
    main.code = vec![
        C::PushI64(0),
        C::CallPure {
            func: pinewasm_hir::PureFunc::StrategyClosedTrade(
                pinewasm_hir::TradeAccessor::EntryTime,
            ),
            argc: 1,
        },
        C::IntToF64,
        C::PlotF64 { plot_slot: 0 },
        C::Return,
    ];
    main.source_map = vec![span; main.code.len()];
    main.max_stack = 1;
    pinewasm_csil::validate(&program).unwrap();
    assert!(matches!(
        pinewasm_aot::analysis::analyze(&program),
        Err(AotError::UnsupportedIntegerNaEncoding { pc: 1 })
    ));
}

// ---- the fraction carry on the chart tier ----------------------------------

/// The interpreter's plot as lane bits: an int widened, `na` the canonical
/// NaN.
fn plot_lane_bits(outcome: &pinewasm_vm::RunOutcome, plot: usize) -> Vec<u64> {
    outcome.plots[plot]
        .values
        .iter()
        .map(|value| match value {
            Value::Float(x) => x.to_bits(),
            Value::Int(x) => (*x as f64).to_bits(),
            Value::Na => f64::NAN.to_bits(),
            other => panic!("plot holds {other:?}"),
        })
        .collect()
}

/// One plot lane against the interpreter's, bit for bit, `na` included.
/// Returns the number of fractional samples.
fn assert_lane_matches(lane: &[f64], expected: &[u64], context: &str) -> usize {
    assert_eq!(lane.len(), expected.len(), "{context}: lane length");
    let mut fractional = 0;
    for (bar, (aot, interp)) in lane.iter().zip(expected).enumerate() {
        let value = f64::from_bits(*interp);
        fractional += usize::from(value.is_finite() && value.fract() != 0.0);
        assert_eq!(
            aot.to_bits(),
            *interp,
            "{context}: bar {bar}: module {aot:?} vs interpreter {value:?}"
        );
    }
    fractional
}

fn kept_quotient_program(n: i64, body: &str) -> pinewasm_csil::CsilProgram {
    compile_csil(&format!(
        "//@version=6\nindicator(\"kept quotient\")\nn = input.int({n}, \"N\")\n{body}"
    ))
}

/// The chart tier (`compile_program`) runs a kept int quotient as float code
/// too, so a script that divides two ints is not sent to the interpreter
/// just for that. Every shape of the standalone table must agree with the
/// interpreter bit for bit, `na` included.
#[test]
fn kept_integer_division_runs_natively_on_the_chart_tier() {
    let data = synthetic_data(24);
    let options = RunOptions::default();
    let mut compared = 0;
    let mut fractional = 0;
    let mut failed_in_both = 0;
    for n in [7_i64, 8, 9, 1, 0, -7, -8, 6] {
        for (name, body) in KEPT_INT_DIVISION_BODIES
            .iter()
            .chain(KEPT_INT_DIVISION_BODIES_ON_MAIN)
        {
            let context = format!("{name}, n = {n}");
            let program = kept_quotient_program(n, body);
            let reference = run(&program, &data, &options);
            let aot = match compile_program(&program) {
                Ok(aot) => aot,
                Err(error) => {
                    // Integer arithmetic in unreachable code has no site for
                    // the run-time integer-minimum guard, with or without a
                    // quotient; the carry cannot help, so the refusal the
                    // program was written with stands and it runs hosted.
                    assert_eq!(*name, "unreachable quotient", "{context}: {error}");
                    assert_eq!(
                        error,
                        pinewasm_aot::AotError::UnsupportedOpcode("PineIntDiv".to_string()),
                        "{context}"
                    );
                    let hosted = compile_program_to_wasm(&program).expect("hosted");
                    assert_eq!(
                        hosted.layout.execution_mode,
                        ExecutionMode::InterpreterHosted
                    );
                    continue;
                }
            };
            assert_eq!(
                aot.layout.execution_mode,
                ExecutionMode::Native,
                "{context}"
            );
            wasmparser::validate(&aot.wasm).unwrap_or_else(|e| panic!("{context}: {e}"));
            let aot_run = run_aot(&aot, &program, &data, &options);
            if !reference.errors.is_empty() {
                // A zero or wrong-way loop step: both engines must stop.
                assert_ne!(aot_run.status, status::OK, "{context}");
                failed_in_both += 1;
                continue;
            }
            assert_eq!(
                aot_run.status,
                status::OK,
                "{context}: {:?}",
                aot_run.host_error
            );
            fractional +=
                assert_lane_matches(&aot_run.plots[0], &plot_lane_bits(&reference, 0), &context);
            compared += 1;
        }
    }
    // 59 of the 60 shapes at 8 input values.
    assert_eq!(compared + failed_in_both, 59 * 8);
    assert!(fractional > 400, "only {fractional} fractional samples");
    eprintln!(
        "{compared} scripts compared, {fractional} fractional samples, {failed_in_both} failed in both"
    );
}

/// The everyday quotient shapes, by execution mode. A quotient that is itself formatted as text stays on
/// the interpreter: the host would be handed a float where it holds an int.
#[test]
fn the_chart_tier_modes_of_the_everyday_quotient_shapes() {
    let mode = |body: &str| {
        let program = compile_csil(&format!("//@version=6\nindicator(\"p\")\n{body}"));
        let module = compile_program_to_wasm(&program).expect("universal entry point");
        (module.layout.execution_mode, module.layout.imports_vm)
    };
    // `true`: the typed pass refuses the program as written and the carry
    // makes it native. `false`: native as written (a quotient slot, a cast
    // read at once), so the carry is never tried and the bytes are the
    // typed pass's own.
    for (body, carried, imports_vm) in [
        ("plot(close[input.int(10) / 2])\n", true, false),
        ("plot(int(bar_index / 2))\n", false, false),
        (
            "k = bar_index / 2 + 1\nplot(k % 3 == 0 ? 1 : -1)\n",
            true,
            false,
        ),
        ("half = bar_index / 2\nplot(half)\n", false, false),
        (
            "half = bar_index / 2\nplot(half)\nif barstate.islast\n    label.new(bar_index, close, \"last\")\n",
            false,
            true,
        ),
    ] {
        assert_eq!(mode(body), (ExecutionMode::Native, imports_vm), "{body}");
        let program = compile_csil(&format!("//@version=6\nindicator(\"p\")\n{body}"));
        assert_eq!(
            pinewasm_aot::analysis::analyze(&program).is_err(),
            carried,
            "{body}: as written"
        );
    }
    for body in [
        "half = bar_index / 2\nlog.info(str.tostring(half))\nplot(half)\n",
        "k = bar_index / 2\nplot(str.length(str.tostring(k)))\n",
        "half = bar_index / 2 + 1\nlabel.new(bar_index, high, str.format(\"{0}\", half))\nplot(close)\n",
    ] {
        let program = compile_csil(&format!("//@version=6\nindicator(\"text\")\n{body}"));
        assert!(compile_program(&program).is_err(), "{body}");
        assert!(pinewasm_aot::supports(&program).is_err(), "{body}");
        assert!(pinewasm_aot::host_site_plan(&program).is_none(), "{body}");
        assert_eq!(
            mode(body),
            (ExecutionMode::InterpreterHosted, false),
            "{body}"
        );
    }
}

/// The carry inserts instructions the interpreter never ran. The chart
/// module charges none of them, so both engines stop at the same budget, on
/// the same bar, and name the same instruction of the program as written.
#[test]
fn a_carried_chart_module_meters_and_reports_like_the_interpreter() {
    let data = synthetic_data(3);
    for body in [
        "half = bar_index / 2\nplot(half + 1)\n",
        "var int acc = 0\nacc := acc + bar_index / 2\nplot(close[acc % 3])\n",
        "k = n / 2 + 1\nplot(bar_index % 2 == 0 ? k * 3 : -k)\n",
        "k = nz(n / (bar_index % 3), 1)\nplot(math.max(k, 2) + math.abs(k) - math.min(k, n))\n",
    ] {
        let program = kept_quotient_program(7, body);
        assert!(
            pinewasm_aot::analysis::analyze(&program).is_err(),
            "{body}: the fixture must need the carry"
        );
        let aot = compile_program(&program).unwrap_or_else(|e| panic!("{body}: {e}"));
        let code_len = program.main().code.len() as u64;
        for limit in 0..=code_len + 1 {
            let context = format!("{body}: per-bar budget {limit}");
            let options = RunOptions {
                max_instructions_per_bar: limit,
                ..RunOptions::default()
            };
            let outcome = run(&program, &data, &options);
            let aot_run = run_aot(&aot, &program, &data, &options);
            match outcome.errors.first() {
                Some(error) => {
                    assert_eq!(aot_run.status, status::BUDGET_EXCEEDED, "{context}");
                    assert_eq!(Some(aot_run.err_bar), error.bar_index, "{context}");
                    assert_eq!(Some(aot_run.err_pc), error.instruction_offset, "{context}");
                }
                None => {
                    assert_eq!(aot_run.status, status::OK, "{context}");
                    assert_lane_matches(&aot_run.plots[0], &plot_lane_bits(&outcome, 0), &context);
                }
            }
        }
        for limit in 0..=code_len * data.bars() as u64 + 1 {
            let context = format!("{body}: run budget {limit}");
            let options = RunOptions {
                max_instructions_per_run: limit,
                ..RunOptions::default()
            };
            let outcome = run(&program, &data, &options);
            let aot_run = run_aot(&aot, &program, &data, &options);
            match outcome.errors.first() {
                Some(error) => {
                    assert_eq!(aot_run.status, status::RUN_BUDGET_EXCEEDED, "{context}");
                    assert_eq!(Some(aot_run.err_bar), error.bar_index, "{context}");
                    assert_eq!(Some(aot_run.err_pc), error.instruction_offset, "{context}");
                }
                None => assert_eq!(aot_run.status, status::OK, "{context}"),
            }
        }
    }

    // A history offset past `max_bars_back`, read through a carried value.
    let program = compile_csil(
        "//@version=6\nindicator(\"history\")\nfar = bar_index * 4 / 2 + 0\nplot(close[far])\n",
    );
    let bars = program.metadata.max_bars_back as usize + 8;
    let data = synthetic_data(bars);
    let outcome = run(&program, &data, &RunOptions::default());
    assert_eq!(outcome.errors.len(), 1, "interpreter must abort past mbb");
    let aot = compile_program(&program).expect("carried offset compiles");
    let aot_run = run_aot(&aot, &program, &data, &RunOptions::default());
    assert_eq!(aot_run.status, status::HISTORY_LIMIT);
    assert_eq!(Some(aot_run.err_bar), outcome.errors[0].bar_index);
    assert_eq!(Some(aot_run.err_pc), outcome.errors[0].instruction_offset);
}

/// A carried program may still call the VM host. The host holds
/// the program as written, so every site must name that program's
/// instruction even though the carry moved it. A carried value itself never
/// reaches the host.
#[test]
fn a_carried_chart_module_serves_host_sites_from_the_program_as_written() {
    let data = synthetic_data(24);
    let options = RunOptions::default();
    for body in [
        "half = bar_index / 2 + 1\nif bar_index % 5 == 0\n    log.info(str.tostring(close))\nplot(half)\n",
        "var int acc = 0\nacc := acc + n / 2\ns = str.tostring(volume) + \"x\"\nplot(acc + str.length(s))\n",
        "k = n / 2 + 1\nvar label l = na\nif barstate.islast\n    l := label.new(bar_index, high, \"last\")\nplot(k)\n",
        // `x` is widened where it meets the quotient; the host still reads
        // it as the int the interpreter holds.
        "x = bar_index * 3\nk = x / 2 + x\nif bar_index % 5 == 0\n    log.info(str.tostring(x) + str.tostring(n))\nplot(k)\n",
    ] {
        let program = kept_quotient_program(7, body);
        assert!(
            pinewasm_aot::analysis::analyze(&program).is_err(),
            "{body}: the fixture must need the carry"
        );
        let aot = compile_program(&program).unwrap_or_else(|e| panic!("{body}: {e}"));
        assert!(aot.layout.imports_vm, "{body}: fixture must use host.vm");
        // The module and the host number sites from one plan, in the
        // coordinates of the program the host executes.
        let (sites, cells) = pinewasm_aot::host_site_plan(&program).expect("site plan");
        assert_eq!(sites.len(), aot.layout.host_sites.len(), "{body}");
        assert_eq!(cells, aot.layout.scratch_cells, "{body}");
        for (site, emitted) in sites.iter().zip(&aot.layout.host_sites) {
            assert_eq!(
                (site.pc, &site.args, site.ret),
                (emitted.pc, &emitted.args, emitted.ret),
                "{body}"
            );
            assert!(
                (site.pc as usize) < program.main().code.len(),
                "{body}: site pc {} is outside the program as written",
                site.pc
            );
        }
        let outcome = run(&program, &data, &options);
        assert!(outcome.errors.is_empty(), "{body}: {:?}", outcome.errors);
        let aot_run = run_aot(&aot, &program, &data, &options);
        assert_eq!(aot_run.status, status::OK, "{body}: {:?}", aot_run.host_error);
        assert_lane_matches(&aot_run.plots[0], &plot_lane_bits(&outcome, 0), body);
        assert_alerts_match(&outcome, &aot_run, &program, body);
        assert_eq!(outcome.logs, aot_run.vm_logs, "{body}: log stream");
        if let Some(drawings) = &aot_run.vm_drawings {
            assert_eq!(&outcome.drawings, drawings, "{body}: drawings");
        }
    }

    // The same with a native heap in the module (a user-defined type, an
    // array) beside the host sites. Everything is compared except each
    // drawing's `id`: with a native heap the host numbers drawings 0, 1, 2
    // where the interpreter's shared handle counter gives 1, 4, 7. That is
    // an existing defect of the native-heap tier, present without any
    // quotient (`k = bar_index * 0.5 + 1`, `T.new(int(k), k)`,
    // `label.new(...)`), and is not this test's subject.
    let without_ids = |drawings: &pinewasm_vm::Drawings| {
        let mut json = serde_json::to_value(drawings).expect("drawings serialize");
        fn strip(value: &mut serde_json::Value) {
            match value {
                serde_json::Value::Object(map) => {
                    map.remove("id");
                    map.values_mut().for_each(strip);
                }
                serde_json::Value::Array(items) => items.iter_mut().for_each(strip),
                _ => {}
            }
        }
        strip(&mut json);
        json
    };
    for body in [
        "type T\n    int whole\n    float part\nk = n / 2 + 1\nt = T.new(int(k), close)\nif bar_index % 6 == 0\n    label.new(bar_index, high, \"mark\")\n    log.info(str.tostring(t.part))\nplot(k + t.whole + t.part)\n",
        "k = bar_index / 2 + n\na = array.new_float(4, 1.5)\narray.set(a, bar_index % 4, close)\nif bar_index % 5 == 0\n    label.new(bar_index, low, str.tostring(array.get(a, 0)))\nlog.info(str.tostring(array.sum(a)))\nplot(k + array.get(a, 1))\n",
        "type C\n    int count\n    float last\nvar c = C.new(0, 0.0)\nk = n / 2 + 1\nc.count := c.count + 1\nc.last := close\nif bar_index % 6 == 0\n    label.new(bar_index, high, str.tostring(c.last))\nplot(k + c.count + c.last)\n",
    ] {
        let program = kept_quotient_program(7, body);
        assert!(
            pinewasm_aot::analysis::analyze(&program).is_err(),
            "{body}: the fixture must need the carry"
        );
        let aot = compile_program(&program).unwrap_or_else(|e| panic!("{body}: {e}"));
        assert!(aot.layout.imports_vm, "{body}: fixture must use host.vm");
        assert!(aot.layout.native_heap, "{body}: fixture must use the native heap");
        let (sites, cells) = pinewasm_aot::host_site_plan(&program).expect("site plan");
        assert_eq!(sites.len(), aot.layout.host_sites.len(), "{body}");
        assert_eq!(cells, aot.layout.scratch_cells, "{body}");
        for (site, emitted) in sites.iter().zip(&aot.layout.host_sites) {
            assert_eq!(
                (site.pc, &site.args, site.ret),
                (emitted.pc, &emitted.args, emitted.ret),
                "{body}"
            );
            assert!((site.pc as usize) < program.main().code.len(), "{body}");
        }
        let outcome = run(&program, &data, &options);
        assert!(outcome.errors.is_empty(), "{body}: {:?}", outcome.errors);
        let aot_run = run_aot(&aot, &program, &data, &options);
        assert_eq!(aot_run.status, status::OK, "{body}: {:?}", aot_run.host_error);
        assert_lane_matches(&aot_run.plots[0], &plot_lane_bits(&outcome, 0), body);
        assert_alerts_match(&outcome, &aot_run, &program, body);
        assert_eq!(outcome.logs, aot_run.vm_logs, "{body}: log stream");
        let drawings = aot_run.vm_drawings.as_ref().expect("host drawings");
        assert!(!outcome.drawings.labels.is_empty(), "{body}: no labels drawn");
        assert_eq!(
            without_ids(&outcome.drawings),
            without_ids(drawings),
            "{body}: drawings apart from their ids"
        );
    }
}

/// Past 2^53 the chart module makes the same exactness checks as the
/// standalone one. Where that one stops, this one hands the run back: its
/// loader has the interpreter, which holds those ints exactly. It agrees bit
/// for bit or reroutes, and never returns a different value.
#[test]
fn large_ints_meeting_a_kept_quotient_stay_exact_or_reroute_the_chart_module() {
    let data = synthetic_data(4);
    let options = RunOptions::default();
    // `true` when the module ran, `false` when it rerouted.
    let outcome = |big: i64, body: &str| {
        let source = format!(
            "//@version=6\nindicator(\"range\")\nn = input.int(7, \"N\")\nbig = {big} + bar_index * 1024\n{body}"
        );
        let context = format!("big = {big}: {body}");
        let program = compile_csil(&source);
        let reference = run(&program, &data, &options);
        assert!(
            reference.errors.is_empty(),
            "{context}: {:?}",
            reference.errors
        );
        let aot = compile_program(&program).unwrap_or_else(|e| panic!("{context}: {e}"));
        let aot_run = run_aot(&aot, &program, &data, &options);
        if aot_run.status == status::ABI_MISMATCH {
            assert!(
                (aot_run.err_pc as usize) < program.main().code.len(),
                "{context}: reroute pc {} is outside the program as written",
                aot_run.err_pc
            );
            return false;
        }
        assert_eq!(aot_run.status, status::OK, "{context}");
        assert_lane_matches(&aot_run.plots[0], &plot_lane_bits(&reference, 0), &context);
        true
    };

    assert!(outcome(
        21_000_000_000_000_000,
        "x = big / 4\nplot(x + 0)\n"
    ));
    assert!(outcome(9_007_199_254_740_000, "k = big / 2 + 1\nplot(k)\n"));
    assert!(!outcome(i64::MAX, "x = big / 1\nplot(x % 1000)\n"));
    assert!(!outcome(i64::MAX - 1, "x = big / 1\nplot(x % 1000)\n"));
    assert!(!outcome(
        -4_611_686_018_427_387_904,
        "k = big / 1\nplot(-(k * 2))\n"
    ));
    assert!(!outcome(
        9_007_199_254_740_993,
        "k = big / 2 + 1\nplot(k)\n"
    ));

    let mut ran = 0;
    let mut rerouted = 0;
    for &big in LARGE_INTS {
        for body in LARGE_INT_BODIES {
            if outcome(big, body) {
                ran += 1;
            } else {
                rerouted += 1;
            }
        }
    }
    assert!(ran > rerouted, "ran {ran}, rerouted {rerouted}");
    eprintln!("{ran} ran and agreed, {rerouted} rerouted");
}

/// What the standalone carry guarantees holds on the chart tier too: an
/// `na` operand on either side of every carried operation gives the
/// canonical NaN, and a quotient never reaches an int-source engine.
#[test]
fn a_carried_chart_module_keeps_na_canonical_and_refuses_int_source_engines() {
    let data = synthetic_data(12);
    let options = RunOptions::default();
    let mut na_samples = 0;
    for op in ["+", "-", "*", "/", "%"] {
        for (left, right) in [
            ("x", "1"),
            ("1", "x"),
            ("0", "x"),
            ("x", "y"),
            ("y", "x"),
            ("x", "x[1]"),
        ] {
            let body = format!(
                "x = n / (bar_index % 3)\ny = n / 2\nplot({left} {op} {right})\nplot(-x + ({left} {op} {right}))\n"
            );
            let program = kept_quotient_program(7, &body);
            let aot = compile_program(&program).unwrap_or_else(|e| panic!("{body}: {e}"));
            let reference = run(&program, &data, &options);
            assert!(reference.errors.is_empty(), "{body}");
            let aot_run = run_aot(&aot, &program, &data, &options);
            assert_eq!(aot_run.status, status::OK, "{body}");
            for plot in 0..2 {
                let expected = plot_lane_bits(&reference, plot);
                assert_lane_matches(&aot_run.plots[plot], &expected, &body);
                na_samples += expected
                    .iter()
                    .filter(|bits| **bits == f64::NAN.to_bits())
                    .count();
            }
        }
    }
    assert!(na_samples > 200, "only {na_samples} na samples");

    for body in [
        "plot(n / 2 + 1)\nplot(ta.valuewhen(bar_index % 2 == 1, bar_index / 2, 0))\n",
        "k = n / 2 + 1\nplot(fixnan(k))\n",
        "k = n / 2 + 1\nint v = ta.valuewhen(close > open, k, 0)\nplot(v)\n",
    ] {
        let program = kept_quotient_program(7, body);
        assert_eq!(
            compile_program(&program).err(),
            Some(pinewasm_aot::AotError::UnsupportedOpcode(
                "PineIntDiv".to_string()
            )),
            "{body}"
        );
        let hosted = compile_program_to_wasm(&program).expect("hosted");
        assert_eq!(
            hosted.layout.execution_mode,
            ExecutionMode::InterpreterHosted,
            "{body}"
        );
    }
}

/// `ta.pivot_point_levels` is a `host.vm` site that pops three operands (the
/// type, the anchor and the developing flag) and pushes an array handle. The
/// typed pass must hand all three to the host. Recording none would leave the
/// host an empty stack (`InvalidProgram` at bar 0), and the emitter would keep
/// three cells on its virtual stack (an invalid module when the handle was
/// read back).
#[test]
fn pivot_point_levels_host_site_passes_its_operands() {
    let data = synthetic_data(300);
    for (name, lines) in [
        (
            "size",
            "a = ta.pivot_point_levels(\"Traditional\", bar_index % 7 == 0)\nplot(array.size(a) + k)\n",
        ),
        (
            "element",
            "a = ta.pivot_point_levels(\"Traditional\", bar_index % 7 == 0)\nplot(array.get(a, 0) + k)\n",
        ),
        (
            "developing",
            "a = ta.pivot_point_levels(\"Fibonacci\", bar_index % 5 == 0, true)\nplot(array.get(a, 1) - array.get(a, 2) + k)\nplot(array.size(a))\n",
        ),
    ] {
        for (kind, k) in [
            ("no quotient", "k = bar_index * 2 + 1\n"),
            ("typed-pass quotient", "k = bar_index / 2\n"),
            ("carried quotient", "k = bar_index / 2 + 1\n"),
        ] {
            let context = format!("{name}, {kind}");
            let source =
                format!("//@version=6\nindicator(\"p\")\nn = input.int(7)\n{k}{lines}");
            let program = compile_csil(&source);
            let native = compile_program(&program)
                .unwrap_or_else(|e| panic!("{context}: not native: {e}"));
            assert!(native.layout.imports_vm, "{context}");
            let outcome = differential_program(&program, &data, &context);
            // The levels are real numbers on some bar, so the comparison
            // says something.
            assert!(
                outcome.plots[0]
                    .values
                    .iter()
                    .any(|value| matches!(value, Value::Float(_))),
                "{context}: the plot is never a number"
            );
        }
    }
}

/// Run a native chart module that reaches the VM host and require
/// every observable of the interpreter's run from it: plots, alerts, logs
/// and drawings.
fn assert_native_host_run_matches(
    program: &pinewasm_csil::CsilProgram,
    aot: &AotModule,
    data: &MarketData,
    context: &str,
) {
    assert_eq!(
        aot.layout.execution_mode,
        ExecutionMode::Native,
        "{context}"
    );
    assert!(aot.layout.imports_vm, "{context}: no host site");
    wasmparser::validate(&aot.wasm).unwrap_or_else(|e| panic!("{context}: {e}"));
    let options = RunOptions::default();
    let outcome = run(program, data, &options);
    assert!(outcome.errors.is_empty(), "{context}: {:?}", outcome.errors);
    let aot_run = run_aot(aot, program, data, &options);
    assert_eq!(
        aot_run.status,
        status::OK,
        "{context}: bar {} pc {} {:?}: {:?} {:?}",
        aot_run.err_bar,
        aot_run.err_pc,
        program.main().code.get(aot_run.err_pc as usize),
        aot_run.mismatch,
        aot_run.host_error
    );
    assert_outcomes_match(&outcome, &aot_run, data.bars(), context);
    assert_alerts_match(&outcome, &aot_run, program, context);
    for (plot, lane) in aot_run.plots.iter().enumerate() {
        assert_lane_matches(
            lane,
            &plot_lane_bits(&outcome, plot),
            &format!("{context} plot {plot}"),
        );
    }
    assert_eq!(outcome.logs, aot_run.vm_logs, "{context}: log stream");
    let drawings = aot_run
        .vm_drawings
        .as_ref()
        .unwrap_or_else(|| panic!("{context}: the host reported no drawings"));
    assert_eq!(&outcome.drawings, drawings, "{context}: drawings");
}

/// A `host.vm` result whose cell type no consumer states defaults to an f64
/// cell. A string or a heap handle does not fit one, so the module returned
/// the reroute status 6 at bar 0 and the loader reruns the whole request on
/// the interpreter; an int fits, and goes back to the host as a float. Each
/// script reaches one such site, and its module runs to the end and equals
/// the interpreter:
///
/// * `syminfo.ticker` and `syminfo.tickerid`, the two string symbol reads
///   the builtin cell table did not list;
/// * `footprint.rows(fp)`, a host object's collection field, typed by the
///   collection call that receives it;
/// * `chart.point`'s `index`, typed by the type's schema, so a setter that
///   copies it into a drawing stores the int the interpreter stores
///   (`line.get_x1` then reads an int, and the drawing lists agree).
#[test]
fn host_results_take_the_cell_type_of_the_value_the_host_returns() {
    let data = synthetic_data(40);
    let header = "//@version=6\nindicator(\"t\")\n";
    for body in [
        "plot(str.length(syminfo.ticker) + str.length(syminfo.sector))\n",
        "plot(str.length(syminfo.tickerid))\n",
        "plot(syminfo.ticker == syminfo.tickerid ? 1 : 0)\n",
        "fp = request.footprint(syminfo.tickerid)\nrows = footprint.rows(fp)\nplot(array.size(rows))\n",
        "plot(array.size(footprint.rows(request.footprint(syminfo.tickerid))))\n",
        "var l = line.new(0, 0.0, 1, 1.0)\nline.set_first_point(l, chart.point.from_index(6, 600.0))\nline.set_second_point(l, chart.point.from_index(bar_index, close))\nplot(line.get_x1(l) == 6 ? 1 : 0)\nplot(line.get_x2(l))\n",
    ] {
        let source = format!("{header}{body}");
        let program = compile_csil(&source);
        let aot =
            compile_program(&program).unwrap_or_else(|e| panic!("{body}: not native: {e}"));
        assert_native_host_run_matches(&program, &aot, &data, body);
    }
}

/// The `footprint.*` and `volume_row.*` accessor results and the `ticker.*`
/// overloads take `series` arguments
/// (`crates/pinewasm_compiler/tests/v6_series_accessors.rs`). Each accessor
/// runs on every bar, its history and a paint color derived from it change
/// per bar, and each `ticker.*` id folds a per-bar string. The Wasm tier
/// equals the interpreter bit for bit on every bar.
#[test]
fn series_footprint_accessors_and_ticker_overloads_match_interpreter() {
    use pinewasm_vm::{HostFootprint, HostVolumeRow};
    let mut data = synthetic_data(40);
    let samples = (0..data.bars())
        .map(|bar| {
            let b = bar as f64;
            Some(HostFootprint {
                buy_volume: 30.0 + b,
                sell_volume: 20.0 + (b * 0.7).sin(),
                total_volume: 50.0 + 10.0 * b,
                // An `na` delta on some bars reaches the lanes and history.
                delta: if bar % 7 == 3 {
                    f64::NAN
                } else {
                    10.0 + (b * 1.3).cos() * 4.0
                },
                poc: 101.0 + b,
                vah: 102.0 + b,
                val: 100.0 + b,
                rows: vec![HostVolumeRow {
                    buy_volume: 3.0 + b,
                    sell_volume: 2.0,
                    total_volume: 5.0 + b,
                    delta: 1.0 + b,
                    up_price: 1.0e6,
                    down_price: 0.0,
                    has_buy_imbalance: bar % 2 == 0,
                    has_sell_imbalance: bar % 3 == 0,
                }],
            })
        })
        .collect();
    data.footprint_data
        .insert("footprint/XYZ".to_string(), samples);
    let header = "//@version=6\nindicator(\"t\")\nfp = request.footprint(\"XYZ\")\nr = footprint.get_row_by_price(fp, close)\n";
    for body in [
        "plot(footprint.delta(fp)[1])\nplot(footprint.total_volume(fp)[2])\nplot(footprint.poc(fp) - footprint.val(fp)[1])\n",
        "plot(footprint.buy_volume(fp)[1] - footprint.sell_volume(fp)[1])\nplot(footprint.vah(fp)[1])\n",
        "plot(close, color = footprint.delta(fp) > 10 ? color.red : color.blue)\n",
        "plot(volume_row.total_volume(r)[1])\nplot(volume_row.delta(r)[1])\nplot(volume_row.has_buy_imbalance(r)[1] ? 1 : 0)\n",
        "plot(close, color = volume_row.has_sell_imbalance(r) ? color.green : color.gray)\n",
        "s = close > open ? \"AAA\" : \"BBB\"\nplot(str.length(ticker.new(\"X\", s)))\nplot(ticker.heikinashi(s) == \"heikinashi:AAA\" ? 1 : 0)\nplot(str.length(ticker.renko(s, \"ATR\", bar_index % 5 + 1)))\n",
    ] {
        let source = format!("{header}{body}");
        differential_with(&source, &data, body);
    }
    // Bars where the host sends no footprint: every accessor reads `na`, the
    // bar after reads that `na` through `[1]`, and `footprint.rows` is empty.
    let present: Vec<_> = data.footprint_data["footprint/XYZ"]
        .iter()
        .enumerate()
        .map(|(bar, sample)| if bar % 5 == 2 { None } else { sample.clone() })
        .collect();
    data.footprint_data
        .insert("footprint/XYZ".to_string(), present);
    let body = "plot(footprint.delta(fp)[1])\nplot(footprint.poc(fp)[1])\nplot(footprint.total_volume(fp)[2])\nplot(array.size(footprint.rows(fp))[1])\nplot(na(r) ? 1 : 0)\nplot(close, color = na(footprint.delta(fp)[1]) ? color.red : color.blue)\n";
    let outcome = differential_program(
        &compile_csil(&format!("{header}{body}")),
        &data,
        "missing footprint samples",
    );
    let delta_history: Vec<bool> = outcome.plots[0]
        .values
        .iter()
        .map(|value| matches!(value, Value::Na) || matches!(value, Value::Float(v) if v.is_nan()))
        .collect();
    for (bar, is_na) in delta_history.iter().enumerate() {
        // `na` on bar 0 (no history), after a missing sample (bar % 5 == 3)
        // and after a sample whose delta is `na` (bar % 7 == 4).
        let want = bar == 0 || bar % 5 == 3 || bar % 7 == 4;
        assert_eq!(*is_na, want, "footprint.delta(fp)[1] at bar {bar}");
    }
}

/// The regression fixture contains `table.delete`, so strict native AOT refuses
/// the alias invalidation site and the universal WebAssembly package routes it
/// through the interpreter. Compare the full chart output to that VM path.
#[test]
fn whole_transparency_language_fixture_uses_the_hosted_tier_for_table_delete() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/regression/language-indicator.pine");
    let source = std::fs::read_to_string(path).expect("fixture");
    let source = whole_transparency_language_fixture(&source);
    let program = compile_csil(&source);
    assert_eq!(
        compile_program(&program).unwrap_err(),
        AotError::UnsupportedOpcode("table.delete alias invalidation requires interpreter".into())
    );
    let aot = compile_program_to_wasm(&program).expect("universal entry point");
    assert_eq!(aot.layout.execution_mode, ExecutionMode::InterpreterHosted);
    wasmparser::validate(&aot.wasm).expect("valid module");

    let data = synthetic_data(300);
    let options = RunOptions::default();
    let outcome = run(&program, &data, &options);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    let actual = run_aot(&aot, &program, &data, &options);
    assert_eq!(actual.status, status::OK);
    assert_outcomes_match(&outcome, &actual, data.bars(), "language-indicator");
    assert_alerts_match(&outcome, &actual, &program, "language-indicator");
    assert_eq!(outcome.logs, actual.vm_logs, "log stream");
    assert_eq!(Some(outcome.drawings), actual.vm_drawings, "drawings");
}

fn whole_transparency_language_fixture(source: &str) -> String {
    let mut derived = source.to_string();
    // Only the color precision cases are substituted. Keep every original
    // integer expression and every native guard assertion in the tests above.
    for (from, to) in [
        (
            "lang_core_c2 = #FF880080",
            "lang_core_c2 = color.new(#FF8800, 50)",
        ),
        (
            "colRgbF = color.rgb(1.4, 2.6, 3.5, 0.4)",
            "colRgbF = color.rgb(1.4, 2.6, 3.5, 0.0)",
        ),
        (
            "colNewF = color.new(color.red, 49.6)",
            "colNewF = color.new(color.red, 50.0)",
        ),
    ] {
        assert_eq!(derived.matches(from).count(), 1, "{from}");
        derived = derived.replace(from, to);
    }
    derived
}

#[test]
fn unchanged_language_indicator_routes_through_interpreter_for_table_delete() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/regression/language-indicator.pine");
    let source = std::fs::read_to_string(path).expect("fixture");
    let program = compile_csil(&source);
    assert_eq!(
        compile_program(&program).unwrap_err(),
        AotError::UnsupportedOpcode("table.delete alias invalidation requires interpreter".into())
    );
    let aot = compile_program_to_wasm(&program).unwrap();
    assert_eq!(aot.layout.execution_mode, ExecutionMode::InterpreterHosted);
    let data = synthetic_data(12);
    let options = RunOptions::default();
    let reference = run(&program, &data, &options);
    assert!(reference.errors.is_empty(), "{:?}", reference.errors);
    let actual = run_aot(&aot, &program, &data, &options);
    assert_eq!(actual.status, status::OK);
    assert_outcomes_match(
        &reference,
        &actual,
        data.bars(),
        "unchanged language-indicator",
    );
    assert_alerts_match(
        &reference,
        &actual,
        &program,
        "unchanged language-indicator",
    );
    assert_eq!(reference.logs, actual.vm_logs);
    assert_eq!(Some(reference.drawings), actual.vm_drawings);
}

/// Every `host.vm` site must hand the host exactly the operands its
/// instruction pops: the site's argument list is the typed pass's
/// `operand_tys`, the host pushes those and runs the instruction, and the
/// emitter consumes as many cells from its own stack. An arm that pops
/// without recording (as `CallPivotPointLevels` did) breaks all three.
///
/// Checked over every `.pine` file in the repository the chart tier analyzes
/// and over small scripts aimed at the host arms the fixtures do not reach.
#[test]
fn every_host_site_records_all_of_its_operands() {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "pine") {
                out.push(path);
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    walk(&root.join("tests"), &mut files);
    walk(&root.join("benchmarks"), &mut files);
    for krate in std::fs::read_dir(root.join("crates"))
        .expect("crates")
        .flatten()
    {
        walk(&krate.path().join("tests"), &mut files);
    }
    let mut sources: Vec<String> = files
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .collect();
    let header = "//@version=6\nindicator(\"h\")\n";
    for body in [
        "a = ta.pivot_point_levels(\"Traditional\", bar_index % 7 == 0)\nplot(array.size(a))\n",
        "a = ta.pivot_point_levels(\"Woodie\", bar_index % 7 == 0, true)\nplot(array.get(a, 0))\n",
        "plot(request.security(syminfo.tickerid, \"1W\", close))\n",
        "plot(request.security(syminfo.tickerid, \"1W\", close + ta.ema(close, 5)))\n",
        "a = request.security_lower_tf(syminfo.tickerid, \"1\", close)\nplot(array.size(a))\n",
        "plot(time(\"1W\"))\nplot(time_close(\"1W\"))\n",
        "log.info(\"{0} {1}\", close, bar_index)\nlog.warning(str.tostring(close) + \"x\")\nplot(close)\n",
        "if close > open\n    alert(\"up \" + str.tostring(close), alert.freq_once_per_bar)\nplot(close)\n",
        "if bar_index > 1000000\n    runtime.error(\"far \" + str.tostring(bar_index))\nplot(close)\n",
        "var t = table.new(position.top_right, 3, 3)\ntable.cell(t, 0, 1, str.tostring(close), text_color = color.red)\ntable.merge_cells(t, 0, 0, 1, 1)\ntable.clear(t, 0, 0, 2, 2)\nplot(close)\n",
        "var l = label.new(bar_index, close, \"x\")\nlabel.set_xy(l, bar_index, high)\nif bar_index % 9 == 0\n    label.delete(l)\nplot(array.size(label.all))\n",
        "type P\n    float x\n    array<float> xs\nvar p = P.new(1.0, array.new_float(2, 0.0))\nq = P.copy(p)\nq.x := close\nplot(q.x + array.size(q.xs))\n",
        "m = map.new<string, float>()\nmap.put(m, syminfo.ticker, close)\nplot(map.size(m))\nplot(syminfo.ticker == \"X\" ? 1 : 0)\n",
        "plot(str.length(syminfo.sector + timeframe.period))\nplot(dayofweek)\nplot(input.string(\"a\", \"S\") == \"a\" ? 1 : 0)\n",
    ] {
        sources.push(format!("{header}{body}"));
    }

    let mut sites_by_opcode: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    let mut programs = 0;
    for source in &sources {
        let result = pinewasm_compiler::compile(source, pinewasm_compiler::CompileOptions::all());
        let Some(program) = result.program else {
            continue;
        };
        let Ok(analysis) = analyze(&program) else {
            continue;
        };
        programs += 1;
        for (pc, instr) in program.main().code.iter().enumerate() {
            if !analysis.host_site[pc] {
                continue;
            }
            let effect = instr
                .resolve_stack_effect(&program.ta_states)
                .expect("stack effect");
            let opcode = format!("{instr:?}");
            let opcode = opcode
                .split([' ', '(', '{'])
                .next()
                .unwrap_or("?")
                .to_string();
            assert_eq!(
                analysis.operand_tys[pc].len(),
                effect.pops as usize,
                "pc {pc} {instr:?}: operands recorded {:?}",
                analysis.operand_tys[pc]
            );
            assert!(
                analysis.result_tys[pc].len() <= effect.pushes as usize,
                "pc {pc} {instr:?}: results {:?}",
                analysis.result_tys[pc]
            );
            *sites_by_opcode.entry(opcode).or_default() += 1;
        }
    }
    eprintln!("{programs} programs; host sites by opcode: {sites_by_opcode:?}");
    for opcode in [
        "CallPivotPointLevels",
        "CallCollection",
        "CallPure",
        "Log",
        "Alert",
        "RuntimeError",
        "StrConcat",
        "SetCell",
        "NewObject",
        "RegisterDrawing",
        "ReadBuiltin",
    ] {
        assert!(
            sites_by_opcode.contains_key(opcode),
            "no program reaches a {opcode} host site: {sites_by_opcode:?}"
        );
    }
}

/// The integer `ta.valuewhen` result guard inside a carried chart module:
/// its exit names the call in the program as written, not in the rewritten
/// one. The source is an int the engine's float cell cannot return
/// (`i64::MAX` comes back as 2^63), beside an unrelated carried quotient.
#[test]
fn a_ta_result_guard_in_a_carried_chart_module_names_the_written_call() {
    let program = compile_csil(&format!(
        "//@version=6\nindicator(\"guard\")\nbig = input.int({})\nk = bar_index / 2 + 1\nplot(k)\nv = ta.valuewhen(bar_index % 2 == 0, big, 0)\nplot(v > 0 ? 1 : 0)\n",
        i64::MAX
    ));
    assert!(
        analyze(&program).is_err(),
        "the fixture must need the carry"
    );
    let aot = compile_program(&program).expect("carried and native");
    let data = synthetic_data(4);
    let aot_run = run_aot(&aot, &program, &data, &RunOptions::default());
    assert_eq!(aot_run.status, status::ABI_MISMATCH);
    assert!(
        matches!(
            program.main().code[aot_run.err_pc as usize],
            pinewasm_csil::CsilInstr::CallTa { .. }
        ),
        "pc {} is {:?}",
        aot_run.err_pc,
        program.main().code.get(aot_run.err_pc as usize)
    );
}

/// The typed matrix's kept-quotient cells on the chart tier. `typed_matrix.rs`
/// compares the standalone module of every cell with the interpreter, and
/// the byte gate records only whether the chart tier builds a native module.
/// This compares the chart module itself: every matrix cell whose program
/// contains `PINE_INT_DIV` and builds natively on the chart tier is run with
/// its host sites served, and every plot must equal the interpreter's bit
/// for bit, `na` as the canonical NaN. The cells are the matrix's own
/// (`typed_matrix/cells.rs`), selected by what they compile to, so cells
/// added later are covered without an edit here.
#[test]
fn kept_quotient_matrix_cells_match_the_interpreter_on_the_chart_tier() {
    let data = synthetic_data(48);
    let options = RunOptions::default();
    let started = std::time::Instant::now();
    let (mut compared, mut carried, mut hosted, mut rerouted) = (0, 0, 0, 0);
    for cell in matrix::cells() {
        let result =
            pinewasm_compiler::compile(&cell.program, pinewasm_compiler::CompileOptions::all());
        if result
            .diagnostics
            .iter()
            .any(pinewasm_span::Diagnostic::is_error)
        {
            continue;
        }
        let Some(program) = result.program else {
            continue;
        };
        if !program
            .main()
            .code
            .iter()
            .any(|instr| matches!(instr, pinewasm_csil::CsilInstr::PineIntDiv))
        {
            continue;
        }
        let Ok(aot) = compile_program(&program) else {
            // A quotient reaching an int-source engine, or another declared
            // limit: the universal entry point hosts it.
            hosted += 1;
            continue;
        };
        carried += usize::from(analyze(&program).is_err());
        let reference = run(&program, &data, &options);
        assert!(
            reference.errors.is_empty(),
            "{}: {:?}",
            cell.id,
            reference.errors
        );
        let aot_run = run_aot(&aot, &program, &data, &options);
        if aot_run.status == status::ABI_MISMATCH {
            // A guarded value the module cannot hold: the loader reruns on
            // the interpreter. Bars before it must still agree.
            rerouted += 1;
        } else {
            assert_eq!(
                aot_run.status,
                status::OK,
                "{}: {:?}",
                cell.id,
                aot_run.host_error
            );
        }
        let completed = if aot_run.status == status::OK {
            data.bars()
        } else {
            aot_run.err_bar as usize
        };
        assert_eq!(aot_run.plots.len(), reference.plots.len(), "{}", cell.id);
        for (plot, lane) in aot_run.plots.iter().enumerate() {
            let expected = plot_lane_bits(&reference, plot);
            assert_lane_matches(
                &lane[..completed],
                &expected[..completed],
                &format!("{} plot {plot}", cell.id),
            );
        }
        compared += 1;
    }
    eprintln!(
        "{compared} kept-quotient cells compared on the chart tier ({carried} through the carry, \
         {rerouted} rerouted), {hosted} hosted, in {:?}",
        started.elapsed()
    );
    assert!(carried >= 400, "only {carried} carried cells");
    assert!(
        compared > carried,
        "no as-written quotient cell was compared"
    );
}

/// A `strategy()` source never reaches the chart-tier carry: the chart tier
/// refuses every strategy before analysis, so one with a kept quotient is
/// interpreter-hosted, with the strategy refusal as its blocker, and
/// `supports` and `host_site_plan` agree with `compile_program`.
#[test]
fn a_strategy_with_a_kept_quotient_runs_natively_through_the_carry() {
    let source = "//@version=6\nstrategy(\"carried strategy\")\nk = bar_index / 2 + 1\nif k % 3 == 0\n    strategy.entry(\"L\", strategy.long)\nif k % 5 == 0\n    strategy.close(\"L\")\nplot(k)\n";
    let program = compile_csil(source);
    assert!(pinewasm_aot::supports(&program).is_ok());
    assert!(pinewasm_aot::host_site_plan(&program).is_some());
    let outcome = strategy_differential(source, &synthetic_data(120), "carried strategy");
    assert!(
        !outcome.strategy.unwrap().trades.is_empty(),
        "the fixture trades"
    );
}

/// Int `ta.mode` and `ta.range` on the chart tier. A possibly fractional
/// source refuses the program there too, whether the quotient is one the
/// typed pass lowers or one the carry would take, and the universal entry
/// point hosts it. With a whole source the module is native, through the
/// carry when there is a kept quotient elsewhere in the script, and equals
/// the interpreter on every plot.
#[test]
fn int_ta_mode_and_range_on_the_chart_tier_refuse_a_quotient_source_only() {
    let data = synthetic_data(24);
    let options = RunOptions::default();
    for body in [
        "x = n / 2 + bar_index % 3\nplot(ta.mode(x, 3))\n",
        "x = bar_index % 2 == 0 ? n / 2 : bar_index\nplot(ta.range(x, 3))\n",
        "plot(ta.mode(bar_index / 2, 3))\n",
        "k = bar_index / 2 + 1\nplot(ta.range(k, 3))\nplot(k)\n",
    ] {
        let program = kept_quotient_program(7, body);
        assert_eq!(
            compile_program(&program).err(),
            Some(pinewasm_aot::AotError::UnsupportedOpcode(
                "PineIntDiv".to_string()
            )),
            "{body}"
        );
        let hosted = compile_program_to_wasm(&program).expect("hosted");
        assert_eq!(
            hosted.layout.execution_mode,
            ExecutionMode::InterpreterHosted,
            "{body}"
        );
    }

    let mut carried = 0;
    for body in [
        "plot(ta.mode(bar_index % 4, 3))\nplot(ta.range(bar_index % 5, 3))\n",
        "plot(ta.mode(bar_index % 4, 3))\nplot(bar_index / 2)\n",
        "plot(ta.mode(bar_index % 4, 3))\nplot(bar_index / 2 + 1)\n",
        "k = n / 2 + 1\nplot(ta.range(bar_index % 5, 3) + 1)\nplot(k)\n",
        "m = ta.mode(bar_index % 4, 3)\nplot(m + bar_index / 2)\n",
        "r = ta.range(bar_index % 5, 4)\nplot(r * (n / 2) - 1)\n",
        "plot(ta.mode(int(bar_index / 2), 3))\nplot(n / 2 + 1)\n",
        "plot(ta.range(math.floor(bar_index / 2), 3))\nplot(-(n / 2))\n",
        "plot(ta.median(bar_index / 2, 3))\nplot(n / 2 + 1)\n",
    ] {
        let program = kept_quotient_program(7, body);
        carried += usize::from(analyze(&program).is_err());
        let aot = compile_program(&program).unwrap_or_else(|e| panic!("{body}: {e}"));
        assert_eq!(aot.layout.execution_mode, ExecutionMode::Native, "{body}");
        let reference = run(&program, &data, &options);
        assert!(
            reference.errors.is_empty(),
            "{body}: {:?}",
            reference.errors
        );
        let aot_run = run_aot(&aot, &program, &data, &options);
        assert_eq!(aot_run.status, status::OK, "{body}");
        for (plot, lane) in aot_run.plots.iter().enumerate() {
            assert_lane_matches(
                lane,
                &plot_lane_bits(&reference, plot),
                &format!("{body} plot {plot}"),
            );
        }
    }
    assert!(
        carried >= 5,
        "only {carried} programs went through the carry"
    );
}

/// The bodies of a chart module's per-bar code, as body indices: `bar_step`,
/// the last defined function, and, when its body is split
/// (`body/split.rs`), every piece it reaches. A piece is called with the bar
/// (`local.get 0`), and its result is either kept (`local.tee`) or, for the
/// first piece, returned by `bar_step`; it sits after `run` and before the
/// three stop functions that precede `bar_step`.
fn bar_bodies(wasm: &[u8]) -> Vec<usize> {
    use wasmparser::{Operator, Parser, Payload, TypeRef};

    let mut imports = 0usize;
    let mut run = None;
    let mut bodies: Vec<Vec<Operator>> = Vec::new();
    for payload in Parser::new(0).parse_all(wasm) {
        match payload.unwrap() {
            Payload::ImportSection(section) => {
                for import in section.into_imports() {
                    imports += usize::from(matches!(import.unwrap().ty, TypeRef::Func(_)));
                }
            }
            Payload::ExportSection(section) => {
                for export in section {
                    let export = export.unwrap();
                    if export.name == "run" {
                        run = Some(export.index as usize);
                    }
                }
            }
            Payload::CodeSectionEntry(body) => bodies.push(
                body.get_operators_reader()
                    .unwrap()
                    .into_iter()
                    .map(Result::unwrap)
                    .collect(),
            ),
            _ => {}
        }
    }
    let bar_step = bodies
        .len()
        .checked_sub(1)
        .expect("a chart module has a bar_step");
    let run = run.expect("a chart module exports run") - imports;
    let first_stop = bar_step - 3;
    let mut found = vec![bar_step];
    let mut at = 0;
    while at < found.len() {
        let ops = &bodies[found[at]];
        for i in 1..ops.len().saturating_sub(1) {
            if let (
                Operator::LocalGet { local_index: 0 },
                Operator::Call { function_index },
                Operator::LocalTee { .. } | Operator::End,
            ) = (&ops[i - 1], &ops[i], &ops[i + 1])
            {
                let Some(body) = (*function_index as usize).checked_sub(imports) else {
                    continue;
                };
                if body > run && body < first_stop && !found.contains(&body) {
                    found.push(body);
                }
            }
        }
        at += 1;
    }
    found
}

/// How many of a split body's piece calls ([`bar_bodies`]) return into
/// code of their caller: after the call (`local.get 0; call; local.tee`),
/// its status test and its reloads, the branch to the callee's exit names
/// a label inside the calling piece, whose whole body is one outer `block`,
/// rather than that block itself (the caller then leaves too).
fn returning_piece_calls(wasm: &[u8]) -> usize {
    use wasmparser::{Operator, Parser, Payload, TypeRef};

    let pieces = bar_bodies(wasm);
    let mut imports = 0usize;
    let mut bodies: Vec<Vec<Operator>> = Vec::new();
    for payload in Parser::new(0).parse_all(wasm) {
        match payload.unwrap() {
            Payload::ImportSection(section) => {
                for import in section.into_imports() {
                    imports += usize::from(matches!(import.unwrap().ty, TypeRef::Func(_)));
                }
            }
            Payload::CodeSectionEntry(body) => bodies.push(
                body.get_operators_reader()
                    .unwrap()
                    .into_iter()
                    .map(Result::unwrap)
                    .collect(),
            ),
            _ => {}
        }
    }
    let mut returning = 0;
    for &body in &pieces {
        let ops = &bodies[body];
        let mut labels = 0usize;
        for (i, op) in ops.iter().enumerate() {
            match op {
                Operator::Block { .. } | Operator::If { .. } | Operator::Loop { .. } => labels += 1,
                Operator::End => labels = labels.saturating_sub(1),
                Operator::Call { function_index }
                    if i > 0
                        && matches!(ops[i - 1], Operator::LocalGet { local_index: 0 })
                        && matches!(ops.get(i + 1), Some(Operator::LocalTee { .. }))
                        && (*function_index as usize)
                            .checked_sub(imports)
                            .is_some_and(|callee| pieces.contains(&callee)) =>
                {
                    // The first branch after the status test's `end`.
                    let branch = ops[i + 2..]
                        .iter()
                        .skip_while(|op| !matches!(op, Operator::End))
                        .find_map(|op| match op {
                            Operator::Br { relative_depth } => Some(*relative_depth as usize),
                            _ => None,
                        })
                        .expect("a piece call branches to its callee's exit");
                    returning += usize::from(branch + 1 < labels);
                }
                _ => {}
            }
        }
    }
    returning
}

/// The grouped-fuel prelude costs in a chart module's per-bar code
/// ([`bar_bodies`]). The pattern is the guard/debit template of
/// `emit_fuel_group` on the counter locals 1 (per bar) and 2 (run).
fn chart_fuel_groups(wasm: &[u8]) -> Vec<i64> {
    chart_fuel_groups_by_body(wasm)
        .into_iter()
        .flat_map(|(_, costs)| costs)
        .collect()
}

/// [`chart_fuel_groups`] per body of the per-bar code that has any.
fn chart_fuel_groups_by_body(wasm: &[u8]) -> Vec<(usize, Vec<i64>)> {
    use wasmparser::{Parser, Payload};

    let mut bodies = Vec::new();
    for payload in Parser::new(0).parse_all(wasm) {
        if let Payload::CodeSectionEntry(body) = payload.unwrap() {
            bodies.push(body);
        }
    }
    let mut out = Vec::new();
    for index in bar_bodies(wasm) {
        let ops: Vec<_> = bodies[index]
            .get_operators_reader()
            .unwrap()
            .into_iter()
            .map(Result::unwrap)
            .collect();
        let costs = window_costs(&ops);
        if !costs.is_empty() {
            out.push((index, costs));
        }
    }
    out
}

/// The window prelude costs in one body's operators.
fn window_costs(ops: &[wasmparser::Operator]) -> Vec<i64> {
    use wasmparser::Operator;
    ops.windows(16)
        .filter_map(|ops| match ops {
            [Operator::LocalGet { local_index: 1 },
             Operator::I64Const { value: bar_cost },
             Operator::I64GeU,
             Operator::LocalGet { local_index: 2 },
             Operator::I64Const { value: run_cost },
             Operator::I64GeU,
             Operator::I32And,
             Operator::If { blockty: wasmparser::BlockType::Empty },
             Operator::LocalGet { local_index: 1 },
             Operator::I64Const { value: debit_bar_cost },
             Operator::I64Sub,
             Operator::LocalSet { local_index: 1 },
             Operator::LocalGet { local_index: 2 },
             Operator::I64Const { value: debit_run_cost },
             Operator::I64Sub,
             Operator::LocalSet { local_index: 2 }]
                if bar_cost == run_cost
                    && bar_cost == debit_bar_cost
                    && bar_cost == debit_run_cost
                    && *bar_cost > 0 =>
            {
                Some(*bar_cost)
            }
            _ => None,
        })
        .collect()
}

/// Run `program` natively and on the interpreter with both budgets set, and
/// require the interpreter's verdict: the same status, bar and instruction
/// for a stop, the same output for every bar before it, the same alerts.
/// Returns whether the interpreter finished, or `None` when the module handed
/// the request to the interpreter (status 10 at a `host.vm` site whose run
/// budget is below the per-bar one), which replays it exactly by definition.
fn chart_budget_cut_matches(
    program: &pinewasm_csil::CsilProgram,
    aot: &AotModule,
    data: &MarketData,
    per_bar: u64,
    per_run: u64,
) -> Option<bool> {
    let options = RunOptions {
        max_instructions_per_bar: per_bar,
        max_instructions_per_run: per_run,
        ..RunOptions::default()
    };
    let context = format!("chart grouped fuel bar={per_bar}, run={per_run}");
    let expected = run(program, data, &options);
    let actual = run_aot(aot, program, data, &options);
    if actual.status == status::RUN_BUDGET_REROUTE {
        assert!(
            aot.layout.imports_vm,
            "{context}: reroute without a host site"
        );
        assert!(
            per_run < u64::MAX,
            "{context}: reroute with an unbounded run budget"
        );
        return None;
    }
    assert_alerts_match(&expected, &actual, program, &context);
    match expected.errors.first() {
        Some(error) => {
            if actual.status == pinewasm_vm::aot_host::vm_status::HOST_ERROR {
                // The budget ran out inside a `host.vm` site's own charge:
                // the host raised the interpreter's structured error there.
                let host = actual
                    .host_error
                    .as_ref()
                    .unwrap_or_else(|| panic!("{context}: HOST_ERROR without an error"));
                assert_eq!(host.kind, error.kind, "{context}");
                assert_eq!(host.bar_index, error.bar_index, "{context}");
                assert_eq!(
                    host.instruction_offset, error.instruction_offset,
                    "{context}"
                );
            } else {
                let status = match error.kind {
                    pinewasm_vm::RuntimeErrorKind::InstructionLimitExceeded => {
                        status::BUDGET_EXCEEDED
                    }
                    pinewasm_vm::RuntimeErrorKind::RunInstructionLimitExceeded => {
                        status::RUN_BUDGET_EXCEEDED
                    }
                    pinewasm_vm::RuntimeErrorKind::HistoryBufferLimitExceeded => {
                        status::HISTORY_LIMIT
                    }
                    other => panic!("{context}: unexpected interpreter error {other:?}"),
                };
                assert_eq!(actual.status, status, "{context}");
            }
            assert_eq!(Some(actual.err_bar), error.bar_index, "{context}");
            assert_eq!(Some(actual.err_pc), error.instruction_offset, "{context}");
            assert_eq!(expected.bars_executed, actual.err_bar, "{context}");
            assert_outcomes_match(
                &expected,
                &actual,
                expected.bars_executed as usize,
                &context,
            );
            Some(false)
        }
        None => {
            assert_eq!(
                actual.status,
                status::OK,
                "{context}: {:?}",
                actual.host_error
            );
            assert_outcomes_match(&expected, &actual, data.bars(), &context);
            Some(true)
        }
    }
}

/// A chart window (`body/chart_fuel.rs`) precharges the summed, weighted cost
/// of its instructions, may close with its block's branch unit, and may hold
/// an instruction that stops the run. Every per-bar and every run budget, cut
/// anywhere inside a window, must still stop where the interpreter stops,
/// after the same effects.
#[test]
fn chart_fuel_groups_preserve_every_budget_cut() {
    let sources = [
        // Weighted windows: each `ta.sma` charges its length; the ternary
        // closes windows with branch units.
        concat!(
            "//@version=6\nindicator(\"Chart fuel weights\")\na = ta.sma(close, 5)\n",
            "b = ta.sma(open, 3)\nx = (a + b) * 2.0 - close / 3.0\nplot(x)\n",
            "plot(close > open ? a : b)\nalertcondition(x > close, \"X\", \"x above\")\n",
        ),
        // A loop of native array reads and dynamic history reads.
        concat!(
            "//@version=6\nindicator(\"Chart fuel loop\")\nvar a = array.new_float(0)\n",
            "array.push(a, close)\ns = 0.0\nn = math.min(array.size(a), 4)\n",
            "for i = 0 to n - 1\n    s += array.get(a, array.size(a) - 1 - i) * 2.0 - close[i]\n",
            "plot(s)\n",
        ),
        // `host.vm` sites between windows.
        concat!(
            "//@version=6\nindicator(\"Chart fuel sites\")\n",
            "s = str.tostring(close * 2.0 - open)\n",
            "plot(str.length(s) + close * 3.0 - open / 2.0)\n",
            "alertcondition(close > open, \"Up\", \"up\")\n",
        ),
    ];
    let mut programs: Vec<pinewasm_csil::CsilProgram> =
        sources.iter().map(|source| compile_csil(source)).collect();
    // A fraction carry: budget-free placeholders and exactness guards inside
    // windows.
    programs.push(kept_quotient_program(
        7,
        "k = n / 2 + 1\nplot(bar_index % 2 == 0 ? k * 3 : -k)\n",
    ));
    let data = synthetic_data(3);
    for program in &programs {
        let aot = compile_program(program).unwrap();
        let groups = chart_fuel_groups(&aot.wasm);
        assert!(!groups.is_empty(), "the fixture must form chart windows");
        let mut cuts = 0;
        for remaining in 0..=20_000_u64 {
            let mut finished = true;
            for (per_bar, per_run) in [
                (remaining, u64::MAX),
                (u64::MAX, remaining),
                (remaining, remaining),
            ] {
                // A rerouted request never finishes natively: once the run
                // counter is below the per-bar one, every `host.vm` site
                // reroutes. It does not hold the loop open.
                if let Some(done) = chart_budget_cut_matches(program, &aot, &data, per_bar, per_run)
                {
                    finished &= done;
                    cuts += usize::from(!done);
                }
            }
            if finished {
                break;
            }
            assert!(remaining < 20_000, "the fixture never finishes");
        }
        assert!(cuts > 0);
    }
    // The weighted fixture's windows carry each average's whole length.
    let aot = compile_program(&programs[0]).unwrap();
    assert!(
        chart_fuel_groups(&aot.wasm).iter().any(|&cost| cost >= 5),
        "{:?}",
        chart_fuel_groups(&aot.wasm)
    );
}

/// A generated indicator long enough that its per-bar body is split
/// (`body/split.rs`): `groups` statement groups, each an `if`/`else if`, a
/// ternary (a value on the operand stack across a join), a `var`
/// accumulator and a history read; the middle third sits in one large `if`
/// block, whose body becomes a piece that returns into code of its caller;
/// then a `for` loop with `continue` and `break` and a `while` loop. `host`
/// adds a `host.vm` site (text formatting) inside the large block;
/// `stop_after` adds a dynamic history read after that group that passes
/// `max_bars_back` on a later bar.
fn split_body_source(groups: usize, host: bool, stop_after: Option<usize>) -> String {
    let mut source = String::from("//@version=6\n");
    source.push_str(if stop_after.is_some() {
        "indicator(\"Split body\", max_bars_back = 20)\n"
    } else {
        "indicator(\"Split body\")\n"
    });
    // A cell the first piece keeps in a local (a loop writes it) and only
    // the last piece reads: every call in between must hand it on.
    source
        .push_str("float total = 0.0\nfloat early = 0.0\nfor j = 0 to 4\n    early += close * j\n");
    let (block_start, block_end) = (groups / 3, 2 * groups / 3);
    for i in 0..groups {
        if i == block_start {
            source.push_str("if bar_index % 3 != 1\n");
        }
        let indent = if (block_start..block_end).contains(&i) {
            "    "
        } else {
            ""
        };
        let mut group = format!(
            "float a{i} = close * {}.0 - open * {i}.5\n\
             if a{i} > high\n    a{i} := a{i} - high\n\
             else if a{i} < low\n    a{i} := low - a{i}\n\
             float b{i} = a{i} > close ? a{i} - close : close - a{i}\n\
             var float acc{i} = 0.0\n\
             acc{i} += b{i} * 0.001 + nz(a{i}[1], 0.0) * 0.0001\n\
             total += acc{i}\n",
            i + 1
        );
        if host && i == usize::midpoint(block_start, block_end) {
            group.push_str("string shown = str.tostring(total)\ntotal += str.length(shown)\n");
        }
        if stop_after == Some(i) {
            group.push_str("far = bar_index * 2\ntotal += nz(close[far], 0.0) * 0.5\n");
        }
        for line in group.lines() {
            source.push_str(indent);
            source.push_str(line);
            source.push('\n');
        }
    }
    source.push_str(concat!(
        "float looped = 0.0\n",
        "for j = 0 to 6\n",
        "    if j == 3\n",
        "        continue\n",
        "    looped += nz(a0[j], 0.0) * 0.5 + acc1\n",
        "    if looped > 1e12\n",
        "        break\n",
        "int k = 0\n",
        "while k < 4\n",
        "    k += 1\n",
        "    looped += k\n",
        "plot(total, color = total > close ? color.green : color.red)\n",
        "plot(looped)\n",
        "plot(acc0[2])\n",
        "plot(early)\n",
        "alertcondition(total > close, \"Total\", \"total above close\")\n",
    ));
    source
}

/// A split body moves function boundaries, not instructions: its pieces run
/// the unsplit body's steps in the same order, charge them in the same
/// windows and stop through the same functions, with the two counters and
/// the current-bar cells handed between pieces in globals. Every run must
/// stay the interpreter's: full and shorter runs, a `host.vm` site inside a
/// piece, a history stop raised from inside a piece (status, bar,
/// instruction and every earlier bar's output), and per-bar, run and paired
/// budget cuts landing in every piece.
#[test]
fn a_split_chart_body_matches_the_vm_at_every_budget() {
    // The budget fixture: no `host.vm` site, so run-budget cuts stop natively
    // rather than rerouting.
    let program = compile_csil(&split_body_source(60, false, None));
    let aot = compile_program(&program).unwrap();
    let pieces = bar_bodies(&aot.wasm);
    assert!(
        pieces.len() > 3,
        "the fixture must split into pieces: {pieces:?}"
    );
    assert!(
        returning_piece_calls(&aot.wasm) > 0,
        "the large `if` block's piece must return into its caller's code"
    );
    let windows = chart_fuel_groups_by_body(&aot.wasm);
    assert!(
        windows.len() > 2,
        "fuel windows must land in several pieces: {:?}",
        windows
            .iter()
            .map(|(body, costs)| (body, costs.len()))
            .collect::<Vec<_>>()
    );
    let data = synthetic_data(3);
    for bars in [data.bars(), 1] {
        let options = RunOptions {
            max_bars: Some(bars),
            ..RunOptions::default()
        };
        let expected = run(&program, &data, &options);
        assert!(expected.errors.is_empty(), "{:?}", expected.errors);
        let actual = run_aot(&aot, &program, &data, &options);
        assert_eq!(actual.status, status::OK, "bars={bars}");
        assert_outcomes_match(&expected, &actual, bars, "split body");
    }
    // Every per-bar budget near the start of the bar, then every 13th: each
    // stops in bar 0 at that charge, in whichever piece it falls. Run and
    // paired budgets stop in later bars too.
    let mut cuts = 0;
    for (kind, stride) in [(0, 13_u64), (1, 61), (2, 89)] {
        let mut remaining = 0_u64;
        loop {
            let (per_bar, per_run) = match kind {
                0 => (remaining, u64::MAX),
                1 => (u64::MAX, remaining),
                _ => (remaining, remaining),
            };
            let done = chart_budget_cut_matches(&program, &aot, &data, per_bar, per_run)
                .expect("no host site to reroute");
            if done {
                break;
            }
            cuts += 1;
            remaining += if kind == 0 && remaining < 200 {
                1
            } else {
                stride
            };
            assert!(remaining < 100_000, "the fixture never finishes");
        }
    }
    assert!(cuts > 300, "{cuts} cuts");

    // The host and stop fixture: a `host.vm` site in a middle piece, and a
    // history read past `max_bars_back` raised from a middle piece.
    let program = compile_csil(&split_body_source(60, true, Some(30)));
    let aot = compile_program(&program).unwrap();
    assert!(aot.layout.imports_vm, "the fixture must call the host");
    assert!(bar_bodies(&aot.wasm).len() > 3, "the fixture must split");
    let data = synthetic_data(16);
    let expected = run(&program, &data, &RunOptions::default());
    assert_eq!(
        expected.errors.first().map(|error| error.kind),
        Some(pinewasm_vm::RuntimeErrorKind::HistoryBufferLimitExceeded),
        "the fixture must pass max_bars_back"
    );
    // The default budgets: a run counter above the per-bar one, so the host
    // site runs natively instead of rerouting.
    assert_eq!(
        chart_budget_cut_matches(
            &program,
            &aot,
            &data,
            pinewasm_vm::DEFAULT_MAX_INSTRUCTIONS_PER_BAR,
            pinewasm_vm::DEFAULT_MAX_INSTRUCTIONS_PER_RUN,
        ),
        Some(false)
    );
    for bars in [8, 3] {
        let options = RunOptions {
            max_bars: Some(bars),
            ..RunOptions::default()
        };
        let expected = run(&program, &data, &options);
        assert!(expected.errors.is_empty(), "{:?}", expected.errors);
        let actual = run_aot(&aot, &program, &data, &options);
        assert_eq!(actual.status, status::OK, "bars={bars}");
        assert_outcomes_match(&expected, &actual, bars, "split body with a host site");
    }
}

/// What a split leaves whole, run against the interpreter. A loop whose body
/// is larger than a piece stays in one function (`body/split.rs` pins that
/// on the plan). A region entered with a value on the operand stack never
/// becomes a piece, because a function boundary would lose the value: here
/// every other statement is `total := total + (c ? a : b)`, whose join, and
/// the rest of the script after it, starts with `total` still on the stack,
/// so the cuts the plan would otherwise make land there half the time. The
/// run still matches the interpreter, and the module is still split at the
/// other joins.
#[test]
fn a_split_body_keeps_loops_and_operand_stack_values_whole() {
    let mut source = String::from(
        "//@version=6\nindicator(\"Split body, whole parts\")\nf_big(float x) =>\n    float r = x\n",
    );
    for _ in 0..60 {
        source.push_str(concat!(
            "    if r > 100.0\n        r := r * 0.99\n",
            "    else if r < 50.0\n        r := r * 1.01\n",
            "    r := r + (r > 75.0 ? 0.5 : -0.5)\n",
        ));
    }
    source.push_str("    r\nfloat total = 0.0\nfor i = 0 to 1\n");
    for g in 0..30 {
        source.push_str(&format!(
            "    float l{g} = close * {}.0 - open * i\n    if l{g} > high\n        l{g} := l{g} - high\n    \
             total += l{g} > close ? l{g} - close : close - l{g}\n",
            g + 1
        ));
    }
    source.push_str(concat!(
        "float arm = bar_index % 2 == 0 ? f_big(close) : open\n",
        "float pending = close + (bar_index % 3 == 0 ? f_big(open) : 0.0)\n",
    ));
    for g in 0..60 {
        source.push_str(&format!(
            "float t{g} = close * {}.0 - open\nif t{g} > high\n    t{g} := t{g} - high\n\
             total := total + (t{g} > close ? t{g} - close : 0.5)\n",
            g + 1
        ));
    }
    source.push_str("plot(total)\nplot(arm)\nplot(pending)\n");
    let program = compile_csil(&source);
    let aot = compile_program(&program).unwrap();
    assert!(bar_bodies(&aot.wasm).len() > 2, "the fixture must split");
    let data = synthetic_data(7);
    for bars in [data.bars(), 2] {
        let options = RunOptions {
            max_bars: Some(bars),
            ..RunOptions::default()
        };
        let expected = run(&program, &data, &options);
        assert!(expected.errors.is_empty(), "{:?}", expected.errors);
        let actual = run_aot(&aot, &program, &data, &options);
        assert_eq!(actual.status, status::OK, "bars={bars}");
        assert_outcomes_match(&expected, &actual, bars, "split body, whole parts");
    }
}

/// A region that leaves a value on the operand stack for the label it
/// branches to never becomes a piece either. The front end stores a
/// ternary's arms in temporaries, so no compiled script has such a region
/// larger than a few instructions; valid CSIL can, and a function boundary
/// would lose the value. The fixture inserts one, a then-arm of some 2,200
/// instructions that leaves its sum for the join, which stores it to the
/// plotted slot, into a script long enough to split.
#[test]
fn a_split_body_never_moves_a_region_that_leaves_a_value_on_the_stack() {
    use pinewasm_csil::CsilInstr as C;
    let mut source =
        String::from("//@version=6\nindicator(\"Split body, stack exit\")\nfloat t = 0.0\n");
    for g in 0..60 {
        source.push_str(&format!(
            "float a{g} = close * {}.0 - open\nif a{g} > high\n    a{g} := a{g} - high\nt += a{g}\n",
            g + 1
        ));
    }
    source.push_str("plot(t)\n");
    let mut program = compile_csil(&source);
    let slot = program
        .series_slots
        .iter()
        .position(|def| def.name == "t")
        .expect("the plotted slot") as u32;
    let main = &mut program.functions[0];
    let plot = main
        .code
        .iter()
        .rposition(|instr| matches!(instr, C::PlotF64 { .. } | C::PlotStyledF64 { .. }))
        .expect("the plot");
    let color_count = match main.code[plot] {
        C::PlotF64 { .. } => 0,
        C::PlotStyledF64 { color_count, .. } => usize::from(color_count),
        _ => unreachable!("selected a plot instruction"),
    };
    // Default paint is explicit. Keep the inserted stack-valued region
    // before the numeric operand, without moving or removing color operands.
    let at = plot.checked_sub(1 + color_count).expect("plot operands");
    assert!(matches!(main.code[at], C::ReadUserSlot { slot: s, offset: 0 } if s == slot));
    assert!(main.code[at + 1..plot]
        .iter()
        .all(|instr| matches!(instr, C::PushColor { .. })));
    // Before the plot's read: on even bars a long sum, on odd bars 2.0, left
    // on the stack for the join, which stores it to `t`.
    let arm = 1 + 2 * 1_100;
    let mut inserted = vec![
        C::ReadBuiltin {
            source: pinewasm_hir::BuiltinSeries::BarIndex,
            offset: 0,
        },
        C::PushI64(2),
        C::IntRem,
        C::PushI64(0),
        C::CmpEq,
    ];
    let else_at = (at + inserted.len() + 1 + arm + 1) as u32;
    inserted.push(C::JumpIfFalse { target: else_at });
    inserted.push(C::ReadBuiltin {
        source: pinewasm_hir::BuiltinSeries::Close,
        offset: 0,
    });
    for _ in 0..1_100 {
        inserted.push(C::PushF64(pinewasm_csil::F64Bits::new(0.5)));
        inserted.push(C::F64Add);
    }
    let join = else_at + 1;
    inserted.push(C::Jump { target: join });
    inserted.push(C::PushF64(pinewasm_csil::F64Bits::new(2.0)));
    inserted.push(C::StoreUserSlotCurrent { slot });
    let n = inserted.len() as u32;
    for instr in &mut main.code {
        match instr {
            C::Jump { target }
            | C::JumpIfFalse { target }
            | C::JumpIfExact { target, .. }
            | C::JumpIfSlotWritten { target, .. }
                if *target as usize >= at =>
            {
                *target += n;
            }
            _ => {}
        }
    }
    let span = main.source_map[at];
    main.code.splice(at..at, inserted);
    main.source_map
        .splice(at..at, std::iter::repeat_n(span, n as usize));
    main.max_stack = main.max_stack.max(2);
    main.budget_reference = None;
    pinewasm_csil::validate(&program).expect("the edited program is valid CSIL");
    let aot = compile_program(&program).unwrap();
    assert!(bar_bodies(&aot.wasm).len() > 2, "the fixture must split");
    let data = synthetic_data(5);
    let options = RunOptions::default();
    let expected = run(&program, &data, &options);
    assert!(expected.errors.is_empty(), "{:?}", expected.errors);
    let actual = run_aot(&aot, &program, &data, &options);
    assert_eq!(actual.status, status::OK);
    assert_outcomes_match(&expected, &actual, data.bars(), "split body, stack exit");
}

/// The ABI returns any nonzero `host.vm` result from `run` unchanged, with
/// its error triple. A split body's pieces leave through their exit
/// with 0, so every other result, negative ones included, must reach `run`'s
/// caller exactly as the host returned it, from a site inside a piece.
#[test]
fn a_split_body_returns_any_nonzero_host_status_unchanged() {
    let program = compile_csil(&split_body_source(60, true, None));
    let aot = compile_program(&program).unwrap();
    assert!(bar_bodies(&aot.wasm).len() > 3, "the fixture must split");
    let first_site = aot
        .layout
        .host_sites
        .iter()
        .map(|site| site.pc)
        .min()
        .expect("the fixture calls the host");
    for host_status in [-1, -7, i32::MIN, 5] {
        let engine = wasmi::Engine::default();
        let module = wasmi::Module::new(&engine, &aot.wasm[..]).expect("wasmi accepts the module");
        let mut store = wasmi::Store::new(&engine, ());
        let mut linker = <wasmi::Linker<()>>::new(&engine);
        linker
            .func_wrap("host", "vm", move |_site: i32, _bar: i32| -> i32 {
                host_status
            })
            .expect("define host.vm");
        let instance = linker
            .instantiate_and_start(&mut store, &module)
            .expect("instantiate");
        let prepare = instance
            .get_typed_func::<i32, i32>(&store, "prepare")
            .expect("prepare export");
        assert_eq!(prepare.call(&mut store, 3).unwrap(), status::OK);
        let memory = instance
            .get_memory(&store, "memory")
            .expect("memory export");
        // A per-bar budget below the run budget `prepare` left in scratch
        // cell 0, so the site calls the host instead of rerouting.
        let budget = header::OFF_BUDGET_PER_BAR as usize;
        memory.data_mut(&mut store)[budget..budget + 8]
            .copy_from_slice(&pinewasm_vm::DEFAULT_MAX_INSTRUCTIONS_PER_BAR.to_le_bytes());
        let run_export = instance
            .get_typed_func::<(), i32>(&store, "run")
            .expect("run export");
        assert_eq!(run_export.call(&mut store, ()).unwrap(), host_status);
        let mem = memory.data(&store);
        assert_eq!(read_u32(mem, header::OFF_ERR_CODE) as i32, host_status);
        assert_eq!(read_u32(mem, header::OFF_ERR_BAR), 0);
        assert_eq!(read_u32(mem, header::OFF_ERR_PC), first_site);
    }
}

/// An instruction that stops the run from inside a window: the history read
/// passes `max_bars_back` with the whole window already debited. The stop,
/// its bar and instruction, and every earlier bar's output are the
/// interpreter's, at every budget.
#[test]
fn a_chart_window_can_stop_the_run_from_its_middle() {
    let program = compile_csil(concat!(
        "//@version=6\nindicator(\"Chart fuel stop\", max_bars_back = 20)\n",
        "far = bar_index * 2\nplot(close * 2.0 + close[far] - open / 4.0)\n",
    ));
    let data = synthetic_data(16);
    let aot = compile_program(&program).unwrap();
    assert!(!chart_fuel_groups(&aot.wasm).is_empty());
    let stop = run(&program, &data, &RunOptions::default());
    assert_eq!(
        stop.errors.first().map(|error| error.kind),
        Some(pinewasm_vm::RuntimeErrorKind::HistoryBufferLimitExceeded),
        "the fixture must pass max_bars_back"
    );
    assert_eq!(
        chart_budget_cut_matches(&program, &aot, &data, u64::MAX, u64::MAX),
        Some(false)
    );
    let per_bar = program.main().code.len() as u64 + 1;
    for per_run in 0..=per_bar * data.bars() as u64 {
        chart_budget_cut_matches(&program, &aot, &data, per_bar, per_run);
    }
    for per_bar in 0..=per_bar {
        chart_budget_cut_matches(&program, &aot, &data, per_bar, u64::MAX);
    }
}

/// [`standalone_wasm_fuel_test_module`] for a program that may use the native
/// heap: the chart tier's module supplies the layout `run_aot` loads, so both
/// tiers must plan the same layout (checked here).
fn standalone_wasm_heap_fuel_test_module(program: &pinewasm_csil::CsilProgram) -> AotModule {
    let mut aot = compile_program(program).unwrap();
    assert!(!aot.layout.imports_vm);
    let (wasm, groups) = standalone_wasm_with_prepare_and_run(program);
    assert!(
        !groups.is_empty(),
        "the fixture must form standalone windows"
    );
    aot.wasm = wasm;
    aot
}

/// Standalone WebAssembly charges its budgets with the chart window kernel
/// (`body/chart_fuel.rs`): a window precharges the summed, weighted cost of
/// its instructions, may close with its block's branch unit, may hold native
/// heap reads, stores and pure calls, and may hold an instruction that stops
/// the run. Every per-bar and every run budget, cut anywhere inside a window,
/// must still stop where the interpreter stops, after the same effects.
#[test]
fn standalone_fuel_windows_preserve_every_budget_cut() {
    let sources = [
        // Weighted windows: each `ta.sma` charges its length; the ternary
        // closes windows with branch units.
        concat!(
            "//@version=6\nindicator(\"Standalone fuel weights\")\na = ta.sma(close, 5)\n",
            "b = ta.sma(open, 3)\nx = (a + b) * 2.0 - close / 3.0\n",
            "plot(close > open ? a + x * 0.0 : b)\n",
        ),
        // A loop of native array reads, pure calls and dynamic history
        // reads: the shape of a nearest-neighbour scan.
        concat!(
            "//@version=6\nindicator(\"Standalone fuel loop\")\nvar a = array.new_float(0)\n",
            "array.push(a, close)\ns = 0.0\nn = math.min(array.size(a), 4)\n",
            "for i = 0 to n - 1\n",
            "    s += math.log(1.0 + math.abs(array.get(a, array.size(a) - 1 - i) - close[i]))\n",
            "plot(s)\n",
        ),
    ];
    let data = synthetic_data(3);
    for source in sources {
        let program = compile_csil(source);
        let aot = standalone_wasm_heap_fuel_test_module(&program);
        let mut cuts = 0;
        for remaining in 0..=20_000_u64 {
            let mut finished = true;
            for (per_bar, per_run) in [
                (remaining, u64::MAX),
                (u64::MAX, remaining),
                (remaining, remaining),
            ] {
                let done = chart_budget_cut_matches(&program, &aot, &data, per_bar, per_run)
                    .expect("a standalone module never reroutes");
                finished &= done;
                cuts += usize::from(!done);
            }
            if finished {
                break;
            }
            assert!(remaining < 20_000, "the fixture never finishes");
        }
        assert!(cuts > 0);
    }
    // The weighted fixture's windows carry each average's whole length.
    let (_, groups) = standalone_wasm_with_prepare_and_run(&compile_csil(sources[0]));
    assert!(groups.iter().any(|&cost| cost >= 5), "{groups:?}");
}

/// An instruction that stops a standalone run from inside a window: the
/// history read passes `max_bars_back` with the whole window already
/// debited. The stop, its bar and instruction, and every earlier bar's output
/// are the interpreter's, at every budget.
#[test]
fn a_standalone_window_can_stop_the_run_from_its_middle() {
    let program = compile_csil(concat!(
        "//@version=6\nindicator(\"Standalone fuel stop\", max_bars_back = 20)\n",
        "far = bar_index * 2\nplot(close * 2.0 + close[far] - open / 4.0)\n",
    ));
    let data = synthetic_data(16);
    let aot = standalone_wasm_heap_fuel_test_module(&program);
    let stop = run(&program, &data, &RunOptions::default());
    assert_eq!(
        stop.errors.first().map(|error| error.kind),
        Some(pinewasm_vm::RuntimeErrorKind::HistoryBufferLimitExceeded),
        "the fixture must pass max_bars_back"
    );
    assert_eq!(
        chart_budget_cut_matches(&program, &aot, &data, u64::MAX, u64::MAX),
        Some(false)
    );
    let per_bar = program.main().code.len() as u64 + 1;
    for per_run in 0..=per_bar * data.bars() as u64 {
        chart_budget_cut_matches(&program, &aot, &data, per_bar, per_run);
    }
    for per_bar in 0..=per_bar {
        chart_budget_cut_matches(&program, &aot, &data, per_bar, u64::MAX);
    }
}

/// Every per-bar cut, every run cut and every equal pair of cuts, from zero
/// until none of the three changes the run any more, compared with the
/// interpreter by [`chart_budget_cut_matches`]. A cut has stopped mattering
/// once the run ends as it does without one: finished, or stopped by the
/// program itself at the same bar and instruction. Returns how many cuts
/// stopped the run.
fn sweep_standalone_budget_cuts(
    program: &pinewasm_csil::CsilProgram,
    aot: &AotModule,
    data: &MarketData,
    limit: u64,
) -> usize {
    let stop_of = |result: &pinewasm_vm::RunOutcome| {
        result
            .errors
            .first()
            .map(|error| (error.kind, error.bar_index, error.instruction_offset))
    };
    let free = stop_of(&run(program, data, &RunOptions::default()));
    let mut cuts = 0;
    for remaining in 0..=limit {
        let mut settled = true;
        for (per_bar, per_run) in [
            (remaining, u64::MAX),
            (u64::MAX, remaining),
            (remaining, remaining),
        ] {
            let done = chart_budget_cut_matches(program, aot, data, per_bar, per_run)
                .expect("a standalone module never reroutes");
            cuts += usize::from(!done);
            settled &= done || {
                let options = RunOptions {
                    max_instructions_per_bar: per_bar,
                    max_instructions_per_run: per_run,
                    ..RunOptions::default()
                };
                free.is_some() && stop_of(&run(program, data, &options)) == free
            };
        }
        if settled {
            return cuts;
        }
    }
    panic!("the fixture never settles within {limit} instructions");
}

/// Windows across bars and loops: `var` state carried from bar to bar, a loop
/// left by `break` and `continue`, history reads of the slots the loop
/// updates, and a run stopped from inside a loop body inside `bar_step`.
#[test]
fn standalone_fuel_windows_hold_across_var_state_loops_and_history() {
    let data = synthetic_data(5);
    let sources = [
        concat!(
            "//@version=6\nindicator(\"Standalone fuel state\", max_bars_back = 50)\n",
            "var float acc = 0.0\nvar float n = 0.0\nn += 1.0\nfloat s = 0.0\n",
            "for i = 0 to 6\n",
            "    if i == 2\n        continue\n",
            "    if i > n + 1.0\n        break\n",
            "    s += close[i] * 0.5 + nz(acc[1]) * 0.25\n",
            "acc := acc + s / 7.0\nplot(acc + nz(s[2]))\n",
        ),
        concat!(
            "//@version=6\nindicator(\"Standalone fuel while\")\n",
            "var float carry = 1.0\nfloat k = 0.0\nfloat t = 0.0\n",
            "while k < 5.0\n    k += 1.0\n",
            "    if k == 3.0\n        continue\n",
            "    t += math.abs(close - open) * k + carry\n",
            "    if t > 1.0e9\n        break\n",
            "carry := t / 10.0\nplot(t + nz(t[1]) - nz(carry[2]))\n",
        ),
    ];
    for source in sources {
        let program = compile_csil(source);
        let aot = standalone_wasm_heap_fuel_test_module(&program);
        assert!(sweep_standalone_budget_cuts(&program, &aot, &data, 20_000) > 0);
    }
}

/// A history read past `max_bars_back` inside a loop body stops the run from
/// the middle of a window inside `bar_step`, at every budget.
#[test]
fn a_standalone_window_can_stop_the_run_inside_a_loop() {
    let program = compile_csil(concat!(
        "//@version=6\nindicator(\"Standalone loop stop\", max_bars_back = 20)\n",
        "var float acc = 0.0\nfloat s = 0.0\n",
        "for i = 0 to 2\n    s += close[bar_index * 2 + i] * 2.0 - open / 4.0\n",
        "acc += s\nplot(acc)\n",
    ));
    let data = synthetic_data(16);
    let aot = standalone_wasm_heap_fuel_test_module(&program);
    let stop = run(&program, &data, &RunOptions::default());
    assert_eq!(
        stop.errors.first().map(|error| error.kind),
        Some(pinewasm_vm::RuntimeErrorKind::HistoryBufferLimitExceeded),
        "the fixture must pass max_bars_back inside the loop"
    );
    assert!(sweep_standalone_budget_cuts(&program, &aot, &data, 20_000) > 0);
}

/// A standalone module with more than 256 slots bounds its cached
/// current-bar cells, including in pieces. Every budget cut on such a
/// program, with a loop over hot cells, history reads of hot and cold cells
/// and `var` state, must still stop where the interpreter stops.
#[test]
fn a_large_slot_standalone_module_preserves_every_budget_cut() {
    use std::fmt::Write as _;
    const VARS: usize = 264;
    let mut source = String::from(
        "//@version=6\nindicator(\"Many slots\", max_bars_back = 10)\nvar float total = 0.0\n",
    );
    source.push_str("v0 = close * 1.0\n");
    for i in 1..VARS {
        // Each variable reads its predecessor's history, so every slot is
        // stored and read rather than forwarded away.
        let _ = writeln!(source, "v{i} = v{} - nz(v{}[1]) * 0.5", i - 1, i - 1);
    }
    source.push_str("float s = 0.0\nfor i = 0 to 3\n");
    source.push_str("    if i == 1\n        continue\n");
    source.push_str("    s += v3 * i + v7 - nz(v5[1]) + nz(v250[2])\n");
    source.push_str("total += s\n");
    let _ = writeln!(source, "plot(total + v{} + nz(v120[1]))", VARS - 1);
    let program = compile_csil(&source);
    assert!(
        program.series_slots.len() > 256,
        "{} slots: the fixture must take the hot-local and global placement",
        program.series_slots.len()
    );
    assert_large_slot_placement(&program);
    let data = synthetic_data(2);
    let aot = standalone_wasm_heap_fuel_test_module(&program);
    assert!(sweep_standalone_budget_cuts(&program, &aot, &data, 50_000) > 0);
}

/// The large-slot placement, read off the emitted module: an unsplit
/// `bar_step` caches exactly 64 cells; a split wrapper caches none and each
/// piece caches at most 64. Every uncached cell in an unsplit body, or every
/// cell in a split body, has a private global initialized by `bar_step`.
///
/// The current-cell globals are found by their declarations, not by an
/// offset. The emitter declares them as one contiguous run, each initialized
/// to its type's `na` (an f64 NaN, `i64::MIN`, bool3 `1`, or `-1` for a color,
/// string or reference), followed by exactly three mutable i64 globals
/// initialized to zero, plus a fourth per-bar handoff counter in a split
/// module. No other global declared just before the run carries an `na`
/// initializer, so the run's boundaries are exact.
fn assert_large_slot_placement(program: &pinewasm_csil::CsilProgram) {
    use wasmparser::{ConstExpr, GlobalType, Operator, Parser, Payload, ValType};
    let (wasm, _) = standalone_wasm_with_prepare_and_run(program);
    let mut bodies = Vec::new();
    let mut globals: Vec<(GlobalType, Vec<Operator>)> = Vec::new();
    for payload in Parser::new(0).parse_all(&wasm) {
        match payload.unwrap() {
            Payload::CodeSectionEntry(body) => bodies.push(body),
            Payload::GlobalSection(section) => {
                for global in section {
                    let global = global.unwrap();
                    let init: ConstExpr = global.init_expr;
                    let ops = init
                        .get_operators_reader()
                        .into_iter()
                        .map(Result::unwrap)
                        .collect();
                    globals.push((global.ty, ops));
                }
            }
            _ => {}
        }
    }
    // The re-exported module keeps the body order: evaluate is last.
    let bar_step = &bodies[bodies.len() - 2];
    let mut locals = 0;
    for group in bar_step.get_locals_reader().unwrap() {
        locals += group.unwrap().0;
    }
    let split = locals == 0;
    if split {
        let mut cached = 0;
        for body in &bodies[..bodies.len() - 5] {
            let ops = body
                .get_operators_reader()
                .unwrap()
                .into_iter()
                .map(Result::unwrap)
                .collect::<Vec<_>>();
            if !matches!(
                ops.as_slice(),
                [
                    Operator::GlobalGet { .. },
                    Operator::LocalSet { local_index: 1 },
                    Operator::GlobalGet { .. },
                    Operator::LocalSet { local_index: 2 },
                    ..
                ]
            ) {
                continue;
            }
            let cells = ops[4..]
                .chunks(2)
                .take_while(|pair| {
                    matches!(
                        pair,
                        [Operator::GlobalGet { .. }, Operator::LocalSet { .. }]
                    )
                })
                .count();
            assert!(cells <= 64, "a piece caches {cells} current cells");
            cached += cells;
        }
        assert!(cached > 0, "no piece caches any current cell");
    } else {
        let fixed = 14 + 4 * program.main().max_stack;
        assert_eq!(locals - fixed, 64, "hot current-cell locals of {locals}");
    }

    let zero_i64 = |(ty, init): &(GlobalType, Vec<Operator>)| {
        ty.mutable
            && ty.content_type == ValType::I64
            && matches!(
                init.as_slice(),
                [Operator::I64Const { value: 0 }, Operator::End]
            )
    };
    let na_cell = |(ty, init): &(GlobalType, Vec<Operator>)| {
        ty.mutable
            && match (ty.content_type, init.as_slice()) {
                (ValType::F64, [Operator::F64Const { value }, Operator::End]) => {
                    f64::from_bits(value.bits()).is_nan()
                }
                (ValType::I64, [Operator::I64Const { value }, Operator::End]) => *value == i64::MIN,
                (ValType::I32, [Operator::I32Const { value }, Operator::End]) => {
                    *value == 1 || *value == -1
                }
                _ => false,
            }
    };
    let total = globals.len();
    let counters = if split { 4 } else { 3 };
    assert!(
        globals[total - counters..].iter().all(zero_i64),
        "the run, fallback and optional handoff counters close the global section"
    );
    let end = total - counters;
    let mut start = end;
    while start > 0 && na_cell(&globals[start - 1]) {
        start -= 1;
    }
    let cold = program.series_slots.len() - if split { 0 } else { 64 };
    assert_eq!(end - start, cold, "one current-cell global per cold slot");

    let mut written = std::collections::BTreeSet::new();
    for op in bar_step.get_operators_reader().unwrap() {
        if let Operator::GlobalSet { global_index } = op.unwrap() {
            written.insert(global_index as usize);
        }
    }
    let unwritten: Vec<usize> = (start..end).filter(|g| !written.contains(g)).collect();
    assert!(
        unwritten.is_empty(),
        "bar_step never writes current-cell globals {unwritten:?}"
    );
}

// ---------------------------------------------------------------------------
// Chart-tier strategies: the broker is the VM host's
// ---------------------------------------------------------------------------

/// A chart-tier strategy compiles natively, its broker is the host's, and
/// the run's plots, alerts and strategy results are the interpreter's.
fn strategy_differential(
    source: &str,
    data: &MarketData,
    context: &str,
) -> pinewasm_vm::RunOutcome {
    let program = compile_csil(source);
    let aot = compile_program(&program)
        .unwrap_or_else(|e| panic!("{context}: strategy must lower natively: {e}"));
    assert!(aot.layout.host_broker, "{context}: host broker");
    assert!(!aot.layout.links_broker, "{context}: no linked broker");
    assert!(aot.layout.imports_vm, "{context}: orders are host sites");
    let outcome = differential_program(&program, data, context);
    assert!(outcome.strategy.is_some(), "{context}: a strategy outcome");
    outcome
}

/// An hourly random walk with gaps between bars. Every bar's high and low
/// bound its open and close, so the strategy runs natively throughout.
fn walk_data(n: usize, seed: u64) -> MarketData {
    let mut state = seed;
    let mut next = move || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((state >> 11) as f64) / ((1u64 << 53) as f64)
    };
    let mut data = MarketData::default();
    let mut price = 100.0;
    for i in 0..n {
        let open: f64 = price;
        let close = (open + (next() - 0.5) * 4.0).max(1.0);
        let high = open.max(close) + next() * 1.5;
        let low = (open.min(close) - next() * 1.5).max(0.5);
        data.open.push(open);
        data.high.push(high);
        data.low.push(low);
        data.close.push(close);
        data.volume.push(1000.0 + (next() * 500.0).floor());
        data.time.push(1_600_000_000_000 + i as i64 * 3_600_000);
        price = (close + (next() - 0.5) * 0.6).max(1.0);
    }
    data.timeframe = "60".to_string();
    data
}

const CHART_BRACKETS: &str = "//@version=6
strategy(\"Brackets\", initial_capital = 10000, default_qty_value = 2, pyramiding = 2)
basis = ta.sma(close, 8)
if close > basis and strategy.position_size == 0
    strategy.entry(\"L\", strategy.long, limit = close - 0.5)
if close < basis and strategy.position_size == 0
    strategy.entry(\"S\", strategy.short, stop = low - 0.25)
strategy.exit(\"XL\", \"L\", profit = 150, loss = 100, trail_points = 80, trail_offset = 20)
strategy.exit(\"XS\", \"S\", limit = close - 3, stop = close + 2)
plot(strategy.position_size)
plot(strategy.position_avg_price)
";

const CHART_GETTERS: &str = "//@version=6
strategy(\"Getters\", initial_capital = 5000, default_qty_type = strategy.percent_of_equity, default_qty_value = 20, commission_type = strategy.commission.percent, commission_value = 0.05, slippage = 2)
fast = ta.ema(close, 5)
slow = ta.ema(close, 15)
if ta.crossover(fast, slow)
    strategy.entry(\"L\", strategy.long)
if ta.crossunder(fast, slow)
    strategy.entry(\"S\", strategy.short)
plot(strategy.equity)
plot(strategy.openprofit)
plot(strategy.netprofit)
plot(strategy.grossprofit)
plot(strategy.grossloss)
plot(strategy.max_drawdown)
plot(strategy.max_runup)
plot(strategy.avg_trade)
plot(strategy.avg_winning_trade)
plot(strategy.avg_losing_trade)
plot(strategy.netprofit_percent)
plot(strategy.max_drawdown_percent)
plot(strategy.openprofit_percent)
plot(strategy.closedtrades)
plot(strategy.wintrades)
plot(strategy.losstrades)
plot(strategy.eventrades)
plot(strategy.opentrades)
plot(strategy.position_size[1])
plot(strategy.equity[3])
plot(strategy.max_contracts_held_all)
plot(strategy.margin_liquidation_price)
plot(strategy.opentrades.capital_held)
plot(strategy.closedtrades.first_index)
plot(str.length(strategy.position_entry_name))
plot(str.length(strategy.account_currency))
";

const CHART_TRADES: &str = "//@version=6
strategy(\"Trades\", initial_capital = 10000, pyramiding = 3, default_qty_value = 1)
if bar_index % 9 == 0
    strategy.entry(\"L\" + str.tostring(bar_index % 3), strategy.long)
if bar_index % 23 == 0
    strategy.close_all()
n = strategy.closedtrades
last = n > 0 ? n - 1 : 0
lastProfit = n > 0 ? strategy.closedtrades.profit(last) : na
lastEntry = n > 0 ? strategy.closedtrades.entry_price(last) : na
lastExitBar = n > 0 ? strategy.closedtrades.exit_bar_index(last) : na
lastRunup = n > 0 ? strategy.closedtrades.max_runup(last) : na
lastCommission = n > 0 ? strategy.closedtrades.commission(last) : na
lastId = n > 0 ? strategy.closedtrades.entry_id(last) : \"none\"
openBar = strategy.opentrades > 0 ? strategy.opentrades.entry_bar_index(0) : na
openProfit = strategy.opentrades > 0 ? strategy.opentrades.profit(0) : na
openSize = strategy.opentrades > 0 ? strategy.opentrades.size(0) : na
plot(lastProfit)
plot(lastEntry)
plot(lastExitBar)
plot(lastRunup)
plot(lastCommission)
plot(str.length(lastId))
plot(openBar)
plot(openProfit)
plot(openSize)
plot(strategy.closedtrades.exit_time(last))
";

const CHART_RISK: &str = "//@version=6
strategy(\"Risk\", initial_capital = 10000, default_qty_value = 3)
strategy.risk.allow_entry_in(strategy.direction.long)
strategy.risk.max_intraday_filled_orders(3)
strategy.risk.max_drawdown(25, strategy.percent_of_equity)
strategy.risk.max_cons_loss_days(4)
strategy.risk.max_position_size(6)
strategy.risk.max_intraday_loss(200, strategy.cash)
if close > open
    strategy.entry(\"L\", strategy.long)
if close < open
    strategy.entry(\"S\", strategy.short)
if bar_index % 5 == 0
    strategy.close(\"L\")
plot(strategy.position_size)
";

const CHART_ORDERS: &str = "//@version=6
strategy(\"Orders\", initial_capital = 20000, pyramiding = 3, close_entries_rule = \"ANY\")
if bar_index % 7 == 0
    strategy.order(\"Buy\", strategy.long, qty = 1)
if bar_index % 11 == 0
    strategy.order(\"Sell\", strategy.short, qty = 2, limit = close + 1, oca_name = \"g\", oca_type = strategy.oca.cancel)
    strategy.order(\"Sell2\", strategy.short, qty = 1, stop = close - 1, oca_name = \"g\", oca_type = strategy.oca.cancel)
if bar_index % 13 == 0
    strategy.cancel(\"Sell\")
if bar_index % 17 == 0
    strategy.cancel_all()
if bar_index % 29 == 0
    strategy.close_all(comment = \"flat\")
plot(strategy.position_size)
";

const CHART_ON_CLOSE: &str = "//@version=6
strategy(\"On close\", process_orders_on_close = true, initial_capital = 10000)
if close > open and strategy.position_size <= 0
    strategy.entry(\"L\", strategy.long)
if close < open and strategy.position_size > 0
    strategy.close(\"L\", immediately = true)
if bar_index % 10 == 0
    strategy.entry(\"S\", strategy.short, qty = 1)
strategy.exit(\"XS\", \"S\", profit = 50, loss = 50)
plot(strategy.position_size)
plot(strategy.equity)
";

const CHART_ALERTS: &str = "//@version=6
strategy(\"Alerts\", initial_capital = 10000)
fast = ta.ema(close, 4)
slow = ta.ema(close, 12)
if ta.crossover(fast, slow)
    alert(\"cross up \" + str.tostring(bar_index), alert.freq_once_per_bar)
    strategy.entry(\"L\", strategy.long, alert_message = \"enter \" + str.tostring(close))
if ta.crossunder(fast, slow)
    strategy.close(\"L\", alert_message = \"exit\")
    alert(\"cross down\", alert.freq_all)
plot(fast)
";

const CHART_COSTS: &str = "//@version=6
strategy(\"Costs\", initial_capital = 10000, default_qty_type = strategy.percent_of_equity, default_qty_value = 50, commission_type = strategy.commission.cash_per_contract, commission_value = 0.5, slippage = 3, margin_long = 50, margin_short = 50, backtest_fill_limits_assumption = 2)
qty = strategy.default_entry_qty(close)
if ta.rising(close, 2)
    strategy.entry(\"L\", strategy.long, limit = close - 0.2)
if ta.falling(close, 2)
    strategy.entry(\"S\", strategy.short)
plot(qty)
plot(strategy.position_size)
plot(strategy.convert_to_account(close))
plot(strategy.convert_to_symbol(100))
";

/// Orders decided by a rolling window kept in an array: the native heap
/// beside a host broker.
const CHART_ARRAY_SIGNAL: &str = "//@version=6
strategy(\"Array signal\", initial_capital = 10000)
var float[] window = array.new_float(0)
array.push(window, close)
if array.size(window) > 8
    array.shift(window)
mean = array.avg(window)
if close > mean
    strategy.entry(\"L\", strategy.long)
if close < mean
    strategy.close(\"L\")
plot(mean)
";

/// Display code, built comments and alert messages around the orders.
const CHART_DISPLAY: &str = "//@version=6
strategy(\"Display\", initial_capital = 10000)
var float[] window = array.new_float(0)
array.push(window, close)
if array.size(window) > 6
    array.shift(window)
mean = array.avg(window)
goLong = close > mean
goFlat = close < mean
var table stats = table.new(position.top_right, 1, 1)
if barstate.islast
    table.cell(stats, 0, 0, str.format(\"{0}\", strategy.netprofit))
if goLong
    label.new(bar_index, low, \"Buy \" + str.tostring(close))
reason = goFlat ? \"Below\" : \"Above\"
if goLong
    strategy.entry(\"L\", strategy.long, comment = reason, alert_message = \"Open | \" + syminfo.ticker + \"@\" + str.tostring(close) + \" (\" + timeframe.period + \")\")
if goFlat
    strategy.close(\"L\", comment = \"Exit \" + reason, alert_message = \"Close | \" + syminfo.ticker)
plot(mean, \"Mean\")
";

const CHART_SECURITY: &str = "//@version=6
strategy(\"Security\", initial_capital = 10000)
trend = request.security(syminfo.tickerid, \"1D\", ta.ema(close, 4))
if not na(trend) and close > trend
    strategy.entry(\"L\", strategy.long)
if not na(trend) and close < trend
    strategy.close(\"L\")
plot(close)
";

const CHART_STRATEGIES: &[(&str, &str)] = &[
    ("brackets", CHART_BRACKETS),
    ("getters", CHART_GETTERS),
    ("trades", CHART_TRADES),
    ("risk", CHART_RISK),
    ("orders", CHART_ORDERS),
    ("on close", CHART_ON_CLOSE),
    ("alerts", CHART_ALERTS),
    ("costs", CHART_COSTS),
    ("array signal", CHART_ARRAY_SIGNAL),
    ("display", CHART_DISPLAY),
    ("security", CHART_SECURITY),
    (
        "compile-pass demo",
        include_str!("../../../tests/compile-pass/strategy_demo.pine"),
    ),
    (
        "compile-pass orders",
        include_str!("../../../tests/compile-pass/strategy_orders.pine"),
    ),
];

#[test]
fn chart_strategies_match_interpreter_on_random_walks() {
    for (name, source) in CHART_STRATEGIES {
        for seed in [1, 7, 42] {
            let data = walk_data(600, seed);
            let outcome = strategy_differential(source, &data, &format!("{name}, seed {seed}"));
            if seed == 1 {
                let strategy = outcome.strategy.as_ref().expect("strategy outcome");
                assert!(
                    !strategy.trades.is_empty() || strategy.position.size != 0.0,
                    "{name}: the fixture must trade"
                );
            }
        }
    }
}

#[test]
fn chart_strategy_market_entries_and_closes_match_interpreter() {
    let source = "//@version=6\nstrategy(\"Cross\", overlay=true)\n\
fast = ta.sma(close, 5)\nslow = ta.sma(close, 13)\n\
if ta.crossover(fast, slow)\n    strategy.entry(\"L\", strategy.long)\n\
if ta.crossunder(fast, slow)\n    strategy.close(\"L\")\n\
plot(fast)\nplot(slow)\n";
    let outcome = strategy_differential(source, &synthetic_data(400), "cross");
    let trades = &outcome.strategy.as_ref().unwrap().trades;
    assert!(!trades.is_empty(), "fixture must trade");
}

#[test]
fn chart_strategy_heap_runs_natively_beside_the_host_broker() {
    let aot = compile_program(&compile_csil(CHART_ARRAY_SIGNAL)).expect("lowers natively");
    assert!(aot.layout.native_heap, "the array lives on the native heap");
    assert!(aot.layout.host_broker);
}

/// Run a chart-tier strategy natively and on the interpreter under
/// `options` and require the interpreter's verdict, strategy results
/// included: the same structured error and bar when the run stops, the same
/// output for every bar before it, the same alerts and the same strategy
/// results.
fn strategy_run_matches(
    program: &pinewasm_csil::CsilProgram,
    data: &MarketData,
    options: &RunOptions,
    context: &str,
) -> pinewasm_vm::RunOutcome {
    let aot = compile_program(program)
        .unwrap_or_else(|e| panic!("{context}: strategy must lower natively: {e}"));
    assert!(aot.layout.host_broker, "{context}: host broker");
    let expected = run(program, data, options);
    let actual = run_aot(&aot, program, data, options);
    assert_eq!(actual.mismatch, None, "{context}: rerouted");
    assert_alerts_match(&expected, &actual, program, context);
    assert_strategy_match(&expected, &actual, context);
    match expected.errors.first() {
        Some(error) => {
            assert_eq!(
                actual.status,
                pinewasm_vm::aot_host::vm_status::HOST_ERROR,
                "{context}"
            );
            let host = actual
                .host_error
                .as_ref()
                .unwrap_or_else(|| panic!("{context}: HOST_ERROR without an error"));
            assert_eq!(host.kind, error.kind, "{context}");
            assert_eq!(host.message, error.message, "{context}");
            assert_eq!(host.bar_index, error.bar_index, "{context}");
            assert_eq!(
                host.instruction_offset, error.instruction_offset,
                "{context}"
            );
            assert_eq!(expected.bars_executed, actual.err_bar, "{context}");
            assert_outcomes_match(&expected, &actual, expected.bars_executed as usize, context);
        }
        None => {
            assert_eq!(
                actual.status,
                status::OK,
                "{context}: {:?}",
                actual.host_error
            );
            let bars = options.max_bars.map_or(data.bars(), |m| m.min(data.bars()));
            assert_outcomes_match(&expected, &actual, bars, context);
        }
    }
    expected
}

const CHART_STOPS: &str = "//@version=6
strategy(\"Stops mid-run\", overlay=true)
fast = ta.sma(close, 4)
slow = ta.sma(close, 11)
if ta.crossover(fast, slow)
    strategy.entry(\"L\", strategy.long)
if ta.crossunder(fast, slow)
    strategy.entry(\"S\", strategy.short)
if bar_index == 377
    runtime.error(\"stopped at \" + str.tostring(bar_index))
plot(strategy.equity)
";

/// A run a runtime error stops keeps the interpreter's strategy results:
/// the error bar's order phase ran, its script did not finish, and the
/// equity and trades end where the interpreter's do.
#[test]
fn a_chart_strategy_stopped_by_an_error_keeps_the_interpreter_results() {
    let program = compile_csil(CHART_STOPS);
    let outcome = strategy_run_matches(
        &program,
        &walk_data(600, 5),
        &RunOptions::default(),
        "stopped at 377",
    );
    assert_eq!(outcome.bars_executed, 377);
    let strategy = outcome.strategy.as_ref().expect("strategy results");
    assert!(
        !strategy.trades.is_empty() || strategy.position.size != 0.0,
        "the fixture must trade before it stops"
    );
}

/// `max_bars` cuts the feed for both engines; the host's broker closes the
/// last bar the run reached, not the feed's.
#[test]
fn a_chart_strategy_honors_max_bars() {
    let program = compile_csil(CHART_ORDERS);
    for max_bars in [0, 1, 137, 600] {
        let options = RunOptions {
            max_bars: Some(max_bars),
            ..RunOptions::default()
        };
        strategy_run_matches(
            &program,
            &walk_data(400, 9),
            &options,
            &format!("max_bars {max_bars}"),
        );
    }
}

/// Strategy properties a run overrides reach the host's broker: capital,
/// sizing, costs, pyramiding, fill assumptions and the close rule.
#[test]
fn a_chart_strategy_takes_run_overrides_of_its_properties() {
    let program = compile_csil(CHART_COSTS);
    let overrides = pinewasm_vm::StrategyRunOverrides {
        initial_capital: Some(25_000.0),
        default_qty_type: Some("percent_of_equity".to_string()),
        default_qty_value: Some(12.5),
        commission_type: Some("cash_per_order".to_string()),
        commission_value: Some(3.0),
        slippage: Some(2),
        pyramiding: Some(3),
        backtest_fill_limits_assumption: Some(1),
        process_orders_on_close: Some(true),
        close_entries_rule: Some("ANY".to_string()),
        ..pinewasm_vm::StrategyRunOverrides::default()
    };
    let options = RunOptions {
        strategy: overrides,
        ..RunOptions::default()
    };
    let outcome = strategy_run_matches(&program, &walk_data(500, 21), &options, "overrides");
    assert_eq!(
        outcome
            .strategy
            .as_ref()
            .expect("strategy results")
            .initial_capital,
        25_000.0
    );
}

const CHART_FX: &str = "//@version=6
strategy(\"FX account\", overlay=true, initial_capital=50000, currency=\"EUR\", default_qty_type=strategy.cash, default_qty_value=5000)
if bar_index % 23 == 0
    strategy.entry(\"L\", strategy.long)
if bar_index % 23 == 11
    strategy.close(\"L\")
plot(strategy.convert_to_account(close))
plot(strategy.convert_to_symbol(100.0))
plot(strategy.netprofit)
";

/// An account currency other than the symbol's: the host's broker reads
/// the same `currency_rate/<symbol>/<account>` table the interpreter reads,
/// bar by bar, and an absent table behaves as on the interpreter.
#[test]
fn a_chart_strategy_converts_currencies_like_the_interpreter() {
    let program = compile_csil(CHART_FX);
    let mut data = walk_data(300, 31);
    data.syminfo.currency = "USD".to_string();
    strategy_run_matches(
        &program,
        &data,
        &RunOptions::default(),
        "fx without a table",
    );
    let rates = (0..data.bars())
        .map(|bar| match bar % 50 {
            7 => f64::NAN,
            13 => 0.0,
            _ => 0.9 + (bar as f64) * 0.0004,
        })
        .collect();
    data.request_data
        .insert("currency_rate/USD/EUR".to_string(), rates);
    let outcome = strategy_run_matches(&program, &data, &RunOptions::default(), "fx");
    let strategy = outcome.strategy.as_ref().expect("strategy results");
    assert!(strategy.currency_rates.iter().any(Option::is_some));
}

/// The supported-language strategy fixture runs natively beside the host's
/// broker and matches the interpreter.
#[test]
fn the_strategy_regression_fixture_matches_the_interpreter() {
    let program = compile_csil(include_str!(
        "../../../tests/regression/language-strategy.pine"
    ));
    assert_eq!(
        compile_program(&program).unwrap_err(),
        AotError::UnsupportedOpcode("color input provenance".into())
    );
    let artifact = compile_program_to_wasm(&program).expect("strategy remains executable");
    assert_eq!(
        artifact.layout.execution_mode,
        ExecutionMode::InterpreterHosted
    );
    for seed in [2, 17] {
        let data = walk_data(500, seed);
        let options = RunOptions::default();
        let expected = run(&program, &data, &options);
        let actual = execute_hosted_wrapper(&artifact, &program, &data, &options);
        assert_eq!(actual, expected, "language-strategy seed {seed}");
    }
}

/// Preserve native broker coverage separately from the strategy regression fixture's
/// input-color surface, which requires the exact-color interpreter wrapper.
#[test]
fn the_strategy_regression_with_fixed_colors_remains_native() {
    let source = include_str!("../../../tests/regression/language-strategy.pine");
    let mut fixed = source.to_string();
    for (input, default) in [
        ("strategy_request_input_tint = input.color(color.new(color.blue, 90), \"Tint\", group = \"Text\")", "strategy_request_input_tint = color.new(color.blue, 90)"),
        ("strategy_request_input_gcol = input(color.red, \"Generic Color\", group = \"Generic\")", "strategy_request_input_gcol = color.red"),
    ] {
        assert_eq!(fixed.matches(input).count(), 1);
        fixed = fixed.replace(input, default);
    }
    let original = compile_csil(source);
    let program = compile_csil(&fixed);
    for seed in [2, 17] {
        let data = walk_data(500, seed);
        let options = RunOptions::default();
        let expected = strategy_run_matches(
            &program,
            &data,
            &options,
            &format!("language-strategy fixed-color seed {seed}"),
        );
        let original_default = run(&original, &data, &options);
        assert_eq!(
            expected.strategy, original_default.strategy,
            "broker defaults: {seed}"
        );
        assert_eq!(
            expected.plots, original_default.plots,
            "plotted defaults: {seed}"
        );
    }
}

/// A host reset between runs rebuilds the broker: the second run's strategy
/// results equal the first's, not a continuation of it.
#[test]
fn a_reset_host_starts_the_broker_over() {
    let program = compile_csil(CHART_BRACKETS);
    let aot = compile_program(&program).expect("lowers natively");
    let data = walk_data(400, 3);
    let options = RunOptions::default();
    let vm = fresh_vm_host(&aot, &program, &data, &options);
    let (first, vm) = run_aot_with_host(&aot, &program, &data, &options, vm);
    let mut vm = vm.expect("a host-broker module imports host.vm");
    vm.reset();
    let (second, _) = run_aot_with_host(&aot, &program, &data, &options, Some(vm));
    assert_eq!(first.status, status::OK);
    assert_eq!(second.status, status::OK);
    assert!(first.strategy.is_some());
    assert_eq!(
        format!("{:?}", first.strategy),
        format!("{:?}", second.strategy)
    );
    assert_eq!(first.vm_alerts, second.vm_alerts);
    let expected = run(&program, &data, &options);
    assert_strategy_match(&expected, &second, "after reset");
}

/// Requests the host's broker does not reproduce stop the native run at
/// its first host call with an ABI mismatch naming the reason, so the
/// loader reruns them on the interpreter; draining the strategy refuses
/// with the same reason.
#[test]
fn chart_strategies_hand_unreproducible_requests_to_the_interpreter() {
    let program = compile_csil(CHART_GETTERS);
    let aot = compile_program(&program).expect("lowers natively");
    assert!(aot.layout.host_broker);
    let refused = |data: &MarketData, options: &RunOptions, why: &str| {
        let actual = run_aot(&aot, &program, data, options);
        assert_eq!(
            actual.status,
            pinewasm_vm::aot_host::vm_status::ABI_MISMATCH,
            "{why}"
        );
        let reason = actual.mismatch.unwrap_or_default();
        assert!(
            reason.contains("strategy() needs the interpreter"),
            "{why}: {reason}"
        );
        assert!(actual.strategy.is_none(), "{why}");
        let mut vm = fresh_vm_host(&aot, &program, data, options).expect("host");
        let drained = vm
            .drain_strategy(pinewasm_vm::aot_host::vm_status::ABI_MISMATCH, 0)
            .expect_err("drain refuses");
        assert!(reason.ends_with(&drained), "{why}: {reason} / {drained}");
    };
    let data = walk_data(200, 4);
    refused(
        &data,
        &RunOptions {
            strategy: pinewasm_vm::StrategyRunOverrides {
                calc_on_order_fills: Some(true),
                ..pinewasm_vm::StrategyRunOverrides::default()
            },
            ..RunOptions::default()
        },
        "calc_on_order_fills override",
    );
    refused(
        &data,
        &RunOptions {
            strategy: pinewasm_vm::StrategyRunOverrides {
                calc_on_every_history_tick: Some(true),
                ..pinewasm_vm::StrategyRunOverrides::default()
            },
            ..RunOptions::default()
        },
        "calc_on_every_history_tick override",
    );
    let mut unordered = data.clone();
    unordered.high[150] = unordered.low[150] - 1.0;
    refused(&unordered, &RunOptions::default(), "high below low");
    let mut missing = data.clone();
    missing.high[42] = f64::NAN;
    refused(&missing, &RunOptions::default(), "na high");
    // A missing open or close leaves the closing tick's high and low as
    // supplied, so those bars stay native.
    let mut gaps = data.clone();
    gaps.open[42] = f64::NAN;
    gaps.close[97] = f64::NAN;
    strategy_run_matches(&program, &gaps, &RunOptions::default(), "na open and close");
}

/// A host checks the bars it was built for, not the whole feed: a bar its
/// closing tick would not show as supplied refuses a run that executes it,
/// and one past the host's bar count changes nothing.
#[test]
fn chart_strategy_refusals_follow_the_hosts_bars() {
    let program = compile_csil(CHART_GETTERS);
    let aot = compile_program(&program).expect("lowers natively");
    let mut data = walk_data(400, 4);
    data.high[300] = data.low[300] - 1.0;
    let options = RunOptions::default();
    let built_for = |bars: usize| vm_host_for_bars(&aot, &program, &data, &options, bars);
    assert_eq!(built_for(300).strategy_refusal(), None);
    for bars in [301, 400] {
        let mut vm = built_for(bars);
        let why = vm.strategy_refusal().expect("the run executes bar 300");
        assert!(why.contains("high or low"), "{bars} bars: {why}");
        assert_eq!(
            vm.drain_strategy(status::OK, bars as u32),
            Err(why.to_string()),
            "{bars} bars"
        );
    }
    // A host built for the first 300 bars follows a native run over them,
    // and the results are the interpreter's for a request capped there.
    let capped = RunOptions {
        max_bars: Some(300),
        ..RunOptions::default()
    };
    let (actual, _) = run_aot_with_host(&aot, &program, &data, &capped, Some(built_for(300)));
    assert_eq!(actual.status, status::OK, "{:?}", actual.mismatch);
    let expected = run(&program, &data, &capped);
    assert!(expected.strategy.is_some());
    assert_strategy_match(&expected, &actual, "host built for 300 of 400 bars");
}

/// Market data the interpreter rejects (here, a price lane or the time lane
/// shorter than the bars) is reported there as malformed. A strategy host
/// over it refuses, so the run goes there, rather than reading past a lane
/// while it is built or while its broker catches up.
#[test]
fn a_chart_strategy_host_refuses_malformed_market_data() {
    let program = compile_csil(CHART_GETTERS);
    let aot = compile_program(&program).expect("lowers natively");
    let options = RunOptions::default();
    let mut short_high = walk_data(200, 4);
    short_high.high.truncate(120);
    let mut short_time = walk_data(200, 4);
    short_time.time.truncate(120);
    for (data, lane) in [(short_high, "high"), (short_time, "time")] {
        let mut vm = vm_host_for_bars(&aot, &program, &data, &options, 200);
        let why = vm.strategy_refusal().expect(lane);
        assert_eq!(
            vm.drain_strategy(status::OK, 200),
            Err(why.to_string()),
            "{lane}"
        );
        let mut cells = vec![0i64; aot.layout.scratch_cells as usize];
        assert_eq!(
            vm.vm_call(0, 0, &mut cells),
            pinewasm_vm::aot_host::vm_status::ABI_MISMATCH,
            "{lane}"
        );
        let expected = run(&program, &data, &options);
        assert!(
            matches!(
                expected.errors.first().map(|error| &error.kind),
                Some(pinewasm_vm::RuntimeErrorKind::MalformedMarketData)
            ),
            "{lane}: {:?}",
            expected.errors
        );
    }
}

const CHART_TA_INPUT: &str = "//@version=6
strategy(\"TA input\", overlay=true)
n = input.int(5, \"Length\")
mean = ta.sma(close, n)
if ta.crossover(close, mean)
    strategy.entry(\"L\", strategy.long)
if ta.crossunder(close, mean)
    strategy.close(\"L\")
plot(mean)
";

/// An input-backed TA length outside its legal range stops both engines
/// before the first bar. The interpreter's outcome then has no strategy
/// results, and neither does the drain, though a run stopped on its first
/// bar also reports zero executed bars.
#[test]
fn a_chart_strategy_stopped_before_its_first_bar_has_no_strategy_results() {
    use pinewasm_vm::aot_host::pre_bar_status;
    assert_eq!(pre_bar_status::TA_INIT_FAILED, status::TA_INIT_FAILED);
    assert_eq!(pre_bar_status::INVALID_TA_LENGTH, status::INVALID_TA_LENGTH);

    let program = compile_csil(CHART_TA_INPUT);
    let aot = compile_program(&program).expect("lowers natively");
    assert!(aot.layout.host_broker);
    let data = walk_data(200, 6);
    let options = RunOptions {
        inputs: std::collections::HashMap::from([(0, InputValue::Int(0))]),
        ..RunOptions::default()
    };
    let expected = run(&program, &data, &options);
    assert_eq!(
        expected.errors.first().map(|error| error.kind),
        Some(pinewasm_vm::RuntimeErrorKind::InvalidTaLength)
    );
    assert!(expected.strategy.is_none());
    let actual = run_aot(&aot, &program, &data, &options);
    assert_eq!(actual.status, status::INVALID_TA_LENGTH);
    assert_strategy_match(&expected, &actual, "TA length 0");
    assert_eq!(actual.vm_alerts.as_deref(), Some(&[][..]));

    // The same zero bars after an error on the first bar keep that bar's
    // broker state, as the interpreter's outcome does.
    let mut vm = fresh_vm_host(&aot, &program, &data, &RunOptions::default()).expect("host");
    let drained = vm
        .drain_strategy(pinewasm_vm::aot_host::vm_status::HOST_ERROR, 0)
        .expect("drains");
    assert!(drained.is_some());
    strategy_run_matches(&program, &data, &RunOptions::default(), "TA length 5");
}

/// Standalone compilation has no host, so a strategy has no broker there.
#[test]
fn standalone_compilation_refuses_strategies() {
    let program = compile_csil(CHART_ORDERS);
    assert!(matches!(
        pinewasm_aot::compile_standalone_program(&program),
        Err(AotError::StrategyRequiresInterpreter)
    ));
}

#[test]
fn universal_wasm_preserves_equal_paint_palette_entry_selection() {
    for (sources, refusal) in [
        (
            "a=input.color(#ff0000, \"A\")\nb=input.color(#ff0000, \"B\")",
            "color input provenance",
        ),
        (
            "a=color.new(#ff0000,50.01)\nb=color.new(#ff0000,50.02)",
            "fractional color transparency",
        ),
    ] {
        let source = format!("//@version=6\nindicator(\"Palette portable identity\")\n{sources}\nc=bar_index%2==0?a:b\np=plot(close,color=c)\nq=plot(open,color=color.blue)\nplot(close,color=c[1])\nbgcolor(c)\nbarcolor(b)\nfill(p,q,color=c)\nplotcandle(open,high,low,close,color=c,wickcolor=a,bordercolor=b)\n");
        let program = compile_csil(&source);
        assert_eq!(
            compile_program(&program).unwrap_err(),
            AotError::UnsupportedOpcode(refusal.into())
        );
        let artifact = compile_program_to_wasm(&program).unwrap();
        assert_eq!(
            artifact.layout.execution_mode,
            ExecutionMode::InterpreterHosted
        );
        wasmparser::validate(&artifact.wasm).unwrap();
        for bars in [0, 1, 17, 1025] {
            let data = synthetic_data(bars);
            let options = RunOptions::default();
            let expected = run(&program, &data, &options);
            let actual = execute_hosted_wrapper(&artifact, &program, &data, &options);
            assert!(actual.errors.is_empty(), "{:?}", actual.errors);
            assert_eq!(actual, expected);
            assert_eq!(
                serde_json::to_value(&actual).unwrap(),
                serde_json::to_value(&expected).unwrap()
            );
            assert_eq!(
                actual.plots[0].colors[0].palette_indices,
                (0..bars)
                    .map(|bar| Some((bar % 2) as u32))
                    .collect::<Vec<_>>()
            );
        }
    }
}

#[test]
fn residual_heap_keeps_array_from_color_result_cells() {
    let source = r#"//@version=6
indicator("Residual palette")
a = array.from(color.red, color.blue)
b = array.copy(a)
plot(na(b.get(0)) ? 0 : 1)
"#;
    let program = compile_csil(source);
    let module = compile_program_to_wasm(&program).unwrap();
    assert_eq!(module.layout.execution_mode, ExecutionMode::Native);
    let data = synthetic_data(3);
    let options = RunOptions::default();
    let reference = run(&program, &data, &options);
    let actual = run_aot(&module, &program, &data, &options);
    assert_eq!(actual.status, status::OK);
    assert_outcomes_match(&reference, &actual, 3, "residual color array");
}

#[test]
fn residual_object_arrays_preserve_reference_field_result_cells() {
    let source = r#"//@version=6
indicator("Object array fields")
type Holder
    array<int> values
objects = array.new<Holder>()
objects.push(Holder.new(array.from(7, 9)))
item = objects.get(0)
plot(na(item.values) ? 0 : item.values.get(1))
"#;
    let program = compile_csil(source);
    let module = compile_program_to_wasm(&program).unwrap();
    assert_eq!(module.layout.execution_mode, ExecutionMode::Native);
    let data = synthetic_data(3);
    let options = RunOptions::default();
    let reference = run(&program, &data, &options);
    let actual = run_aot(&module, &program, &data, &options);
    assert_eq!(actual.status, status::OK);
    assert_outcomes_match(&reference, &actual, 3, "residual object array fields");
}

#[test]
fn residual_bool_fields_keep_their_lane_at_generic_host_calls() {
    let source = r#"//@version=6
indicator("Boolean field")
type Holder
    bool ready
h = Holder.new()
h.ready := false
copy = Holder.new()
copy.ready := h.ready
extra = array.copy(array.from(1, 2))
plot(str.length(str.tostring(copy.ready)) + extra.get(0))
"#;
    let program = compile_csil(source);
    let module = compile_program_to_wasm(&program).unwrap();
    let data = synthetic_data(3);
    let options = RunOptions::default();
    let reference = run(&program, &data, &options);
    let actual = run_aot(&module, &program, &data, &options);
    assert_eq!(actual.status, status::OK);
    assert_outcomes_match(&reference, &actual, 3, "residual boolean field");
}

#[test]
fn residual_field_copies_refine_after_array_element_lanes() {
    let source = r#"//@version=6
indicator("Copied integer field")
type Holder
    int limit = 100
values = array.copy(array.from(18, 32))
h = Holder.new()
h.limit := values.get(0)
copy = Holder.new()
copy.limit := h.limit
plot(copy.limit * 128)
"#;
    let program = compile_csil(source);
    let module = compile_program_to_wasm(&program).unwrap();
    assert!(!module.layout.native_heap);
    let data = synthetic_data(3);
    let options = RunOptions::default();
    let reference = run(&program, &data, &options);
    let actual = run_aot(&module, &program, &data, &options);
    assert_eq!(actual.status, status::OK);
    assert_outcomes_match(&reference, &actual, 3, "copied integer field");
}

/// `plotbar`/`plotcandle` colors with a side effect are evaluated once per bar
/// into a hidden slot that all four OHLC channels read (semantic capture). The
/// compiled chart module must run the captured slots exactly as the VM does:
/// collection state, an engine `ta.*` state, a seeded `math.random` and a
/// named color written before `open` (it still runs after `open`, in
/// parameter order).
#[test]
fn captured_ohlc_colors_match_the_interpreter() {
    let source = r#"//@version=6
indicator("Captured OHLC colors", overlay=true)
var a = array.from(1.0,2.0,3.0,4.0,5.0,6.0,7.0,8.0,9.0,10.0,11.0,12.0,13.0,14.0,15.0,16.0)
var c = array.from(#000001,#000002,#000003,#000004,#000005,#000006,#000007,#000008,#000009,#00000a,#00000b,#00000c,#00000d,#00000e,#00000f,#000010)
plotbar(color=color.rgb(0,0,array.size(a)), open=array.shift(a), high=high, low=low, close=close)
plotcandle(open,high,low,close,color=c.shift(),wickcolor=color.rgb(0,0,ta.cum(1)),bordercolor=color.rgb(math.random(0,255,7),0,0))
plot(array.size(a))
plot(c.size())
"#;
    let program = compile_csil(source);
    let module = compile_program_to_wasm(&program).unwrap();
    assert_eq!(module.layout.execution_mode, ExecutionMode::Native);
    for bars in [0, 1, 3, 12] {
        let data = synthetic_data(bars);
        let options = RunOptions::default();
        let reference = run(&program, &data, &options);
        assert!(reference.errors.is_empty(), "{:?}", reference.errors);
        // The interpreter's run itself carries one color per bar on all four channels.
        for (first, role) in [(0, 0), (4, 0), (4, 1), (4, 2)] {
            for channel in first + 1..first + 4 {
                assert_eq!(
                    reference.plots[channel].colors[role].values,
                    reference.plots[first].colors[role].values,
                    "interpreter channel {channel} role {role}"
                );
            }
        }
        let actual = run_aot(&module, &program, &data, &options);
        assert_eq!(actual.status, status::OK);
        assert_outcomes_match(&reference, &actual, bars, "captured OHLC colors");
    }
}

/// Call arguments run in bound parameter order, including the hoisted body of
/// an inlined user function: an earlier argument's inline side effect (an
/// `array.pop`) is captured into a hidden slot before a later argument's user
/// function body runs. The compiled chart module must run those captures
/// exactly as the VM does, for every display call and drawing constructor.
#[test]
fn call_arguments_with_hoisted_user_functions_match_the_interpreter() {
    let source = r#"//@version=6
indicator("Argument order", overlay=true)
var a = array.from(1.0, 2.0, 3.0)
var t = table.new(position.top_right, 1, 1)
var b = array.new_float(3, 0.0)
f_color() =>
    array.push(a, 100.0 + bar_index)
    #0000ff
f_num() =>
    array.push(a, 100.0 + bar_index)
    0.0
f_int() =>
    array.push(a, 100.0 + bar_index)
    0
plot(array.pop(a), "plot", color = f_color())
plot(color = f_color(), title = "named", series = array.pop(a))
plot(close, "color", color = color.rgb(0, 0, array.pop(a)), linewidth = f_int() + 1)
plot(array.pop(a) + f_num(), "binary")
plot(math.max(array.pop(a), f_num()), "pure")
plot(ta.sma(array.pop(a) + f_num(), 1), "ta")
plot(ta.sma(array.pop(a), f_int() + 1), "ta length")
plot(ta.highest(array.pop(a), f_int() + 2), "ta window")
plot(ta.crossover(array.pop(a), f_num()) ? 1 : 0, "ta pair")
plotshape(array.pop(a) > 0, color = f_color())
plotchar(array.pop(a), "char", color = f_color())
plotcandle(array.pop(a), high, low, close, color = f_color())
plotbar(array.pop(a), high, low, close, color = f_color())
plotcandle(open, high, low, close, color = color.rgb(0, 0, array.pop(a)), wickcolor = f_color())
p1 = plot(high)
p2 = plot(low)
fill(p1, p2, array.pop(a), 0.0, color.red, f_color())
bgcolor(color.rgb(0, 0, array.pop(a)), offset = f_int())
barcolor(color.rgb(0, 0, array.pop(a)), offset = f_int())
table.cell(t, 0, 0, str.tostring(array.pop(a)), text_color = f_color())
label.new(bar_index, array.pop(a), color = f_color())
line.new(bar_index, array.pop(a), bar_index + 1, 0.0, color = f_color())
box.new(bar_index, array.pop(a), bar_index + 1, 0.0, border_color = f_color())
l2 = line.new(bar_index, 0.0, bar_index + 1, 0.0)
linefill.new(line.new(bar_index, array.pop(a), bar_index + 1, 1.0), l2, f_color())
array.fill(b, array.pop(a), f_int())
plot(array.get(b, 0), "collection")
plot(array.get(a, 2), "tail")
"#;
    let program = compile_csil(source);
    let module = compile_program_to_wasm(&program).unwrap();
    for bars in [0, 1, 3, 12] {
        let data = synthetic_data(bars);
        let options = RunOptions::default();
        let reference = run(&program, &data, &options);
        assert!(reference.errors.is_empty(), "{:?}", reference.errors);
        if bars > 0 {
            // Every call pops before its user function pushes, so the last
            // element of `a` is the value pushed on this bar.
            let tail = reference
                .plots
                .iter()
                .find(|plot| plot.title.as_deref() == Some("tail"))
                .expect("tail plot");
            assert_eq!(
                tail.values[0],
                Value::Float(100.0),
                "reference parameter order"
            );
        }
        let actual = run_aot(&module, &program, &data, &options);
        assert_eq!(actual.status, status::OK);
        assert_outcomes_match(&reference, &actual, bars, "call argument order");
    }
}

#[test]
fn destructured_ta_tuple_arguments_run_in_parameter_order_in_vm_and_wasm() {
    // A tuple `ta.*` call on the right of a destructuring declaration pops
    // its source before the user function in a later argument pushes, in
    // the interpreter and the compiled module, so the last element of `a` is
    // the value pushed on this bar.
    for body in [
        "f_mult() =>\n    array.push(a, 100.0 + bar_index)\n    1.0\n[v, up, lo] = ta.vwap(array.pop(a), true, f_mult())\nplot(v, \"value\")",
        "f_anchor() =>\n    array.push(a, 100.0 + bar_index)\n    true\n[v, up, lo] = ta.vwap(array.pop(a), f_anchor(), 1.0)\nplot(v, \"value\")",
        "f_anchor() =>\n    array.push(a, 100.0 + bar_index)\n    true\n[v, up, lo] = (ta.vwap(array.pop(a), f_anchor(), 1.0))\nplot(v, \"value\")",
        "f_anchor() =>\n    array.push(a, 100.0 + bar_index)\n    true\n[v, up, lo] = ta.vwap(source = array.pop(a), stdev_mult = 1.0, anchor = f_anchor())\nplot(v, \"value\")",
    ] {
        let source = format!(
            "//@version=6\nindicator(\"Tuple order\")\nvar a = array.from(1.0, 2.0, 3.0)\n\
             f_num() =>\n    array.push(a, 100.0 + bar_index)\n    0.0\n{body}\nplot(array.get(a, 2), \"tail\")\n"
        );
        let program = compile_csil(&source);
        let module = compile_program_to_wasm(&program).unwrap();
        for bars in [1, 3] {
            let data = synthetic_data(bars);
            let options = RunOptions::default();
            let reference = run(&program, &data, &options);
            assert!(
                reference.errors.is_empty(),
                "{body}: {:?}",
                reference.errors
            );
            let tail = reference
                .plots
                .iter()
                .find(|plot| plot.title.as_deref() == Some("tail"))
                .expect("tail plot");
            assert_eq!(tail.values[0], Value::Float(100.0), "{body}");
            let actual = run_aot(&module, &program, &data, &options);
            assert_eq!(actual.status, status::OK, "{body}");
            assert_outcomes_match(&reference, &actual, bars, body);
        }
    }
}

/// One map program: it must lower to the native module (not the hosted
/// interpreter), run to `OK` with no `ABI_MISMATCH` fallback, match the
/// interpreter bit for bit, and plot `expected` on its last bar (values worked
/// out by hand from the `map.*` semantics the compiler implements).
fn map_element_case(name: &str, body: &str, expected: &[f64]) {
    let source = format!("//@version=6\nindicator(\"map {name}\")\n{body}");
    let program = compile_csil(&source);
    let aot = compile_program_to_wasm(&program)
        .unwrap_or_else(|e| panic!("{name}: program must produce a valid Wasm tier: {e}"));
    assert!(
        !aot.layout.imports_interpreter,
        "{name}: the program must lower natively, not fall back to the hosted interpreter"
    );
    let data = synthetic_data(5);
    let options = RunOptions::default();
    let outcome = run(&program, &data, &options);
    assert!(outcome.errors.is_empty(), "{name}: {:?}", outcome.errors);
    let got = run_aot(&aot, &program, &data, &options);
    assert!(
        got.mismatch.is_none(),
        "{name}: ABI_MISMATCH fallback: {:?}",
        got.mismatch
    );
    assert_eq!(
        got.status,
        status::OK,
        "{name}: {:?}",
        got.host_error.map(|e| e.message)
    );
    assert_outcomes_match(&outcome, &got, data.bars(), name);
    let last = data.bars() - 1;
    let plotted: Vec<u64> = got.plots.iter().map(|lane| lane[last].to_bits()).collect();
    let expected: Vec<u64> = expected.iter().map(|x| x.to_bits()).collect();
    assert_eq!(
        plotted, expected,
        "{name}: last-bar plots (bits; NaN is na)"
    );
}

#[test]
fn map_keys_with_int_elements_round_trip_through_map_get_natively() {
    // Keys whose array elements were typed float would reach `map.get` as 2.0,
    // and the host would stop with "`map.get` element type mismatch"
    // (HOST_ERROR) where the interpreter plots 2.5.
    map_element_case(
        "int keys",
        "m = map.new<int, float>()\n\
         m.put(2, 2.5)\n\
         k = map.keys(m)\n\
         plot(m.get(k.get(0)))\n\
         plot(k.get(0) * 3)\n\
         plot(m.contains(k.get(0)) ? 1 : 0)\n",
        &[2.5, 6.0, 1.0],
    );
}

#[test]
fn map_values_with_int_elements_and_float_keys_run_natively() {
    // An int value read from `map.values` keys a second `map<int, float>`:
    // typed float, it reached that map's `put` as 7.0 (HOST_ERROR).
    map_element_case(
        "float keys, int values",
        "m = map.new<float, int>()\n\
         m.put(1.5, 7)\n\
         k = map.keys(m)\n\
         v = map.values(m)\n\
         plot(m.get(k.get(0)))\n\
         plot(v.get(0) + 1)\n\
         plot(k.get(0))\n\
         n = map.new<int, float>()\n\
         n.put(v.get(0), 0.5)\n\
         plot(n.get(7))\n",
        &[7.0, 8.0, 1.5, 0.5],
    );
}

#[test]
fn map_keys_and_values_with_string_elements_run_natively() {
    // A string key or value read from the projected array must fit its float
    // cell; otherwise it reports ABI_MISMATCH and the loader reruns the
    // request on the interpreter.
    map_element_case(
        "string keys and values",
        "m = map.new<string, string>()\n\
         m.put(\"ab\", \"xyz\")\n\
         k = map.keys(m)\n\
         v = map.values(m)\n\
         plot(str.length(k.get(0) + v.get(0)))\n\
         plot(str.length(m.get(k.get(0))))\n\
         plot(str.length(m.remove(k.get(0))))\n\
         plot(m.size())\n",
        &[5.0, 3.0, 3.0, 0.0],
    );
}

#[test]
fn map_keys_with_bool_elements_round_trip_natively() {
    map_element_case(
        "bool keys",
        "m = map.new<bool, int>()\n\
         m.put(true, 4)\n\
         m.put(false, 9)\n\
         k = map.keys(m)\n\
         plot(m.get(k.get(0)))\n\
         plot(m.get(k.get(1)))\n\
         plot(k.get(0) ? 1 : 0)\n",
        &[4.0, 9.0, 1.0],
    );
}

#[test]
fn map_keys_and_values_with_color_elements_run_natively() {
    // The compiler's built-in color table: color.red is #F23645 and
    // color.blue is #2962FF.
    map_element_case(
        "color keys and values",
        "m = map.new<color, color>()\n\
         m.put(color.red, color.blue)\n\
         k = map.keys(m)\n\
         v = map.values(m)\n\
         plot(color.r(k.get(0)))\n\
         plot(color.b(v.get(0)))\n\
         plot(color.g(m.get(k.get(0))), color = m.get(k.get(0)))\n",
        &[242.0, 255.0, 98.0],
    );
}

#[test]
fn map_values_with_object_elements_run_natively() {
    map_element_case(
        "object values",
        "type P\n    float x\n\
         m = map.new<string, P>()\n\
         m.put(\"a\", P.new(3.0))\n\
         k = map.keys(m)\n\
         v = map.values(m)\n\
         plot(v.get(0).x)\n\
         plot(m.get(k.get(0)).x)\n",
        &[3.0, 3.0],
    );
}

#[test]
fn map_copy_and_put_all_keep_element_types_natively() {
    // `map.copy` is a map of the same types; `map.put_all` fills a map
    // declared with them. Both projections must keep their key lane, and an
    // absent key gives na through the int value cell.
    map_element_case(
        "copy and put_all",
        "m = map.new<string, int>()\n\
         m.put(\"a\", 5)\n\
         c = m.copy()\n\
         ck = c.keys()\n\
         d = map.new<string, int>()\n\
         d.put_all(c)\n\
         dk = d.keys()\n\
         plot(c.get(ck.get(0)))\n\
         plot(d.get(dk.get(0)) + str.length(dk.get(0)))\n\
         plot(c.values().get(0))\n\
         plot(na(c.get(\"absent\")) ? 1 : 0)\n\
         plot(c.get(\"absent\"))\n",
        &[5.0, 6.0, 5.0, 1.0, f64::NAN],
    );
}

#[test]
fn map_held_in_an_object_field_keeps_element_types_natively() {
    // The map handle travels through a UDT field (NewObject, GetField).
    map_element_case(
        "object field",
        "type Holder\n    map<string, int> m\n\
         h = Holder.new(map.new<string, int>())\n\
         h.m.put(\"abc\", 3)\n\
         k = h.m.keys()\n\
         plot(h.m.get(k.get(0)) + str.length(k.get(0)))\n",
        &[6.0],
    );
}

#[test]
fn persistent_map_keys_iterate_natively_across_bars() {
    // A `var` map (a user slot carried across bars) whose keys a `for … in`
    // loop reads back: keys "0", "1" and "2" from bar 2 on, each holding the
    // close of the last bar that wrote it. On bar 6 the keys, in insertion
    // order, hold the closes of bars 6, 4 and 5, and the size is 3.
    let source = "//@version=6\nindicator(\"map var\")\n\
                  var m = map.new<string, float>()\n\
                  m.put(str.tostring(bar_index % 3), close)\n\
                  total = 0.0\n\
                  for key in m.keys()\n    total += m.get(key)\n\
                  plot(total)\n\
                  plot(m.size())\n";
    let program = compile_csil(source);
    let aot = compile_program_to_wasm(&program).expect("artifact");
    assert!(!aot.layout.imports_interpreter, "map var: hosted fallback");
    let data = synthetic_data(7);
    let options = RunOptions::default();
    let got = run_aot(&aot, &program, &data, &options);
    assert!(got.mismatch.is_none(), "map var: {:?}", got.mismatch);
    let outcome = differential_program(&program, &data, "map var");
    let total = 0.0 + data.close[6] + data.close[4] + data.close[5];
    assert_eq!(
        outcome.plots[0].values[6],
        Value::Float(total),
        "map var: total of the last three closes"
    );
    assert_eq!(outcome.plots[1].values[6], Value::Float(3.0));
}

#[test]
fn map_handles_keep_element_types_through_functions_merges_history_and_loops() {
    // Every path a map handle can travel besides a plain local: a function
    // parameter, a function return, a tuple return, a method on an object
    // field, a `?:` merge and a `switch` merge of two maps, a history read
    // `h[1]`, and a loop back edge. None of these may stop with HOST_ERROR
    // ("`map.get` element type mismatch: array holds int, got float"), which
    // would happen if the int key read from `map.keys` reached `map.get` as a
    // float.
    //
    // Last bar (bar_index 4, even):
    // - f(a): first key 5, value "ab" -> 5 * 10 + 2 = 52
    // - f(m): `m` is `a` on even bars -> 52
    // - f(s): `switch` takes `b` on even bars: key 3, "xyz" -> 33
    // - f(p) + one: key 2, "wxyz", plus 1 -> 25
    // - bx.first(): `b`'s first key -> 3
    // - h[1] is the same `var` map, keys 0..4: last key 4 holds 0.25 -> 4.25
    // - sum of c.get(key) over keys 0..3 of i * i -> 14
    map_element_case(
        "handle paths",
        "f(map<int, string> mm) =>\n    k = mm.keys()\n    k.get(0) * 10 + str.length(mm.get(k.get(0)))\n\
         mk() =>\n    r = map.new<int, string>()\n    r.put(5, \"ab\")\n    r\n\
         pair() =>\n    p = map.new<int, string>()\n    p.put(2, \"wxyz\")\n    [p, 1]\n\
         type Holder\n    map<int, string> m\n\
         method first(Holder self) =>\n    self.m.keys().get(0)\n\
         a = mk()\n\
         b = map.new<int, string>()\n\
         b.put(3, \"xyz\")\n\
         m = bar_index % 2 == 0 ? a : b\n\
         s = switch bar_index % 2\n    0 => b\n    => a\n\
         [p, one] = pair()\n\
         bx = Holder.new(b)\n\
         var h = map.new<int, float>()\n\
         h.put(bar_index, 0.25)\n\
         hh = bar_index > 0 ? h[1] : h\n\
         last = hh.keys().get(hh.size() - 1)\n\
         c = map.new<int, int>()\n\
         for i = 0 to 3\n    c.put(i, i * i)\n\
         t = 0\n\
         for key in c.keys()\n    t += c.get(key)\n\
         plot(f(a))\n\
         plot(f(m))\n\
         plot(f(s))\n\
         plot(f(p) + one)\n\
         plot(bx.first())\n\
         plot(hh.get(last) + last)\n\
         plot(t)\n",
        &[52.0, 52.0, 33.0, 25.0, 3.0, 4.25, 14.0],
    );
}

/// Hourly UTC bars from Monday 2024-01-01 00:00 over twenty days.
fn hourly_timeframe_feed() -> MarketData {
    let mut data = MarketData::default();
    for i in 0..24 * 20 {
        let base = 100.0 + (i % 7) as f64;
        data.open.push(base);
        data.high.push(base + 1.0);
        data.low.push(base - 1.0);
        data.close.push(base);
        data.volume.push(1000.0);
        data.time.push(1_704_067_200_000 + i64::from(i) * 3_600_000);
    }
    data.timeframe = "60".into();
    data
}

#[test]
fn timeframe_change_with_a_runtime_timeframe_matches_the_literal_in_vm_and_wasm() {
    // Every `timeframe.change` lowers to ordinary statements over
    // `time(tf)`; a literal passes the literal to `time`, any other timeframe
    // a temporary read on every execution. Both must give the same bars in
    // the interpreter and in the compiled module, including when the call
    // sits in a ternary arm that is skipped on some bars.
    let data = hourly_timeframe_feed();
    let literal_body = |tf_d: &str, tf_w: &str| {
        format!(
            "//@version=6\nindicator(\"t\")\n{tf_d}{tf_w}plot(timeframe.change(D) ? 1 : 0)\nplot(timeframe.change(W) ? 1 : 0)\nplot(bar_index % 5 == 0 ? 0 : timeframe.change(D) ? 1 : 0)\n"
        )
    };
    let literal = literal_body("", "")
        .replace("(D)", "(\"D\")")
        .replace("(W)", "(\"W\")");
    let literal_program = compile_csil(&literal);
    let expected = differential_program(&literal_program, &data, "literal");
    for (name, declarations) in [
        (
            "input",
            "D = input.timeframe(\"D\")\nW = input.timeframe(\"W\")\n",
        ),
        (
            "series",
            "D = bar_index >= 0 ? \"D\" : \"W\"\nW = bar_index >= 0 ? \"W\" : \"D\"\n",
        ),
    ] {
        let program = compile_csil(&literal_body(declarations, ""));
        let outcome = differential_program(&program, &data, name);
        for slot in 0..3 {
            assert_eq!(
                outcome.plots[slot].values, expected.plots[slot].values,
                "{name}: plot {slot}"
            );
        }
    }
}

#[test]
fn timeframe_change_with_a_switching_timeframe_matches_in_vm_and_wasm() {
    // The lowering compares bucket opening times. Bar `i` opens at
    // hour `i` of 2024-01-01 UTC; switching from "60" to "120" on bar 25
    // (an odd hour, inside the 2-hour bucket bar 24 opened) reports no
    // change there, then a change on every even-hour bar. The interpreter
    // and the compiled module must agree (`differential_program` checks
    // them bar by bar) and give these hand-computed flags.
    let data = hourly_timeframe_feed();
    let program = compile_csil(
        "//@version=6\nindicator(\"t\")\ntf = bar_index < 25 ? \"60\" : \"120\"\nplot(timeframe.change(tf) ? 1 : 0)\n",
    );
    let outcome = differential_program(&program, &data, "switching");
    let flags: Vec<bool> = outcome.plots[0]
        .values
        .iter()
        .map(|v| matches!(v, Value::Int(1)) || matches!(v, Value::Float(f) if *f == 1.0))
        .collect();
    let expected: Vec<bool> = (0..data.time.len()).map(|i| i < 25 || i % 2 == 0).collect();
    assert_eq!(flags, expected);
}

#[test]
fn timeframe_change_with_a_const_timeframe_compiles_as_the_literal() {
    // A `const` alias or const-folding expression is known at compile time,
    // so it passes the literal to `time` rather than a temporary the module
    // would read through `host.vm`. A fixed-width literal is a standalone
    // WASM module (the projection serves its `time` call from a lane); a
    // calendar week is not, because its buckets follow trading days the
    // request does not carry.
    let data = hourly_timeframe_feed();
    let options = StandaloneWasmOptions {
        signal_plot: None,
        module_type: StandaloneWasmModuleType::Indicator,
    };
    let literal = compile_csil(
        "//@version=6\nindicator(\"t\")\nplot(timeframe.change(\"D\") ? 1 : 0)\nplot(timeframe.change(\"240\") ? 1 : 0)\n",
    );
    let expected = differential_program(&literal, &data, "literal");
    compile_standalone_wasm_program(&literal, &options)
        .expect("the fixed-width literal form is a standalone module");
    let weekly =
        compile_csil("//@version=6\nindicator(\"t\")\nplot(timeframe.change(\"W\") ? 1 : 0)\n");
    assert!(
        compile_standalone_wasm_program(&weekly, &options).is_err(),
        "a calendar week keeps host.vm"
    );
    for source in [
        "//@version=6\nindicator(\"t\")\nconst string D = \"D\"\nplot(timeframe.change(D) ? 1 : 0)\nplot(timeframe.change(\"24\" + \"0\") ? 1 : 0)\n",
        "//@version=6\nindicator(\"t\")\nconst string U = \"D\"\nconst string D = U\nplot(timeframe.change(timeframe = D) ? 1 : 0)\nplot(timeframe.change((\"240\")) ? 1 : 0)\n",
    ] {
        let program = compile_csil(source);
        let outcome = differential_program(&program, &data, source);
        assert_eq!(outcome.plots[0].values, expected.plots[0].values, "{source}");
        assert_eq!(outcome.plots[1].values, expected.plots[1].values, "{source}");
        if let Err(error) = compile_standalone_wasm_program(&program, &options) {
            panic!("{source}: must be a standalone module like the literal: {error:?}");
        }
    }
}
