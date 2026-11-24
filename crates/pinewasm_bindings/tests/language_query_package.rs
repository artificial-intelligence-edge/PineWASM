//! Exercise language queries through the actual generated package, alongside
//! native query semantics and the normal package identity boundary.
//!
//! The existing `cargo xtask wasm-test` gate runs this after package generation.

use pinewasm_lsp::query::{query, Request};
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process::Command;

const NODE_SCRIPT: &str = r#"
import { readFileSync } from 'node:fs';
import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

assert.equal(process.argv.length, 4, 'Need native fixtures, shared HTF fixtures, and exact compiler source revision');
const [nativeFixturePath, htfFixturePath, expectedRevision] = process.argv.slice(1);
assert.match(expectedRevision, /^[a-f0-9]{40}$/);
const packageUrl = pathToFileURL(resolve('target/wasm-pkg/pinewasm.js')).href;
const { default: init, languageQuery, languageCapabilities, version, compileAndRun, compileAndRunWithLibraries } = await import(packageUrl);
await init({ module_or_path: readFileSync('target/wasm-pkg/pinewasm_bg.wasm') });
const fixtures = JSON.parse(readFileSync('crates/pinewasm_lsp/fixtures/language_queries.json', 'utf8'));
const expected = JSON.parse(readFileSync(nativeFixturePath, 'utf8'));
const capabilities = ['analyze', 'hover', 'definition', 'references', 'codeActions', 'prepareRename', 'rename'];
assert.deepEqual(languageCapabilities(), {
  schemaVersion: 1,
  serviceVersion: 'pinewasm-language-v1',
  capabilities,
  maxSourceBytes: 262144,
  maxResults: 4096,
  maxResolutionWork: 67108864,
});
for (const name of ['schemaVersion', 'maxSourceBytes', 'maxResults', 'maxResolutionWork']) {
  assert.equal(typeof languageCapabilities()[name], 'number', `${name} must be a JS number`);
}
assert.equal(version().sourceRevision, expectedRevision);
const htfFixture = JSON.parse(readFileSync(htfFixturePath, 'utf8'));
const names = fixtures.map(item => item.name);
assert.equal(new Set(names).size, names.length, 'Duplicate source fixtures');
assert.equal(new Set(expected.map(item => item.name)).size, expected.length, 'Duplicate native outputs');
assert.deepEqual(expected.map(item => item.name), names, 'Native fixture roster/order drift');
assert.deepEqual([...new Set(fixtures.map(item => item.request.capability))].sort(), [...capabilities].sort());
for (const [index, fixture] of fixtures.entries()) {
  let outcome;
  try { outcome = { response: languageQuery(fixture.request) }; }
  catch (error) { outcome = { error: String(error) }; }
  assert.deepEqual(outcome, expected[index].outcome, fixture.name);
}

const request = fixtures[0].request;
assert.throws(() => languageQuery({ ...request, document: { ...request.document, source: '\ud800' } }), /invalid_language_query_data/);
let getterCalled = false;
const accessor = { ...request, document: { ...request.document } };
Object.defineProperty(accessor.document, 'source', { get() { getterCalled = true; return 'x=1'; } });
assert.throws(() => languageQuery(accessor), /invalid_language_query_data/);
assert.equal(getterCalled, false);
const { proxy, revoke } = Proxy.revocable({}, {});
revoke();
assert.throws(() => languageQuery(proxy), /invalid_language_query_data/);
assert.throws(() => languageQuery({ ...request, extra: true }), /invalid language query request/);
assert.throws(() => languageQuery({
  ...request,
  document: { ...request.document, extra: true },
}), /invalid language query request/);
assert.throws(() => languageQuery({
  ...request,
  position: { line: 0, character: 0, extra: true },
}), /invalid language query request/);
const libraryRequest = fixtures.find(item => item.name === 'definition-library').request;
assert.throws(() => languageQuery({
  ...libraryRequest,
  libraries: [{ ...libraryRequest.libraries[0], extra: true }],
}), /invalid language query request/);
const rangeRequest = fixtures.find(item => item.name === 'code-action').request;
assert.throws(() => languageQuery({
  ...rangeRequest,
  range: { ...rangeRequest.range, start: { ...rangeRequest.range.start, extra: true } },
}), /invalid language query request/);
// Only 32 small records are allocated; primitive-only shapes must consume the
// snapshot budget before Serde reaches unknown-field rejection.
const primitiveRecords = Array.from({ length: 32 }, () =>
  Object.fromEntries(Array.from({ length: 16 }, (_, index) => [`n${index}`, 0])));
