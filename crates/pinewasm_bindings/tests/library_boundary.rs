//! Exercises the real generated JS/Wasm boundary, including proxy and accessor
//! behavior that native Serde tests cannot reproduce.
//!
//! Build the package with `cargo xtask wasm`, then run:
//! `cargo test --locked -p pinewasm_bindings --test library_boundary -- --ignored`

use std::path::Path;
use std::process::Command;

#[test]
#[ignore = "requires cargo xtask wasm output and a Node.js runtime"]
fn generated_library_boundary_snapshots_only_bounded_data_properties() {
    let package = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/wasm-pkg")
        .canonicalize()
        .expect("generate target/wasm-pkg with cargo xtask wasm before this test");
    let output = Command::new("node")
        .args(["--input-type=module", "--eval", LIBRARY_BOUNDARY_REGRESSION])
        .arg(package)
        .output()
        .expect("Node.js is required for the generated package regression");
    assert!(
        output.status.success(),
        "library boundary regression failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

const LIBRARY_BOUNDARY_REGRESSION: &str = r#"
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
const pkg = resolve(process.argv.at(-1));
const api = await import(pathToFileURL(`${pkg}/pinewasm.js`).href);
await api.default({ module_or_path: readFileSync(`${pkg}/pinewasm_bg.wasm`) });
const main = '//@version=6\nindicator("Boundary")\nplot(close)\n';
const lib = '//@version=6\nlibrary("L")\nexport f(float x) => x\n';
let checks = 0;
function accepts(libraries) {
  const result = api.compileWithLibraries(main, libraries);
  assert.equal(typeof result.success, 'boolean');
  checks++;
  return result;
}
function rejects(libraries, pattern) {
  assert.throws(() => api.compileWithLibraries(main, libraries), pattern);
  checks++;
}
assert.equal(accepts([['p/L/1', lib]]).success, true);
assert.equal(accepts(null).success, true);
assert.equal(accepts(undefined).success, true);
rejects({}, /must be an array/);
rejects(Array(129).fill(['p/L/1', lib]), /maximum 128/);
assert.equal(accepts(Array.from({length: 128}, (_, i) => [`p/L${i}/1`, lib])).success, true);
rejects(Array(1), /own data property/);
rejects([['p/L/1']], /exactly two/);
rejects([['p/L/1', lib, 'extra']], /exactly two/);
rejects([[1, lib]], /path must be a string/);
rejects([['p/L/1', 1]], /source must be a string/);
rejects([['x'.repeat(257), lib]], /path exceeds 256 bytes/);
rejects([['é'.repeat(129), lib]], /path exceeds 256 bytes/);
accepts([['x'.repeat(252) + '/L/1', lib]]);
accepts([['p/L/1', 'é'.repeat(524288)]]);
rejects([['p/L/1', 'é'.repeat(524288) + 'x']], /source exceeds 1048576 bytes/);
rejects([['p/L/1', 'x'.repeat(1048577)]], /source exceeds 1048576 bytes/);
accepts(Array.from({length: 8}, (_, i) => [`p/L${i}/1`, 'x'.repeat(1048576)]));
rejects(Array.from({length: 9}, (_, i) => [`p/L${i}/1`, 'x'.repeat(1048576)]), /8388608 bytes in total/);
rejects([['p/L/1', '\ud800']], /well-formed UTF-16/);
rejects([['p/\ud800/1', lib]], /well-formed UTF-16/);
let getterCalls = 0;
const tableAccessor = [];
Object.defineProperty(tableAccessor, '0', { get() { getterCalls++; return ['p/L/1', lib]; }, configurable: true });
rejects(tableAccessor, /own data property/);
for (const field of ['0', '1']) {
  const pair = ['p/L/1', lib];
  Object.defineProperty(pair, field, { get() { getterCalls++; return field === '0' ? 'p/L/1' : lib; } });
  rejects([pair], /own data property/);
}
assert.equal(getterCalls, 0, 'accessors must never execute');
const iterated = [['p/L/1', lib]];
iterated[Symbol.iterator] = () => { throw new Error('untrusted outer iterator executed'); };
iterated[0][Symbol.iterator] = () => { throw new Error('untrusted pair iterator executed'); };
assert.equal(accepts(iterated).success, true);
const revokedTable = Proxy.revocable([], {});
revokedTable.revoke();
rejects(revokedTable.proxy, /must be an array/);
const revokedPair = Proxy.revocable(['p/L/1', lib], {});
revokedPair.revoke();
rejects([revokedPair.proxy], /must be an array/);
const throwingTable = new Proxy([['p/L/1', lib]], { getOwnPropertyDescriptor() { throw new Error('descriptor failed'); } });
rejects(throwingTable, /own data property/);
let sourceReads = 0;
const changingPair = new Proxy(['p/L/1', lib], {
  getOwnPropertyDescriptor(target, key) {
    if (key === '1') {
      sourceReads++;
      return { configurable: true, enumerable: true, writable: true, value: sourceReads === 1 ? lib : 'x'.repeat(1048577) };
    }
    return Reflect.getOwnPropertyDescriptor(target, key);
  },
  get() { throw new Error('unchecked proxy reread'); }
});
assert.equal(accepts([changingPair]).success, true);
assert.equal(sourceReads, 1, 'validated source must be snapshotted once');
for (const entryPoint of [api.compileResident, api.compileAndRunWithLibraries]) {
  assert.throws(() => entryPoint(main, tableAccessor, {}), /own data property/);
  checks++;
}
// Older valid Wasm hosts must retain strict UTF-16 checking without the recent
// String.isWellFormed helper. Restore the exact descriptor even if a case fails.
const wellFormedDescriptor = Object.getOwnPropertyDescriptor(String.prototype, 'isWellFormed');
try {
  delete String.prototype.isWellFormed;
  assert.equal(typeof String.prototype.isWellFormed, 'undefined');
  assert.equal(accepts([['p/L/1', lib]]).success, true);
  assert.equal(accepts([['p/L/1', lib + '// café\n']]).success, true);
  assert.equal(accepts([['p/L/1', lib + '// \ud83d\ude42\n']]).success, true);
  for (const malformed of ['\ud800', '\udc00', '\ud800x', '\ud800\ud800\udc00', '\udc00\ud800']) {
    rejects([['p/L/1', malformed]], /well-formed UTF-16/);
  }
  rejects([['p/\ud800/1', lib]], /well-formed UTF-16/);
  rejects([['x'.repeat(257), lib]], /path exceeds 256 bytes/);
  rejects([['é'.repeat(129), lib]], /path exceeds 256 bytes/);
  rejects([['🙂'.repeat(65), lib]], /path exceeds 256 bytes/);
  rejects([['p/L/1', 'x'.repeat(1048577)]], /source exceeds 1048576 bytes/);
  rejects([['p/L/1', 'é'.repeat(524288) + 'x']], /source exceeds 1048576 bytes/);
  const searchOptions = {caseSensitive: true, wholeWord: false, isRegex: false, maxResults: 10};
  assert.equal(api.searchFiles([{path: 'p.pine', content: 'café 🙂'}], '🙂', searchOptions).total, 1n);
  checks++;
  assert.throws(() => api.searchFiles([{path: 'p.pine', content: 'x'}], '\ud800', searchOptions), /well-formed UTF-16/);
  checks++;
} finally {
  if (wellFormedDescriptor === undefined) {
    delete String.prototype.isWellFormed;
  } else {
    Object.defineProperty(String.prototype, 'isWellFormed', wellFormedDescriptor);
  }
}
console.log(`Library boundary regression passed (${checks} cases).`);
"#;
