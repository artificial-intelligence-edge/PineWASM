//! JavaScript/WebAssembly bindings for PineWASM's programmatic host APIs.
//!
//! Everything here is a thin serialization shim over
//! `pinewasm_compiler`; no language or runtime semantics live in this
//! crate. The JS-facing contract is documented in `docs/browser.md`.
//!
//! # Adapter wiring check
//!
//! The generated JavaScript package is the supported non-Rust interface; this
//! implementation crate is not a supported downstream Rust import surface.
//! This compile-only example keeps the adapter entry point wired to the
//! compiler without pretending that a native process can supply JavaScript
//! values:
//!
//! ```no_run
//! use wasm_bindgen::JsValue;
//!
//! let result = pinewasm_bindings::compile(
//!     "//@version=6\nindicator(\"Binding\")\nplot(close)",
//!     JsValue::UNDEFINED,
//! )?;
//! assert!(!result.is_undefined());
//! # Ok::<(), JsValue>(())
//! ```
#![forbid(unsafe_code)]

use pinewasm_compiler::{
    CompileOptions, MarketData, RunOptions, MAX_LIBRARY_COUNT, MAX_LIBRARY_PATH_LEN,
    MAX_LIBRARY_SOURCE_BYTES, MAX_SOURCE_LEN,
};
use pinewasm_search::{
    MAX_SEARCH_FILES, MAX_SEARCH_FILE_CONTENT_BYTES, MAX_SEARCH_PATH_BYTES, MAX_SEARCH_QUERY_BYTES,
    MAX_SEARCH_RESULTS, MAX_SEARCH_TOTAL_CONTENT_BYTES,
};
use serde::{Deserialize, Serialize};
use wasm_bindgen::{prelude::*, JsCast};

mod typescript;

#[wasm_bindgen]
extern "C" {
    /// The global `performance.now()` — present on both `Window` and
    /// `WorkerGlobalScope`, so this binds in the compile worker too. Injected
    /// into the compiler as its clock: `std::time::Instant` traps on
    /// wasm32-unknown-unknown, so the engine cannot time itself.
    #[wasm_bindgen(js_namespace = performance, js_name = now)]
    fn performance_now() -> f64;

    /// Fallible `Array.isArray`: revoked proxies can make the native operation
    /// throw, and search preflight must reject those inputs instead of trapping.
    #[wasm_bindgen(js_namespace = Array, js_name = isArray, catch)]
    fn try_array_is_array(value: &JsValue) -> Result<bool, JsValue>;

    /// Browser/worker UTF-8 measurement without copying the source string into
    /// Wasm memory. A UTF-16-length guard bounds each temporary `Uint8Array`.
    #[wasm_bindgen(js_name = TextEncoder)]
    type SearchTextEncoder;

    #[wasm_bindgen(
        js_name = String,
        extends = js_sys::Object,
        is_type_of = JsValue::is_string,
        typescript_type = "string"
    )]
    type SearchJsString;

    #[wasm_bindgen(constructor, catch, js_class = TextEncoder)]
    fn new() -> Result<SearchTextEncoder, JsValue>;

    #[wasm_bindgen(method, js_name = encode, catch)]
    fn encode_utf8(
        this: &SearchTextEncoder,
        input: &JsValue,
    ) -> Result<js_sys::Uint8Array, JsValue>;

    /// Keep the later Rust conversion lossless. JavaScript otherwise replaces
    /// lone surrogates with U+FFFD, which can collapse distinct paths.
    #[wasm_bindgen(method, js_class = String, js_name = isWellFormed, catch)]
    fn try_is_well_formed(this: &SearchJsString) -> Result<bool, JsValue>;
}

/// The host clock handed to `pinewasm_compiler`'s `*_profiled` entry
/// points; produces the `timings` / `runMs` fields hosts read.
fn js_clock() -> f64 {
    performance_now()
}

/// Machine-readable rendering domains generated from Rust's Serde models.
/// Its integers, including `schemaVersion`, are JavaScript numbers.
#[wasm_bindgen(js_name = renderingContract, unchecked_return_type = "RenderingContract")]
pub fn rendering_contract() -> Result<JsValue, JsValue> {
    descriptor_to_js(&pinewasm_compiler::rendering_contract())
}

// The descriptor is a `serde_json::Value`, which stores every non-negative
// integer as a `u64`. `to_js` turns each `u64` into a BigInt, so
// `schemaVersion` became `1n` while `version().renderingContractVersion`
// (a `u32`) is `1`, and every integer the declarations type as `number`
// arrived as a BigInt. The descriptor's integers are small schema constants,
// so it is published with JavaScript numbers. A value outside the
// safe-integer range fails serialization instead of losing precision.
fn descriptor_to_js<T: Serialize>(value: &T) -> Result<JsValue, JsValue> {
    value
        .serialize(&serde_wasm_bindgen::Serializer::json_compatible())
        .map_err(|e| JsValue::from_str(&format!("serialization error: {e}")))
}

/// Install the panic hook so an unexpected Rust panic surfaces as a
/// readable JavaScript stack trace instead of an opaque
/// `unreachable` trap.
#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}

fn to_js<T: Serialize>(value: &T) -> Result<JsValue, JsValue> {
    // Start from the object/null-oriented serializer so `Option`/`Value::Na`
    // map to `null` rather than `undefined` and Rust maps become plain objects.
    // Opt back into BigInt for 64-bit fields: direct JavaScript consumers keep
    // exact integers, at the documented cost of needing a codec before JSON
    // persistence.
    let serializer = serde_wasm_bindgen::Serializer::json_compatible()
        .serialize_large_number_types_as_bigints(true);
    value
        .serialize(&serializer)
        .map_err(|e| JsValue::from_str(&format!("serialization error: {e}")))
}

fn options_from_js(options: JsValue) -> Result<CompileOptions, JsValue> {
    if options.is_undefined() || options.is_null() {
        return Ok(CompileOptions::default());
    }
    serde_wasm_bindgen::from_value(options)
        .map_err(|e| JsValue::from_str(&format!("invalid compile options: {e}")))
}

/// Apply the compiler's source boundary before a navigation endpoint builds a
/// line index or parse tree. Returning the stable host-facing message lets native
/// tests pin both its wording and its UTF-8 byte semantics.
fn validate_navigation_source(source: &str) -> Result<(), String> {
    if source.len() > MAX_SOURCE_LEN {
        return Err(format!(
            "source is {} bytes; the maximum supported size is {MAX_SOURCE_LEN}",
            source.len()
        ));
    }
    Ok(())
}

fn preflight_navigation_source(source: &str) -> Result<(), JsValue> {
    validate_navigation_source(source).map_err(|error| JsValue::from_str(&error))
}

/// One Outline row: a parse-only document symbol with zero-based UTF-16
/// editor coordinates (the compiler's spans are UTF-8 byte offsets; the
/// conversion happens here where the source is at hand).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OutlineSymbol {
    name: String,
    /// Stable human label: "variable" | "function" | "parameter" | "type" |
    /// "field" | "enum member" | "method" | "library alias".
    kind: &'static str,
    detail: String,
    signature: Option<String>,
    container: Option<String>,
    line: u32,
    character: u32,
    end_line: u32,
}

/// Find-in-files over in-memory sources, powered by ripgrep's regex engine
/// (`grep-regex`; see `crates/pinewasm_search`). `files` is
/// `[{ path, content }]`, `options` is `{ caseSensitive, smartCase,
/// wholeWord, isRegex, maxResults }`; returns `{ files: [{ path, matches:
/// [{ line, lineText, ranges: [[startCol, endCol]] }] }], hitLimit, total }`
/// with zero-based lines and UTF-16 columns. Invalid patterns reject.
#[wasm_bindgen(js_name = searchFiles, unchecked_return_type = "SearchResults")]
pub fn search_files(
    #[wasm_bindgen(unchecked_param_type = "ReadonlyArray<SearchFile>")] files: JsValue,
    query: js_sys::JsString,
    #[wasm_bindgen(unchecked_param_type = "SearchOptions")] options: JsValue,
) -> Result<JsValue, JsValue> {
    if !<js_sys::JsString as AsRef<JsValue>>::as_ref(&query).is_string() {
        return Err(JsValue::from_str("invalid search query: expected a string"));
    }
    let encoder = SearchTextEncoder::new()
        .map_err(|_| JsValue::from_str("invalid search request: UTF-8 preflight is unavailable"))?;
    validate_search_query(&query, &encoder)?;
    let files = snapshot_search_files(&files, &encoder)?;
    let options = snapshot_search_options(&options)?;

    // Only bounded, immutable snapshots cross into Wasm linear memory. This
    // also prevents an accessor or proxy from returning a small value during
    // preflight and a larger value while Serde walks the input.
    let files: Vec<pinewasm_search::SearchFile> = serde_wasm_bindgen::from_value(files.into())
        .map_err(|e| JsValue::from_str(&format!("invalid search files: {e}")))?;
    let options: pinewasm_search::SearchOptions = serde_wasm_bindgen::from_value(options.into())
        .map_err(|e| JsValue::from_str(&format!("invalid search options: {e}")))?;
    let query = String::from(query);
    let results = pinewasm_search::search_files(&files, &query, &options)
        .map_err(|e| JsValue::from_str(&e))?;
    to_js(&results)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BoundedUtf8Len {
    Bytes(usize),
    ExceedsLimit,
    InvalidUtf16,
}

/// Measure an encoded string with an allocation guard. Every UTF-16 code unit
/// contributes at least one UTF-8 byte, so an oversized `known_units` value can
/// reject without invoking the potentially allocating encoder.
fn bounded_utf8_len<E>(
    known_units: usize,
    limit: usize,
    is_well_formed: impl FnOnce() -> Result<bool, E>,
    encoded_len: impl FnOnce() -> Result<usize, E>,
) -> Result<BoundedUtf8Len, E> {
    if known_units > limit {
        return Ok(BoundedUtf8Len::ExceedsLimit);
    }
    if !is_well_formed()? {
        return Ok(BoundedUtf8Len::InvalidUtf16);
    }
    let bytes = encoded_len()?;
    if bytes > limit {
        Ok(BoundedUtf8Len::ExceedsLimit)
    } else {
        Ok(BoundedUtf8Len::Bytes(bytes))
    }
}

fn bounded_js_string_utf8_len(
    value: &js_sys::JsString,
    encoder: &SearchTextEncoder,
    limit: usize,
) -> Result<BoundedUtf8Len, ()> {
    let well_formed_string: &SearchJsString = value.unchecked_ref();
    bounded_utf8_len(
        value.length() as usize,
        limit,
        || {
            // `isWellFormed` is recent enough to be absent on otherwise valid
            // Wasm hosts. The allocation-free js-sys fallback walks primitive
            // UTF-16 code units with `charCodeAt`, never a Rust string copy.
            // `bounded_utf8_len` has already admitted the code-unit count.
            Ok(well_formed_string
                .try_is_well_formed()
                .unwrap_or_else(|_| value.is_valid_utf16()))
        },
        || {
            encoder
                .encode_utf8(value.as_ref())
                .map(|encoded| encoded.length() as usize)
        },
    )
    .map_err(|_| ())
}

fn validate_search_query(
    query: &js_sys::JsString,
    encoder: &SearchTextEncoder,
) -> Result<(), JsValue> {
    match bounded_js_string_utf8_len(query, encoder, MAX_SEARCH_QUERY_BYTES) {
        Ok(BoundedUtf8Len::Bytes(_)) => Ok(()),
        Ok(BoundedUtf8Len::ExceedsLimit) => Err(JsValue::from_str(&format!(
            "search query exceeds {MAX_SEARCH_QUERY_BYTES} bytes"
        ))),
        Ok(BoundedUtf8Len::InvalidUtf16) => Err(JsValue::from_str(
            "invalid search query: expected well-formed UTF-16",
        )),
        Err(()) => Err(JsValue::from_str(
            "invalid search query: UTF-8 byte length could not be measured",
        )),
    }
}

#[derive(Debug)]
enum OwnProperty {
    Missing,
    Data(JsValue),
    Accessor,
}

/// Read a property descriptor without invoking an accessor. Proxy traps can
/// still throw, so every reflection step is caught and mapped to a stable
/// boundary error by the caller.
fn own_property(target: &JsValue, key: &JsValue) -> Result<OwnProperty, ()> {
    if !target.is_object() {
        return Err(());
    }
    let object: &js_sys::Object = target.unchecked_ref();
    let descriptor = js_sys::Reflect::get_own_property_descriptor(object, key).map_err(|_| ())?;
    if descriptor.is_undefined() {
        return Ok(OwnProperty::Missing);
    }
    let value_key = JsValue::from_str("value");
    if !js_sys::Reflect::has(&descriptor, &value_key).map_err(|_| ())? {
        return Ok(OwnProperty::Accessor);
    }
    js_sys::Reflect::get(&descriptor, &value_key)
        .map(OwnProperty::Data)
        .map_err(|_| ())
}

fn is_plain_record(value: &JsValue, object_prototype: &JsValue) -> Result<bool, ()> {
    if !value.is_object() || try_array_is_array(value).map_err(|_| ())? {
        return Ok(false);
    }
    let prototype: JsValue = js_sys::Reflect::get_prototype_of(value)
        .map_err(|_| ())?
        .into();
    Ok(prototype.is_null() || &prototype == object_prototype)
}

fn own_data_property(
    target: &JsValue,
    key: &str,
    error: impl FnOnce() -> String,
) -> Result<JsValue, JsValue> {
    match own_property(target, &JsValue::from_str(key)) {
        Ok(OwnProperty::Data(value)) => Ok(value),
        Ok(OwnProperty::Missing | OwnProperty::Accessor) | Err(()) => {
            Err(JsValue::from_str(&error()))
        }
    }
}

fn set_snapshot_property(
    target: &js_sys::Object,
    key: &str,
    value: &JsValue,
    error: &'static str,
) -> Result<(), JsValue> {
    match js_sys::Reflect::set(target, &JsValue::from_str(key), value) {
        Ok(true) => Ok(()),
        Ok(false) | Err(_) => Err(JsValue::from_str(error)),
    }
}

/// Build a plain, bounded JS snapshot before Serde allocates Rust strings or
/// the `Vec<SearchFile>`. Only own data properties are accepted, so accessors
/// cannot change their answer between validation and deserialization.
fn snapshot_search_files(
    files: &JsValue,
    encoder: &SearchTextEncoder,
) -> Result<js_sys::Array, JsValue> {
    if !matches!(try_array_is_array(files), Ok(true)) {
        return Err(JsValue::from_str("invalid search files: expected an array"));
    }

    let length = own_data_property(files, "length", || {
        "invalid search files: array length must be an own data property".to_string()
    })?;
    let Some(length) = length.as_f64() else {
        return Err(JsValue::from_str(
            "invalid search files: array length must be an integer",
        ));
    };
    if !length.is_finite() || length.fract() != 0.0 || !(0.0..=u32::MAX as f64).contains(&length) {
        return Err(JsValue::from_str(
            "invalid search files: array length must be an integer",
        ));
    }
    let length = length as usize;
    if length > MAX_SEARCH_FILES {
        return Err(JsValue::from_str(&format!(
            "search accepts at most {MAX_SEARCH_FILES} files"
        )));
    }

    let object_prototype = js_sys::Object::get_prototype_of(&js_sys::Object::new());
    let snapshot = js_sys::Array::new();
    let mut total_content_bytes = 0usize;
    for index in 0..length {
        let entry = match own_property(files, &JsValue::from_f64(index as f64)) {
            Ok(OwnProperty::Data(value)) => value,
            Ok(OwnProperty::Missing | OwnProperty::Accessor) | Err(()) => {
                return Err(JsValue::from_str(&format!(
                    "invalid search files: entry {index} must be an own data property"
                )));
            }
        };
        match is_plain_record(&entry, object_prototype.as_ref()) {
            Ok(true) => {}
            Ok(false) | Err(()) => {
                return Err(JsValue::from_str(&format!(
                    "invalid search files: entry {index} must be a plain object"
                )));
            }
        }

        let path = own_data_property(&entry, "path", || {
            format!("invalid search files: entry {index} path must be an own data property")
        })?;
        if !path.is_string() {
            return Err(JsValue::from_str(&format!(
                "invalid search files: entry {index} path must be a string"
            )));
        }
        let path: js_sys::JsString = path.unchecked_into();
        match bounded_js_string_utf8_len(&path, encoder, MAX_SEARCH_PATH_BYTES) {
            Ok(BoundedUtf8Len::Bytes(_)) => {}
            Ok(BoundedUtf8Len::ExceedsLimit) => {
                return Err(JsValue::from_str(&format!(
                    "search path exceeds {MAX_SEARCH_PATH_BYTES} bytes"
                )));
            }
            Ok(BoundedUtf8Len::InvalidUtf16) => {
                return Err(JsValue::from_str(&format!(
                    "invalid search files: entry {index} path must be well-formed UTF-16"
                )));
            }
            Err(()) => {
                return Err(JsValue::from_str(&format!(
                    "invalid search files: entry {index} path byte length could not be measured"
                )));
            }
        }

        let content = own_data_property(&entry, "content", || {
            format!("invalid search files: entry {index} content must be an own data property")
        })?;
        if !content.is_string() {
            return Err(JsValue::from_str(&format!(
                "invalid search files: entry {index} content must be a string"
            )));
        }
        let content: js_sys::JsString = content.unchecked_into();
        let content_bytes =
            match bounded_js_string_utf8_len(&content, encoder, MAX_SEARCH_FILE_CONTENT_BYTES) {
                Ok(BoundedUtf8Len::Bytes(bytes)) => bytes,
                Ok(BoundedUtf8Len::ExceedsLimit) => {
                    return Err(JsValue::from_str(&format!(
                        "search file content exceeds {MAX_SEARCH_FILE_CONTENT_BYTES} bytes"
                    )));
                }
                Ok(BoundedUtf8Len::InvalidUtf16) => {
                    return Err(JsValue::from_str(&format!(
                        "invalid search files: entry {index} content must be well-formed UTF-16"
                    )));
                }
                Err(()) => {
                    let error = format!(
                    "invalid search files: entry {index} content byte length could not be measured"
                );
                    return Err(JsValue::from_str(&error));
                }
            };
        total_content_bytes = total_content_bytes.saturating_add(content_bytes);
        if total_content_bytes > MAX_SEARCH_TOTAL_CONTENT_BYTES {
            return Err(JsValue::from_str(&format!(
                "search aggregate content exceeds {MAX_SEARCH_TOTAL_CONTENT_BYTES} bytes"
            )));
        }

        let normalized = js_sys::Object::new();
        set_snapshot_property(
            &normalized,
            "path",
            path.as_ref(),
            "invalid search files: failed to snapshot path",
        )?;
        set_snapshot_property(
            &normalized,
            "content",
            content.as_ref(),
            "invalid search files: failed to snapshot content",
        )?;
        snapshot.push(normalized.as_ref());
    }

    Ok(snapshot)
}

fn search_option_data_property(
    options: &JsValue,
    key: &str,
    required: bool,
) -> Result<Option<JsValue>, JsValue> {
    match own_property(options, &JsValue::from_str(key)) {
        Ok(OwnProperty::Data(value)) => Ok(Some(value)),
        Ok(OwnProperty::Missing) if !required => Ok(None),
        Ok(OwnProperty::Missing | OwnProperty::Accessor) | Err(()) => Err(JsValue::from_str(
            &format!("invalid search options: {key} must be an own data property"),
        )),
    }
}

fn snapshot_search_boolean(
    input: &JsValue,
    output: &js_sys::Object,
    key: &str,
    default: Option<bool>,
) -> Result<(), JsValue> {
    let value = search_option_data_property(input, key, default.is_none())?;
    let value = match value {
        Some(value) if value.is_undefined() && default.is_some() => {
            JsValue::from_bool(default.expect("optional boolean has a default"))
        }
        Some(value) => value,
        None => JsValue::from_bool(default.expect("optional boolean has a default")),
    };
    if value.as_bool().is_none() {
        return Err(JsValue::from_str(&format!(
            "invalid search options: {key} must be a boolean"
        )));
    }
    set_snapshot_property(
        output,
        key,
        &value,
        "invalid search options: failed to snapshot boolean",
    )
}

/// Normalize search options into a fresh object so Serde never observes an
/// accessor or a second proxy result. `smartCase` retains its native default.
fn snapshot_search_options(options: &JsValue) -> Result<js_sys::Object, JsValue> {
    let object_prototype = js_sys::Object::get_prototype_of(&js_sys::Object::new());
    match is_plain_record(options, object_prototype.as_ref()) {
        Ok(true) => {}
        Ok(false) | Err(()) => {
            return Err(JsValue::from_str(
                "invalid search options: expected a plain object",
            ));
        }
    }

    let snapshot = js_sys::Object::new();
    snapshot_search_boolean(options, &snapshot, "caseSensitive", None)?;
    snapshot_search_boolean(options, &snapshot, "smartCase", Some(false))?;
    snapshot_search_boolean(options, &snapshot, "wholeWord", None)?;
    snapshot_search_boolean(options, &snapshot, "isRegex", None)?;

    let max_results = search_option_data_property(options, "maxResults", true)?
        .expect("required option was checked above");
    let Some(max_results_number) = max_results.as_f64() else {
        return Err(JsValue::from_str(
            "invalid search options: maxResults must be an integer",
        ));
    };
    if !max_results_number.is_finite() || max_results_number.fract() != 0.0 {
        return Err(JsValue::from_str(
            "invalid search options: maxResults must be an integer",
        ));
    }
    if !(0.0..=MAX_SEARCH_RESULTS as f64).contains(&max_results_number) {
        return Err(JsValue::from_str(&format!(
            "search maxResults must be between 0 and {MAX_SEARCH_RESULTS}"
        )));
    }
    set_snapshot_property(
        &snapshot,
        "maxResults",
        &max_results,
        "invalid search options: failed to snapshot maxResults",
    )?;

    Ok(snapshot)
}

/// One node of the hierarchical Outline: the walker's byte offsets resolved
/// to zero-based editor lines.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OutlineTreeNode {
    label: String,
    kind: &'static str,
    detail: Option<String>,
    line: u32,
    children: Vec<OutlineTreeNode>,
}

