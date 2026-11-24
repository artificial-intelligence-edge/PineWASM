//! Emits transport-neutral fixture outputs for comparison with the actual Wasm bindings.
use pinewasm_lsp::query::{query, Request};
use serde_json::{json, Value};

fn main() {
    let fixtures: Vec<Value> =
        serde_json::from_str(include_str!("../fixtures/language_queries.json")).unwrap();
    let results = fixtures
        .into_iter()
        .map(|fixture| {
            let request: Request = serde_json::from_value(fixture["request"].clone()).unwrap();
            let outcome = match query(request) {
                Ok(response) => json!({ "response": response }),
                Err(error) => json!({ "error": error.to_string() }),
            };
            json!({ "name": fixture["name"], "outcome": outcome })
        })
        .collect::<Vec<_>>();
    println!("{}", serde_json::to_string_pretty(&results).unwrap());
}
