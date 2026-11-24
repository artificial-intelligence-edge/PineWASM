//! The `parameter_schema` and `module_metadata` custom sections: an artifact
//! that declares its own search space and its own identity.
//!
//! A Pine input declaration such as `input.int(8, "Window", minval = 1, maxval = 100)` already
//! carries everything a parameter-search host needs: a key, a
//! type, a default, and inclusive bounds. The compiler parses, types, and
//! bounds-checks it. Serializing it into the emitted module means a host can
//! learn the search space from the artifact instead of from a hand-written
//! registry entry that has to be edited and redeployed before the module can
//! be searched.
//!
//! The payload is UTF-8 JSON (see docs/aot.md §11). This section is read once
//! per artifact, never per trial, so a bespoke binary layout would buy nothing
//! and cost every reader a decoder.
//!
//! # The shape is the reader's, not ours
//!
//! Both section names and both payload shapes are fixed by the standalone WebAssembly wire
//! contract documented in `docs/aot.md`:
//! `parameter_schema` is a top-level array whose entries carry `name`,
//! `label`, and a `type` of `float | int | bool`, and `module_metadata` is the
//! object described below. Matching the contract is exact rather than approximate,
//! because a reader specified as best-effort has no way to tell us we got it
//! wrong.
//!
//! The payload follows the wire contract in docs/aot.md §11. The validated
//! program keeps the full `InputDecl` set for any PineWASM-aware host that needs
//! more than this section carries.
//!
//! Emission is a pure function of the program, with a stable field order and
//! no floating-point formatting choices left to a library, so re-emitting the
//! same program reproduces the same bytes, which makes artifact hashes stable.

use pinewasm_csil::CsilProgram;
use pinewasm_hir::{InputDecl, InputKind, InputValue};
use std::collections::BTreeSet;
use std::fmt::Write as _;

/// Custom-section name fixed by the standalone WebAssembly wire contract alongside
/// `module_metadata`.
pub const PARAMETER_SCHEMA_SECTION: &str = "parameter_schema";

/// Sibling section carrying the module's identity.
pub const MODULE_METADATA_SECTION: &str = "module_metadata";

/// The standalone WebAssembly hard cap on `module_metadata`, over which the producer audit rejects
/// the artifact rather than degrading it. It applies to this section and
/// only this section: `parameter_schema` is unbounded on both sides.
pub const MODULE_METADATA_MAX_BYTES: usize = 1024;

/// The `parameter_schema` payload: a top-level JSON array of the reader's
/// `ParameterDef`.
///
/// An absent section and an empty one mean different things ("this producer
/// says nothing about parameters" versus "this program has none"), and
/// conflating them lets a search run with no parameters and still return a
/// plausible score. Both tiers always emit the section, so `[]`
/// here is a positive statement.
pub fn parameter_schema_json(program: &CsilProgram) -> String {
    let keys = derive_keys(&program.inputs);
    let mut out = String::from("[");
    let mut first = true;
    for (index, decl) in program.inputs.iter().enumerate() {
        let Some(ty) = consumer_type(decl.kind) else {
            continue;
        };
        if !first {
            out.push(',');
        }
        first = false;
        write_parameter(&mut out, decl, &keys[index], ty);
    }
    out.push(']');
    out
}

/// standalone WebAssembly's schema view.  The reader accepts only float/int/bool, so a finite
/// `input.string(options = [...])` is exposed as the integer option index
/// that the standalone WebAssembly body maps back to a canonical string handle.  This view is
/// deliberately standalone WebAssembly-only: ordinary artifacts continue to omit strings rather
/// than misrepresenting their runtime input contract.
pub(crate) fn parameter_schema_json_standalone_wasm(
    program: &CsilProgram,
    projection: &crate::projection::StandaloneWasmProjection,
) -> String {
    let keys = derive_keys(&program.inputs);
    let mut out = String::from("[");
    let mut first = true;
    for (index, decl) in program.inputs.iter().enumerate() {
        if let Some(options) = projection.string_inputs.get(&decl.id) {
            if !first {
                out.push(',');
            }
            first = false;
            write_string_option_parameter(&mut out, decl, &keys[index], options);
            continue;
        }
        let Some(ty) = consumer_type(decl.kind) else {
            continue;
        };
        if !first {
            out.push(',');
        }
        first = false;
        write_parameter(&mut out, decl, &keys[index], ty);
    }
    out.push(']');
    out
}

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------

