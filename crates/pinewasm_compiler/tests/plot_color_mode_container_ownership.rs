//! Internal wire ownership; current output family claims do not replace
//! the actual plot, visual, or fill container under legacy version zero.
use pinewasm_compiler::{compile, run_program, CompileOptions, MarketData, RunOptions};

fn program(source: &str) -> pinewasm_compiler::Program {
    let result = compile(source, CompileOptions::default());
    assert!(result.success, "{:?}", result.diagnostics);
    result.program.unwrap()
}
fn ordinary() -> pinewasm_compiler::Program {
    program("indicator(\"ordinary owner\")\nplot(close,color=#336699)")
}
fn forged_visual(version: u32) -> pinewasm_compiler::Program {
    let mut p = program("indicator(\"visual owner\")\nbgcolor(#336699)");
    p.metadata.rendering_contract_version = version;
    let ordinary = ordinary();
    p.visuals[0].style = ordinary.plots[0].style.clone();
    p.visuals[0]
        .color_roles
        .clone_from(&ordinary.plots[0].color_roles);
    p
}

#[test]
fn legacy_visual_cannot_forge_ordinary_plot_color_mode_ownership() {
    let p = forged_visual(0);
    let errors = format!("{:?}", pinewasm_csil::validate(&p).unwrap_err());
    assert!(errors.contains("plot color mode belongs only to ordinary plot outputs"));
}

#[test]
fn fill_cannot_forge_ordinary_plot_color_mode_ownership() {
    for version in [0, 1] {
        let mut p =
            program("indicator(\"fill owner\")\np=plot(close)\nq=plot(open)\nfill(p,q,#336699)");
        p.metadata.rendering_contract_version = version;
        p.fills[0].style = ordinary().plots[0].style.clone();
        let errors = format!("{:?}", pinewasm_csil::validate(&p).unwrap_err());
        assert!(errors.contains("plot color mode belongs only to ordinary plot outputs"));
    }
}

#[test]
fn real_plot_color_mode_retains_current_and_legacy_execution() {
    for version in [0, 1] {
        let mut p = ordinary();
        p.metadata.rendering_contract_version = version;
        pinewasm_csil::validate(&p).unwrap();
        let run = run_program(&p, &MarketData::default(), &RunOptions::default());
        assert!(run.runtime_errors.is_empty());
        assert_eq!(
            run.plots[0].style.plot_color_mode,
            p.plots[0].style.plot_color_mode
        );
    }
}

#[test]
fn extension_absent_legacy_visual_retains_previous_compatibility() {
    let mut p = forged_visual(0);
    p.visuals[0].style.plot_color_mode = None;
    pinewasm_csil::validate(&p).unwrap();
    assert!(pinewasm_csil::validate(&forged_visual(1)).is_err());
}
