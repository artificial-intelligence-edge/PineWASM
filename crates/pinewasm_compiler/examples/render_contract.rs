fn main() {
    println!(
        "{}",
        serde_json::to_string_pretty(&pinewasm_compiler::rendering_contract()).unwrap()
    );
}