fn to_tree(
    node: pinewasm_lsp::outline::OutlineNode,
    index: &pinewasm_lsp::text::LineIndex,
) -> OutlineTreeNode {
    let (line, _) = index.line_utf16(node.span_start);
    OutlineTreeNode {
        label: node.label,
        kind: node.kind,
        detail: node.detail,
        line,
        children: node
            .children
            .into_iter()
            .map(|child| to_tree(child, index))
            .collect(),
    }
}

/// Hierarchical program outline from the AST (`crates/pinewasm_lsp`
/// outline.rs): declaration, imports, inputs grouped by their `group=`
/// titles, variables, functions with locals, types, and the plot/alert/
/// strategy output surface. Parse-tolerant; zero-based lines. Sources larger
/// than 1 MiB reject before indexing or parsing.
#[wasm_bindgen(unchecked_return_type = "ReadonlyArray<OutlineNode>")]
pub fn outline(source: &str) -> Result<JsValue, JsValue> {
    preflight_navigation_source(source)?;
    let index = pinewasm_lsp::text::LineIndex::new(source);
    let tree: Vec<OutlineTreeNode> = pinewasm_lsp::outline::outline_from_source(source)
        .into_iter()
        .map(|node| to_tree(node, &index))
        .collect();
    to_js(&tree)
}

/// Rewrite Pine v5 source as v6 (the `pinewasm migrate` command). Returns
/// the status, the migrated source, one text edit, and the findings that
/// need review, with UTF-8 byte offsets into `source`. `idiomatic` selects
/// conventional v6 spellings over keeping v5 results where
/// the two differ. Available only in builds with the `migrate` feature.
#[cfg(feature = "migrate")]
#[wasm_bindgen(js_name = migrateToV6, unchecked_return_type = "Migration")]
pub fn migrate_to_v6(source: &str, idiomatic: bool) -> Result<JsValue, JsValue> {
    preflight_navigation_source(source)?;
    let options = pinewasm_migrate::MigrationOptions {
        profile: if idiomatic {
            pinewasm_migrate::Profile::Idiomatic
        } else {
            pinewasm_migrate::Profile::Exact
        },
        ..pinewasm_migrate::MigrationOptions::default()
    };
    to_js(&pinewasm_migrate::migrate(source, &options))
}

/// Parse-only document symbols for the Outline view. Error-tolerant: a source
/// that no longer compiles still yields the symbols its parse tree carries.
/// Sources larger than 1 MiB reject before indexing or parsing.
#[wasm_bindgen(unchecked_return_type = "ReadonlyArray<DocumentSymbol>")]
pub fn symbols(source: &str) -> Result<JsValue, JsValue> {
    preflight_navigation_source(source)?;
    let index = pinewasm_lsp::text::LineIndex::new(source);
    let rows: Vec<OutlineSymbol> = pinewasm_lsp::analysis::symbols_from_source(source)
        .into_iter()
        .map(|symbol| {
            let (line, character) = index.line_utf16(symbol.span.start);
            let (end_line, _) = index.line_utf16(symbol.full_span.end);
            OutlineSymbol {
                name: symbol.name,
                kind: symbol.kind.label(),
                detail: symbol.detail,
                signature: symbol.signature,
                container: symbol.container,
                line,
                character,
                end_line,
            }
        })
        .collect();
    to_js(&rows)
}

/// Compile Pine Script® source. Returns the generated
/// `CompileResult` contract. Debug fields and `program` are always present on
/// that object and use `null` when their output was not requested or produced.
#[wasm_bindgen(unchecked_return_type = "CompileResult")]
pub fn compile(
    source: &str,
    #[wasm_bindgen(unchecked_optional_param_type = "CompileOptions | null")] options: JsValue,
) -> Result<JsValue, JsValue> {
    let options = options_from_js(options)?;
    let result =
        pinewasm_compiler::compile_with_libraries_profiled(source, &[], options, Some(&js_clock));
    to_js(&result)
}

/// Compile and execute against data described by the generated `MarketData`
/// contract. `mintick` defaults to 0.01. The generated `RunOptions` contract
/// covers budgets, input and external-source maps, realtime execution,
/// profiling, and strategy overrides. The cumulative instruction, string, and
/// VM-owned-memory defaults are one billion units, 32 MiB, and 256 MiB
/// respectively; exhaustion returns the corresponding structured runtime
/// error. `realtime` is `{ ticks: Tick[] }`; when
/// `realtimeTrace` is true the `RunResult` carries a per-tick `realtimeTrace`.
/// Returns a `RunResult`; plot values use `null` for `na`.
#[wasm_bindgen(js_name = compileAndRun, unchecked_return_type = "RunResult")]
pub fn compile_and_run(
    source: &str,
    #[wasm_bindgen(unchecked_param_type = "MarketData")] market_data: JsValue,
    #[wasm_bindgen(unchecked_optional_param_type = "CompileOptions | null")]
    compile_options: JsValue,
    #[wasm_bindgen(unchecked_optional_param_type = "RunOptions | null")] run_options: JsValue,
) -> Result<JsValue, JsValue> {
    let market_data: MarketData = serde_wasm_bindgen::from_value(market_data)
        .map_err(|e| JsValue::from_str(&format!("invalid market data: {e}")))?;
    let compile_options = options_from_js(compile_options)?;
    let run_options: RunOptions = if run_options.is_undefined() || run_options.is_null() {
        RunOptions::default()
    } else {
        serde_wasm_bindgen::from_value(run_options)
            .map_err(|e| JsValue::from_str(&format!("invalid run options: {e}")))?
    };

    let result = pinewasm_compiler::compile_and_run_with_libraries_profiled(
        source,
        &[],
        &market_data,
        compile_options,
        &run_options,
        Some(&js_clock),
    );
    to_js(&result)
}

/// Compile Pine Script® source with a set of importable libraries.
/// `libraries` is an array of `[importPath, source]` pairs, where `importPath`
/// is `Owner/Name/Version`; each `import` in `source` resolves against
/// this map **only** (no network). Returns a `CompileResult` (the same shape
/// as `compile`). An `import` whose path is absent from the map is an
/// unresolved-import diagnostic (`E0243`).
#[wasm_bindgen(js_name = compileWithLibraries, unchecked_return_type = "CompileResult")]
pub fn compile_with_libraries(
    source: &str,
    #[wasm_bindgen(unchecked_param_type = "Libraries | null | undefined")] libraries: JsValue,
    #[wasm_bindgen(unchecked_optional_param_type = "CompileOptions | null")] options: JsValue,
) -> Result<JsValue, JsValue> {
    let libraries = libraries_from_js(libraries)?;
    let options = options_from_js(options)?;
    let result = pinewasm_compiler::compile_with_libraries_profiled(
        source,
        &libraries,
        options,
        Some(&js_clock),
    );
    to_js(&result)
}

/// Compile source and retain its CSIL in this Wasm instance. The result has
/// the usual diagnostics, timings and requested debug fields; `program` is an
/// immutable `CompiledProgram` handle rather than serialized CSIL. Libraries
/// have exactly the same limits and resolution rules as `compileWithLibraries`.
/// Call the handle's `free()` when it is no longer needed.
#[wasm_bindgen(js_name = compileResident, unchecked_return_type = "ResidentCompileResult")]
pub fn compile_resident(
    source: &str,
    #[wasm_bindgen(unchecked_param_type = "Libraries | null | undefined")] libraries: JsValue,
    #[wasm_bindgen(unchecked_optional_param_type = "CompileOptions | null")] options: JsValue,
) -> Result<JsValue, JsValue> {
    let libraries = libraries_from_js(libraries)?;
    let options = options_from_js(options)?;
    let result = pinewasm_compiler::compile_with_libraries_profiled(
        source,
        &libraries,
        options,
        Some(&js_clock),
    );
    let (result, program) = retain_compilation(result);
    let out = to_js(&result)?;
    if let Some(program) = program {
        js_sys::Reflect::set(&out, &"program".into(), &JsValue::from(program))?;
    }
    Ok(out)
}

/// Move the exact compiler-authored program out before serializing the rest
/// of the result. This neither rewrites CSIL nor caches mutable runtime state.
fn retain_compilation(
    mut result: pinewasm_compiler::CompileResult,
) -> (pinewasm_compiler::CompileResult, Option<CompiledProgram>) {
    let program = result
        .program
        .take()
        .map(|program| CompiledProgram { program });
    (result, program)
}

/// Immutable compiler-authored CSIL retained in its originating Wasm instance.
/// Inspect small declarations through the getters, or explicitly serialize the
/// entire program with `exportProgram()`. Handles cannot cross workers or Wasm
/// instances. Free them on eviction; prepared VMs own independent code.
#[wasm_bindgen]
pub struct CompiledProgram {
    program: pinewasm_compiler::Program,
}

#[wasm_bindgen]
impl CompiledProgram {
    #[wasm_bindgen(getter, unchecked_return_type = "ReadonlyArray<InputDeclaration>")]
    pub fn inputs(&self) -> Result<JsValue, JsValue> {
        to_js(&self.program.inputs)
    }

    #[wasm_bindgen(getter, unchecked_return_type = "ReadonlyArray<CsilPlot>")]
    pub fn plots(&self) -> Result<JsValue, JsValue> {
        to_js(&self.program.plots)
    }

    #[wasm_bindgen(getter, unchecked_return_type = "ProgramMetadata")]
    pub fn metadata(&self) -> Result<JsValue, JsValue> {
        to_js(&self.program.metadata)
    }

    /// Emit the native chart module from the exact retained compiler output.
    /// No instruction program is serialized through JavaScript and re-imported.
    #[wasm_bindgen(js_name = compileChartWasm, unchecked_return_type = "CsawCompileResult")]
    pub fn compile_chart_wasm(&self) -> Result<JsValue, JsValue> {
        aot_result_to_js(
            pinewasm_aot::compile_chart_wasm_program(&self.program),
            true,
        )
    }

    #[wasm_bindgen(getter, unchecked_return_type = "ReadonlyArray<CsilPlot>")]
    pub fn visuals(&self) -> Result<JsValue, JsValue> {
        to_js(&self.program.visuals)
    }

    #[wasm_bindgen(getter, unchecked_return_type = "ReadonlyArray<CsilFill>")]
    pub fn fills(&self) -> Result<JsValue, JsValue> {
        to_js(&self.program.fills)
    }

    /// Export the full versioned program only when inspection or transport
    /// requires it. Mutating this copy never changes the retained program.
    #[wasm_bindgen(js_name = exportProgram, unchecked_return_type = "Program")]
    pub fn export_program(&self) -> Result<JsValue, JsValue> {
        to_js(&self.program)
    }

    /// Keep the ordinary prepared-VM validator and fresh-state run contract.
    /// The clone is within Rust; no instruction crosses the JS boundary.
    #[wasm_bindgen(js_name = prepareVm)]
    pub fn prepare_vm(&self) -> Result<PreparedProgram, JsValue> {
        let inner = pinewasm_compiler::PreparedProgram::new(self.program.clone())
            .map_err(|e| JsValue::from_str(&format!("invalid program: {e}")))?;
        Ok(PreparedProgram { inner })
    }

