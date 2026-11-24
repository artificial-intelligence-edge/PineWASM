//! Minimal native embedding example for the public compiler API.

use pinewasm_compiler::{compile_and_run, CompileOptions, MarketData, RunOptions};
use std::io;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let source = r#"//@version=6
indicator("Example")
x = close - close[1]
plot(x)
"#;
    let close = vec![10.0, 12.0, 11.0];
    let market = MarketData {
        open: close.clone(),
        high: close.clone(),
        low: close.clone(),
        close,
        volume: vec![1.0; 3],
        ..MarketData::default()
    };

    let result = compile_and_run(
        source,
        &market,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    if !result.diagnostics.is_empty() || !result.runtime_errors.is_empty() {
        return Err(io::Error::other(format!(
            "compile diagnostics: {:?}; runtime errors: {:?}",
            result.diagnostics, result.runtime_errors
        ))
        .into());
    }

    let plot = result
        .plots
        .first()
        .ok_or_else(|| io::Error::other("script produced no plot"))?;
    let values = serde_json::to_string(&plot.values)?;
    if values != "[null,2.0,-1.0]" {
        return Err(io::Error::other(format!("unexpected plot values: {values}")).into());
    }
    println!("{values}");
    Ok(())
}