assert.throws(() => languageQuery({ ...request, extra: primitiveRecords }), /invalid_language_query_data/);
assert.throws(() => languageQuery({ ...request, document: { ...request.document, source: ' '.repeat(262145) } }), /source_budget_exceeded/);
const oversizedLanguageSource = ' '.repeat(300 * 1024 + 1);
assert.throws(() => languageQuery({
  ...request,
  document: { ...request.document, source: oversizedLanguageSource },
}), /source_budget_exceeded/);
assert.throws(() => languageQuery({
  ...libraryRequest,
  libraries: [{ ...libraryRequest.libraries[0], source: oversizedLanguageSource }],
}), /source_budget_exceeded/);

// Inherited setters must not replace values in the immutable snapshot. The
// source setter covers normalized record properties; the array-index setter
// covers safe construction of the library list and nested arrays.
const setterRequest = {
  ...libraryRequest,
  capability: 'hover',
  document: { ...libraryRequest.document },
  libraries: libraryRequest.libraries.map((library) => ({ ...library })),
};
let inheritedSourceSetterCalls = 0;
let inheritedArraySetterCalls = 0;
const previousSourceSetter = Object.getOwnPropertyDescriptor(Object.prototype, 'source');
const previousArrayIndexSetter = Object.getOwnPropertyDescriptor(Array.prototype, '0');
Object.defineProperty(Object.prototype, 'source', {
  configurable: true,
  set() { inheritedSourceSetterCalls += 1; },
});
Object.defineProperty(Array.prototype, '0', {
  configurable: true,
  set() { inheritedArraySetterCalls += 1; },
});
let setterResponse;
let setterError;
try {
  setterResponse = languageQuery(setterRequest);
} catch (error) {
  setterError = String(error);
} finally {
  if (previousSourceSetter) Object.defineProperty(Object.prototype, 'source', previousSourceSetter);
  else delete Object.prototype.source;
  if (previousArrayIndexSetter) Object.defineProperty(Array.prototype, '0', previousArrayIndexSetter);
  else delete Array.prototype[0];
}
assert.equal(setterError, undefined, `prototype setter changed the query: ${setterError}`);
assert.equal(setterResponse.documentId, setterRequest.document.id);
assert.equal(inheritedSourceSetterCalls, 0, 'snapshot record invoked an inherited source setter');
assert.equal(inheritedArraySetterCalls, 0, 'snapshot array invoked an inherited index setter');


const largeMainSource = '//@version=6\nindicator("Resolution budget")\n';
let largeResolutionSource = largeMainSource;
for (let index = 0; index < 3000; index++) largeResolutionSource += 'v' + index + '=1\n';
for (let index = 0; index < 10000; index++) largeResolutionSource += 'v0 := v0 + 1\n';
assert.ok(largeResolutionSource.length < 262144);
largeResolutionSource += ' '.repeat(262144 - largeResolutionSource.length);
assert.equal(largeResolutionSource.length, 262144);
const referencesOverBudget = {
  schemaVersion: 1,
  capability: 'references',
  document: { id: 'work-budget', version: 'snapshot-1', source: largeResolutionSource },
  position: { line: 2, character: 0 },
  includeDeclaration: true,
};
function boundedOutcome(request) {
  try {
    return { data: languageQuery(request) };
  } catch (error) {
    return { error: error instanceof Error ? error.message : String(error) };
  }
}
assert.deepEqual(boundedOutcome(referencesOverBudget), { error: 'query_work_budget_exceeded' });
assert.deepEqual(boundedOutcome({
  ...referencesOverBudget,
  capability: 'rename',
  newName: 'renamed',
}), { error: 'query_work_budget_exceeded' });

const editPrefix = '//@version=6\nindicator("😀")\n//';
let editBody = '';
for (let index = 0; index < 1536; index++) editBody += 'x' + index + '=1\n';
for (let index = 0; index < 1536; index++) editBody += 'use' + index + '=x0\n';
const editPaddingBytes = 262144 - new TextEncoder().encode(editPrefix + editBody).length - 1;
const editSource = editPrefix + ' '.repeat(editPaddingBytes) + '\n' + editBody;
assert.equal(new TextEncoder().encode(editSource).length, 262144);
const lateEdits = languageQuery({
  schemaVersion: 1,
  capability: 'rename',
  document: { id: 'late-edits', version: 'utf16-v1', source: editSource },
  position: { line: 1539, character: 6 },
  newName: 'renamed',
  includeDeclaration: true,
}).data.edits;
assert.equal(lateEdits.length, 1537);
const expectedLateOffsets = [...editSource.matchAll(/x0/g)].map(match => match.index);
assert.equal(expectedLateOffsets.length, 1537);
for (const index of [0, 1, 255, 512, 1024, 1536]) {
  assert.equal(lateEdits[index].startUtf16, expectedLateOffsets[index]);
  assert.equal(lateEdits[index].endUtf16, expectedLateOffsets[index] + 2);
}
assert.ok(lateEdits.at(-1).startUtf16 > 250000);