    /// Use the same standalone planner, validation, emission and byte audit
    /// as `compileStandaloneWasmProgram`, borrowing the retained CSIL.
    #[wasm_bindgen(js_name = compileStandaloneWasm, unchecked_return_type = "StandaloneWasmCompileResult")]
    pub fn compile_standalone_wasm(
        &self,
        #[wasm_bindgen(unchecked_param_type = "StandaloneWasmOptions")] options: JsValue,
    ) -> Result<JsValue, JsValue> {
        standalone_wasm_result_to_js(&self.program, options)
    }
}

/// Decode the JS `libraries` argument (an array of `[importPath, source]`
/// pairs) into the `(String, String)` vector the compiler expects. `undefined`
/// / `null` decode to an empty list (equivalent to a single-source compile).
fn libraries_from_js(libraries: JsValue) -> Result<Vec<(String, String)>, JsValue> {
    if libraries.is_undefined() || libraries.is_null() {
        return Ok(Vec::new());
    }
    let encoder = SearchTextEncoder::new()
        .map_err(|_| JsValue::from_str("invalid libraries: UTF-8 preflight is unavailable"))?;
    let snapshot = snapshot_libraries_js(&libraries, &encoder)?;
    serde_wasm_bindgen::from_value(snapshot.into())
        .map_err(|e| JsValue::from_str(&format!("invalid libraries: {e}")))
}

/// Read an array's actual own length without invoking its iterator or getters.
/// Proxy traps remain fallible; callers never reread the array during Serde.
fn library_array_length(value: &JsValue, context: &str) -> Result<usize, JsValue> {
    if !matches!(try_array_is_array(value), Ok(true)) {
        return Err(JsValue::from_str(&format!(
            "invalid libraries: {context} must be an array"
        )));
    }
    let length = own_data_property(value, "length", || {
        format!("invalid libraries: {context} length must be an own data property")
    })?;
    let Some(length) = length.as_f64() else {
        return Err(JsValue::from_str(&format!(
            "invalid libraries: {context} length must be an integer"
        )));
    };
    if !length.is_finite() || length.fract() != 0.0 || !(0.0..=u32::MAX as f64).contains(&length) {
        return Err(JsValue::from_str(&format!(
            "invalid libraries: {context} length must be an integer"
        )));
    }
    Ok(length as usize)
}

fn library_string_bytes(
    value: &JsValue,
    encoder: &SearchTextEncoder,
    limit: usize,
    index: usize,
    field: &str,
) -> Result<usize, JsValue> {
    if !value.is_string() {
        return Err(JsValue::from_str(&format!(
            "invalid libraries: entry {index} {field} must be a string"
        )));
    }
    let value: &js_sys::JsString = value.unchecked_ref();
    match bounded_js_string_utf8_len(value, encoder, limit) {
        Ok(BoundedUtf8Len::Bytes(bytes)) => Ok(bytes),
        Ok(BoundedUtf8Len::ExceedsLimit) => Err(JsValue::from_str(&format!(
            "invalid libraries: entry {index} {field} exceeds {limit} bytes"
        ))),
        Ok(BoundedUtf8Len::InvalidUtf16) => Err(JsValue::from_str(&format!(
            "invalid libraries: entry {index} {field} must be well-formed UTF-16"
        ))),
        Err(()) => Err(JsValue::from_str(&format!(
            "invalid libraries: entry {index} {field} byte length could not be measured"
        ))),
    }
}

/// Snapshot bounded own data properties before Serde copies library strings
/// into Wasm. Accessors, iterators, sparse entries and throwing proxies cannot
/// bypass preflight or change a checked value during deserialization.
fn snapshot_libraries_js(
    libraries: &JsValue,
    encoder: &SearchTextEncoder,
) -> Result<js_sys::Array, JsValue> {
    let length = library_array_length(libraries, "library table")?;
    if length > MAX_LIBRARY_COUNT {
        return Err(JsValue::from_str(&format!(
            "invalid libraries: {length} entries exceed the maximum {MAX_LIBRARY_COUNT}"
        )));
    }
    let snapshot = js_sys::Array::new();
    let mut source_bytes = 0usize;
    for index in 0..length {
        let value = own_data_property(libraries, &index.to_string(), || {
            format!("invalid libraries: entry {index} must be an own data property")
        })?;
        if library_array_length(&value, &format!("entry {index}"))? != 2 {
            return Err(JsValue::from_str(&format!(
                "invalid libraries: entry {index} must contain exactly two strings"
            )));
        }
        let path = own_data_property(&value, "0", || {
            format!("invalid libraries: entry {index} path must be an own data property")
        })?;
        library_string_bytes(&path, encoder, MAX_LIBRARY_PATH_LEN, index, "path")?;
        let source = own_data_property(&value, "1", || {
            format!("invalid libraries: entry {index} source must be an own data property")
        })?;
        source_bytes = source_bytes.saturating_add(library_string_bytes(
            &source,
            encoder,
            MAX_SOURCE_LEN,
            index,
            "source",
        )?);
        if source_bytes > MAX_LIBRARY_SOURCE_BYTES {
            return Err(JsValue::from_str(&format!(
                "invalid libraries: sources exceed {MAX_LIBRARY_SOURCE_BYTES} bytes in total"
            )));
        }
        let pair = js_sys::Array::new();
        pair.push(&path);
        pair.push(&source);
        snapshot.push(pair.as_ref());
    }
    Ok(snapshot)
}

/// Compile against a set of importable libraries and, on success, execute —
/// the library-linking counterpart of `compileAndRun`. `libraries` follows
/// `compileWithLibraries` (an array of `[importPath, source]` pairs);
/// `market_data` and `run_options` follow `compileAndRun`. Returns a
/// `RunResult`; diagnostics attribute to sources via `span.source` (0 = main
/// buffer, `i + 1` = `libraries[i]`).
#[wasm_bindgen(
    js_name = compileAndRunWithLibraries,
    unchecked_return_type = "RunResult"
)]
pub fn compile_and_run_with_libraries(
    source: &str,
    #[wasm_bindgen(unchecked_param_type = "Libraries | null | undefined")] libraries: JsValue,
    #[wasm_bindgen(unchecked_param_type = "MarketData")] market_data: JsValue,
    #[wasm_bindgen(unchecked_optional_param_type = "CompileOptions | null")]
    compile_options: JsValue,
    #[wasm_bindgen(unchecked_optional_param_type = "RunOptions | null")] run_options: JsValue,
) -> Result<JsValue, JsValue> {
    let libraries = libraries_from_js(libraries)?;
    let market_data: MarketData = serde_wasm_bindgen::from_value(market_data)
        .map_err(|e| JsValue::from_str(&format!("invalid market data: {e}")))?;
    let compile_options = options_from_js(compile_options)?;
    let run_options: RunOptions = if run_options.is_undefined() || run_options.is_null() {
        RunOptions::default()
    } else {
        serde_wasm_bindgen::from_value(run_options)
            .map_err(|e| JsValue::from_str(&format!("invalid run options: {e}")))?
    };

    let result = pinewasm_compiler::compile_and_run_with_libraries_profiled(
        source,
        &libraries,
        &market_data,
        compile_options,
        &run_options,
        Some(&js_clock),
    );
    to_js(&result)
}

/// Execute an already-compiled program (`CompileResult.program`, previously
/// returned by `compile`/`compileAndRun`) against fresh market data /
/// run options WITHOUT recompiling — CSIL is a durable, versioned artifact,
/// and this is its cached-rerun entry point (input-override changes route
/// here). The program is re-validated before execution; a tampered or
/// version-skewed program yields a structured `InvalidProgram` runtime error
/// in the returned `RunResult`, never a panic.
#[wasm_bindgen(js_name = runProgram, unchecked_return_type = "RunResult")]
pub fn run_program(
    #[wasm_bindgen(unchecked_param_type = "Program")] program: JsValue,
    #[wasm_bindgen(unchecked_param_type = "MarketData")] market_data: JsValue,
    #[wasm_bindgen(unchecked_optional_param_type = "RunOptions | null")] run_options: JsValue,
) -> Result<JsValue, JsValue> {
    let program: pinewasm_compiler::Program = serde_wasm_bindgen::from_value(program)
        .map_err(|e| JsValue::from_str(&format!("invalid program: {e}")))?;
    let market_data: MarketData = serde_wasm_bindgen::from_value(market_data)
        .map_err(|e| JsValue::from_str(&format!("invalid market data: {e}")))?;
    let run_options: RunOptions = if run_options.is_undefined() || run_options.is_null() {
        RunOptions::default()
    } else {
        serde_wasm_bindgen::from_value(run_options)
            .map_err(|e| JsValue::from_str(&format!("invalid run options: {e}")))?
    };
    let result = pinewasm_compiler::run_program_profiled(
        &program,
        &market_data,
        &run_options,
        Some(&js_clock),
    );
    to_js(&result)
}

/// Immutable validated code held in Wasm. Call `free()` on eviction; each run
/// still creates fresh evaluation and broker state, including input overrides.
#[wasm_bindgen]
pub struct PreparedProgram {
    inner: pinewasm_compiler::PreparedProgram,
}

#[wasm_bindgen]
impl PreparedProgram {
    #[wasm_bindgen(constructor)]
    pub fn new(
        #[wasm_bindgen(unchecked_param_type = "Program")] program: JsValue,
    ) -> Result<PreparedProgram, JsValue> {
        let program = serde_wasm_bindgen::from_value(program)
            .map_err(|e| JsValue::from_str(&format!("invalid program: {e}")))?;
        let inner = pinewasm_compiler::PreparedProgram::new(program)
            .map_err(|e| JsValue::from_str(&format!("invalid program: {e}")))?;
        Ok(Self { inner })
    }

    /// Bulk input uses six contiguous f64 lanes: open/high/low/close/volume/time.
    /// Metadata is a normal `MarketData` object with empty primary lanes. All
    /// optional host feeds remain supported. Times must be safe integer ms.
    /// `numeric_plots` explicitly omits visual/color/drawing output channels;
    /// the full strategy report and diagnostics remain available. Numeric
    /// output lanes are owned `Float64Array` values, independent of later memory grows.
    #[wasm_bindgen(js_name = runColumns, unchecked_return_type = "RunResult | NumericRunResult")]
    pub fn run_columns(
        &self,
        columns: &[f64],
        #[wasm_bindgen(unchecked_param_type = "MarketData")] metadata: JsValue,
        #[wasm_bindgen(unchecked_param_type = "RunOptions | null | undefined")] options: JsValue,
        numeric_plots: bool,
    ) -> Result<JsValue, JsValue> {
        let data = market_data_from_columns(columns, metadata)?;
        let options: RunOptions = if options.is_undefined() || options.is_null() {
            RunOptions::default()
        } else {
            serde_wasm_bindgen::from_value(options)
                .map_err(|e| JsValue::from_str(&format!("invalid run options: {e}")))?
        };
        let result = self.inner.run_profiled(&data, &options, Some(&js_clock));
        if numeric_plots {
            numeric_result_to_js(result)
        } else {
            to_js(&result)
        }
    }
}

/// Immutable owned market data for repeated program evaluations. The primary
/// dataset is limited to one million bars; optional host feeds use the normal
/// `MarketData` contract. Call `free()` when the campaign is finished.
#[wasm_bindgen]
pub struct PreparedDataset {
    inner: MarketData,
}

#[wasm_bindgen]
impl PreparedDataset {
    #[wasm_bindgen(constructor)]
    pub fn new(
        columns: &[f64],
        #[wasm_bindgen(unchecked_param_type = "MarketData")] metadata: JsValue,
    ) -> Result<PreparedDataset, JsValue> {
        if columns.is_empty() || columns.len() > 6_000_000 {
            return Err(JsValue::from_str(
                "resident data requires 1 to 1000000 bars",
            ));
        }
        Ok(Self {
            inner: market_data_from_columns(columns, metadata)?,
        })
    }

    #[wasm_bindgen(js_name = barCount)]
    pub fn bar_count(&self) -> usize {
        self.inner.close.len()
    }

    /// Owned primary-lane bytes only; excludes optional metadata and host feeds.
    #[wasm_bindgen(js_name = primaryBytes)]
    pub fn primary_bytes(&self) -> usize {
        self.inner.close.len() * 48
    }
}

#[wasm_bindgen]
impl PreparedProgram {
    /// Reuse immutable market data without copying it across the JS boundary.
    /// Every evaluation still creates fresh runtime, input and broker state.
    #[wasm_bindgen(js_name = runDataset, unchecked_return_type = "RunResult | NumericRunResult")]
    pub fn run_dataset(
        &self,
        data: &PreparedDataset,
        #[wasm_bindgen(unchecked_param_type = "RunOptions | null | undefined")] options: JsValue,
        numeric_plots: bool,
    ) -> Result<JsValue, JsValue> {
        let options: RunOptions = if options.is_undefined() || options.is_null() {
            RunOptions::default()
        } else {
            serde_wasm_bindgen::from_value(options)
                .map_err(|e| JsValue::from_str(&format!("invalid run options: {e}")))?
        };
        let result = self
            .inner
            .run_profiled(&data.inner, &options, Some(&js_clock));
        if numeric_plots {
            numeric_result_to_js(result)
        } else {
            to_js(&result)
        }
    }
}

fn market_data_from_columns(columns: &[f64], metadata: JsValue) -> Result<MarketData, JsValue> {
    if columns.len() % 6 != 0 {
        return Err(JsValue::from_str("numeric input requires six equal lanes"));
    }
    let n = columns.len() / 6;
    let mut data: MarketData = serde_wasm_bindgen::from_value(metadata)
        .map_err(|e| JsValue::from_str(&format!("invalid market metadata: {e}")))?;
    if !data.open.is_empty()
        || !data.high.is_empty()
        || !data.low.is_empty()
        || !data.close.is_empty()
        || !data.volume.is_empty()
        || !data.time.is_empty()
    {
        return Err(JsValue::from_str("primary metadata lanes must be empty"));
    }
    data.open = columns[..n].to_vec();
    data.high = columns[n..n * 2].to_vec();
    data.low = columns[n * 2..n * 3].to_vec();
    data.close = columns[n * 3..n * 4].to_vec();
    data.volume = columns[n * 4..n * 5].to_vec();
    for &time in &columns[n * 5..] {
        if !time.is_finite() || time.abs() > 9_007_199_254_740_991.0 || time != (time as i64) as f64
        {
            return Err(JsValue::from_str("numeric time must be a safe integer"));
        }
        data.time.push(time as i64);
    }
    Ok(data)
}

fn numeric_result_to_js(mut result: pinewasm_compiler::RunResult) -> Result<JsValue, JsValue> {
    use pinewasm_compiler::Value;
    let mut lanes = Vec::with_capacity(result.plots.len());
    for plot in &mut result.plots {
        let mut lane = Vec::with_capacity(plot.values.len());
        for value in std::mem::take(&mut plot.values) {
            lane.push(match value {
                Value::Na => f64::NAN,
                Value::Float(v) => v,
                Value::Int(v) if v.unsigned_abs() <= 9_007_199_254_740_991 => v as f64,
                _ => {
                    return Err(JsValue::from_str(
                        "numeric projection requires numeric plots with safe integers",
                    ))
                }
            });
        }
        plot.colors.clear();
        lanes.push(lane);
    }
    result.visuals.clear();
    result.fills.clear();
    result.drawings = pinewasm_compiler::Drawings::default();
    let strategy_lanes = result.strategy.as_mut().map(|s| {
        (
            std::mem::take(&mut s.equity),
            std::mem::take(&mut s.margin_used),
        )
    });
    let out = to_js(&result)?;
    let plots = js_sys::Array::from(&js_sys::Reflect::get(&out, &"plots".into())?);
    for (i, lane) in lanes.iter().enumerate() {
        let array = js_sys::Float64Array::from(lane.as_slice());
        js_sys::Reflect::set(&plots.get(i as u32), &"values".into(), &array)?;
    }
    if let Some((equity, margin)) = strategy_lanes {
        let strategy = js_sys::Reflect::get(&out, &"strategy".into())?;
        js_sys::Reflect::set(
            &strategy,
            &"equity".into(),
            &js_sys::Float64Array::from(equity.as_slice()),
        )?;
        js_sys::Reflect::set(
            &strategy,
            &"marginUsed".into(),
            &js_sys::Float64Array::from(margin.as_slice()),
        )?;
    }
    Ok(out)
}

/// Worker-owned persistent VM session. The generated JS class has a
/// `free()` method; hosts must call it when a script closes or a bounded
/// worker cache evicts the session.
#[wasm_bindgen]
pub struct IncrementalSession {
    inner: pinewasm_compiler::IncrementalSession,
}

