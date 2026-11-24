//! Versioned, transport-neutral language queries over an explicitly supplied document set.
//! No filesystem lookup, global workspace, network, or executable command is available.

use crate::analysis::{
    exported_symbol_from_source, is_builtin_series_name, is_reserved_user_function_name,
    DocumentAnalysis, LibrarySource, LocalSymbol, LocalSymbolKind,
};
use crate::text::LineIndex;
use pinewasm_span::{SourceId, Span};
use pinewasm_syntax::TokenKind;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fmt;

pub const SERVICE_VERSION: &str = "pinewasm-language-v1";
pub const MAX_SOURCE_BYTES: usize = 256 * 1024;
pub const MAX_RESULTS: usize = 4096;
/// Maximum estimated resolver work units for references and rename. The
/// estimate is deterministic and is advertised by
/// `languageCapabilities()`.
pub const MAX_RESOLUTION_WORK: usize = 64 * 1024 * 1024;
pub const RESOLUTION_WORK_BUDGET_EXCEEDED: &str = "query_work_budget_exceeded";

/// Machine-readable query failures. The work-budget variant is kept distinct
/// so callers can report a bounded refusal without treating it as an empty or
/// truncated result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryError {
    Message(String),
    WorkBudgetExceeded,
}

impl QueryError {
    pub fn code(&self) -> &str {
        match self {
            Self::Message(message) => message,
            Self::WorkBudgetExceeded => RESOLUTION_WORK_BUDGET_EXCEEDED,
        }
    }
}

impl fmt::Display for QueryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for QueryError {}

impl From<String> for QueryError {
    fn from(message: String) -> Self {
        Self::Message(message)
    }
}

impl From<&str> for QueryError {
    fn from(message: &str) -> Self {
        Self::Message(message.to_owned())
    }
}