let largeDiagnosticLibrary = '//@version=6\nlibrary("Large")\n';
for (let index = 0; index < 256; index++) {
  largeDiagnosticLibrary += '!\n';
}
const largeDiagnosticMain = [
  '//@version=6',
  'import example/Large/1 as large',
  'indicator("Large diagnostics")',
  'plot(large.f0())',
  '',
].join('\n');
largeDiagnosticLibrary += ' '.repeat(262144 - largeDiagnosticMain.length - largeDiagnosticLibrary.length);
assert.equal(largeDiagnosticMain.length + largeDiagnosticLibrary.length, 262144);
const largeDiagnostics = languageQuery({
  schemaVersion: 1,
  capability: 'analyze',
  document: { id: 'main-large-diagnostics', version: 'main-large-v1', source: largeDiagnosticMain },
  libraries: [{
    id: 'large-diagnostics-library',
    version: 'library-large-v1',
    importPath: 'example/Large/1',
    source: largeDiagnosticLibrary,
  }],
}).data.diagnostics;
assert.ok(largeDiagnostics.length >= 256);
const mappedLargeDiagnosticLabels = largeDiagnostics.flatMap(item => item.labels)
  .filter(label => label.location?.documentId === 'large-diagnostics-library'
    && label.location.documentVersion === 'library-large-v1');
assert.ok(mappedLargeDiagnosticLabels.length >= 256);
assert.ok(mappedLargeDiagnosticLabels.some(label => label.location.range.start.line === 257));

const result = compileAndRun('//@version=6\nindicator("Language facade regression")\nplot(close * 2)\n', { open: [1, 2, 3], high: [1, 2, 3], low: [1, 2, 3], close: [1, 2, 3], volume: [1, 1, 1] });
assert.equal(result.compile.success, true);
assert.deepEqual(result.runtimeErrors, []);
assert.deepEqual(result.plots[0].values, [2, 4, 6]);
const close = Array.from({ length: 360 }, (_, i) => 100 + Math.sin(i / 24) * 8 + i * 0.01);
const marketData = {
  open: close,
  high: close.map(value => value + 1),
  low: close.map(value => value - 1),
  close,
  volume: Array(360).fill(1000),
  time: Array.from({ length: 360 }, (_, i) => 1704067200000 + i * 14400000),
  timeframe: '240',
};
const htfResult = compileAndRunWithLibraries(
  htfFixture.source,
  [['test/Kernel/2', htfFixture.library]],
  marketData,
);
assert.equal(htfResult.compile.success, true, JSON.stringify(htfResult.diagnostics));
assert.deepEqual(htfResult.runtimeErrors, []);
assert.equal(htfResult.barsExecuted, 360);
const daily = close.filter((_, index) => index % 6 === 5);
function kernelReference(values) {
  let weighted = 0;
  let total = 0;
  for (let i = 0; i < 27; i++) {
    const weight = Math.pow(1 + (i * i) / 1024, -8);
    weighted += values[values.length - 1 - i] * weight;
    total += weight;
  }
  return weighted / total;
}
for (let i = 0; i < 168; i++) {
  assert.equal(htfResult.plots[1].values[i], 0);
  assert.equal(htfResult.plots[2].values[i], 0);
}
for (let i = 168; i < 360; i++) {
  const day = Math.floor(i / 6);
  const expected = kernelReference(daily.slice(0, day));
  const previous = kernelReference(daily.slice(0, day - 1));
  assert.ok(
    Math.abs(htfResult.plots[0].values[i] - expected) < 1e-10,
    `HTF kernel bar ${i}: ${htfResult.plots[0].values[i]} != ${expected}`,
  );
  assert.equal(htfResult.plots[1].values[i], expected > previous ? 1 : 0);
  assert.equal(htfResult.plots[2].values[i], expected < previous ? 1 : 0);
}

const scalarHelperSource = [
  '//@version=6',
  'indicator("Scalar helper")',
  'f() =>',
  '    unused = str.tostring(close)',
  '    close',
  'requested = request.security(syminfo.tickerid, "1D", f())',
  'plot(requested)',
  '',
].join('\n');
const scalarHelperResult = compileAndRun(scalarHelperSource, marketData);
assert.equal(scalarHelperResult.compile.success, true, JSON.stringify(scalarHelperResult.diagnostics));