#[wasm_bindgen]
impl IncrementalSession {
    /// Validate and install a compiled program, then execute the confirmed
    /// prefix of bars exactly once.
    #[wasm_bindgen(constructor)]
    pub fn new(
        #[wasm_bindgen(unchecked_param_type = "Program")] program: JsValue,
        #[wasm_bindgen(unchecked_param_type = "MarketData")] market_data: JsValue,
        #[wasm_bindgen(unchecked_optional_param_type = "RunOptions | null")] run_options: JsValue,
    ) -> Result<IncrementalSession, JsValue> {
        let program: pinewasm_compiler::Program = serde_wasm_bindgen::from_value(program)
            .map_err(|error| JsValue::from_str(&format!("invalid program: {error}")))?;
        let market_data: MarketData = serde_wasm_bindgen::from_value(market_data)
            .map_err(|error| JsValue::from_str(&format!("invalid market data: {error}")))?;
        let run_options: RunOptions = if run_options.is_undefined() || run_options.is_null() {
            RunOptions::default()
        } else {
            serde_wasm_bindgen::from_value(run_options)
                .map_err(|error| JsValue::from_str(&format!("invalid run options: {error}")))?
        };
        let inner =
            pinewasm_compiler::IncrementalSession::new(&program, &market_data, &run_options)
                .map_err(|error| {
                    JsValue::from_str(&format!("incremental session rejected: {}", error.message))
                })?;
        Ok(IncrementalSession { inner })
    }

    /// Current result after history initialization or the latest advance.
    #[wasm_bindgen(unchecked_return_type = "RunResult")]
    pub fn result(&self) -> Result<JsValue, JsValue> {
        to_js(&self.inner.result())
    }

    /// Advance with an ordered array of realtime tick snapshots.
    #[wasm_bindgen(unchecked_return_type = "RunResult")]
    pub fn advance(
        &mut self,
        #[wasm_bindgen(unchecked_param_type = "ReadonlyArray<Tick>")] ticks: JsValue,
    ) -> Result<JsValue, JsValue> {
        let ticks: Vec<pinewasm_compiler::Tick> = serde_wasm_bindgen::from_value(ticks)
            .map_err(|error| JsValue::from_str(&format!("invalid realtime ticks: {error}")))?;
        to_js(&self.inner.advance_profiled(&ticks, Some(&js_clock)))
    }

    #[wasm_bindgen(getter, js_name = committedBars)]
    pub fn committed_bars(&self) -> u32 {
        self.inner.committed_bars()
    }

    #[wasm_bindgen(getter, js_name = residentBars)]
    pub fn resident_bars(&self) -> usize {
        self.inner.resident_bars()
    }

    #[wasm_bindgen(getter, js_name = failed)]
    pub fn failed(&self) -> bool {
        self.inner.is_failed()
    }
}

/// Ahead-of-time compile an already-compiled program (`CompileResult.program`
/// / the artifact envelope's payload) into a per-script WebAssembly
/// module (docs/aot.md). For every valid program, returns a wasm byte array
/// plus its ABI/layout metadata: `abiVersion`, `slotCount`, `plotCount`,
/// `ring`, `maxStack`, `executionMode`, `nativeBlocker`, `linksTa`,
/// `linksRuntime`, `alertSites`, `importsVm`, `hostSites`,
/// `hostSitesInLoops`, `nativeHeap`,
/// `nativeHeapHot`, `distanceScanKernels`, `vmHeapResidual`, `importsInterpreter`, `hostBroker`,
/// `linkedVm`, `scratchCells`, and `sourceInputs`. `hostBroker` marks a strategy chart module whose broker is
/// the `AotHost`'s: read its results with `AotHost.drainStrategy`. `executionMode` is `native` when strict lowering succeeds and
/// `hosted` when the module's `run` delegates to the PineWASM interpreter;
/// hosted results retain the first native blocker as optimization metadata.
/// `{ unsupported: string }` is reserved for invalid/tampered programs.
#[wasm_bindgen(
    js_name = compileProgramToWasm,
    unchecked_return_type = "CsawCompileResult"
)]
pub fn compile_program_to_wasm(
    #[wasm_bindgen(unchecked_param_type = "Program")] program: JsValue,
) -> Result<JsValue, JsValue> {
    let program: pinewasm_compiler::Program = serde_wasm_bindgen::from_value(program)
        .map_err(|e| JsValue::from_str(&format!("invalid program: {e}")))?;
    aot_result_to_js(pinewasm_aot::compile_program_to_wasm(&program), false)
}

/// The `{data, runOptions, bars}` JSON that a linked chart module's
/// `chartInit` reads. Pass the live `runOptions` object: its `Map`-backed
/// `inputs` and `externalSources` are encoded as objects here, which
/// `JSON.stringify` would turn into `{}`.
#[wasm_bindgen(js_name = chartRequestJson)]
pub fn chart_request_json(
    #[wasm_bindgen(unchecked_param_type = "MarketData")] data: JsValue,
    #[wasm_bindgen(unchecked_param_type = "RunOptions | null | undefined")] run_options: JsValue,
    bars: u32,
) -> Result<String, JsValue> {
    let data: pinewasm_vm::MarketData = serde_wasm_bindgen::from_value(data)
        .map_err(|e| JsValue::from_str(&format!("invalid market data: {e}")))?;
    // As in `runProgram`, an absent option set means the defaults.
    let options: pinewasm_vm::RunOptions = if run_options.is_undefined() || run_options.is_null() {
        pinewasm_vm::RunOptions::default()
    } else {
        serde_wasm_bindgen::from_value(run_options)
            .map_err(|e| JsValue::from_str(&format!("invalid run options: {e}")))?
    };
    Ok(pinewasm_vm::chart_request_json(
        &data,
        &options,
        bars as usize,
    ))
}

/// `compileProgramToWasm` for a chart module that imports nothing: the
/// VM that an ordinary native chart module calls through its
/// `host.vm` import is linked into the module instead (docs/aot.md §14).
///
/// The result has the shape `compileProgramToWasm` returns, with `linkedVm:
/// true` and `importsVm: false`. The module exports `chartAlloc`,
/// `chartInit` and `chartCommand` beside `memory`, `prepare` and `run`: its
/// loader passes the run's request to `chartInit` and reads the host's side
/// of the result through `chartCommand`, where the loader of an importing
/// module calls `AotHost`.
///
/// A linked host keeps full colors and resolves string and color inputs
/// before the run, so a program whose colors send `compileProgramToWasm` to
/// the interpreter-hosted tier compiles natively here. Returns `{
/// unsupported: string }` for a program the native tier refuses, and from a
/// compiler built without the chart runtime.
#[wasm_bindgen(
    js_name = compileChartWasmProgram,
    unchecked_return_type = "CsawCompileResult"
)]
pub fn compile_chart_wasm_program(
    #[wasm_bindgen(unchecked_param_type = "Program")] program: JsValue,
) -> Result<JsValue, JsValue> {
    let program: pinewasm_compiler::Program = serde_wasm_bindgen::from_value(program)
        .map_err(|e| JsValue::from_str(&format!("invalid program: {e}")))?;
    aot_result_to_js(pinewasm_aot::compile_chart_wasm_program(&program), true)
}

/// The JavaScript result of an AOT compile: the module's bytes and layout
/// metadata, or `{ unsupported }`. `linked_vm` says the module links its own
/// host.
fn aot_result_to_js(
    result: Result<pinewasm_aot::AotModule, pinewasm_aot::AotError>,
    linked_vm: bool,
) -> Result<JsValue, JsValue> {
    let out = js_sys::Object::new();
    let set = |key: &str, value: JsValue| {
        // Reflect::set only fails on non-objects; `out` is always an object.
        let _ = js_sys::Reflect::set(&out, &JsValue::from_str(key), &value);
    };
    match result {
        Ok(module) => {
            let layout = &module.layout;
            set("linkedVm", JsValue::from_bool(linked_vm));
            set(
                "wasm",
                js_sys::Uint8Array::from(module.wasm.as_slice()).into(),
            );
            set(
                "abiVersion",
                JsValue::from_f64(f64::from(pinewasm_aot::header::ABI_VERSION)),
            );
            set("slotCount", JsValue::from_f64(f64::from(layout.slot_count)));
            set("plotCount", JsValue::from_f64(f64::from(layout.plot_count)));
            set("ring", JsValue::from_f64(f64::from(layout.ring)));
            set("maxStack", JsValue::from_f64(f64::from(layout.max_stack)));
            set(
                "executionMode",
                JsValue::from_str(match layout.execution_mode {
                    pinewasm_aot::ExecutionMode::Native => "native",
                    pinewasm_aot::ExecutionMode::InterpreterHosted => "hosted",
                }),
            );
            set(
                "nativeBlocker",
                layout
                    .native_blocker
                    .as_deref()
                    .map_or(JsValue::NULL, JsValue::from_str),
            );
            set("linksTa", JsValue::from_bool(layout.links_ta));
            set("linksRuntime", JsValue::from_bool(layout.links_runtime));
            set(
                "alertSites",
                JsValue::from_f64(f64::from(layout.alert_sites)),
            );
            set("importsVm", JsValue::from_bool(layout.imports_vm));
            set(
                "hostSites",
                JsValue::from_f64(layout.host_sites.len() as f64),
            );
            set(
                "hostSitesInLoops",
                JsValue::from_f64(f64::from(layout.host_sites_in_loops)),
            );
            set("nativeHeap", JsValue::from_bool(layout.native_heap));
            set("nativeHeapHot", JsValue::from_bool(layout.native_heap_hot));
            set(
                "distanceScanKernels",
                JsValue::from_f64(f64::from(layout.distance_scan_kernels)),
            );
            set(
                "vmHeapResidual",
                JsValue::from_bool(layout.vm_heap_residual),
            );
            set(
                "importsInterpreter",
                JsValue::from_bool(layout.imports_interpreter),
            );
            set("hostBroker", JsValue::from_bool(layout.host_broker));
            set(
                "scratchCells",
                JsValue::from_f64(f64::from(layout.scratch_cells)),
            );
            let sources = js_sys::Array::new();
            for input in &layout.source_inputs {
                sources.push(&JsValue::from_f64(f64::from(*input)));
            }
            set("sourceInputs", sources.into());
            let styles = js_sys::Array::new();
            for slot in &layout.style_slots {
                let item = js_sys::Object::new();
                for (key, value) in [
                    ("slot", slot.slot),
                    ("tag", u32::from(slot.tag)),
                    ("offset", slot.offset),
                    ("mask", slot.mask),
                ] {
                    js_sys::Reflect::set(
                        &item,
                        &JsValue::from_str(key),
                        &JsValue::from_f64(f64::from(value)),
                    )?;
                }
                styles.push(&item);
            }
            set("styleSlots", styles.into());
        }
        Err(reason) => set("unsupported", JsValue::from_str(&reason.to_string())),
    }
    Ok(out.into())
}

/// The JS `{ signal_plot, module_type, signal_semantics?,
/// max_collection_elements? }` argument of `compileStandaloneWasmProgram`.
///
/// Deliberately `snake_case`: this mirrors `pinewasm_aot::StandaloneWasmOptions` field
/// for field (it is an options object, not one of the camelCase wire DTOs
/// `docs/browser.md` documents), so the two cannot drift in naming.
#[derive(Deserialize)]
struct StandaloneWasmOptionsInput {
    /// The plot whose per-bar posture becomes the OUT `signals` series.
    /// Absent/`null` defaults to plot 0 iff the program has exactly one
    /// plot; a multi-plot indicator classified as a strategy must designate
    /// explicitly. A `strategy()` source must leave this absent.
    #[serde(default)]
    signal_plot: Option<usize>,
    module_type: StandaloneWasmModuleTypeInput,
    /// Designated indicator plot event contract. Omission preserves the
    /// original reversal-only `position-events` behavior.
    #[serde(default)]
    signal_semantics: Option<StandaloneWasmSignalSemanticsInput>,
    /// Bounded native-heap element budget baked into the standalone WebAssembly artifact.
    #[serde(default)]
    max_collection_elements: Option<u32>,
    /// Compile only what can reach the designated plot. Omission means
    /// `true`: a host asking for a signal wants display code out of the
    /// way. `false` keeps every instruction and every plot lane.
    #[serde(default)]
    signal_slice: Option<bool>,
}

/// The standalone WebAssembly consumer-surface classification, as the JS strings `'strategy'` /
/// `'indicator'`.
#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum StandaloneWasmModuleTypeInput {
    Strategy,
    Indicator,
}

/// The designated indicator plot's standalone WebAssembly event contract.
#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "kebab-case")]
enum StandaloneWasmSignalSemanticsInput {
    PositionEvents,
    PostureEvents,
}

/// SHA-256 as lowercase hex so a host can compare artifact identity without
/// reimplementing the producer's byte normalization.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Classify a standalone WebAssembly refusal into `(code, imports, feature)`.
///
/// The code is the machine-readable half of the contract, so it is derived
/// from the error *variant*, never from its text. `imports` is the one
/// detail a host cannot re-derive (it has no emitter): both producers of
/// `StandaloneWasmNotStandalone` format the offending module names as a `, `-joined
/// tail after a fixed prefix, and that tail is lifted off here rather than
/// pattern-matched loosely — an unrecognized shape yields `None`, never a
/// guess. `feature` is `StandaloneWasmStrategyFeatureUnsupported`'s structured payload:
/// the named strategy feature keeping this script out of the native tier.
/// The error of a `StandaloneWasmNotStandalone` refusal names no call, so its
/// `feature` is filled in from the eligibility refusal
/// ([`compile_standalone_wasm_bytes`]).
fn classify_standalone_wasm_refusal(
    error: &pinewasm_aot::AotError,
) -> (&'static str, Option<Vec<String>>, Option<String>) {
    use pinewasm_aot::AotError;
    match error {
        AotError::StandaloneWasmNotStandalone(detail) => {
            let names = detail
                // `compile_standalone_wasm_program`'s up-front planner check.
                .strip_prefix("program would import ")
                // `audit_standalone_wasm`'s bytes-not-flags backstop.
                .or_else(|| detail.strip_prefix("module imports "))
                .map(|tail| tail.split(", ").map(str::to_string).collect());
            ("StandaloneWasmNotStandalone", names, None)
        }
        AotError::StandaloneWasmSourceInputUnsupported(_) => {
            ("StandaloneWasmSourceInputUnsupported", None, None)
        }
        AotError::StandaloneWasmNoDesignatedPlot(_) => {
            ("StandaloneWasmNoDesignatedPlot", None, None)
        }
        AotError::StandaloneWasmInvalidOptions(_) => {
            ("StandaloneWasmProgramUnsupported", None, None)
        }
        // The emitted bytes failed their own producer-side gate: an engine
        // bug to report, not a script to fix.
        AotError::StandaloneWasmExportSurface(_)
        | AotError::StandaloneWasmMetadata(_)
        | AotError::NotStandalone(_) => ("StandaloneWasmAuditFailed", None, None),
        // A strategy() source compiles to standalone WebAssembly natively; what
        // remains is the per-feature gate — calc_on_order_fills, non-default
        // currency, the per-trade accessors — each named so the dialog can
        // say exactly which declaration argument to change. This narrowed
        // code replaces the former blanket strategy refusal.
        AotError::StrategyFeatureUnsupported { feature } => (
            "StandaloneWasmStrategyFeatureUnsupported",
            None,
            Some(feature.clone()),
        ),
        // Everything else the shared AOT front end rejects: an opcode outside
        // the subset, an invalid/tampered program, CSIL-version skew.
        _ => ("StandaloneWasmProgramUnsupported", None, None),
    }
}

/// Where a standalone WebAssembly refusal points, as the `site` property of
/// the `refused` object: exactly one of `{ pc }`, `{ input }` or `{ plot }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StandaloneWasmRefusalSite {
    /// An instruction index in the program the compiler examined. With
    /// `signal_slice` that may be the signal slice, whose indices differ
    /// from the supplied program's, so this is a diagnostic, not a source
    /// position.
    Pc(usize),
    /// The id of an input declaration.
    Input(u32),
    /// The plot index the caller passed as `signal_plot`.
    Plot(usize),
}

impl StandaloneWasmRefusalSite {
    /// The property name and its value on the wire.
    fn wire(self) -> (&'static str, f64) {
        match self {
            Self::Pc(pc) => ("pc", pc as f64),
            Self::Input(id) => ("input", f64::from(id)),
            Self::Plot(index) => ("plot", index as f64),
        }
    }
}

impl From<pinewasm_aot::StandaloneRefusalSite> for StandaloneWasmRefusalSite {
    fn from(site: pinewasm_aot::StandaloneRefusalSite) -> Self {
        use pinewasm_aot::StandaloneRefusalSite;
        match site {
            StandaloneRefusalSite::Instruction { pc } | StandaloneRefusalSite::HostSite { pc } => {
                Self::Pc(pc)
            }
            StandaloneRefusalSite::Input { id } => Self::Input(id),
            StandaloneRefusalSite::Plot { index } => Self::Plot(index),
        }
    }
}