fn exceeds_resolution_work(analysis: &DocumentAnalysis, resolution_passes: usize) -> bool {
    let identifiers = analysis
        .tokens()
        .iter()
        .filter(|token| token.kind == TokenKind::Ident)
        .count();
    let symbol_candidates = analysis.symbols().len();
    let token_scans = analysis.tokens().len();
    let source_bytes = analysis.source().len();
    let token_lookup_depth = usize::BITS as usize - token_scans.max(1).leading_zeros() as usize;
    let candidate_checks = identifiers
        .saturating_mul(symbol_candidates)
        .saturating_mul(resolution_passes);
    let indexed_token_lookups = identifiers
        .saturating_mul(token_lookup_depth)
        .saturating_mul(resolution_passes);
    let scan_work = token_scans
        .saturating_add(source_bytes)
        .saturating_mul(resolution_passes);
    candidate_checks
        .saturating_add(indexed_token_lookups)
        .saturating_add(scan_work)
        > MAX_RESOLUTION_WORK
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Document {
    pub id: String,
    pub version: String,
    pub source: String,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Library {
    pub id: String,
    pub version: String,
    pub import_path: String,
    pub source: String,
}
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Capability {
    Analyze,
    Hover,
    Definition,
    References,
    CodeActions,
    PrepareRename,
    Rename,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub schema_version: u32,
    pub capability: Capability,
    pub document: Document,
    #[serde(default)]
    pub libraries: Vec<Library>,
    pub position: Option<Position>,
    pub range: Option<Range>,
    pub new_name: Option<String>,
    #[serde(default)]
    pub include_declaration: bool,
}
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Response {
    pub schema_version: u32,
    pub service_version: &'static str,
    pub capability: Capability,
    pub document_id: String,
    pub document_version: String,
    pub data: Value,
}

pub struct Hover {
    pub markdown: String,
    pub span: Span,
}
pub struct QuickFix {
    pub title: String,
    pub span: Span,
    pub text: String,
    pub diagnostic_code: Option<String>,
}

pub fn range(index: &LineIndex, span: Span) -> Range {
    let (line, character) = index.line_utf16(span.start);
    let (end_line, end_character) = index.line_utf16(span.end);
    Range {
        start: Position { line, character },
        end: Position {
            line: end_line,
            character: end_character,
        },
    }
}

pub fn hover(analysis: &DocumentAnalysis, offset: u32) -> Option<Hover> {
    if analysis
        .dotted_name_at(offset)
        .is_some_and(|(name, _)| name.contains('.'))
    {
        if let Some((item, span)) = analysis.language_item_at(offset) {
            return Some(Hover {
                markdown: builtin_markdown(item),
                span,
            });
        }
    }
    if let Some(symbol) = analysis.local_symbol_at(offset) {
        return Some(Hover {
            markdown: local_markdown(symbol, analysis.line_index()),
            span: symbol.span,
        });
    }
    analysis.language_item_at(offset).map(|(item, span)| Hover {
        markdown: builtin_markdown(item),
        span,
    })
}

pub fn local_definition(analysis: &DocumentAnalysis, offset: u32) -> Option<Span> {
    analysis.local_symbol_at(offset).map(|symbol| symbol.span)
}

pub fn references(
    analysis: &DocumentAnalysis,
    offset: u32,
    include_declaration: bool,
) -> Result<Vec<Span>, QueryError> {
    if exceeds_resolution_work(analysis, 1) {
        return Err(QueryError::WorkBudgetExceeded);
    }
    let Some(symbol) = analysis.local_symbol_at(offset) else {
        return Ok(Vec::new());
    };
    Ok(analysis
        .references_to(symbol)
        .into_iter()
        .filter(|span| include_declaration || *span != symbol.span)
        .collect())
}

pub fn code_actions(analysis: &DocumentAnalysis, selection: Range) -> Vec<QuickFix> {
    let mut actions = Vec::new();
    for diagnostic in analysis.diagnostics_for_source(SourceId::MAIN) {
        let Some(span) = diagnostic.primary_span() else {
            continue;
        };
        let location = range(analysis.line_index(), span);
        if location.end < selection.start || location.start > selection.end {
            continue;
        }
        let mut suggestions = diagnostic
            .notes
            .iter()
            .map(String::as_str)
            .chain(diagnostic.labels.iter().map(|label| label.message.as_str()))
            .filter_map(did_you_mean)
            .collect::<Vec<_>>();
        suggestions.sort_unstable();
        suggestions.dedup();
        for suggestion in suggestions {
            actions.push(QuickFix {
                title: format!("Replace with `{suggestion}`"),
                span,
                text: suggestion.to_string(),
                diagnostic_code: diagnostic.code.clone(),
            });
        }
    }
    actions
}

pub fn prepare_rename(analysis: &DocumentAnalysis, offset: u32) -> Option<Span> {
    analysis
        .local_symbol_at(offset)
        .filter(|symbol| renameable_symbol(symbol))
        .and_then(|_| analysis.identifier_at(offset).map(|(_, span)| span))
}

pub fn checked_rename(
    analysis: &DocumentAnalysis,
    offset: u32,
    new_name: &str,
) -> Result<Vec<Span>, String> {
    if !valid_identifier(new_name) {
        return Err(format!("`{new_name}` is not a valid Pine identifier"));
    }
    if exceeds_resolution_work(analysis, 3) {
        return Err(RESOLUTION_WORK_BUDGET_EXCEEDED.into());
    }
    let Some(symbol) = analysis.local_symbol_at(offset) else {
        return Err("only user-defined symbols can be renamed".into());
    };
    if !renameable_symbol(symbol) {
        return Err("field, enum-member, and method rename requires typed member resolution and is not exposed".into());
    }
    if symbol.kind == LocalSymbolKind::Function && is_reserved_user_function_name(new_name) {
        return Err(format!(
            "`{new_name}` is reserved by the compiler as a built-in callable"
        ));
    }
    if is_builtin_series_name(new_name) {
        return Err(format!(
            "`{new_name}` is reserved by the compiler as a built-in series"
        ));
    }
    if analysis.symbols().iter().any(|candidate| {
        candidate.name == new_name
            && candidate.span != symbol.span
            && candidate.container == symbol.container
            && candidate.shares_lexical_scope(symbol)
    }) {
        return Err(format!("`{new_name}` is already declared in this scope"));
    }
    if !analysis.rename_preserves_bindings(symbol, new_name) {
        return Err(format!(
            "renaming to `{new_name}` would capture or change an existing symbol reference"
        ));
    }
    Ok(analysis.references_to(symbol))
}

pub(crate) fn did_you_mean(message: &str) -> Option<&str> {
    message
        .strip_prefix("did you mean `")
        .and_then(|value| value.strip_suffix("`?"))
}

pub(crate) fn valid_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(first) if first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
        && pinewasm_syntax::TokenKind::keyword_from_str(name).is_none()
}
fn renameable_symbol(symbol: &LocalSymbol) -> bool {
    !matches!(
        symbol.kind,
        LocalSymbolKind::Field | LocalSymbolKind::EnumMember | LocalSymbolKind::Method
    )
}
pub(crate) fn local_markdown(symbol: &LocalSymbol, index: &LineIndex) -> String {
    let heading = symbol
        .signature
        .as_ref()
        .map_or_else(|| symbol.name.clone(), Clone::clone);
    format!(
        "**{}** *({})*\n\n```pine\n{}\n```\n\n{}\n\nDeclared on line {}.",
        symbol.name,
        symbol.kind.label(),
        heading,
        symbol.detail,
        index.line_utf16(symbol.span.start).0 + 1
    )
}
pub(crate) fn builtin_markdown(item: &crate::analysis::LanguageItem) -> String {
    let mut markdown = format!("**{}** *({:?})*\n\n{}", item.name, item.kind, item.detail);
    if let Some(signature) = &item.signature {
        markdown.push_str("\n\n```pine\n");
        markdown.push_str(signature);
        markdown.push_str("\n```");
    }
    if !item.parameters.is_empty() {
        markdown.push_str("\n\n**Parameters**\n");
        for parameter in &item.parameters {
            markdown.push_str(&format!(
                "\n- `{}`{} — {}",
                parameter.label,
                if parameter.required {
                    ""
                } else {
                    " (optional)"
                },
                parameter.detail
            ));
        }
    }
    markdown
}

fn edit(document: &Document, index: &LineIndex, span: Span, replacement: &str) -> Value {
    json!({ "documentId": document.id, "documentVersion": document.version,
        "startUtf16": index.utf16_offset(span.start),
        "endUtf16": index.utf16_offset(span.end), "text": replacement })
}

pub fn query(request: Request) -> Result<Response, QueryError> {
    if request.schema_version != 1 {
        return Err("language_contract_unavailable".into());
    }
    let mut ids = HashSet::new();
    let mut imports = HashSet::new();
    if request.document.id.is_empty()
        || request.document.id.len() > 128
        || request.document.version.len() > 128
        || request.document.version.is_empty()
        || request.libraries.len() > 32
    {
        return Err("invalid_document_identity".into());
    }
    ids.insert(request.document.id.as_str());
    let mut bytes = request.document.source.len();
    for library in &request.libraries {
        if library.id.is_empty()
            || library.id.len() > 128
            || library.version.is_empty()
            || library.version.len() > 128
            || library.import_path.is_empty()
            || library.import_path.len() > 256
            || !ids.insert(library.id.as_str())
            || !imports.insert(library.import_path.as_str())
        {
            return Err("invalid_library_identity".into());
        }
        bytes = bytes
            .checked_add(library.source.len())
            .ok_or("source_budget_exceeded")?;
    }
    if bytes > MAX_SOURCE_BYTES {
        return Err("source_budget_exceeded".into());
    }
    let libraries = request
        .libraries
        .iter()
        .map(|library| LibrarySource {
            import_path: library.import_path.clone(),
            source: library.source.clone(),
            file_name: Some(library.id.clone()),
        })
        .collect();
    let analysis = DocumentAnalysis::new(&request.document.id, &request.document.source, libraries);
    let position_offset = |position: Position| -> Result<u32, QueryError> {
        let offset = analysis
            .line_index()
            .byte_offset_utf16(position.line, position.character);
        if analysis.line_index().line_utf16(offset) != (position.line, position.character) {
            return Err("invalid_utf16_position".into());
        }
        Ok(offset)
    };
    let offset = request.position.map(position_offset).transpose()?;
    if let Some(selection) = request.range {
        if selection.start > selection.end {
            return Err("invalid_range".into());
        }
        position_offset(selection.start)?;
        position_offset(selection.end)?;
    }
    let location = |span| json!({ "documentId": request.document.id, "documentVersion": request.document.version, "range": range(analysis.line_index(), span) });
    let data = match request.capability {
        Capability::Analyze => {
            let source_diagnostics = analysis.diagnostics();
            if source_diagnostics.len() > MAX_RESULTS {
                return Err("result_budget_exceeded".into());
            }
            let library_line_indexes = request
                .libraries
                .iter()
                .map(|library| LineIndex::new(&library.source))
                .collect::<Vec<_>>();
            let diagnostics = source_diagnostics.iter().map(|diagnostic| {
                let labels = diagnostic.labels.iter().map(|label| {
                    let location = if label.span.source == SourceId::MAIN {
                        Some(json!({
                            "documentId": request.document.id,
                            "documentVersion": request.document.version,
                            "range": range(analysis.line_index(), label.span)
                        }))
                    } else {
                        label.span.source.0.checked_sub(1).and_then(|index| {
                            let index = index as usize;
                            request.libraries.get(index).zip(library_line_indexes.get(index))
                        })
                            .map(|(library, line_index)| json!({
                                "documentId": library.id,
                                "documentVersion": library.version,
                                "range": range(line_index, label.span)
                            }))
                    };
                    json!({ "message": label.message, "location": location })
                }).collect::<Vec<_>>();
                json!({ "severity": diagnostic.severity, "code": diagnostic.code, "message": diagnostic.message, "labels": labels, "notes": diagnostic.notes })
            }).collect::<Vec<_>>();
            json!({ "success": analysis.success(), "diagnostics": diagnostics })
        },
        Capability::Hover => hover(&analysis, offset.ok_or("position_required")?).map_or(Value::Null, |value| json!({ "markdown": value.markdown, "range": range(analysis.line_index(), value.span) })),
        Capability::Definition => {
            let offset = offset.ok_or("position_required")?;
            let imported = analysis.dotted_name_at(offset).and_then(|(name, _)| name.split_once('.'))
                .and_then(|(alias, member)| analysis.symbols().iter().find(|symbol| symbol.kind == LocalSymbolKind::Module && symbol.name == alias)
                .and_then(|symbol| symbol.detail.strip_prefix("import "))
                .and_then(|path| request.libraries.iter().find(|library| library.import_path == path))
                .and_then(|library| exported_symbol_from_source(&library.source, member)
                .map(|symbol| json!({ "documentId": library.id, "documentVersion": library.version, "range": range(&LineIndex::new(&library.source), symbol.span) }))));
            imported.or_else(|| local_definition(&analysis, offset).map(location)).unwrap_or(Value::Null)
        }
        Capability::References => {
            let offset = offset.ok_or("position_required")?;
            let spans = references(&analysis, offset, request.include_declaration)?;
            if spans.len() > MAX_RESULTS { return Err("result_budget_exceeded".into()); }
            Value::Array(spans.into_iter().map(location).collect())
        }
        Capability::CodeActions => {
            let actions = code_actions(&analysis, request.range.ok_or("range_required")?);
            if actions.len() > MAX_RESULTS { return Err("result_budget_exceeded".into()); }
            Value::Array(actions.into_iter().map(|action| json!({ "title": action.title, "diagnosticCode": action.diagnostic_code, "edits": [edit(&request.document, analysis.line_index(), action.span, &action.text)] })).collect())
        }
        Capability::PrepareRename => prepare_rename(&analysis, offset.ok_or("position_required")?).map_or(Value::Null, |span| json!({ "range": range(analysis.line_index(), span) })),
        Capability::Rename => {
            let name = request.new_name.as_deref().ok_or("new_name_required")?;
            if name.len() > 128 { return Err("invalid_identifier".into()); }
            let offset = offset.ok_or("position_required")?;
            let spans = checked_rename(&analysis, offset, name).map_err(|error| {
                if error == RESOLUTION_WORK_BUDGET_EXCEEDED {
                    QueryError::WorkBudgetExceeded
                } else {
                    QueryError::Message(error)
                }
            })?;
            if spans.len() > MAX_RESULTS { return Err("result_budget_exceeded".into()); }
            json!({ "edits": spans.into_iter().map(|span| edit(&request.document, analysis.line_index(), span, name)).collect::<Vec<_>>() })
        }
    };
    if serde_json::to_vec(&data)
        .map_err(|_| "result_serialization_failed")?
        .len()
        > 256 * 1024
    {
        return Err("result_budget_exceeded".into());
    }
    Ok(Response {
        schema_version: 1,
        service_version: SERVICE_VERSION,
        capability: request.capability,
        document_id: request.document.id,
        document_version: request.document.version,
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixtures() -> Vec<Value> {
        serde_json::from_str(include_str!("../fixtures/language_queries.json")).unwrap()
    }
    fn fixture(name: &str) -> Request {
        serde_json::from_value(
            fixtures()
                .into_iter()
                .find(|item| item["name"] == name)
                .unwrap()["request"]
                .clone(),
        )
        .unwrap()
    }
    #[test]
    fn exact_shadowed_binding_and_utf16_edits() {
        let result = query(fixture("rename-shadow")).unwrap();
        let edits = result.data["edits"].as_array().unwrap();
        assert_eq!(edits.len(), 2);
        assert!(edits.iter().all(
            |edit| edit["documentId"] == "main-α" && edit["documentVersion"] == "sha256:main-1"
        ));
        let source = fixture("rename-shadow").document.source;
        let start = edits[0]["startUtf16"].as_u64().unwrap() as usize;
        assert_eq!(source.encode_utf16().nth(start), Some(u16::from(b'x')));
        assert_eq!(
            query(fixture("references-shadow"))
                .unwrap()
                .data
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            query(fixture("definition-shadow")).unwrap().data["range"]["start"]["line"],
            4
        );
    }
    #[test]
    fn explicit_library_identity_and_diagnostics_survive() {
        let definition = query(fixture("definition-library")).unwrap();
        assert_eq!(definition.data["documentId"], "library-A");
        assert_eq!(definition.data["documentVersion"], "lib-sha");
        let analyzed = query(fixture("diagnostics-library")).unwrap();
        assert_eq!(analyzed.data["success"], false);
        assert!(analyzed.data["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic["labels"]
                .as_array()
                .unwrap()
                .iter()
                .any(|label| label["location"]["documentId"] == "library-😀")));
    }
    #[test]
    fn two_library_source_ids_definitions_and_references_keep_request_identity() {
        let analyzed = query(fixture("analyze-two-library-order")).unwrap();
        let diagnostics = analyzed.data["diagnostics"].as_array().unwrap();
        for (needle, id, version) in [
            ("missing_second", "library-two-request-first", "two-v1"),
            ("missing_first", "library-one-request-second", "one-v1"),
        ] {
            assert!(
                diagnostics.iter().any(|diagnostic| {
                    diagnostic["message"]
                        .as_str()
                        .unwrap_or_default()
                        .contains(needle)
                        && diagnostic["labels"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|label| {
                                label["location"]["documentId"] == id
                                    && label["location"]["documentVersion"] == version
                            })
                }),
                "diagnostic for {needle} must keep {id}/{version}"
            );
        }

        for (fixture_name, id, version) in [
            ("definition-two-library-first", "library-one", "one-v1"),
            ("definition-two-library-second", "library-two", "two-v1"),
        ] {
            let definition = query(fixture(fixture_name)).unwrap();
            assert_eq!(definition.data["documentId"], id);
            assert_eq!(definition.data["documentVersion"], version);
            assert_eq!(definition.data["range"]["start"]["line"], 6);
            assert_eq!(definition.data["range"]["start"]["character"], 7);
        }

        for fixture_name in [
            "references-two-library-first",
            "references-two-library-second",
        ] {
            let references = query(fixture(fixture_name)).unwrap();
            let locations = references.data.as_array().unwrap();
            assert_eq!(locations.len(), 2, "{fixture_name}");
            assert!(locations.iter().all(|location| {
                location["documentId"] == "main-two" && location["documentVersion"] == "m-two"
            }));
        }
    }
    #[test]
    fn analyze_maps_a_large_diagnostic_set_with_one_library_index() {
        let main_source = concat!(
            "//@version=6\n",
            "import example/Large/1 as large\n",
            "indicator(\"Large diagnostics\")\n",
            "plot(large.f0())\n",
        );
        let mut library_source = String::from("//@version=6\nlibrary(\"Large\")\n");
        for _ in 0..256 {
            library_source.push_str("!\n");
        }
        let available = MAX_SOURCE_BYTES - main_source.len() - library_source.len();
        library_source.push_str(&" ".repeat(available));
        assert_eq!(main_source.len() + library_source.len(), MAX_SOURCE_BYTES);

        let request = Request {
            schema_version: 1,
            capability: Capability::Analyze,
            document: Document {
                id: "main-large-diagnostics".into(),
                version: "main-large-v1".into(),
                source: main_source.into(),
            },
            libraries: vec![Library {
                id: "large-diagnostics-library".into(),
                version: "library-large-v1".into(),
                import_path: "example/Large/1".into(),
                source: library_source,
            }],
            position: None,
            range: None,
            new_name: None,
            include_declaration: false,
        };
        let response = query(request).unwrap();
        let diagnostics = response.data["diagnostics"].as_array().unwrap();
        assert!(diagnostics.len() >= 256);
        assert!(
            diagnostics
                .iter()
                .flat_map(|diagnostic| { diagnostic["labels"].as_array().unwrap().iter() })
                .filter(|label| {
                    label["location"]["documentId"] == "large-diagnostics-library"
                        && label["location"]["documentVersion"] == "library-large-v1"
                })
                .count()
                >= 256
        );
        assert!(diagnostics
            .iter()
            .flat_map(|diagnostic| { diagnostic["labels"].as_array().unwrap().iter() })
            .any(|label| {
                label["location"]["documentId"] == "large-diagnostics-library"
                    && label["location"]["range"]["start"]["line"] == 257
            }));
    }
    #[test]
    fn rename_maps_many_late_edits_with_sparse_utf16_checkpoints() {
        let prefix = r#"//@version=6
indicator("😀")
//"#;
        let mut body = String::new();
        for index in 0..1_536 {
            body.push_str(&format!("x{index}=1\n"));
        }
        for index in 0..1_536 {
            body.push_str(&format!("use{index}=x0\n"));
        }
        let padding = MAX_SOURCE_BYTES - prefix.len() - 1 - body.len();
        let source = format!("{prefix}{}\n{body}", " ".repeat(padding));
        assert_eq!(source.len(), MAX_SOURCE_BYTES);

        let request = Request {
            schema_version: 1,
            capability: Capability::Rename,
            document: Document {
                id: "late-edits".into(),
                version: "utf16-v1".into(),
                source: source.clone(),
            },
            libraries: Vec::new(),
            position: Some(Position {
                line: 1_539,
                character: 6,
            }),
            range: None,
            new_name: Some("renamed".into()),
            include_declaration: true,
        };
        let response = query(request).unwrap();
        let edits = response.data["edits"].as_array().unwrap();
        let expected = source
            .match_indices("x0")
            .map(|(byte_offset, _)| source[..byte_offset].encode_utf16().count() as u64)
            .collect::<Vec<_>>();
        assert_eq!(edits.len(), expected.len());
        assert_eq!(edits.len(), 1_537);
        for index in [0, 1, 255, 512, 1_024, 1_536] {
            assert_eq!(edits[index]["startUtf16"], expected[index]);
            assert_eq!(edits[index]["endUtf16"], expected[index] + 2);
        }
        assert!(expected.last().unwrap() > &250_000);
    }
    #[test]
    fn rejects_capture_invalid_positions_versions_and_oversize() {
        assert!(query(fixture("reject-capture"))
            .unwrap_err()
            .to_string()
            .contains("capture"));
        assert_eq!(
            query(fixture("reject-surrogate-position"))
                .unwrap_err()
                .to_string(),
            "invalid_utf16_position"
        );
        assert_eq!(
            query(fixture("reject-version")).unwrap_err().to_string(),
            "language_contract_unavailable"
        );
        let mut request = fixture("analyze-unicode");
        request.document.source = "a".repeat(MAX_SOURCE_BYTES + 1);
        assert_eq!(
            query(request).unwrap_err().to_string(),
            "source_budget_exceeded"
        );
        let mut request = fixture("definition-library");
        request.libraries[0].id.clone_from(&request.document.id);
        assert_eq!(
            query(request).unwrap_err().to_string(),
            "invalid_library_identity"
        );
    }
    #[test]
    fn request_schema_rejects_unknown_fields_recursively() {
        let mut root = serde_json::to_value(fixture("analyze-unicode")).unwrap();
        root["extra"] = json!(true);
        assert!(serde_json::from_value::<Request>(root).is_err());

        let mut document = serde_json::to_value(fixture("analyze-unicode")).unwrap();
        document["document"]["extra"] = json!(true);
        assert!(serde_json::from_value::<Request>(document).is_err());

        let mut position = serde_json::to_value(fixture("hover-builtin")).unwrap();
        position["position"]["extra"] = json!(true);
        assert!(serde_json::from_value::<Request>(position).is_err());

        let mut selection = serde_json::to_value(fixture("code-action")).unwrap();
        selection["range"]["start"]["extra"] = json!(true);
        assert!(serde_json::from_value::<Request>(selection).is_err());

        let mut library = serde_json::to_value(fixture("definition-library")).unwrap();
        library["libraries"][0]["extra"] = json!(true);
        assert!(serde_json::from_value::<Request>(library).is_err());
    }
    #[test]
    fn references_and_rename_refuse_resolution_work_beyond_the_budget() {
        fn request(capability: Capability) -> Request {
            let mut source = String::from("//@version=6\nindicator(\"Resolution work budget\")\n");
            for index in 0..3_000 {
                source.push_str(&format!("v{index}=1\n"));
            }
            for _ in 0..10_000 {
                source.push_str("v0 := v0 + 1\n");
            }
            assert!(source.len() < MAX_SOURCE_BYTES);
            source.push_str(&" ".repeat(MAX_SOURCE_BYTES - source.len()));
            assert_eq!(source.len(), MAX_SOURCE_BYTES);
            Request {
                schema_version: 1,
                capability,
                document: Document {
                    id: "work-budget".into(),
                    version: "snapshot-1".into(),
                    source,
                },
                libraries: Vec::new(),
                position: Some(Position {
                    line: 2,
                    character: 0,
                }),
                range: None,
                new_name: matches!(capability, Capability::Rename).then(|| "renamed".into()),
                include_declaration: true,
            }
        }

        for capability in [Capability::References, Capability::Rename] {
            assert_eq!(
                query(request(capability)).unwrap_err(),
                QueryError::WorkBudgetExceeded
            );
            assert_eq!(
                QueryError::WorkBudgetExceeded.code(),
                RESOLUTION_WORK_BUDGET_EXCEEDED
            );
        }
    }
    #[test]
    fn exposes_registry_hover_and_compiler_quick_fix() {
        assert!(query(fixture("hover-builtin")).unwrap().data["markdown"]
            .as_str()
            .unwrap()
            .contains("plot"));
        assert!(!query(fixture("code-action"))
            .unwrap()
            .data
            .as_array()
            .unwrap()
            .is_empty());
    }
}