const varipHelperSource = [
  '//@version=6',
  'indicator("Varip helper")',
  'f() =>',
  '    varip float seed = 0.0',
  '    seed := seed + close',
  '    total = 0.0',
  '    for i=0 to 2',
  '        total := total + seed',
  '    [total, total > 0]',
  '[value, positive] = request.security(syminfo.tickerid, "1D", f())',
  'plot(value)',
  'plot(positive ? 1.0 : 0.0)',
  '',
].join('\n');
const varipHelperResult = compileAndRun(varipHelperSource, marketData);
assert.equal(varipHelperResult.compile.success, false);
assert.ok(
  varipHelperResult.diagnostics.some(diagnostic => diagnostic.code === 'E0244'),
  JSON.stringify(varipHelperResult.diagnostics),
);

const stateLibrary = [
  '//@version=6',
  'library("State")',
  'var float carried = close',
  'export read() => carried',
  '',
].join('\n');
const libraryVarResult = compileAndRunWithLibraries(
  [
    '//@version=6',
    'import test/State/1 as lib',
    'indicator("Library var HTF")',
    'requested = request.security(syminfo.tickerid, "1D", lib.read())',
    'plot(requested)',
    '',
  ].join('\n'),
  [['test/State/1', stateLibrary]],
  marketData,
);
assert.equal(libraryVarResult.compile.success, false);
assert.ok(
  libraryVarResult.diagnostics.some(diagnostic => diagnostic.code === 'E0244'),
  JSON.stringify(libraryVarResult.diagnostics),
);

assert.equal(version().sourceRevision, expectedRevision);
console.log(`Passed ${fixtures.length} native/Wasm parity fixtures, exact capability/identity, input boundary checks, compile/runtime smoke, and imported HTF helper/tuple parity.`);
"#;

#[test]
#[ignore = "requires cargo xtask wasm output and a Node.js runtime"]
fn generated_package_language_queries_match_native_and_boundaries() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root exists");
    assert!(root.join("target/wasm-pkg/pinewasm_bg.wasm").is_file());
    let revision = Command::new("git")
        .current_dir(&root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("read the checkout revision for package qualification");
    assert!(revision.status.success());
    let revision = String::from_utf8(revision.stdout).expect("Git revision is UTF-8");
    let revision = revision.trim();
    assert_eq!(revision.len(), 40);
    assert!(revision.bytes().all(|value| value.is_ascii_hexdigit()));

    let fixtures: Vec<Value> = serde_json::from_str(include_str!(
        "../../pinewasm_lsp/fixtures/language_queries.json"
    ))
    .expect("language fixtures are valid JSON");
    let native = fixtures
        .iter()
        .map(|fixture| {
            let request: Request = serde_json::from_value(fixture["request"].clone())
                .expect("language request is valid");
            let outcome = match query(request) {
                Ok(response) => json!({ "response": response }),
                Err(error) => json!({ "error": error.to_string() }),
            };
            json!({ "name": fixture["name"], "outcome": outcome })
        })
        .collect::<Vec<_>>();
    let file = root
        .join("target")
        .join(format!("language-native-{}.json", std::process::id()));
    let htf_file = root
        .join("target")
        .join(format!("htf-pure-helper-{}.json", std::process::id()));
    let mut native_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&file)
        .expect("use a fresh native language fixture output");
    native_file
        .write_all(&serde_json::to_vec(&native).expect("serialize native fixture outputs"))
        .expect("write native fixture outputs");
    drop(native_file);
    let htf_fixture = fixtures
        .iter()
        .find(|fixture| fixture["name"] == "shared-htf-pure-helper-fixture")
        .expect("shared HTF helper fixture exists")["htfPureHelper"]
        .clone();
    let mut htf_fixture_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&htf_file)
        .expect("use a fresh shared HTF fixture output");
    htf_fixture_file
        .write_all(&serde_json::to_vec(&htf_fixture).expect("serialize shared HTF fixture"))
        .expect("write shared HTF fixture output");
    drop(htf_fixture_file);
    let output = Command::new("node")
        .current_dir(&root)
        .args(["--input-type=module", "--eval", NODE_SCRIPT])
        .arg(&file)
        .arg(&htf_file)
        .arg(revision)
        .output()
        .expect("Node.js is required for generated package regression");
    if output.status.success() {
        fs::remove_file(&file).expect("remove the successful temporary fixture output");
        fs::remove_file(&htf_file).expect("remove the successful shared HTF fixture output");
    }
    assert!(
        output.status.success(),
        "language package regression failed; native outputs retained at {} and {}\nstdout:\n{}\nstderr:\n{}",
        file.display(),
        htf_file.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