/// The reader's `type`, or `None` for an input its format cannot express.
///
/// `price` is a float and `time` an int (epoch ms) at runtime, and an enum
/// input's runtime value *is* its member index, so all three collapse into a
/// numeric domain a search can range over.
///
/// Strings, colors, and sources have no representation in the three types the
/// reader accepts, and it drops unrecognized entries without comment. Omitting
/// them reaches the same outcome by saying so: the parameter is absent from
/// the declared search space and stays at the default the module already
/// holds. A PineWASM-aware host can recover the original declarations from
/// the validated program associated with the module.
fn consumer_type(kind: InputKind) -> Option<&'static str> {
    match kind {
        InputKind::Int | InputKind::Time | InputKind::Enum => Some("int"),
        InputKind::Float | InputKind::Price => Some("float"),
        InputKind::Bool => Some("bool"),
        InputKind::String | InputKind::Color | InputKind::Source => None,
    }
}

/// One `ParameterDef`: `{name, label, type, default}` plus the optional
/// `min`/`max`/`step`/`group` the reader recognizes.
fn write_parameter(out: &mut String, decl: &InputDecl, key: &str, ty: &'static str) {
    out.push('{');
    field_str(out, "name", key, true);
    // A missing label drops the whole entry at the reader, so the key
    // stands in when a script declares no title. A parameter named after
    // itself is worse copy than one named by its author and better than one
    // that vanishes.
    let label = decl
        .title
        .as_deref()
        .filter(|t| !t.is_empty())
        .unwrap_or(key);
    field_str(out, "label", label, false);
    field_str(out, "type", ty, false);
    out.push_str(",\"default\":");
    write_value(out, &decl.defval);
    write_range(out, decl);
    if let Some(group) = decl.group.as_deref().filter(|g| !g.is_empty()) {
        field_str(out, "group", group, false);
    }
    out.push('}');
}

fn write_string_option_parameter(
    out: &mut String,
    decl: &InputDecl,
    key: &str,
    options: &crate::projection::StringOptions,
) {
    out.push('{');
    field_str(out, "name", key, true);
    let label = decl
        .title
        .as_deref()
        .filter(|title| !title.is_empty())
        .unwrap_or(key);
    field_str(out, "label", label, false);
    field_str(out, "type", "int", false);
    let _ = write!(out, ",\"default\":{}", options.default_index);
    let _ = write!(out, ",\"min\":0,\"max\":{}", options.handles.len() - 1);
    if options.handles.len() > 1 {
        out.push_str(",\"step\":1");
    }
    if let Some(group) = decl.group.as_deref().filter(|group| !group.is_empty()) {
        field_str(out, "group", group, false);
    }
    out.push('}');
}

/// The declared domain, as `min`/`max`/`step`, or nothing at all.
///
/// Two sources, and Pine keeps them mutually exclusive: `minval`/`maxval`/
/// `step` for a continuous range, or `options` for a discrete set
/// (`crates/pinewasm_semantics/src/inputs.rs:153`). The `parameter_schema` format
/// has only the range, so a discrete set survives here only when it happens
/// to *be* a range.
///
/// Nothing is ever invented. A declared lone bound remains useful metadata:
/// it constrains one side of a host-selected domain without pretending the
/// script constrained the other side. A range spanning an option set with
/// gaps is still omitted because that would admit values the script never
/// declared.
fn write_range(out: &mut String, decl: &InputDecl) {
    let (min, max, step) = if decl.constraints.options.is_empty() {
        // A declared step survives even with no bounds to pair it with. It is
        // a fact the script stated and the format can carry, which is a
        // different thing from a bound nobody declared.
        (
            decl.constraints.min.clone(),
            decl.constraints.max.clone(),
            decl.constraints.step.clone(),
        )
    } else {
        match range_from_options(&decl.constraints.options) {
            Some((min, max, step)) => (Some(min), Some(max), step),
            None => (None, None, None),
        }
    };

    if let Some(min) = min {
        out.push_str(",\"min\":");
        write_value(out, &min);
    }
    if let Some(max) = max {
        out.push_str(",\"max\":");
        write_value(out, &max);
    }
    if let Some(step) = step {
        out.push_str(",\"step\":");
        write_value(out, &step);
    }
}