/// The `class` property of the `refused` object.
///
/// The match is exhaustive on purpose: a class added to the compiler must be
/// given a wire name before this crate compiles.
fn standalone_wasm_refusal_class(class: pinewasm_aot::RefusalClass) -> &'static str {
    match class {
        pinewasm_aot::RefusalClass::Capability => "capability",
        pinewasm_aot::RefusalClass::Internal => "internal",
    }
}

/// Everything `compileStandaloneWasmProgram` puts in its `refused` object,
/// before it becomes a JavaScript value.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StandaloneWasmRefusalDetail {
    code: &'static str,
    message: String,
    imports: Option<Vec<String>>,
    feature: Option<String>,
    /// `"capability"` or `"internal"`, from [`pinewasm_aot::AotError::class`].
    class: &'static str,
    /// The error's variant name, from [`pinewasm_aot::AotError::kind`].
    kind: &'static str,
    site: Option<StandaloneWasmRefusalSite>,
}

impl StandaloneWasmRefusalDetail {
    /// `site` is passed in because the error alone does not carry it.
    fn new(error: &pinewasm_aot::AotError, site: Option<StandaloneWasmRefusalSite>) -> Self {
        let (code, imports, feature) = classify_standalone_wasm_refusal(error);
        Self {
            code,
            message: error.to_string(),
            imports,
            feature,
            class: standalone_wasm_refusal_class(error.class()),
            kind: error.kind(),
            site,
        }
    }
}

/// The compiler-side arguments of one `compileStandaloneWasmProgram` call.
fn standalone_wasm_build(
    options: &StandaloneWasmOptionsInput,
) -> (
    pinewasm_aot::StandaloneWasmOptions,
    pinewasm_aot::StandaloneWasmBuildConfig,
) {
    let module_type = match options.module_type {
        StandaloneWasmModuleTypeInput::Strategy => pinewasm_aot::StandaloneWasmModuleType::Strategy,
        StandaloneWasmModuleTypeInput::Indicator => {
            pinewasm_aot::StandaloneWasmModuleType::Indicator
        }
    };
    let signal_semantics = match options.signal_semantics {
        Some(StandaloneWasmSignalSemanticsInput::PostureEvents) => {
            pinewasm_aot::StandaloneWasmSignalSemantics::PostureEvents
        }
        Some(StandaloneWasmSignalSemanticsInput::PositionEvents) | None => {
            pinewasm_aot::StandaloneWasmSignalSemantics::PositionEvents
        }
    };
    (
        pinewasm_aot::StandaloneWasmOptions {
            signal_plot: options.signal_plot,
            module_type,
        },
        pinewasm_aot::StandaloneWasmBuildConfig {
            signal_semantics,
            max_collection_elements: options
                .max_collection_elements
                .map_or(pinewasm_aot::heap_defaults::MAX_ELEMENTS, u64::from),
            signal_slice: options.signal_slice.unwrap_or(true),
        },
    )
}

/// The standalone WebAssembly compile behind `compileStandaloneWasmProgram`:
/// the module bytes, or the refusal the host receives.
///
/// The compile's error carries no site. The site is the eligibility
/// refusal's, which the compile hands over when it is the refusal it stopped
/// at ([`pinewasm_aot::StandaloneCompileRefusal::eligibility`]). A refusal
/// raised during emission or by the byte audit therefore has no site.
///
/// The same eligibility refusal names the Pine calls behind a program that
/// would import `host.vm`. They become the refusal's `feature`, as a strategy
/// refusal's already is: `host.vm` names a mechanism, and the calls are what
/// a reader can find in the script and change.
fn compile_standalone_wasm_bytes(
    program: &pinewasm_compiler::Program,
    options: &StandaloneWasmOptionsInput,
) -> Result<Vec<u8>, Box<StandaloneWasmRefusalDetail>> {
    let (opts, config) = standalone_wasm_build(options);
    pinewasm_aot::compile_standalone_wasm_program_explained(program, &opts, &config).map_err(
        |refused| {
            let refusal = refused.eligibility();
            let site = refusal
                .and_then(pinewasm_aot::StandaloneRefusal::site)
                .map(StandaloneWasmRefusalSite::from);
            let mut detail = StandaloneWasmRefusalDetail::new(refused.error(), site);
            if detail.feature.is_none() {
                detail.feature = refusal
                    .map(pinewasm_aot::StandaloneRefusal::host_features)
                    .filter(|names| !names.is_empty())
                    .map(|names| names.join(", "));
            }
            Box::new(detail)
        },
    )
}

/// Compile an already-compiled program (`CompileResult.program` — the same
/// input `compileProgramToWasm` takes) into an **standalone WebAssembly v1** consumer
/// artifact: zero imports, exactly `memory` plus the five spec'd exports,
/// instantiable by a hardened host that knows nothing about CSAW headers,
/// lanes, or plots (docs/aot.md, "The standalone WebAssembly export mode").
///
/// This is `compileProgramToWasm`'s sibling, not its replacement. That
/// one emits a standard CSAW v6 artifact, whose module **may import
/// `host.vm`** and which therefore only a PineWASM-aware loader can run;
/// this one refuses such programs rather than emitting them.
///
/// `options` is `{ signal_plot?: number | bigint | null, module_type: 'strategy'
/// | 'indicator', signal_semantics?: 'position-events' | 'posture-events' |
/// null, max_collection_elements?: number | null, signal_slice?: boolean |
/// null }`. Returns `{ wasm: Uint8Array,
/// moduleType, signalPlot, moduleSha256 }` on success, and on refusal the structured
/// `{ refused: { code, message, imports?, feature?, class, kind, site? } }` — a
/// value, not a thrown string, because refusal is the ordinary outcome (only
/// the standalone subset is standalone WebAssembly-eligible) and the host must
/// branch on the class. Throwing stays reserved for a malformed
/// `program`/`options` argument, as everywhere else in this crate.
///
/// `class` is `"capability"` for a declared limit of the compiler or of the
/// requested build, and `"internal"` for a refusal no program this compiler's
/// front end produced should reach: for such a program it is a compiler
/// defect to report, and for a hand-built, stale or tampered program it is an
/// ordinary rejection. `code` does not separate the two: an unsupported
/// opcode and a type conflict both arrive as
/// `StandaloneWasmProgramUnsupported`. `kind` is the compiler's name for the
/// refusal (`UnsupportedOpcode`, `TypeConflict`, ...), finer than `code` and
/// stable across programs. `site`, when the compiler reports one, is
/// `{ pc }`, `{ input }` or `{ plot }`.
///
/// `code` is one of:
///
/// - `StandaloneWasmNotStandalone` — the program needs `host.vm`; `imports`
///   names them, and `feature` names the Pine calls that need it
///   (comma-separated). The dominant refusal by a wide margin.
/// - `StandaloneWasmSourceInputUnsupported` — an `input.source` whose default is not one
///   of the six raw builtin lanes the standalone WebAssembly wire format carries.
/// - `StandaloneWasmNoDesignatedPlot` — multi-plot with no `signal_plot`, an index out of
///   range, or a `signal_plot` passed for an `indicator`-type module or a
///   `strategy()` source.
/// - `StandaloneWasmAuditFailed` — the emitted bytes failed the producer-side standalone WebAssembly audit.
///   An engine bug to report, never a user error.
/// - `StandaloneWasmStrategyFeatureUnsupported` — a `strategy()` script using a feature
///   the native broker cannot export yet (`calc_on_order_fills`,
///   unsupported currency codes, per-trade accessors, …); `feature` names it.
/// - `StandaloneWasmProgramUnsupported` — outside the AOT subset entirely (unsupported
///   opcode, invalid/tampered CSIL, CSIL-version skew).
#[wasm_bindgen(js_name = compileStandaloneWasmProgram, unchecked_return_type = "StandaloneWasmCompileResult")]
pub fn compile_standalone_wasm_program(
    #[wasm_bindgen(unchecked_param_type = "Program")] program: JsValue,
    #[wasm_bindgen(unchecked_param_type = "StandaloneWasmOptions")] options: JsValue,
) -> Result<JsValue, JsValue> {
    let program: pinewasm_compiler::Program = serde_wasm_bindgen::from_value(program)
        .map_err(|e| JsValue::from_str(&format!("invalid program: {e}")))?;
    standalone_wasm_result_to_js(&program, options)
}

/// Both entry points share the complete options, refusal and artifact contract.
fn standalone_wasm_result_to_js(
    program: &pinewasm_compiler::Program,
    options: JsValue,
) -> Result<JsValue, JsValue> {
    let options: StandaloneWasmOptionsInput = serde_wasm_bindgen::from_value(options)
        .map_err(|e| JsValue::from_str(&format!("invalid standalone WebAssembly options: {e}")))?;
    let out = js_sys::Object::new();
    let set = |target: &js_sys::Object, key: &str, value: JsValue| {
        // Reflect::set only fails on non-objects; both targets are objects.
        let _ = js_sys::Reflect::set(target, &JsValue::from_str(key), &value);
    };
    match compile_standalone_wasm_bytes(program, &options) {
        Ok(wasm) => {
            // A strategy() source forces the strategy module type and
            // designates no plot: its signals come from the broker posture
            // lane, and `signalPlot` is null on the wire so no
            // caller can mistake a plot index for the signal source.
            let strategy_source = program.metadata.strategy.is_some();
            set(
                &out,
                "wasm",
                js_sys::Uint8Array::from(wasm.as_slice()).into(),
            );
            set(
                &out,
                "moduleType",
                JsValue::from_str(if strategy_source {
                    "strategy"
                } else {
                    match options.module_type {
                        StandaloneWasmModuleTypeInput::Strategy => "strategy",
                        StandaloneWasmModuleTypeInput::Indicator => "indicator",
                    }
                }),
            );
            // The plot actually designated in the bytes. A successful
            // `strategy` compile with no `signal_plot` is the single-plot
            // default (every other undesignated case is refused above), so
            // the effective index is 0; an `indicator` — and a strategy()
            // source — designates nothing.
            set(
                &out,
                "signalPlot",
                if strategy_source {
                    JsValue::NULL
                } else {
                    match options.module_type {
                        StandaloneWasmModuleTypeInput::Indicator => JsValue::NULL,
                        StandaloneWasmModuleTypeInput::Strategy => {
                            JsValue::from_f64(options.signal_plot.unwrap_or(0) as f64)
                        }
                    }
                },
            );
            set(&out, "moduleSha256", JsValue::from_str(&sha256_hex(&wasm)));
        }
        Err(detail) => {
            let refused = js_sys::Object::new();
            set(&refused, "code", JsValue::from_str(detail.code));
            set(&refused, "message", JsValue::from_str(&detail.message));
            if let Some(imports) = &detail.imports {
                let names = js_sys::Array::new();
                for import in imports {
                    names.push(&JsValue::from_str(import));
                }
                set(&refused, "imports", names.into());
            }
            if let Some(feature) = &detail.feature {
                set(&refused, "feature", JsValue::from_str(feature));
            }
            set(&refused, "class", JsValue::from_str(detail.class));
            set(&refused, "kind", JsValue::from_str(detail.kind));
            if let Some(site) = detail.site {
                let (key, value) = site.wire();
                let at = js_sys::Object::new();
                set(&at, key, JsValue::from_f64(value));
                set(&refused, "site", at.into());
            }
            set(&out, "refused", refused.into());
        }
    }
    Ok(out.into())
}

/// Host-side backing for an AOT module's remaining import: for programs with
/// `host.vm` sites, the VM host executing heap/string/effect/request
/// opcodes on the interpreter's own state. One instance per run; the loader's
/// import glue calls `vm` and drains the event streams after.
///
/// The ta engine is no longer here. It used to back a `host.ta` import; a
/// native artifact now links `pinewasm_ta` — the same crate the
/// interpreter runs — so the results stay bit-identical without that host
/// import.
#[wasm_bindgen]
pub struct AotHost {
    vm: Option<pinewasm_vm::aot_host::VmHost>,
    style_slots: Vec<u32>,
    style_tags: Vec<u8>,
}

/// The bar count an [`AotHost`] follows: the caller's `bars`, or else the
/// run's effective count, `maxBars` over the feed, as the interpreter runs
/// it. A strategy host finishes its broker over these bars, so a capped run
/// on a host sized to the whole feed would open a bar the module never ran.
fn aot_host_bars(bars: Option<u32>, max_bars: Option<usize>, data_bars: usize) -> usize {
    match bars {
        Some(bars) => bars as usize,
        None => max_bars.map_or(data_bars, |cap| cap.min(data_bars)),
    }
}

#[wasm_bindgen]
impl AotHost {
    /// Build from the program (its `ta_states` table), the run's market
    /// data, the normalized run options, and the effective bar count
    /// (without `bars`, the run's `maxBars` over the feed, as the
    /// interpreter runs it). The options/bars arguments arrived with the
    /// `host.vm` tier; older callers passing only (program, marketData)
    /// still get a ta/pure host.
    #[wasm_bindgen(constructor)]
    pub fn new(
        #[wasm_bindgen(unchecked_param_type = "Program")] program: JsValue,
        #[wasm_bindgen(unchecked_param_type = "MarketData")] market_data: JsValue,
        #[wasm_bindgen(unchecked_optional_param_type = "RunOptions | null")] run_options: JsValue,
        #[wasm_bindgen(unchecked_optional_param_type = "number | null")] bars: Option<u32>,
    ) -> Result<AotHost, JsValue> {
        let program: pinewasm_compiler::Program = serde_wasm_bindgen::from_value(program)
            .map_err(|e| JsValue::from_str(&format!("invalid program: {e}")))?;
        let market_data: MarketData = serde_wasm_bindgen::from_value(market_data)
            .map_err(|e| JsValue::from_str(&format!("invalid market data: {e}")))?;
        let options: pinewasm_vm::RunOptions =
            if run_options.is_null() || run_options.is_undefined() {
                pinewasm_vm::RunOptions::default()
            } else {
                serde_wasm_bindgen::from_value(run_options)
                    .map_err(|e| JsValue::from_str(&format!("invalid run options: {e}")))?
            };
        let bars = aot_host_bars(bars, options.max_bars, market_data.bars());
        let layout = pinewasm_aot::native_host_layout(&program);
        let style_slots = layout
            .as_ref()
            .map(|plan| plan.style_slots.iter().map(|slot| slot.slot).collect())
            .unwrap_or_default();
        let style_tags = layout
            .as_ref()
            .map(|plan| plan.style_slots.iter().map(|slot| slot.tag).collect())
            .unwrap_or_default();
        let has_color_palette = program
            .plots
            .iter()
            .map(|plot| &plot.style)
            .chain(program.visuals.iter().map(|visual| &visual.style))
            .chain(program.fills.iter().map(|fill| &fill.style))
            .any(|style| !style.color_palette.is_empty());
        let vm = layout
            .filter(|plan| {
                !plan.host_sites.is_empty() || !plan.style_slots.is_empty() || has_color_palette
            })
            .map(|plan| {
                pinewasm_vm::aot_host::VmHost::new(
                    program.clone(),
                    market_data.clone(),
                    options,
                    bars,
                    plan.host_sites
                        .into_iter()
                        .map(|s| pinewasm_vm::aot_host::SiteSpec {
                            pc: s.pc,
                            args: s.args,
                            ret: s.ret,
                        })
                        .collect(),
                )
            });
        Ok(AotHost {
            vm,
            style_slots,
            style_tags,
        })
    }

    /// Execute `host.vm` site `site` on `bar`. `cells[0]` carries the
    /// remaining budget in/out; `cells[1..]` the operands and result.
    /// Returns 0 (ok), 5 (structured host error — read `takeError`), or
    /// 6 (ABI mismatch — rerun the request on the interpreter).
    pub fn vm(&mut self, site: u32, bar: u32, cells: &mut [i64]) -> i32 {
        match self.vm.as_mut() {
            Some(host) => host.vm_call(site, bar, cells),
            None => pinewasm_vm::aot_host::vm_status::ABI_MISMATCH,
        }
    }

