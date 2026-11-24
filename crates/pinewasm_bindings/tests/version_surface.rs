//! Checks the version surface of the real generated JS/Wasm package.
//!
//! Native tests compare `serde_json` values, so they cannot see the JavaScript
//! type a value arrives as. `renderingContract().schemaVersion` once arrived as
//! the JavaScript big integer `1n` while `version().renderingContractVersion` was the number
//! `1`, and the package consumer check rejected the release.
//!
//! Build the package with `cargo xtask wasm`, then run:
//! `cargo test --locked -p pinewasm_bindings --test version_surface -- --ignored`

use std::path::Path;
use std::process::Command;

#[test]
#[ignore = "requires cargo xtask wasm output and a Node.js runtime"]
fn generated_package_reports_one_rendering_contract_version() {
    let package = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/wasm-pkg")
        .canonicalize()
        .expect("generate target/wasm-pkg with cargo xtask wasm before this test");
    let output = Command::new("node")
        .args(["--input-type=module", "--eval", VERSION_SURFACE_REGRESSION])
        .arg(package)
        .output()
        .expect("Node.js is required for the generated package regression");
    assert!(
        output.status.success(),
        "version surface regression failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

const VERSION_SURFACE_REGRESSION: &str = r#"
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
const pkg = resolve(process.argv.at(-1));
const api = await import(pathToFileURL(`${pkg}/pinewasm.js`).href);
await api.default({ module_or_path: readFileSync(`${pkg}/pinewasm_bg.wasm`) });
const identity = api.version();
const contract = api.renderingContract();
// The same comparison the package consumer check makes before a release.
assert.equal(typeof identity.renderingContractVersion, 'number');
assert.equal(typeof contract.schemaVersion, 'number', `schemaVersion is ${typeof contract.schemaVersion}`);
assert.ok(
  identity.renderingContractVersion === contract.schemaVersion,
  'version().renderingContractVersion must equal renderingContract().schemaVersion',
);
// Every program the compiler emits carries the same version.
const compiled = api.compile('//@version=6\nindicator("Version")\nplot(close)\n');
assert.equal(compiled.success, true);
assert.equal(compiled.program.metadata.rendering_contract_version, contract.schemaVersion);
// The declarations type every integer in the descriptor as a number.
const bigints = [];
(function walk(value, path) {
  if (typeof value === 'bigint') {
    bigints.push(path);
  } else if (Array.isArray(value)) {
    value.forEach((item, index) => walk(item, `${path}[${index}]`));
  } else if (value !== null && typeof value === 'object') {
    for (const [key, item] of Object.entries(value)) walk(item, `${path}.${key}`);
  }
})(contract, 'renderingContract()');
assert.deepEqual(bigints, [], 'the rendering contract must not contain BigInt values');
assert.equal(contract.programHeader.il_version, identity.ilVersion);
assert.equal(contract.programHeader.language_version, identity.languageVersion);
console.log('Version surface regression passed.');
"#;
