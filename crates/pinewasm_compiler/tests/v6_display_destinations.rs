use pinewasm_compiler::{compile, CompileOptions, PlotDisplay};

#[test]
fn plot_preserves_pine_screener_and_display_arithmetic() {
    for (display, expected) in [
        ("display.pine_screener", PlotDisplay::PineScreener),
        (
            "display.all - display.pine_screener",
            PlotDisplay::PanePriceScaleStatusLineDataWindow,
        ),
        (
            "display.data_window + display.pine_screener",
            PlotDisplay::DataWindowPineScreener,
        ),
        (
            "display.all - display.pine_screener + display.pine_screener",
            PlotDisplay::All,
        ),
    ] {
        let source = format!(
            "//@version=6\nindicator(\"Screener destination\")\nplot(close, display={display})"
        );
        let result = compile(&source, CompileOptions::default());
        assert!(result.success, "{display}: {:?}", result.diagnostics);
        assert_eq!(result.program.unwrap().plots[0].style.display, expected);
    }
}

#[test]
fn input_display_rejects_direct_and_aliased_pine_screeners() {
    for display in ["display.pine_screener", "D"] {
        let source = format!(
            "//@version=6\nindicator(\"Input destination\")\nD = display.data_window + display.pine_screener\nx = input.int(1, display={display})\nplot(x)"
        );
        let result = compile(&source, CompileOptions::default());
        assert!(!result.success, "{display}: {:?}", result.diagnostics);
    }
}

#[test]
fn simple_display_calls_reject_screener_values() {
    for call in [
        "hline(70, display=D)",
        "bgcolor(color.red, display=D)",
        "barcolor(color.red, display=D)",
        "fill(p1, p2, color.red, display=D)",
        "fill(p1, p2, 100, 0, color.red, color.blue, display=D)",
    ] {
        for (display, success) in [
            ("display.pine_screener", false),
            ("display.data_window + display.pine_screener", false),
            ("enabled ? display.pine_screener : display.none", false),
            ("enabled ? display.all : display.none", true),
            ("display.all", true),
            ("display.none", true),
        ] {
            let source = format!(
                "//@version=6\nindicator(\"Simple destinations\")\np1 = plot(close)\np2 = plot(open)\nenabled = input.bool(true)\nD = {display}\n{call}"
            );
            let result = compile(&source, CompileOptions::default());
            assert_eq!(
                result.success, success,
                "{call} with {display}: {:?}",
                result.diagnostics
            );
        }
    }
}

#[test]
fn simple_display_checks_expression_type_before_mask_folding() {
    for (expression, success) in [
        ("(display.none)", true),
        ("true ? display.all : display.none", true),
        ("input.bool(true) ? display.all : display.none", true),
        ("true ? display.none : display.pane", false),
        ("display.all - display.none", false),
        ("display.pane - display.pane", false),
        ("display.none + display.none", false),
        ("display.none\nD := display.all", true),
        ("display.none\nD := display.pane", false),
        (
            "switch input.int(1)\n    1 => display.none\n    => display.all",
            true,
        ),
        (
            "switch input.int(1)\n    1 => display.none\n    => display.pane",
            false,
        ),
    ] {
        let source = format!(
            "//@version=6\nindicator(\"Two-state display\")\nD = {expression}\nhline(70, display=D)"
        );
        let result = compile(&source, CompileOptions::default());
        assert_eq!(
            result.success, success,
            "{expression}: {:?}",
            result.diagnostics
        );
    }
}

#[test]
fn mixed_reassignment_widens_only_subsequent_display_reads() {
    let prefix = "//@version=6\nindicator(\"Display reassignment\")\nD = display.all\nhline(70, display=D)\nif not input.bool(true)\n    D := display.status_line\n";
    let plot = compile(
        &format!("{prefix}plot(close, display=D)"),
        CompileOptions::default(),
    );
    assert!(plot.success, "{:?}", plot.diagnostics);

    let level = compile(
        &format!("{prefix}hline(30, display=D)"),
        CompileOptions::default(),
    );
    assert!(!level.success);
    assert!(format!("{:?}", level.diagnostics).contains("must be plot_simple_display"));
}