    /// The recorded `log.*` stream (drained; call once after `run`).
    #[wasm_bindgen(
        js_name = drainLogs,
        unchecked_return_type = "ReadonlyArray<LogEntry>"
    )]
    pub fn drain_logs(&mut self) -> Result<JsValue, JsValue> {
        let logs = self
            .vm
            .as_mut()
            .map(pinewasm_vm::aot_host::VmHost::drain_logs)
            .unwrap_or_default();
        to_js(&logs)
    }

    /// The recorded alert stream (drained; call once after `run`).
    #[wasm_bindgen(
        js_name = drainAlerts,
        unchecked_return_type = "ReadonlyArray<AlertEvent>"
    )]
    pub fn drain_alerts(&mut self) -> Result<JsValue, JsValue> {
        let alerts = self
            .vm
            .as_mut()
            .map(pinewasm_vm::aot_host::VmHost::drain_alerts)
            .unwrap_or_default();
        to_js(&alerts)
    }

    /// The strategy results of a `hostBroker` module, serialized exactly as
    /// `runProgram` returns them in `strategy`; null for a module without a
    /// broker, or for a run that stopped before its first bar (status 8 or
    /// 12), as `runProgram` reports none for it. `status` is what `run`
    /// returned and `barsExecuted` the number of bars it completed: all of
    /// them, or the bar a runtime error stopped on. Call once after `run`
    /// and before `drainAlerts`, which then carries the order-fill alerts
    /// too. Throws the reason this request must rerun on the PineWASM
    /// interpreter instead (a strategy setting or bar data the native
    /// broker path does not reproduce).
    #[wasm_bindgen(
        js_name = drainStrategy,
        unchecked_return_type = "StrategyOutcome | null"
    )]
    pub fn drain_strategy(&mut self, status: i32, bars_executed: u32) -> Result<JsValue, JsValue> {
        match self.vm.as_mut() {
            Some(host) => match host.drain_strategy(status, bars_executed) {
                Ok(Some(outcome)) => to_js(&outcome),
                Ok(None) => Ok(JsValue::NULL),
                Err(reason) => Err(JsValue::from_str(&reason)),
            },
            None => Ok(JsValue::NULL),
        }
    }

    /// The final live drawings, serialized exactly as `runProgram` returns
    /// them (call once after `run`).
    #[wasm_bindgen(js_name = drainDrawings, unchecked_return_type = "Drawings | null")]
    pub fn drain_drawings(&mut self) -> Result<JsValue, JsValue> {
        match self.vm.as_mut() {
            Some(host) => to_js(&host.drain_drawings()),
            None => Ok(JsValue::NULL),
        }
    }

    /// Convert native fixed style cells through the engine's decoder and
    /// snapshot conversion, including dynamic string handles.
    #[wasm_bindgen(js_name = resolveDisplayStyles, unchecked_return_type = "NativeDisplayStyles")]
    pub fn resolve_display_styles(
        &self,
        cells: &[i64],
        visible_bars: u32,
    ) -> Result<JsValue, JsValue> {
        let host = self
            .vm
            .as_ref()
            .ok_or_else(|| JsValue::from_str("native style host is unavailable"))?;
        let styles = host
            .resolve_display_styles(
                &self.style_slots,
                &self.style_tags,
                cells,
                visible_bars as usize,
            )
            .map_err(|error| JsValue::from_str(&error))?;
        to_js(&styles)
    }

    /// The structured runtime error behind a `vm` status 5, as the wire
    /// `RuntimeError` shape; null when none is pending.
    #[wasm_bindgen(js_name = takeError, unchecked_return_type = "RuntimeError | null")]
    pub fn take_error(&mut self) -> Result<JsValue, JsValue> {
        match self
            .vm
            .as_mut()
            .and_then(pinewasm_vm::aot_host::VmHost::take_error)
        {
            Some(error) => to_js(&error),
            None => Ok(JsValue::NULL),
        }
    }

    /// Why a `vm` status 6 was returned (the request must rerun on the
    /// PineWASM interpreter); null when none.
    #[wasm_bindgen(js_name = abiMismatch, unchecked_return_type = "string | null")]
    pub fn abi_mismatch(&self) -> JsValue {
        match self.vm.as_ref().and_then(|h| h.abi_mismatch()) {
            Some(reason) => JsValue::from_str(reason),
            None => JsValue::NULL,
        }
    }

    /// Re-arm this host for a fresh run over the same program, market data,
    /// and options. Reset reuses the already-deserialized program, market
    /// data, and options instead of reconstructing the host.
    pub fn reset(&mut self) {
        if let Some(vm) = self.vm.as_mut() {
            vm.reset();
        }
    }

    /// Evaluate a pure numeric function by PineWASM numeric-kernel id.
    ///
    /// Native artifacts no longer call this — they link the same
    /// implementation instead — but it stays on the host object because the
    /// interpreter-hosted tier and internal consistency checks both still
    /// want a way to ask the engine for one value.
    pub fn pure(&self, func_id: u32, a: f64, b: f64) -> f64 {
        pinewasm_num::pure(func_id, a, b)
    }
}

/// Compiler/CSIL/language/rendering-contract version info:
/// `{ compilerVersion, ilVersion, languageVersion, renderingContractVersion, tzdataVersion, sourceRevision? }`.
#[wasm_bindgen(unchecked_return_type = "VersionInfo")]
pub fn version() -> Result<JsValue, JsValue> {
    to_js(&pinewasm_compiler::version_info())
}

fn null_prototype_object() -> js_sys::Object {
    let null_prototype: js_sys::Object = JsValue::NULL.unchecked_into();
    js_sys::Object::create(&null_prototype)
}