/// A discrete option set expressed as a range, when the values form an exact
/// arithmetic progression. `None` when they do not.
///
/// The gap this closes exists in both directions:
///
/// - An `input.enum` restricted to a subset keeps the **declaring enum's**
///   member indexes, so `options = [Mode.first, Mode.third]` keeps `[0, 2]`, not
///   `[0, 1]`. Deriving the range from the option *count* would have declared
///   `0..=1`, letting a search pick an undeclared member while never reaching
///   the declared one.
/// - `input.int`/`input.float` with `options` carry no `minval`/`maxval` at
///   all, so their legal domain lives here or nowhere.
///
/// A progression is required rather than just min and max, because
/// `[10, 20, 50]` spanned as `10..=50` admits 37. `[10, 20, 30]` spanned as
/// `10..=30 step 10` admits exactly the declared three.
fn range_from_options(
    options: &[InputValue],
) -> Option<(InputValue, InputValue, Option<InputValue>)> {
    let ints: Option<Vec<i64>> = options
        .iter()
        .map(|option| match option {
            InputValue::Int(value) => Some(*value),
            // An enum's runtime value is its member index.
            InputValue::Enum { index, .. } => Some(i64::from(*index)),
            _ => None,
        })
        .collect();
    if let Some(mut values) = ints {
        values.sort_unstable();
        let (min, max, step) = progression(&values, |a, b| b.checked_sub(a))?;
        return Some((
            InputValue::Int(min),
            InputValue::Int(max),
            step.map(InputValue::Int),
        ));
    }

    let floats: Option<Vec<f64>> = options
        .iter()
        .map(|option| match option {
            InputValue::Float(value) if value.is_finite() => Some(*value),
            _ => None,
        })
        .collect();
    let mut values = floats?;
    values.sort_by(f64::total_cmp);
    // Exact equality on the differences, deliberately. A tolerance here would
    // be inventing a domain again, just more quietly.
    let (min, max, step) = progression(&values, |a, b| Some(b - a))?;
    Some((
        InputValue::Float(min),
        InputValue::Float(max),
        step.map(InputValue::Float),
    ))
}

/// `(min, max, step)` of a sorted slice whose successive differences are all
/// equal and positive. A single value is its own degenerate range with no
/// step. Duplicates (a zero difference) disqualify the set.
fn progression<T: Copy + PartialEq + PartialOrd + Default>(
    values: &[T],
    diff: impl Fn(T, T) -> Option<T>,
) -> Option<(T, T, Option<T>)> {
    let (&first, &last) = (values.first()?, values.last()?);
    if values.len() == 1 {
        return Some((first, last, None));
    }
    let step = diff(values[0], values[1])?;
    if step <= T::default() {
        return None;
    }
    for pair in values.windows(2) {
        if diff(pair[0], pair[1])? != step {
            return None;
        }
    }
    Some((first, last, Some(step)))
}

// ---------------------------------------------------------------------------
// Module identity
// ---------------------------------------------------------------------------

/// The `module_metadata` payload: the artifact's proposed identity.
///
/// `module_id` is kebab-case, not `snake_case`. That is forced rather than
/// chosen: the reader validates this field against
/// `^[a-z0-9]([a-z0-9-]{0,62}[a-z0-9])?$`, which admits no underscores, so a
/// `snake_case` value is structurally rejected.
///
/// The value is a *proposal*, not a claim of ownership. It is slugged from the
/// script's title, so two authors exporting a script called "Momentum" both
/// propose `momentum`.
pub fn module_metadata_json(program: &CsilProgram) -> String {
    let kind = if program.metadata.strategy.is_some() {
        "strategy"
    } else {
        "indicator"
    };
    let mut out = String::from("{\"schema_version\":1");
    write_identity(&mut out, program, kind);
    out.push('}');
    out
}

