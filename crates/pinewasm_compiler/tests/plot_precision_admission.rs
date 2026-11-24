use pinewasm_compiler::{compile, CompileOptions};

const CALLS: &[&str] = &[
    "plot(close,precision=P)",
    "plotshape(close>open,precision=P)",
    "plotchar(close>open,precision=P)",
    "plotarrow(close-open,precision=P)",
    "plotbar(open,high,low,close,precision=P)",
    "plotcandle(open,high,low,close,precision=P)",
];

#[test]
fn plot_family_rejects_const_precision_outside_documented_range() {
    for call in CALLS {
        for precision in [-1, 17, 255] {
            for prefix in [String::new(), format!("const int p={precision}\n")] {
                let expression = if prefix.is_empty() {
                    precision.to_string()
                } else {
                    "p".into()
                };
                let source = format!(
                    "//@version=6\nindicator(\"Precision\")\n{prefix}{}",
                    call.replace('P', &expression)
                );
                let compiled = compile(&source, CompileOptions::all());
                assert!(
                    compiled.diagnostics.iter().any(|d| d.is_error()
                        && d.message.contains("precision")
                        && d.message.contains("0 and 16")),
                    "{source}: {:?}",
                    compiled.diagnostics
                );
            }
        }
    }
}

#[test]
fn plot_family_preserves_boundary_and_input_precision_admission() {
    for call in CALLS {
        for (prefix, expression) in [
            ("", "0"),
            ("", "16"),
            ("p=input.int(2,minval=0,maxval=16)\n", "p"),
        ] {
            let source = format!(
                "//@version=6\nindicator(\"Precision\")\n{prefix}{}",
                call.replace('P', expression)
            );
            let compiled = compile(&source, CompileOptions::all());
            assert!(
                !compiled
                    .diagnostics
                    .iter()
                    .any(pinewasm_compiler::Diagnostic::is_error),
                "{source}: {:?}",
                compiled.diagnostics
            );
        }
    }
}