/// Define a snapshot data property without invoking inherited setters on the
/// destination or on the property descriptor.
fn set_language_snapshot_property(
    target: &js_sys::Object,
    key: &str,
    value: &JsValue,
    error: &'static str,
) -> Result<(), JsValue> {
    let descriptor = js_sys::PropertyDescriptor::new();
    let descriptor_object: &js_sys::Object = descriptor.unchecked_ref();
    let null_prototype: js_sys::Object = JsValue::NULL.unchecked_into();
    js_sys::Object::try_set_prototype_of(descriptor_object, &null_prototype)
        .map_err(|_| JsValue::from_str(error))?;
    descriptor.set_value(value);
    descriptor.set_writable(true);
    descriptor.set_enumerable(true);
    descriptor.set_configurable(true);

    match js_sys::Reflect::define_property(target, &JsValue::from_str(key), descriptor_object) {
        Ok(true) => Ok(()),
        Ok(false) | Err(_) => Err(JsValue::from_str(error)),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LanguageSnapshotContext {
    Request,
    Document,
    Libraries,
    Library,
    Source,
    Other,
}

fn language_snapshot_child_context(
    parent: LanguageSnapshotContext,
    key: &str,
) -> LanguageSnapshotContext {
    match (parent, key) {
        (LanguageSnapshotContext::Request, "document") => LanguageSnapshotContext::Document,
        (LanguageSnapshotContext::Request, "libraries") => LanguageSnapshotContext::Libraries,
        (LanguageSnapshotContext::Document | LanguageSnapshotContext::Library, "source") => {
            LanguageSnapshotContext::Source
        }
        _ => LanguageSnapshotContext::Other,
    }
}

// Snapshot the bounded plain-data protocol before Serde copies strings into Wasm.
// Null-prototype records and direct data-property definitions prevent inherited
// setters from substituting a value after its source bytes have been measured.
fn snapshot_language_value(
    value: &JsValue,
    encoder: &SearchTextEncoder,
    depth: usize,
    remaining: &mut usize,
    remaining_source_bytes: &mut usize,
    remaining_nodes: &mut usize,
    context: LanguageSnapshotContext,
) -> Result<JsValue, JsValue> {
    let invalid = || JsValue::from_str("invalid_language_query_data");
    if depth > 5 || *remaining_nodes == 0 {
        return Err(invalid());
    }
    *remaining_nodes -= 1;
    if value.is_string() {
        let string: &js_sys::JsString = value.unchecked_ref();
        let is_source = context == LanguageSnapshotContext::Source;
        let limit = if is_source {
            *remaining_source_bytes
        } else {
            *remaining
        };
        let bytes = match bounded_js_string_utf8_len(string, encoder, limit) {
            Ok(BoundedUtf8Len::Bytes(bytes)) => bytes,
            Ok(BoundedUtf8Len::ExceedsLimit) if is_source => {
                return Err(JsValue::from_str("source_budget_exceeded"));
            }
            Ok(BoundedUtf8Len::ExceedsLimit | BoundedUtf8Len::InvalidUtf16) | Err(()) => {
                return Err(invalid());
            }
        };
        if bytes > *remaining {
            return Err(invalid());
        }
        *remaining -= bytes;
        if is_source {
            *remaining_source_bytes -= bytes;
        }
        return Ok(value.clone());
    }
    if value.is_null()
        || value.is_undefined()
        || value.as_bool().is_some()
        || value.as_f64().is_some_and(f64::is_finite)
    {
        return Ok(value.clone());
    }
    if matches!(try_array_is_array(value), Ok(true)) {
        let length = own_data_property(value, "length", || "invalid_language_query_data".into())?
            .as_f64()
            .ok_or_else(invalid)?;
        if !(0.0..=32.0).contains(&length) || length.fract() != 0.0 {
            return Err(invalid());
        }
        let snapshot = js_sys::Array::new();
        let snapshot_object: &js_sys::Object = snapshot.unchecked_ref();
        let item_context = if context == LanguageSnapshotContext::Libraries {
            LanguageSnapshotContext::Library
        } else {
            LanguageSnapshotContext::Other
        };
        for index in 0..length as u32 {
            let item = own_data_property(value, &index.to_string(), || {
                "invalid_language_query_data".into()
            })?;
            let item = snapshot_language_value(
                &item,
                encoder,
                depth + 1,
                remaining,
                remaining_source_bytes,
                remaining_nodes,
                item_context,
            )?;
            set_language_snapshot_property(
                snapshot_object,
                &index.to_string(),
                &item,
                "invalid_language_query_data",
            )?;
        }
        return Ok(snapshot.into());
    }
    let prototype: JsValue = js_sys::Reflect::get_prototype_of(&js_sys::Object::new())
        .map_err(|_| invalid())?
        .into();
    if !is_plain_record(value, &prototype).map_err(|()| invalid())? {
        return Err(invalid());
    }
    let keys = js_sys::Reflect::own_keys(value).map_err(|_| invalid())?;
    if keys.length() > 16 {
        return Err(invalid());
    }
    let snapshot = null_prototype_object();
    for key in keys.iter() {
        let key = key
            .as_string()
            .filter(|key| key.len() <= 32)
            .ok_or_else(invalid)?;
        if key == "__proto__" {
            return Err(invalid());
        }
        let item = own_data_property(value, &key, || "invalid_language_query_data".into())?;
        let item = snapshot_language_value(
            &item,
            encoder,
            depth + 1,
            remaining,
            remaining_source_bytes,
            remaining_nodes,
            language_snapshot_child_context(context, &key),
        )?;
        set_language_snapshot_property(&snapshot, &key, &item, "invalid_language_query_data")?;
    }
    Ok(snapshot.into())
}

fn validate_language_record_fields(
    value: &JsValue,
    allowed_fields: &[&str],
) -> Result<(), JsValue> {
    let invalid = || JsValue::from_str("invalid language query request");
    if value.is_null() || value.is_undefined() || !value.is_object() {
        return Ok(());
    }
    if try_array_is_array(value).map_err(|_| invalid())? {
        return Ok(());
    }
    for key in js_sys::Reflect::own_keys(value)
        .map_err(|_| invalid())?
        .iter()
    {
        let key = key.as_string().ok_or_else(invalid)?;
        if !allowed_fields.contains(&key.as_str()) {
            return Err(invalid());
        }
    }
    Ok(())
}

fn language_record_field(value: &JsValue, key: &str) -> Option<JsValue> {
    match own_property(value, &JsValue::from_str(key)) {
        Ok(OwnProperty::Data(value)) => Some(value),
        Ok(OwnProperty::Missing | OwnProperty::Accessor) | Err(()) => None,
    }
}

fn validate_language_position_fields(value: &JsValue) -> Result<(), JsValue> {
    validate_language_record_fields(value, &["line", "character"])
}

/// Preserve the closed-record Serde contract across the JavaScript snapshot
/// boundary. `serde_wasm_bindgen` ignores unrecognized map entries for structs,
/// so validate each record before deserializing it.
fn validate_language_request_fields(request: &JsValue) -> Result<(), JsValue> {
    validate_language_record_fields(
        request,
        &[
            "schemaVersion",
            "capability",
            "document",
            "libraries",
            "position",
            "range",
            "newName",
            "includeDeclaration",
        ],
    )?;
    if let Some(document) = language_record_field(request, "document") {
        validate_language_record_fields(&document, &["id", "version", "source"])?;
    }
    if let Some(libraries) = language_record_field(request, "libraries") {
        if try_array_is_array(&libraries)
            .map_err(|_| JsValue::from_str("invalid language query request"))?
        {
            let libraries: &js_sys::Array = libraries.unchecked_ref();
            for library in libraries.iter() {
                validate_language_record_fields(
                    &library,
                    &["id", "version", "importPath", "source"],
                )?;
            }
        }
    }
    if let Some(position) = language_record_field(request, "position") {
        validate_language_position_fields(&position)?;
    }
    if let Some(range) = language_record_field(request, "range") {
        validate_language_record_fields(&range, &["start", "end"])?;
        if let Some(start) = language_record_field(&range, "start") {
            validate_language_position_fields(&start)?;
        }
        if let Some(end) = language_record_field(&range, "end") {
            validate_language_position_fields(&end)?;
        }
    }
    Ok(())
}

/// Query the explicit in-memory document set. Does not access the filesystem
/// or any host application state.
#[wasm_bindgen(js_name = languageQuery, unchecked_return_type = "LanguageQueryResponse")]
pub fn language_query(
    #[wasm_bindgen(unchecked_param_type = "LanguageQueryRequest")] request: JsValue,
) -> Result<JsValue, JsValue> {
    let encoder =
        SearchTextEncoder::new().map_err(|_| JsValue::from_str("invalid_language_query_data"))?;
    // Document source plus bounded identity/protocol metadata.
    let mut remaining_nodes = 512;
    let mut remaining_source_bytes = pinewasm_lsp::query::MAX_SOURCE_BYTES;
    let request = snapshot_language_value(
        &request,
        &encoder,
        0,
        &mut (300 * 1024),
        &mut remaining_source_bytes,
        &mut remaining_nodes,
        LanguageSnapshotContext::Request,
    )?;
    validate_language_request_fields(&request)?;
    let request: pinewasm_lsp::query::Request = serde_wasm_bindgen::from_value(request)
        .map_err(|_| JsValue::from_str("invalid language query request"))?;
    let response =
        pinewasm_lsp::query::query(request).map_err(|error| JsValue::from_str(error.code()))?;
    response
        .serialize(&serde_wasm_bindgen::Serializer::json_compatible())
        .map_err(|_| JsValue::from_str("language query serialization failed"))
}

/// Versioned capability discovery; old bundles do not expose this export.
#[wasm_bindgen(js_name = languageCapabilities, unchecked_return_type = "LanguageCapabilities")]
pub fn language_capabilities() -> Result<JsValue, JsValue> {
    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    struct LanguageServiceManifest {
        schema_version: u32,
        service_version: &'static str,
        capabilities: [&'static str; 7],
        max_source_bytes: usize,
        max_results: usize,
        max_resolution_work: usize,
    }
    LanguageServiceManifest {
        schema_version: 1,
        service_version: pinewasm_lsp::query::SERVICE_VERSION,
        capabilities: [
            "analyze",
            "hover",
            "definition",
            "references",
            "codeActions",
            "prepareRename",
            "rename",
        ],
        max_source_bytes: pinewasm_lsp::query::MAX_SOURCE_BYTES,
        max_results: pinewasm_lsp::query::MAX_RESULTS,
        max_resolution_work: pinewasm_lsp::query::MAX_RESOLUTION_WORK,
    }
    .serialize(&serde_wasm_bindgen::Serializer::json_compatible())
    .map_err(|_| JsValue::from_str("language capabilities serialization failed"))
}

#[wasm_bindgen(typescript_custom_section)]
const LANGUAGE_QUERY_TYPES: &str = r#"
export type LanguageCapability = 'analyze' | 'hover' | 'definition' | 'references' | 'codeActions' | 'prepareRename' | 'rename';
export interface LanguagePosition { line: number; character: number; }
export interface LanguageRange { start: LanguagePosition; end: LanguagePosition; }
export interface LanguageDocument { id: string; version: string; source: string; }
export interface LanguageLibrary extends LanguageDocument { importPath: string; }
export interface LanguageQueryRequest { schemaVersion: 1; capability: LanguageCapability; document: LanguageDocument; libraries?: LanguageLibrary[]; position?: LanguagePosition; range?: LanguageRange; newName?: string; includeDeclaration?: boolean; }
export interface LanguageLocation { documentId: string; documentVersion: string; range: LanguageRange; }
export interface LanguageEdit { documentId: string; documentVersion: string; startUtf16: number; endUtf16: number; text: string; }
export interface LanguageDiagnostic { severity: 'error' | 'warning'; code: string | null; message: string; labels: { message: string; location: LanguageLocation | null }[]; notes: string[]; }
export type LanguageQueryData = null | LanguageLocation | LanguageLocation[] | { success: boolean; diagnostics: LanguageDiagnostic[] } | { markdown: string; range: LanguageRange } | { range: LanguageRange } | { edits: LanguageEdit[] } | { title: string; diagnosticCode: string | null; edits: LanguageEdit[] }[];
export interface LanguageQueryResponse { schemaVersion: 1; serviceVersion: string; capability: LanguageCapability; documentId: string; documentVersion: string; data: LanguageQueryData; }
export interface LanguageCapabilities { schemaVersion: 1; serviceVersion: string; capabilities: LanguageCapability[]; maxSourceBytes: number; maxResults: number; maxResolutionWork: number; }
"#;

#[cfg(test)]
mod tests {
    #[test]
    fn retained_compilation_moves_exact_code_and_preserves_the_result() {
        let libraries = vec![(
            "Test/Resident/1".to_string(),
            "//@version=6\nlibrary(\"Resident\")\nexport twice(float x) => x * 2\n".to_string(),
        )];
        let sources = [
            "//@version=6\nindicator(\"Resident\")\nplot(close)\n",
            "//@version=6\nindicator(\"Resident\")\nimport Test/Resident/1 as lib\nplot(lib.twice(close))\n",
            "//@version=6\nstrategy(\"Resident\")\nn = input.int(3, \"Length\")\nvar xs = array.new<float>()\narray.push(xs, close)\nif bar_index % n == 0\n    strategy.entry(\"L\", strategy.long)\nplot(array.get(xs, 0))\n",
            "//@version=6\nindicator(\"Invalid\")\nplot(missing)\n",
        ];
        for source in sources {
            for debug in [false, true] {
                let options = super::CompileOptions {
                    emit_ast: debug,
                    emit_hir: debug,
                    emit_mir: debug,
                    emit_csil: debug,
                    ..Default::default()
                };
                let original = pinewasm_compiler::compile_with_libraries_profiled(
                    source,
                    &libraries,
                    options,
                    Some(&|| 0.0),
                );
                let expected = original.clone();
                let code_ptr = original.program.as_ref().map(|p| p.main().code.as_ptr());
                let (mut summary, retained) = super::retain_compilation(original);
                assert!(summary.program.is_none());
                assert_eq!(
                    retained.as_ref().map(|p| p.program.main().code.as_ptr()),
                    code_ptr,
                    "retention moves the allocation instead of cloning instructions"
                );
                summary.program = retained.map(|p| p.program);
                assert_eq!(summary, expected, "source: {source}, debug: {debug}");
            }
        }
    }

    #[test]
    fn retained_code_keeps_vm_state_fresh_and_standalone_bytes_identical() {
        let original = pinewasm_compiler::compile_with_libraries_profiled(
            "//@version=6\nstrategy(\"Resident\", process_orders_on_close=true)\nn = input.int(2, \"Length\")\nvar count = 0\ncount += 1\nif bar_index % n == 0\n    strategy.entry(\"L\", strategy.long)\nelse\n    strategy.close(\"L\")\nplot(count)\n",
            &[],
            super::CompileOptions::default(),
            None,
        );
        assert!(original.success, "{:?}", original.diagnostics);
        let expected = original.program.clone().unwrap();
        let (_, handle) = super::retain_compilation(original);
        let handle = handle.unwrap();
        let data = pinewasm_compiler::MarketData {
            open: vec![10.0, 12.0, 11.0, 15.0],
            high: vec![10.0, 12.0, 11.0, 15.0],
            low: vec![10.0, 12.0, 11.0, 15.0],
            close: vec![10.0, 12.0, 11.0, 15.0],
            volume: vec![1.0; 4],
            ..Default::default()
        };
        let prepared = pinewasm_compiler::PreparedProgram::new(handle.program.clone()).unwrap();
        for fuel in [0, 1, 1_000_000] {
            let options = super::RunOptions {
                max_instructions_per_bar: fuel,
                ..Default::default()
            };
            let reference =
                pinewasm_compiler::run_program_profiled(&expected, &data, &options, None);
            for _ in 0..2 {
                assert_eq!(prepared.run_profiled(&data, &options, None), reference);
            }
        }
        for max_collection_elements in [None, Some(0), Some(4_000_000)] {
            let options = super::StandaloneWasmOptionsInput {
                signal_plot: None,
                module_type: super::StandaloneWasmModuleTypeInput::Strategy,
                signal_semantics: None,
                max_collection_elements,
                signal_slice: None,
            };
            let actual = super::compile_standalone_wasm_bytes(&handle.program, &options);
            let reference = super::compile_standalone_wasm_bytes(&expected, &options);
            match (actual, reference) {
                (Ok(actual), Ok(reference)) => assert_eq!(actual, reference),
                (Err(actual), Err(reference)) => {
                    assert_eq!(actual.code, reference.code);
                    assert_eq!(actual.message, reference.message);
                    assert_eq!(actual.kind, reference.kind);
                }
                _ => panic!("retained and serialized routes disagree on eligibility"),
            }
        }
        assert_eq!(handle.program, expected, "consumers must not mutate code");
    }

    use super::*;
    use proptest::prelude::*;
    use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};

    const DEFAULT_ADVERSARIAL_CASES: u32 = 64;
    const MAX_ADVERSARIAL_CASES: u32 = 100_000;

    fn adversarial_cases() -> u32 {
        let Some(raw) = std::env::var_os("PINEWASM_ADVERSARIAL_CASES") else {
            return DEFAULT_ADVERSARIAL_CASES;
        };
        let raw = raw
            .to_str()
            .expect("PINEWASM_ADVERSARIAL_CASES must be UTF-8");
        let cases: u32 = raw
            .parse()
            .expect("PINEWASM_ADVERSARIAL_CASES must be an integer");
        assert!(
            (1..=MAX_ADVERSARIAL_CASES).contains(&cases),
            "PINEWASM_ADVERSARIAL_CASES must be in 1..={MAX_ADVERSARIAL_CASES}"
        );
        cases
    }

    fn binding_runner() -> TestRunner {
        let config = Config {
            cases: adversarial_cases(),
            failure_persistence: None,
            max_shrink_iters: 4_096,
            ..Config::default()
        };
        TestRunner::new_with_rng(
            config,
            TestRng::from_seed(RngAlgorithm::ChaCha, &[0x44; 32]),
        )
    }

    fn exercise_native_binding_dtos(bytes: &[u8]) {
        // `serde_wasm_bindgen` and `serde_json` drive the same derived
        // `Deserialize` implementations for these DTOs. This native check
        // reaches those implementations without pretending that a native
        // runner can model JavaScript Proxy traps, BigInt, or reflection.
        let _ = serde_json::from_slice::<CompileOptions>(bytes);
        let _ = serde_json::from_slice::<RunOptions>(bytes);
        let _ = serde_json::from_slice::<Vec<(String, String)>>(bytes);
        let _ = serde_json::from_slice::<StandaloneWasmOptionsInput>(bytes);
        let _ = serde_json::from_slice::<MarketData>(bytes);
        if let Ok(program) = serde_json::from_slice::<pinewasm_compiler::Program>(bytes) {
            let _ = pinewasm_csil::validate(&program);
        }
    }

    fn exercise_dto_seed_mutations(seed: &[u8]) {
        exercise_native_binding_dtos(seed);
        for end in [0, seed.len() / 2, seed.len().saturating_sub(1)] {
            exercise_native_binding_dtos(&seed[..end]);
        }
        if !seed.is_empty() {
            for index in [0, seed.len() / 2, seed.len() - 1] {
                let mut mutation = seed.to_vec();
                mutation[index] ^= 0x80;
                exercise_native_binding_dtos(&mutation);
            }
        }
    }

    fn structured_binding_value() -> BoxedStrategy<serde_json::Value> {
        use serde_json::Value;

        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            any::<i64>().prop_map(|value| Value::Number(value.into())),
            any::<u64>().prop_map(|value| Value::Number(value.into())),
            any::<i32>().prop_map(|value| serde_json::json!(f64::from(value) / 16.0)),
            ".{0,8}".prop_map(Value::String),
        ];
        leaf.prop_recursive(3, 32, 4, |inner| {
            prop_oneof![
                proptest::collection::vec(inner.clone(), 0..=4).prop_map(Value::Array),
                proptest::collection::btree_map("[a-z]{0,8}", inner, 0..=4)
                    .prop_map(|fields| Value::Object(fields.into_iter().collect())),
            ]
        })
        .boxed()
    }

    fn structured_binding_seeds(program: &pinewasm_compiler::Program) -> Vec<serde_json::Value> {
        vec![
            serde_json::to_value(CompileOptions::default()).unwrap(),
            serde_json::to_value(RunOptions::default()).unwrap(),
            serde_json::to_value(MarketData::default()).unwrap(),
            serde_json::json!([["library.pine", "library(\"Fixture\")"]]),
            serde_json::json!({
                "signal_plot": null,
                "module_type": "indicator",
                "signal_semantics": null,
                "max_collection_elements": 128,
            }),
            serde_json::from_str(&program.to_json()).unwrap(),
        ]
    }

    fn structured_input_seeds() -> Vec<serde_json::Value> {
        use pinewasm_compiler::{ExternalSourceRef, InputValue};

        [
            InputValue::Int(7),
            InputValue::Float(1.25),
            InputValue::Bool(true),
            InputValue::String("fixture".into()),
            InputValue::Color {
                r: 12,
                g: 34,
                b: 56,
                t: 10.0,
            },
            InputValue::Source(serde_json::from_value(serde_json::json!("Close")).unwrap()),
            InputValue::ExternalSource(ExternalSourceRef {
                script_id: "fixture".into(),
                plot_slot: 0,
                plot_label: "Close".into(),
            }),
            InputValue::Enum {
                name: "Fixture".into(),
                index: 0,
                member: "First".into(),
                title: Some("First".into()),
            },
        ]
        .into_iter()
        .map(|input| serde_json::to_value(input).unwrap())
        .collect()
    }

    fn structured_input_cases(value: &serde_json::Value) -> Vec<serde_json::Value> {
        let mut cases = Vec::new();
        for seed in structured_input_seeds() {
            let (variant, payload) = seed.as_object().unwrap().iter().next().unwrap();
            // Preserve the real external tag so even arbitrary payloads
            // reach a variant, rather than stopping at variant selection.
            let mut replaced = seed.clone();
            replaced[variant] = value.clone();
            cases.push(replaced);
            if let Some(fields) = payload.as_object() {
                for field in fields.keys() {
                    let mut nested = seed.clone();
                    nested[variant][field] = value.clone();
                    cases.push(nested);
                }
            }
            if variant == "Int" && value.is_number() {
                // The custom integer adapter also accepts decimal strings.
                cases.push(serde_json::json!({"Int": value.to_string()}));
            }
            cases.push(seed);
        }
        cases
    }

    /// Keep known outer DTO fields valid while replacing one field or nested
    /// element, so generated JSON reaches typed deserializers, not only the
    /// JSON lexer or an ignored unknown-field path.
    fn structured_binding_cases(
        seeds: &[serde_json::Value],
        value: &serde_json::Value,
    ) -> Vec<serde_json::Value> {
        use serde_json::Value;

        let mut cases = vec![value.clone()];
        for seed in seeds {
            match seed {
                Value::Object(fields) => {
                    for field in fields.keys() {
                        let mut case = seed.clone();
                        case[field] = value.clone();
                        cases.push(case);
                    }
                }
                Value::Array(rows) => {
                    for (row_index, row) in rows.iter().enumerate() {
                        for column_index in 0..row.as_array().unwrap().len() {
                            let mut case = seed.clone();
                            case[row_index][column_index] = value.clone();
                            cases.push(case);
                        }
                    }
                }
                _ => panic!("structured DTO seed must be an object or library-pair array"),
            }
        }
        let mut inputs = seeds[1].clone();
        inputs["inputs"] = serde_json::json!({"0": value});
        cases.push(inputs);
        for input in structured_input_cases(value) {
            let mut inputs = seeds[1].clone();
            inputs["inputs"] = serde_json::json!({"0": input});
            cases.push(inputs);
        }
        let mut market = seeds[2].clone();
        market["open"] = serde_json::json!([value]);
        cases.push(market);
        let mut program = seeds[5].clone();
        program["functions"] = serde_json::json!([value]);
        cases.push(program);
        cases
    }

    fn assert_structured_field_refusal<T: serde::de::DeserializeOwned>(
        cases: &[serde_json::Value],
        invalid: serde_json::Value,
    ) {
        assert!(
            cases.contains(&invalid),
            "fixture must be generated by the real harness"
        );
        let bytes = serde_json::to_vec(&invalid).unwrap();
        serde_json::from_slice::<serde_json::Value>(&bytes).expect("fixture must be valid JSON");
        // Some type-directed enum refusals use serde_json's Syntax category
        // even though the same bytes parse as Value. Generic parsing above,
        // rather than that category, proves this is a DTO-shaped fixture.
        serde_json::from_slice::<T>(&bytes)
            .err()
            .expect("malformed DTO field must be refused");
    }

    #[test]
    fn structured_binding_cases_reach_typed_deserializers() {
        let program = pinewasm_compiler::compile(
            "indicator(\"Structured DTO case\")\nplot(close)\n",
            CompileOptions::default(),
        )
        .program
        .unwrap();
        let seeds = structured_binding_seeds(&program);
        serde_json::from_value::<CompileOptions>(seeds[0].clone()).unwrap();
        serde_json::from_value::<RunOptions>(seeds[1].clone()).unwrap();
        serde_json::from_value::<MarketData>(seeds[2].clone()).unwrap();
        serde_json::from_value::<Vec<(String, String)>>(seeds[3].clone()).unwrap();
        serde_json::from_value::<StandaloneWasmOptionsInput>(seeds[4].clone()).unwrap();
        serde_json::from_value::<pinewasm_compiler::Program>(seeds[5].clone()).unwrap();

        let invalid = serde_json::json!({});
        let cases = structured_binding_cases(&seeds, &invalid);
        let mut compile = seeds[0].clone();
        compile["emitIl"] = invalid.clone();
        assert_structured_field_refusal::<CompileOptions>(&cases, compile);
        let mut options = seeds[1].clone();
        options["maxBars"] = invalid.clone();
        assert_structured_field_refusal::<RunOptions>(&cases, options);
        let mut market = seeds[2].clone();
        market["open"] = serde_json::json!([invalid]);
        assert_structured_field_refusal::<MarketData>(&cases, market);
        let mut libraries = seeds[3].clone();
        libraries[0][0] = invalid.clone();
        assert_structured_field_refusal::<Vec<(String, String)>>(&cases, libraries);
        let mut standalone_wasm = seeds[4].clone();
        standalone_wasm["module_type"] = invalid.clone();
        assert_structured_field_refusal::<StandaloneWasmOptionsInput>(&cases, standalone_wasm);
        let mut csil = seeds[5].clone();
        csil["functions"] = serde_json::json!([invalid]);
        assert_structured_field_refusal::<pinewasm_compiler::Program>(&cases, csil);
    }

    #[test]
    fn structured_input_cases_reach_variant_payloads() {
        use pinewasm_compiler::InputValue;

        let program = pinewasm_compiler::compile(
            "indicator(\"Structured input case\")\nplot(close)\n",
            CompileOptions::default(),
        )
        .program
        .unwrap();
        let seeds = structured_binding_seeds(&program);
        let options = &seeds[1];
        let invalid = serde_json::json!([]);
        let cases = structured_binding_cases(&seeds, &invalid);
        for seed in structured_input_seeds() {
            let expected: InputValue = serde_json::from_value(seed.clone()).unwrap();
            let mut valid = options.clone();
            valid["inputs"] = serde_json::json!({"0": seed});
            assert!(cases.contains(&valid));
            let decoded: RunOptions = serde_json::from_value(valid).unwrap();
            assert_eq!(decoded.inputs.get(&0), Some(&expected));
        }

        // All envelopes retain a known variant name. Structured variants
        // retain valid siblings and replace one actual payload field.
        for mut seed in structured_input_seeds() {
            let (variant, payload) = seed.as_object().unwrap().iter().next().unwrap();
            let variant = variant.clone();
            if let Some(fields) = payload.as_object() {
                for field in fields.keys() {
                    let mut input = seed.clone();
                    input[&variant][field] = invalid.clone();
                    let mut case = options.clone();
                    case["inputs"] = serde_json::json!({"0": input});
                    assert_structured_field_refusal::<RunOptions>(&cases, case);
                }
            } else {
                seed[&variant] = invalid.clone();
                let mut case = options.clone();
                case["inputs"] = serde_json::json!({"0": seed});
                assert_structured_field_refusal::<RunOptions>(&cases, case);
            }
        }

        for value in [i64::MIN, 0, i64::MAX] {
            let cases = structured_binding_cases(&seeds, &serde_json::json!(value));
            let encoded = serde_json::json!({"Int": value.to_string()});
            let mut case = options.clone();
            case["inputs"] = serde_json::json!({"0": encoded});
            assert!(cases.contains(&case));
            let decoded: RunOptions = serde_json::from_value(case).unwrap();
            assert_eq!(decoded.inputs.get(&0), Some(&InputValue::Int(value)));
        }
    }

    fn source_with_two_byte_tail(prefix_bytes: usize) -> String {
        let mut source = "x".repeat(prefix_bytes);
        source.push('é');
        source
    }

    #[test]
    fn navigation_source_just_under_the_cap_is_accepted() {
        let source = source_with_two_byte_tail(MAX_SOURCE_LEN - 3);
        assert_eq!(source.len(), MAX_SOURCE_LEN - 1);
        assert_eq!(validate_navigation_source(&source), Ok(()));
    }

    #[test]
    fn navigation_source_at_the_cap_is_accepted() {
        let source = source_with_two_byte_tail(MAX_SOURCE_LEN - 2);
        assert_eq!(source.len(), MAX_SOURCE_LEN);
        assert_eq!(validate_navigation_source(&source), Ok(()));
    }

    #[test]
    fn navigation_source_one_byte_over_the_cap_is_rejected_stably() {
        let source = source_with_two_byte_tail(MAX_SOURCE_LEN - 1);
        assert_eq!(source.len(), MAX_SOURCE_LEN + 1);

        let error = validate_navigation_source(&source).expect_err("source must exceed the cap");
        let compiler_result = pinewasm_compiler::compile(&source, CompileOptions::default());
        let compiler_error = compiler_result
            .diagnostics
            .first()
            .expect("compiler must report the same source limit");
        assert_eq!(error, compiler_error.message);
        assert_eq!(
            error,
            format!(
                "source is {} bytes; the maximum supported size is {MAX_SOURCE_LEN}",
                MAX_SOURCE_LEN + 1
            )
        );
    }

    #[test]
    fn an_aot_host_without_a_bar_count_follows_the_runs_max_bars() {
        assert_eq!(aot_host_bars(None, None, 400), 400);
        assert_eq!(aot_host_bars(None, Some(300), 400), 300);
        assert_eq!(aot_host_bars(None, Some(0), 400), 0);
        assert_eq!(aot_host_bars(None, Some(600), 400), 400);
        // An explicit count is the caller's, as before.
        assert_eq!(aot_host_bars(Some(250), Some(300), 400), 250);
        assert_eq!(aot_host_bars(Some(400), None, 400), 400);
    }

    #[test]
    fn utf8_measurement_accepts_exact_byte_limit() {
        assert_eq!(
            bounded_utf8_len(5, 10, || Ok::<_, ()>(true), || Ok::<_, ()>(10)),
            Ok(BoundedUtf8Len::Bytes(10))
        );
    }

    #[test]
    fn utf8_measurement_rejects_encoded_multibyte_overflow() {
        assert_eq!(
            bounded_utf8_len(5, 9, || Ok::<_, ()>(true), || Ok::<_, ()>(10)),
            Ok(BoundedUtf8Len::ExceedsLimit)
        );
    }

    #[test]
    fn utf8_measurement_rejects_invalid_utf16_before_encoding() {
        assert_eq!(
            bounded_utf8_len(
                1,
                4,
                || Ok::<_, ()>(false),
                || { panic!("encoder must remain untouched") }
            ),
            Ok(BoundedUtf8Len::InvalidUtf16)
        );
    }

    #[test]
    fn utf8_measurement_rejects_known_oversize_before_inspection() {
        assert_eq!(
            bounded_utf8_len::<()>(
                5,
                4,
                || panic!("well-formed check must remain untouched"),
                || panic!("encoder must remain untouched"),
            ),
            Ok(BoundedUtf8Len::ExceedsLimit)
        );
    }

    #[test]
    #[ignore = "release-gated by cargo xtask fuzz-smoke"]
    fn adversarial_binding_dto_deserialization_is_total() {
        let program = pinewasm_compiler::compile(
            "indicator(\"Binding seed\")\nplot(close)\n",
            CompileOptions::default(),
        )
        .program
        .expect("first-party binding seed must compile");
        let compile_options = br#"{"emitIl":true}"#;
        let run_options = br#"{"maxInstructionsPerBar":64,"maxBars":1}"#;
        let market_data = br#"{"open":[1],"high":[2],"low":[0],"close":[1],"volume":[5]}"#;
        let libraries = br#"[["library.pine","library(\"Fixture\")"]]"#;
        let standalone_wasm_options = br#"{"signal_plot":null,"module_type":"indicator"}"#;
        serde_json::from_slice::<CompileOptions>(compile_options)
            .expect("compile-options seed must reach a successful DTO path");
        serde_json::from_slice::<RunOptions>(run_options)
            .expect("run-options seed must reach a successful DTO path");
        serde_json::from_slice::<MarketData>(market_data)
            .expect("market-data seed must reach a successful DTO path");
        serde_json::from_slice::<Vec<(String, String)>>(libraries)
            .expect("library-pair seed must reach a successful DTO path");
        serde_json::from_slice::<StandaloneWasmOptionsInput>(standalone_wasm_options)
            .expect("standalone WebAssembly-options seed must reach a successful DTO path");
        let program_json = program.to_json();
        serde_json::from_slice::<pinewasm_compiler::Program>(program_json.as_bytes())
            .expect("CSIL seed must reach a successful binding DTO path");

        let valid_seeds = [
            br"{}".as_slice(),
            compile_options.as_slice(),
            run_options.as_slice(),
            market_data.as_slice(),
            libraries.as_slice(),
            standalone_wasm_options.as_slice(),
            program_json.as_bytes(),
        ];
        for seed in valid_seeds {
            exercise_dto_seed_mutations(seed);
        }

        let structured_seeds = structured_binding_seeds(&program);
        binding_runner()
            .run(
                &(
                    proptest::collection::vec(any::<u8>(), 0..=4_096),
                    structured_binding_value(),
                ),
                |(bytes, value)| {
                    exercise_native_binding_dtos(&bytes);
                    prop_assert!(serde_json::to_vec(&value).unwrap().len() <= 8_192);
                    for case in structured_binding_cases(&structured_seeds, &value) {
                        let encoded = serde_json::to_vec(&case).unwrap();
                        prop_assert!(encoded.len() <= 16_384);
                        exercise_native_binding_dtos(&encoded);
                    }
                    Ok(())
                },
            )
            .expect("deterministic native binding-DTO campaign failed");
    }

    // -----------------------------------------------------------------------
    // The `refused` object of `compileStandaloneWasmProgram`.

    /// Accepted as it stands; `plant_mixed_slot` turns it into a program the
    /// front end cannot produce.
    const REFUSAL_FIXTURE: &str = "//@version=6\nindicator(\"refusal class case\")\n\
         var int n = 0\nn := 5\nplot(close, \"Backtest Signal\")\n";

    /// Reads a string input, which the importless module cannot do.
    const HOST_SITE: &str = "//@version=6\nindicator(\"host site\")\n\
         s = input.string(\"a\", \"S\")\n\
         plot(s == \"a\" ? close : open, \"Backtest Signal\")\n";

    /// Defaults an `input.source` to a lane the wire format does not carry.
    const DERIVED_SOURCE: &str = "//@version=6\nindicator(\"derived source\")\n\
         n = input.int(3, \"N\")\nsrc = input.source(hl2, \"Source\")\n\
         plot(src + n, \"Backtest Signal\")\n";

    fn front_end(source: &str) -> pinewasm_compiler::Program {
        let result = pinewasm_compiler::compile(source, CompileOptions::default());
        result
            .program
            .unwrap_or_else(|| panic!("source must compile: {:?}", result.diagnostics))
    }

    fn standalone_options(json: &str) -> StandaloneWasmOptionsInput {
        serde_json::from_str(json).expect("valid standalone WebAssembly options")
    }

    /// What the binding returns for a refusal, and the compiler's own error
    /// for the same program and options.
    fn refusal(
        program: &pinewasm_compiler::Program,
        options: &str,
    ) -> (StandaloneWasmRefusalDetail, pinewasm_aot::AotError) {
        let options = standalone_options(options);
        let detail = *compile_standalone_wasm_bytes(program, &options)
            .expect_err("the program must be refused");
        let (opts, config) = standalone_wasm_build(&options);
        let error =
            pinewasm_aot::compile_standalone_wasm_program_with_config(program, &opts, &config)
                .expect_err("the compiler must refuse the same program");
        (detail, error)
    }

    /// Store a float into the slot the program stores an int into, just
    /// before the final instruction. The validator accepts the result and
    /// the backend refuses it with a type conflict.
    fn plant_mixed_slot(program: &mut pinewasm_compiler::Program) {
        use pinewasm_csil::{CsilInstr, F64Bits};
        let function = &mut program.functions[0];
        let slot = function
            .code
            .iter()
            .find_map(|instr| match instr {
                CsilInstr::StoreUserSlotCurrent { slot } => Some(*slot),
                _ => None,
            })
            .expect("the fixture source stores a user slot");
        let at = function.code.len() - 1;
        function.code.splice(
            at..at,
            [
                CsilInstr::PushF64(F64Bits::new(1.5)),
                CsilInstr::StoreUserSlotCurrent { slot },
            ],
        );
        let span = function.source_map[at];
        function.source_map.splice(at..at, [span, span]);
        // The edit no longer reproduces from the retained budget reference.
        function.budget_reference = None;
    }

    #[test]
    fn the_fixture_program_is_not_refused() {
        compile_standalone_wasm_bytes(
            &front_end(REFUSAL_FIXTURE),
            &standalone_options(r#"{"module_type":"indicator"}"#),
        )
        .expect("the unmodified fixture program builds");
    }

    #[test]
    fn a_host_import_refusal_is_reported_as_a_capability_refusal() {
        let program = front_end(HOST_SITE);
        let (detail, error) = refusal(&program, r#"{"module_type":"strategy"}"#);

        // The fields a host already reads are the ones it read before.
        assert_eq!(detail.code, "StandaloneWasmNotStandalone");
        assert_eq!(
            detail.message,
            "standalone WebAssembly export requires a standalone-eligible program: \
             program would import host.vm"
        );
        assert_eq!(detail.message, error.to_string());
        assert_eq!(detail.imports, Some(vec!["host.vm".to_string()]));
        // `host.vm` names a mechanism; `feature` names what needs it.
        assert_eq!(
            detail.feature.as_deref(),
            Some("input.string with no options list, string comparison (== / !=)")
        );

        assert_eq!(detail.class, "capability");
        assert_eq!(detail.kind, "StandaloneWasmNotStandalone");
        let Some(StandaloneWasmRefusalSite::Pc(pc)) = detail.site else {
            panic!("expected an instruction site, got {:?}", detail.site);
        };
        assert_eq!(
            detail.site.map(StandaloneWasmRefusalSite::wire),
            Some(("pc", pc as f64))
        );
    }

    /// The calls named are the ones in the program the refusal reports on.
    /// `signal_slice` is on unless the host turns it off, so by default that
    /// is the signal slice, and a host call that only feeds the chart is not
    /// what stands between the script and its signal.
    #[test]
    fn a_host_import_refusal_names_the_calls_of_the_program_it_reports_on() {
        let program = front_end(
            "//@version=6\nindicator(\"names\")\n\
             label.new(bar_index, close, str.tostring(close))\n\
             plot(close, \"Price\")\n\
             plot(month(time) > 6 ? 1 : -1, \"Backtest Signal\")\n",
        );
        let (sliced, _) = refusal(&program, r#"{"module_type":"strategy","signal_plot":1}"#);
        assert_eq!(sliced.code, "StandaloneWasmNotStandalone");
        assert_eq!(sliced.feature.as_deref(), Some("month"));

        let (whole, _) = refusal(
            &program,
            r#"{"module_type":"strategy","signal_plot":1,"signal_slice":false}"#,
        );
        assert_eq!(whole.code, "StandaloneWasmNotStandalone");
        // The whole program also formats the label's text on the host.
        assert_eq!(whole.feature.as_deref(), Some("month, str.tostring"));

        // The price plot reads none of them.
        compile_standalone_wasm_bytes(
            &program,
            &standalone_options(r#"{"module_type":"strategy","signal_plot":0}"#),
        )
        .expect("the label is erased and the calendar read is not reached");
    }

    #[test]
    fn a_planted_type_conflict_is_reported_as_an_internal_refusal() {
        let mut program = front_end(REFUSAL_FIXTURE);
        plant_mixed_slot(&mut program);
        pinewasm_csil::validate(&program).expect("the validator accepts the planted program");
        let (detail, error) = refusal(&program, r#"{"module_type":"indicator"}"#);

        assert_eq!(detail.code, "StandaloneWasmProgramUnsupported");
        assert_eq!(detail.message, error.to_string());
        assert!(
            detail.message.starts_with("type conflict: "),
            "{}",
            detail.message
        );
        assert_eq!(detail.imports, None);
        assert_eq!(detail.feature, None);

        assert_eq!(detail.class, "internal");
        assert_eq!(detail.kind, "TypeConflict");
        assert_eq!(detail.site, None);
    }

    #[test]
    fn input_and_plot_refusals_carry_their_site() {
        let (detail, _) = refusal(&front_end(DERIVED_SOURCE), r#"{"module_type":"strategy"}"#);
        assert_eq!(detail.code, "StandaloneWasmSourceInputUnsupported");
        assert_eq!(detail.class, "capability");
        assert_eq!(detail.kind, "StandaloneWasmSourceInputUnsupported");
        assert_eq!(detail.site, Some(StandaloneWasmRefusalSite::Input(1)));
        assert_eq!(
            detail.site.map(StandaloneWasmRefusalSite::wire),
            Some(("input", 1.0))
        );

        let (detail, _) = refusal(
            &front_end(REFUSAL_FIXTURE),
            r#"{"module_type":"strategy","signal_plot":7}"#,
        );
        assert_eq!(detail.code, "StandaloneWasmNoDesignatedPlot");
        assert_eq!(detail.class, "capability");
        assert_eq!(detail.kind, "StandaloneWasmNoDesignatedPlot");
        assert_eq!(detail.site, Some(StandaloneWasmRefusalSite::Plot(7)));
        assert_eq!(
            detail.site.map(StandaloneWasmRefusalSite::wire),
            Some(("plot", 7.0))
        );
    }

    /// One refusal code carries both classes, so a host cannot classify on
    /// `code`. This is the case the `class` field exists for.
    #[test]
    fn one_code_carries_both_classes() {
        use pinewasm_aot::AotError;
        let declared = StandaloneWasmRefusalDetail::new(
            &AotError::UnsupportedOpcode("PineIntDiv".into()),
            None,
        );
        let defect = StandaloneWasmRefusalDetail::new(
            &AotError::TypeConflict("pc 9: operand is F64, needs I64".into()),
            None,
        );
        assert_eq!(declared.code, "StandaloneWasmProgramUnsupported");
        assert_eq!(defect.code, declared.code);
        assert_eq!(declared.message, "unsupported opcode for AOT: PineIntDiv");
        assert_eq!(
            defect.message,
            "type conflict: pc 9: operand is F64, needs I64"
        );
        assert_eq!(
            (declared.class, declared.kind),
            ("capability", "UnsupportedOpcode")
        );
        assert_eq!((defect.class, defect.kind), ("internal", "TypeConflict"));
    }

    /// The fields added to the refusal never change `code`, `imports` or
    /// `feature`, and `message` stays the error's own rendering.
    #[test]
    fn the_added_fields_leave_the_existing_ones_unchanged() {
        use pinewasm_aot::AotError;
        for error in [
            AotError::UnsupportedOpcode("PineIntDiv".into()),
            AotError::UnsupportedIntegerNaEncoding { pc: 4 },
            AotError::TypeConflict("slot 0".into()),
            AotError::InvalidProgram("type inference did not converge".into()),
            AotError::StandaloneWasmNotStandalone("program would import host.vm".into()),
            AotError::StandaloneWasmNoDesignatedPlot("program has 2 plots".into()),
            AotError::StandaloneWasmSourceInputUnsupported("hl2".into()),
            AotError::StandaloneWasmInvalidOptions("max_collection_elements".into()),
            AotError::StandaloneWasmExportSurface("missing export".into()),
            AotError::StrategyFeatureUnsupported {
                feature: "currency".into(),
            },
        ] {
            let (code, imports, feature) = classify_standalone_wasm_refusal(&error);
            let detail = StandaloneWasmRefusalDetail::new(&error, None);
            assert_eq!(detail.code, code, "{error}");
            assert_eq!(detail.imports, imports, "{error}");
            assert_eq!(detail.feature, feature, "{error}");
            assert_eq!(detail.message, error.to_string());
            assert_eq!(detail.kind, error.kind());
            assert_eq!(
                detail.class,
                standalone_wasm_refusal_class(error.class()),
                "{error}"
            );
        }
    }
}