/// The shared identity triple (`module_id`, `display_name`, `type`).
fn write_identity(out: &mut String, program: &CsilProgram, kind: &str) {
    let title = program
        .metadata
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let id = title.map(kebab_slug).filter(|s| !s.is_empty());
    let id = id.as_deref().unwrap_or(kind);
    // The reader takes 1..=100 characters and drops the field outside that,
    // counted in characters rather than bytes so a non-ASCII title is clamped
    // where the reader clamps it.
    let display: String = title.unwrap_or(id).chars().take(100).collect();
    field_str(out, "module_id", id, false);
    field_str(out, "display_name", &display, false);
    field_str(out, "type", kind, false);
}

/// The standalone WebAssembly-mode `module_metadata` payload:
/// the identity triple with `type` taken from [`crate::StandaloneWasmOptions`] rather
/// than `strategy.is_some()` (the contract surface decides, not the
/// source construct), plus `abi_version: 1` — the standalone WebAssembly **reader** contract,
/// deliberately distinct from the CSAW header's `ABI_VERSION` — and, iff
/// strategy-type, the `signal` object whose optional
/// `source` is the designated plot's title under the existing slugging
/// rule, clamped to the spec's 64-byte bound and omitted when it slugs to
/// nothing.
///
/// standalone WebAssembly-only by design: emitting `abi_version` unconditionally would change
/// every existing chart-interface artifact's bytes.
pub fn module_metadata_json_standalone_wasm(
    program: &CsilProgram,
    module_type: crate::StandaloneWasmModuleType,
    signal_plot: Option<u32>,
    strategy_source: bool,
    signal_semantics: crate::StandaloneWasmSignalSemantics,
) -> String {
    let kind = match module_type {
        crate::StandaloneWasmModuleType::Strategy => "strategy",
        crate::StandaloneWasmModuleType::Indicator => "indicator",
    };
    let mut out = String::from("{\"schema_version\":1");
    write_identity(&mut out, program, kind);
    out.push_str(",\"abi_version\":1");
    if strategy_source {
        // A strategy()-sourced module: signals come from the
        // linked broker's flat-capable posture lane under
        // `posture-events` semantics; `source` is the literal "strategy".
        out.push_str(
            ",\"signal\":{\"source\":\"strategy\",\"semantics\":\"posture-events\",\"price_source\":\"close\"}",
        );
        out.push_str(",\"scoring_basis\":\"broker-trades-v1\"");
    } else if matches!(module_type, crate::StandaloneWasmModuleType::Strategy) {
        out.push_str(",\"signal\":{");
        let source = signal_plot
            .and_then(|plot| program.plots.get(plot as usize))
            .and_then(|plot| plot.title.as_deref())
            .map(slug)
            .map(|mut s| {
                // The slug is pure ASCII, so a byte truncate is a char
                // truncate; trim after clamping so it cannot end in `_`.
                s.truncate(64);
                while s.ends_with('_') {
                    s.pop();
                }
                s
            })
            .filter(|s| !s.is_empty());
        if let Some(source) = source {
            field_str(&mut out, "source", &source, true);
            out.push(',');
        }
        field_str(&mut out, "semantics", signal_semantics.as_str(), true);
        out.push_str(",\"price_source\":\"close\"}");
    }
    out.push('}');
    out
}

/// `"Example Momentum"` → `"example-momentum"`.
///
/// Same shape as [`slug`] with a hyphen separator and a 64-character ceiling,
/// matching the reader's pattern: ASCII alphanumerics survive lowercased,
/// every other character becomes one separator, runs collapse, and the edges
/// are trimmed so the result can never start or end with a hyphen. Trimming
/// runs *after* the clamp, because truncating mid-separator is the one way a
/// valid slug becomes an invalid one.
fn kebab_slug(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut pending_break = false;
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_break && !out.is_empty() {
                out.push('-');
            }
            pending_break = false;
            out.push(ch.to_ascii_lowercase());
        } else {
            pending_break = true;
        }
    }
    // Every pushed byte is ASCII, so a byte truncate is a char truncate.
    out.truncate(64);
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// One input value in its declared domain: the number, boolean, or string a
/// host would set. `na` and the host-only external-source override (which
/// cannot appear in compiler-authored metadata) serialize as `null`.
fn write_value(out: &mut String, value: &InputValue) {
    match value {
        InputValue::Int(v) => {
            let _ = write!(out, "{v}");
        }
        InputValue::Float(v) => write_f64(out, *v),
        InputValue::Bool(v) => out.push_str(if *v { "true" } else { "false" }),
        InputValue::String(v) => write_json_string(out, v),
        // Transparency is 0..=100, not an alpha byte, so packing it into a
        // hex string would either lose it or misrepresent it as alpha.
        InputValue::Color { r, g, b, t } => {
            let _ = write!(out, "{{\"r\":{r},\"g\":{g},\"b\":{b},\"t\":{t}}}");
        }
        InputValue::Source(series) => write_json_string(out, series.name()),
        InputValue::ExternalSource(_) => out.push_str("null"),
        InputValue::Enum { index, .. } => {
            let _ = write!(out, "{index}");
        }
    }
}

