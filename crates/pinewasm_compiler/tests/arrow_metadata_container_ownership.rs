//! Container admission. Color and style fields are checked
//! independently of whether a deserialized record belongs to a real plot output.
use pinewasm_compiler::{compile, run_program, CompileOptions, MarketData, RunOptions};

fn program(source: &str) -> pinewasm_compiler::Program {
    let result = compile(source, CompileOptions::default());
    assert!(result.success, "{:?}", result.diagnostics);
    result.program.unwrap()
}
fn arrow() -> pinewasm_compiler::Program {
    program("indicator(\"arrow owner\")\nplotarrow(close,colorup=close>open?#336699:#669933)")
}
fn visual(version: u32) -> pinewasm_compiler::Program {
    let mut p = program("indicator(\"visual owner\")\nbgcolor(#336699)");
    p.metadata.rendering_contract_version = version;
    let arrow = arrow();
    p.visuals[0].style = arrow.plots[0].style.clone();
    // Keep one valid Up role, matching the existing one-color visual opcode.
    // Otherwise generic role ownership would mask the container defect.
    p.visuals[0]
        .color_roles
        .clone_from(&arrow.plots[0].color_roles);
    p
}
fn error_text(program: &pinewasm_compiler::Program) -> String {
    format!("{:?}", pinewasm_csil::validate(program).unwrap_err())
}

#[test]
fn legacy_visual_cannot_forge_static_arrow_container_ownership() {
    let mut p = visual(0);
    p.visuals[0].style.arrow_runtime_colorers = None;
    assert!(error_text(&p).contains("static arrow colors belong only to plotarrow outputs"));
}

#[test]
fn legacy_visual_cannot_forge_complete_runtime_arrow_container_ownership() {
    let p = visual(0);
    let errors = error_text(&p);
    assert!(errors.contains("static arrow colors belong only to plotarrow outputs"));
    assert!(
        errors.contains("runtime arrow colorer bindings require complete plotarrow static colors")
    );
}

#[test]
fn extension_absent_legacy_visual_retains_existing_family_compatibility() {
    let mut p = visual(0);
    p.visuals[0].style.arrow_static_colors = None;
    p.visuals[0].style.arrow_runtime_colorers = None;
    pinewasm_csil::validate(&p).unwrap();
}

#[test]
fn current_visual_container_rejects_forged_arrow_extensions() {
    let p = visual(1);
    assert!(pinewasm_csil::validate(&p).is_err());
}

#[test]
fn legacy_fill_cannot_forge_arrow_extension_ownership() {
    let mut p =
        program("indicator(\"fill owner\")\np=plot(close)\nq=plot(open)\nfill(p,q,#336699)");
    p.metadata.rendering_contract_version = 0;
    p.fills[0].style = arrow().plots[0].style.clone();
    let errors = error_text(&p);
    assert!(errors.contains("static arrow colors belong only to plotarrow outputs"));
    assert!(
        errors.contains("runtime arrow colorer bindings require complete plotarrow static colors")
    );
}

#[test]
fn real_legacy_and_current_arrow_plots_admit_static_intermediate_and_complete_records() {
    for version in [0, 1] {
        for with_runtime_binding in [false, true] {
            let mut p = arrow();
            p.metadata.rendering_contract_version = version;
            if !with_runtime_binding {
                p.plots[0].style.arrow_runtime_colorers = None;
            }
            pinewasm_csil::validate(&p).unwrap();
            let output = run_program(&p, &MarketData::default(), &RunOptions::default());
            assert!(output.runtime_errors.is_empty());
            assert_eq!(
                output.plots[0].style.arrow_static_colors,
                p.plots[0].style.arrow_static_colors
            );
            assert_eq!(
                output.plots[0].style.arrow_runtime_colorers,
                p.plots[0].style.arrow_runtime_colorers
            );
            assert!(output.plots[0].values.is_empty());
        }
    }
}
