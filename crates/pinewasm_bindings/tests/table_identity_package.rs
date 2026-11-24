//! Check retired-table identity through the generated package VM.

#[path = "../../pinewasm_compiler/tests/support/table_identity_case.rs"]
mod table_identity_case;

use std::path::Path;
use std::process::Command;

#[test]
#[ignore = "requires cargo xtask wasm output and a Node.js runtime"]
fn generated_package_preserves_retired_table_identity() {
    let package = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/wasm-pkg")
        .canonicalize()
        .expect("generate target/wasm-pkg with cargo xtask wasm before this test");
    let output = Command::new("node")
        .args(["--input-type=module", "--eval", NODE_SCRIPT])
        .arg(package)
        .arg(table_identity_case::SOURCE)
        .output()
        .expect("Node.js is required for the generated package regression");
    assert!(
        output.status.success(),
        "retired table identity regression failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

const NODE_SCRIPT: &str = r"
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';
const [pkg, source] = process.argv.slice(-2);
const api = await import(pathToFileURL(`${pkg}/pinewasm.js`).href);
await api.default({ module_or_path: readFileSync(`${pkg}/pinewasm_bg.wasm`) });
const market = { open: [10], high: [11], low: [9], close: [10], volume: [1], time: [1704067200000] };
const compiled = api.compile(source);
assert.equal(compiled.success, true, JSON.stringify(compiled.diagnostics));
const expected = 'IDENTITY | a-na=true;b-na=true;c-na=false;index-na=-1;index-a=0;index-b=1;index-c=2;includes-na=false';
for (const run of [api.runProgram(compiled.program, market, null), api.compileAndRun(source, market, null, null)]) {
  assert.equal(run.runtimeErrors.length, 1, JSON.stringify(run.runtimeErrors));
  assert.equal(run.runtimeErrors[0].kind, 'user_error');
  assert.equal(run.runtimeErrors[0].barIndex, 0);
  assert.equal(run.runtimeErrors[0].message, expected);
}
console.log('Generated package retired table identity passed on both VM entry points.');
";