/// JSON has no NaN or infinity; `na` defaults become `null`.
///
/// Whole floats keep a `.0`. JSON itself does not distinguish 2 from 2.0, but
/// the parsers on the other side do (Python's `json` yields `int` for one and
/// `float` for the other), and a float parameter whose default silently
/// arrives as an int is exactly the sort of thing that type-checks all the way
/// to a wrong distribution. Rust's `{}` never uses exponent notation for
/// finite f64, so testing for a `.` is sufficient.
pub(crate) fn write_f64(out: &mut String, value: f64) {
    if value.is_finite() {
        let start = out.len();
        let _ = write!(out, "{value}");
        if !out[start..].contains('.') {
            out.push_str(".0");
        }
    } else {
        out.push_str("null");
    }
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

/// A stable, machine-usable key per input, in declaration order.
///
/// Pine inputs have no key of their own: `id` is the durable identity and the
/// title is the human one. A host's registry wants a name (an env var, a
/// search parameter, a CLI flag), so this slugs the title and falls back to
/// `input_<id>`. Collisions are resolved by suffix rather than by trusting
/// titles to be unique, because they frequently are not (two `Length` inputs
/// in different groups is ordinary Pine).
fn derive_keys(inputs: &[InputDecl]) -> Vec<String> {
    let mut taken: BTreeSet<String> = BTreeSet::new();
    let mut keys = Vec::with_capacity(inputs.len());
    for decl in inputs {
        let base = decl
            .title
            .as_deref()
            .map(slug)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("input_{}", decl.id));
        let mut key = base.clone();
        let mut n = 2u32;
        while !taken.insert(key.clone()) {
            key = format!("{base}_{n}");
            n += 1;
        }
        keys.push(key);
    }
    keys
}

/// `"Window Size"` → `"window_size"`. ASCII-lowercased; every other
/// character becomes one `_`, with runs collapsed and edges trimmed. Non-ASCII
/// titles slug to nothing and fall back to `input_<id>`.
fn slug(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut pending_break = false;
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_break && !out.is_empty() {
                out.push('_');
            }
            pending_break = false;
            out.push(ch.to_ascii_lowercase());
        } else {
            pending_break = true;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The standalone WebAssembly key table
// ---------------------------------------------------------------------------

/// Build the standalone WebAssembly key table: the packed per-input records the
/// runtime blob's `standalone_wasm_fill_lanes` walks to write declaration defaults and
/// overlay wire `params` — byte layout defined on that function
/// (`runtime/src/standalone_wasm.rs`): `kind: u8, key_len: u8, index: u16 LE,
/// cell: [u8; 8], key: [u8; key_len]`.
///
/// Wire keys are the same [`derive_keys`] slugs `parameter_schema`
/// publishes (collision suffixes included), so a host can echo `name`s from
/// that section back as `params` keys verbatim. Cell encodings mirror the
/// input-table cells the artifact's reads expect (`body.rs`
/// `emit_read_input`): float/price f64, int/time/enum exact LE i64, bool
/// f64 0/1. `input.source` declarations become kind-3 records directing a
/// builtin-lane copy; [`crate::compile_standalone_wasm_program`] already refused
/// non-builtin defaults.
pub(crate) fn standalone_wasm_key_table(
    program: &CsilProgram,
    projection: &crate::projection::StandaloneWasmProjection,
) -> Result<Vec<u8>, crate::AotError> {
    let keys = derive_keys(&program.inputs);
    let mut out = Vec::new();
    let mut source_ordinal: u16 = 0;
    for (index, decl) in program.inputs.iter().enumerate() {
        let key = keys[index].as_bytes();
        if key.len() > u8::MAX as usize {
            return Err(crate::AotError::InvalidProgram(format!(
                "input {} derives a wire key longer than 255 bytes",
                decl.id
            )));
        }
        // Kind codes mirror the blob's `standalone_wasm::kind` (runtime/src/standalone_wasm.rs).
        let (kind, table_index, cell): (u8, u16, [u8; 8]) = if let Some(options) =
            projection.string_inputs.get(&decl.id)
        {
            (
                1,
                index as u16,
                i64::from(options.default_index).to_le_bytes(),
            )
        } else {
            match decl.kind {
                InputKind::Source => {
                    let lane = match &decl.defval {
                        InputValue::Source(series) => crate::BUILTIN_LANES
                            .iter()
                            .position(|lane| lane == series)
                            .ok_or_else(|| {
                                crate::AotError::StandaloneWasmSourceInputUnsupported(format!(
                                    "input {} defaults to {:?}, not a raw builtin series",
                                    decl.id, decl.defval
                                ))
                            })?,
                        other => {
                            return Err(crate::AotError::StandaloneWasmSourceInputUnsupported(
                                format!("input {} defaults to {other:?}, not a source", decl.id),
                            ))
                        }
                    };
                    let mut cell = [0u8; 8];
                    cell[..4].copy_from_slice(&(lane as u32).to_le_bytes());
                    let ordinal = source_ordinal;
                    source_ordinal += 1;
                    (3, ordinal, cell)
                }
                InputKind::Int | InputKind::Time | InputKind::Enum => {
                    (1, index as u16, input_cell(&decl.defval))
                }
                InputKind::Bool => (2, index as u16, input_cell(&decl.defval)),
                // Float/price, plus the kinds whose cell is never read
                // (string/color): a NaN f64 cell, same as the loader writes.
                _ => (0, index as u16, input_cell(&decl.defval)),
            }
        };
        out.push(kind);
        out.push(key.len() as u8);
        out.extend_from_slice(&table_index.to_le_bytes());
        out.extend_from_slice(&cell);
        out.extend_from_slice(key);
    }
    // Same-symbol `request.security` lanes follow the source lanes. Kind 4
    // has no wire key; its key field carries a payload counted in 8-byte
    // words (a header word, the per-option timeframe table, then the
    // expression program).
    let mut lane = u32::from(source_ordinal);
    for (index, spec) in projection.security_specs.iter().enumerate() {
        let status_cell =
            u16::try_from(projection.security_status_base as usize + index).map_err(|_| {
                crate::AotError::InvalidProgram("too many request.security lanes".into())
            })?;
        let ordinal = u16::try_from(lane).map_err(|_| {
            crate::AotError::InvalidProgram("too many request.security lanes".into())
        })?;
        let payload = spec.payload();
        let words = u8::try_from(payload.len() / 8)
            .ok()
            .filter(|_| payload.len() % 8 == 0)
            .ok_or_else(|| {
                crate::AotError::InvalidProgram("request.security payload too long".into())
            })?;
        out.push(4);
        out.push(words);
        out.extend_from_slice(&ordinal.to_le_bytes());
        out.extend_from_slice(&spec.cell(status_cell));
        out.extend_from_slice(&payload);
        lane += spec.lanes();
    }
    Ok(out)
}

/// A default's 8 input-table bytes — the encoding the loader-side
/// `input_table_bytes` (tests/differential.rs) and host loaders both write.
fn input_cell(value: &InputValue) -> [u8; 8] {
    match value {
        InputValue::Int(v) => v.to_le_bytes(),
        InputValue::Float(v) => v.to_le_bytes(),
        InputValue::Bool(b) => {
            let value: f64 = if *b { 1.0 } else { 0.0 };
            value.to_le_bytes()
        }
        InputValue::Enum { index, .. } => i64::from(*index).to_le_bytes(),
        // Source/string/color values never land in the numeric table.
        _ => f64::NAN.to_le_bytes(),
    }
}

// ---------------------------------------------------------------------------
// JSON strings
// ---------------------------------------------------------------------------

fn field_str(out: &mut String, name: &str, value: &str, first: bool) {
    if !first {
        out.push(',');
    }
    let _ = write!(out, "\"{name}\":");
    write_json_string(out, value);
}

/// Escape per RFC 8259. Rust strings are valid UTF-8, so there are no lone
/// surrogates to encode; everything below 0x20 takes the short form where one
/// exists and `\u00XX` otherwise.
pub(crate) fn write_json_string(out: &mut String, value: &str) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_titles() {
        assert_eq!(slug("Neighbors Count"), "neighbors_count");
        assert_eq!(slug("  RSI   Length  "), "rsi_length");
        assert_eq!(slug("ADX-Threshold (1)"), "adx_threshold_1");
        assert_eq!(slug("%K"), "k");
        assert_eq!(slug("длина"), "");
    }

    #[test]
    fn escapes_json_strings() {
        let mut out = String::new();
        write_json_string(&mut out, "a\"b\\c\nd\te\u{01}f");
        assert_eq!(out, "\"a\\\"b\\\\c\\nd\\te\\u0001f\"");
    }

    fn enum_options(indexes: &[u32]) -> Vec<InputValue> {
        indexes
            .iter()
            .map(|&index| InputValue::Enum {
                name: "Mode".to_string(),
                index,
                member: format!("m{index}"),
                title: None,
            })
            .collect()
    }

    fn rendered(options: &[InputValue]) -> Option<String> {
        let (min, max, step) = range_from_options(options)?;
        let mut out = String::new();
        write_value(&mut out, &min);
        out.push_str("..");
        write_value(&mut out, &max);
        if let Some(step) = step {
            out.push_str(" by ");
            write_value(&mut out, &step);
        }
        Some(out)
    }

    #[test]
    fn an_option_set_becomes_a_range_only_when_it_is_one() {
        // A subset keeps the declaring enum's indexes, so this is [0, 2] and
        // the range has to step over the gap rather than span it. Spanning
        // 0..=1 would offer an undeclared member and hide a declared one.
        assert_eq!(
            rendered(&enum_options(&[0, 2])).as_deref(),
            Some("0..2 by 2")
        );
        assert_eq!(
            rendered(&enum_options(&[0, 1, 2])).as_deref(),
            Some("0..2 by 1")
        );
        // Not a progression: no range at all, rather than one admitting 1.
        assert_eq!(rendered(&enum_options(&[0, 2, 3])), None);
        // A single option is its own degenerate range with nothing to step.
        assert_eq!(rendered(&enum_options(&[3])).as_deref(), Some("3..3"));

        // Numeric `options` carry no minval/maxval, so this is the only place
        // their domain can come from.
        let ints = |v: &[i64]| v.iter().copied().map(InputValue::Int).collect::<Vec<_>>();
        assert_eq!(
            rendered(&ints(&[10, 20, 30])).as_deref(),
            Some("10..30 by 10")
        );
        // Spanning this as 10..=50 would admit 37.
        assert_eq!(rendered(&ints(&[10, 20, 50])), None);
        // Unsorted input still reads as the progression it is.
        assert_eq!(
            rendered(&ints(&[30, 10, 20])).as_deref(),
            Some("10..30 by 10")
        );
        // Duplicates leave a zero difference, which is not a step.
        assert_eq!(rendered(&ints(&[5, 5])), None);

        let floats = |v: &[f64]| v.iter().copied().map(InputValue::Float).collect::<Vec<_>>();
        assert_eq!(
            rendered(&floats(&[0.5, 1.0, 1.5])).as_deref(),
            Some("0.5..1.5 by 0.5")
        );
        // Exact equality on differences: 0.1 spacing is not exact in binary,
        // so this declines rather than inventing a step.
        assert_eq!(rendered(&floats(&[0.1, 0.2, 0.3])), None);

        // A set the format cannot express in any form.
        let strings = vec![
            InputValue::String("SMA".to_string()),
            InputValue::String("EMA".to_string()),
        ];
        assert_eq!(rendered(&strings), None);
    }

    #[test]
    fn non_finite_defaults_are_null() {
        let mut out = String::new();
        write_f64(&mut out, f64::NAN);
        write_f64(&mut out, f64::INFINITY);
        assert_eq!(out, "nullnull");
    }
}
