//! Compiler-backed language intelligence shared by the LSP and browser adapters.

use pinewasm_compiler::{
    compile_with_libraries, CompileOptions, CompileResult, PhaseTimings, MAX_SOURCE_LEN,
};
use pinewasm_hir::{BuiltinSeries, Type};
use pinewasm_parse::{lex, parse};
use pinewasm_semantics::{
    cast_result_type, BuiltinFunction, BuiltinRegistry, ParamTypeRule, TaParam, TaParamKind,
};
use pinewasm_span::{Diagnostic, Severity, SourceId, Span};
use pinewasm_syntax::{self as ast, Item, Stmt, Token, TokenKind};
use serde::Serialize;
use std::collections::HashSet;
use std::sync::OnceLock;

use crate::text::LineIndex;

pub const LANGUAGE_ID: &str = "pine";
pub const SERVER_NAME: &str = "pinewasm";

pub const KEYWORDS: &[&str] = &[
    "and", "bool", "break", "by", "continue", "else", "enum", "false", "float", "for", "if", "in",
    "int", "method", "na", "not", "or", "string", "switch", "to", "true", "type", "var", "varip",
    "while",
];

#[derive(Debug, Clone)]
pub struct LibrarySource {
    pub import_path: String,
    pub source: String,
    pub file_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum LocalSymbolKind {
    Variable,
    Function,
    Parameter,
    Type,
    Field,
    EnumMember,
    Method,
    Module,
}

impl LocalSymbolKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Variable => "variable",
            Self::Function => "function",
            Self::Parameter => "parameter",
            Self::Type => "type",
            Self::Field => "field",
            Self::EnumMember => "enum member",
            Self::Method => "method",
            Self::Module => "library alias",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalSymbol {
    pub name: String,
    pub kind: LocalSymbolKind,
    pub detail: String,
    pub span: Span,
    pub full_span: Span,
    pub container: Option<String>,
    pub signature: Option<String>,
    /// Parse-only lexical identity used by definition/reference/rename
    /// resolution. This is deliberately omitted from the lightweight
    /// symbol payload: it is an implementation detail, not protocol surface.
    #[serde(skip)]
    scope: LexicalScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LexicalScope {
    id: u32,
    depth: u16,
    /// `None` is the document-global scope. Nested scopes are bounded to the
    /// statements in one callable body, conditional branch, loop body, or
    /// switch arm; headers and sibling branches therefore cannot see locals.
    range: Option<Span>,
}

impl LexicalScope {
    const GLOBAL: Self = Self {
        id: 0,
        depth: 0,
        range: None,
    };

    fn contains(self, byte_offset: u32) -> bool {
        self.range
            .is_none_or(|range| range.start <= byte_offset && byte_offset <= range.end)
    }
}

impl LocalSymbol {
    pub(crate) fn shares_lexical_scope(&self, other: &Self) -> bool {
        self.scope.id == other.scope.id
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum LanguageItemKind {
    Keyword,
    Function,
    Series,
    Constant,
    Namespace,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParameterInfo {
    pub label: String,
    pub detail: String,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LanguageItem {
    pub name: String,
    pub kind: LanguageItemKind,
    pub detail: String,
    pub signature: Option<String>,
    pub parameters: Vec<ParameterInfo>,
    pub insert_text: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Inspection {
    pub success: bool,
    pub diagnostics: Vec<Diagnostic>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ast: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub csil: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timings: Option<PhaseTimings>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectionStage {
    Ast,
    Hir,
    Mir,
    Csil,
    All,
}

impl InspectionStage {
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "ast" => Some(Self::Ast),
            "hir" | "ir" => Some(Self::Hir),
            "mir" => Some(Self::Mir),
            "csil" => Some(Self::Csil),
            "all" | "pipeline" => Some(Self::All),
            _ => None,
        }
    }

    fn options(self) -> CompileOptions {
        CompileOptions {
            emit_ast: matches!(self, Self::Ast | Self::All),
            emit_hir: matches!(self, Self::Hir | Self::All),
            emit_mir: matches!(self, Self::Mir | Self::All),
            emit_csil: matches!(self, Self::Csil | Self::All),
            validate_passes: false,
            ..CompileOptions::default()
        }
    }
}

/// One immutable analysis snapshot. Replacing the snapshot on every document
/// revision makes requests deterministic and prevents mixed-revision results.
#[derive(Debug)]
pub struct DocumentAnalysis {
    source: String,
    file_name: String,
    libraries: Vec<LibrarySource>,
    line_index: LineIndex,
    tokens: Vec<Token>,
    once_keyword_starts: HashSet<u32>,
    symbols: Vec<LocalSymbol>,
    compile: CompileResult,
}

impl DocumentAnalysis {
    pub fn new(
        file_name: impl Into<String>,
        source: impl Into<String>,
        libraries: Vec<LibrarySource>,
    ) -> Self {
        let file_name = file_name.into();
        let source = source.into();
        // Navigation must honor the same source boundary as compilation. An
        // oversized open buffer still carries its E0001 diagnostic, but must
        // not build a second token stream or a parse-only symbol tree first.
        let (symbols, tokens, once_keyword_starts) = if source.len() <= MAX_SOURCE_LEN {
            let parsed = parse(&source);
            (
                collect_symbols(&parsed.program),
                lex(&source).tokens,
                parsed
                    .once_keyword_spans
                    .iter()
                    .map(|span| span.start)
                    .collect(),
            )
        } else {
            (Vec::new(), Vec::new(), HashSet::new())
        };
        let compiler_libraries: Vec<_> = libraries
            .iter()
            .map(|library| (library.import_path.clone(), library.source.clone()))
            .collect();
        let compile =
            compile_with_libraries(&source, &compiler_libraries, CompileOptions::default());
        let line_index = LineIndex::new(source.clone());
        Self {
            source,
            file_name,
            libraries,
            line_index,
            tokens,
            once_keyword_starts,
            symbols,
            compile,
        }
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    pub fn line_index(&self) -> &LineIndex {
        &self.line_index
    }

    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.compile.diagnostics
    }

    pub fn compile_result(&self) -> &CompileResult {
        &self.compile
    }

    pub fn symbols(&self) -> &[LocalSymbol] {
        &self.symbols
    }

    pub fn tokens(&self) -> &[Token] {
        &self.tokens
    }

    pub fn success(&self) -> bool {
        self.compile.success
    }

    pub fn diagnostics_for_source(&self, source: SourceId) -> impl Iterator<Item = &Diagnostic> {
        self.compile.diagnostics.iter().filter(move |diagnostic| {
            diagnostic
                .primary_span()
                .map_or(source == SourceId::MAIN, |span| span.source == source)
        })
    }

    pub fn source_for_id(&self, source: SourceId) -> Option<(&str, &str)> {
        if source == SourceId::MAIN {
            return Some((&self.file_name, &self.source));
        }
        self.libraries
            .get(source.0.checked_sub(1)? as usize)
            .map(|library| {
                (
                    library.file_name.as_deref().unwrap_or(&library.import_path),
                    library.source.as_str(),
                )
            })
    }

    pub fn inspect(&self, stage: InspectionStage) -> Inspection {
        let compiler_libraries: Vec<_> = self
            .libraries
            .iter()
            .map(|library| (library.import_path.clone(), library.source.clone()))
            .collect();
        let result = compile_with_libraries(&self.source, &compiler_libraries, stage.options());
        Inspection {
            success: result.success,
            diagnostics: result.diagnostics,
            ast: result.ast_debug,
            hir: result.hir_debug,
            mir: result.mir_debug,
            csil: result.csil_text,
            timings: result.timings,
        }
    }

    pub fn identifier_at(&self, byte_offset: u32) -> Option<(&str, Span)> {
        let insertion = self
            .tokens
            .partition_point(|token| token.span.start < byte_offset);
        let previous = insertion
            .checked_sub(1)
            .and_then(|index| self.tokens.get(index))
            .filter(|token| {
                self.is_identifier_token(token)
                    && token.span.start <= byte_offset
                    && byte_offset <= token.span.end
            });
        previous
            .or_else(|| {
                self.tokens[insertion..]
                    .iter()
                    .take_while(|token| token.span.start == byte_offset)
                    .find(|token| self.is_identifier_token(token))
            })
            .map(|token| (token.text(&self.source), token.span))
    }

    fn is_identifier_token(&self, token: &Token) -> bool {
        token.kind == TokenKind::Ident && !self.once_keyword_starts.contains(&token.span.start)
    }

    pub fn dotted_name_at(&self, byte_offset: u32) -> Option<(&str, Span)> {
        let mut byte = (byte_offset as usize).min(self.source.len());
        while byte > 0 && !self.source.is_char_boundary(byte) {
            byte -= 1;
        }
        let bytes = self.source.as_bytes();
        let mut start = byte;
        while start > 0 && is_name_byte(bytes[start - 1]) {
            start -= 1;
        }
        let mut end = byte;
        while end < bytes.len() && is_name_byte(bytes[end]) {
            end += 1;
        }
        let name = self.source.get(start..end)?;
        if name.is_empty()
            || !name
                .bytes()
                .any(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        {
            return None;
        }
        Some((name, Span::new(SourceId::MAIN, start as u32, end as u32)))
    }

    pub fn local_symbol_at(&self, byte_offset: u32) -> Option<&LocalSymbol> {
        let (name, span) = self.identifier_at(byte_offset)?;
        let symbol = self.resolve_local(name, byte_offset)?;
        if self.preceded_by_dot(span.start)
            && !matches!(
                symbol.kind,
                LocalSymbolKind::Field | LocalSymbolKind::EnumMember | LocalSymbolKind::Method
            )
        {
            return None;
        }
        Some(symbol)
    }

    pub fn resolve_local(&self, name: &str, byte_offset: u32) -> Option<&LocalSymbol> {
        self.resolve_local_with_rename(name, byte_offset, None)
    }

    fn resolve_local_with_rename<'a>(
        &'a self,
        name: &str,
        byte_offset: u32,
        rename: Option<(&LocalSymbol, &str)>,
    ) -> Option<&'a LocalSymbol> {
        let current_container = self.enclosing_callable(byte_offset);
        let member_access = self
            .identifier_at(byte_offset)
            .is_some_and(|(_, span)| self.preceded_by_dot(span.start));
        self.symbols
            .iter()
            .filter(|symbol| match rename {
                Some((target, new_name)) if same_declaration(symbol, target) => new_name == name,
                _ => symbol.name == name,
            })
            .filter(|symbol| {
                let member_symbol = matches!(
                    symbol.kind,
                    LocalSymbolKind::Field | LocalSymbolKind::EnumMember | LocalSymbolKind::Method
                );
                let at_declaration =
                    symbol.span.start <= byte_offset && byte_offset <= symbol.span.end;
                if member_access && !member_symbol {
                    return false;
                }
                if member_symbol && !member_access && !at_declaration {
                    return false;
                }
                let always_visible = matches!(
                    symbol.kind,
                    LocalSymbolKind::Function
                        | LocalSymbolKind::Method
                        | LocalSymbolKind::Type
                        | LocalSymbolKind::Module
                        | LocalSymbolKind::Field
                        | LocalSymbolKind::EnumMember
                );
                let scope_visible = symbol.container.as_deref().is_none()
                    || symbol.container.as_deref() == current_container
                    || matches!(
                        symbol.kind,
                        LocalSymbolKind::Field | LocalSymbolKind::EnumMember
                    );
                let lexical_visible = at_declaration || symbol.scope.contains(byte_offset);
                (always_visible || symbol.span.start <= byte_offset)
                    && scope_visible
                    && lexical_visible
            })
            .max_by_key(|symbol| {
                (
                    u8::from(symbol.container.as_deref() == current_container),
                    symbol.scope.depth,
                    symbol.span.start,
                )
            })
    }

    /// Confirm that a rename preserves every affected lexical binding.
    ///
    /// Old-name tokens that resolve to `target` will be edited. Existing
    /// new-name tokens will not. Simulating resolution after the declaration
    /// rename catches both unsafe directions: an edited outer reference being
    /// captured by a descendant declaration, and an existing ancestor
    /// reference being captured by the newly renamed inner declaration.
    pub(crate) fn rename_preserves_bindings(&self, target: &LocalSymbol, new_name: &str) -> bool {
        self.tokens
            .iter()
            .filter(|token| self.is_identifier_token(token))
            .all(|token| {
                let old_name = token.text(&self.source);
                let current = self.resolve_local(old_name, token.span.start);
                let edits_target = old_name == target.name
                    && current.is_some_and(|resolved| same_declaration(resolved, target));
                if !edits_target && old_name != new_name {
                    return true;
                }

                let post_rename_name = if edits_target { new_name } else { old_name };
                let after = self.resolve_local_with_rename(
                    post_rename_name,
                    token.span.start,
                    Some((target, new_name)),
                );
                if edits_target {
                    after.is_some_and(|resolved| same_declaration(resolved, target))
                } else {
                    match (current, after) {
                        (Some(before), Some(after)) => same_declaration(before, after),
                        (None, None) => true,
                        _ => false,
                    }
                }
            })
    }

    /// References resolved to one declaration within Pine's document/function
    /// scopes. Member names are excluded for non-member declarations so a
    /// variable rename cannot accidentally rewrite `object.same_name`.
    pub fn references_to(&self, target: &LocalSymbol) -> Vec<Span> {
        self.tokens
            .iter()
            .filter(|token| {
                self.is_identifier_token(token) && token.text(&self.source) == target.name
            })
            .filter(|token| {
                if !matches!(
                    target.kind,
                    LocalSymbolKind::Field | LocalSymbolKind::EnumMember | LocalSymbolKind::Method
                ) && self.preceded_by_dot(token.span.start)
                {
                    return false;
                }
                self.resolve_local(&target.name, token.span.start)
                    .is_some_and(|resolved| same_declaration(resolved, target))
            })
            .map(|token| token.span)
            .collect()
    }

    pub fn references(&self, name: &str) -> Vec<Span> {
        self.tokens
            .iter()
            .filter(|token| self.is_identifier_token(token) && token.text(&self.source) == name)
            .map(|token| token.span)
            .collect()
    }

    /// Resolve a built-in at a source position, selecting a callable overload
    /// when the name is immediately followed by `(`. Pine exposes names such
    /// as `time` in both value and function forms.
    pub fn language_item_at(&self, byte_offset: u32) -> Option<(&'static LanguageItem, Span)> {
        let (name, span) = self.dotted_name_at(byte_offset)?;
        let called = self
            .source
            .get(span.end as usize..)?
            .trim_start()
            .starts_with('(');
        let item = if called {
            language_function(name).or_else(|| language_item(name))
        } else {
            language_item(name)
        }?;
        Some((item, span))
    }

    pub fn completion_items(&self, byte_offset: u32) -> Vec<CompletionCandidate<'_>> {
        let namespace = completion_namespace(&self.source, byte_offset as usize);
        let mut output = Vec::new();
        let mut seen = HashSet::new();

        if namespace.is_none() {
            let current_container = self.enclosing_callable(byte_offset);
            let scoped = self.symbols.iter().filter(|symbol| {
                current_container.is_some() && symbol.container.as_deref() == current_container
            });
            let remaining = self.symbols.iter().filter(|symbol| {
                current_container.is_none() || symbol.container.as_deref() != current_container
            });
            for symbol in scoped
                .chain(remaining)
                .filter(|symbol| self.visible_for_completion(symbol, byte_offset))
            {
                if seen.insert(symbol.name.clone()) {
                    output.push(CompletionCandidate::Local(symbol));
                }
            }
        } else if let Some(namespace) = namespace.as_deref() {
            for symbol in self.symbols.iter().filter(|symbol| {
                symbol.container.as_deref() == Some(namespace)
                    && matches!(
                        symbol.kind,
                        LocalSymbolKind::Field | LocalSymbolKind::EnumMember
                    )
            }) {
                if seen.insert(symbol.name.clone()) {
                    output.push(CompletionCandidate::Local(symbol));
                }
            }
        }

        for item in language_items() {
            if let Some(namespace) = namespace.as_deref() {
                let prefix = format!("{namespace}.");
                if !item.name.starts_with(&prefix) {
                    continue;
                }
            } else if item.name.contains('.') && item.kind != LanguageItemKind::Namespace {
                continue;
            }
            let key = if let Some(namespace) = namespace.as_deref() {
                item.name
                    .strip_prefix(&format!("{namespace}."))
                    .unwrap_or(&item.name)
                    .split('.')
                    .next()
                    .unwrap_or(&item.name)
                    .to_string()
            } else {
                item.name.clone()
            };
            if seen.insert(key) {
                output.push(CompletionCandidate::Builtin(item));
            }
        }
        output
    }

    pub fn enclosing_call(&self, byte_offset: u32) -> Option<CallSite> {
        let byte = (byte_offset as usize).min(self.source.len());
        find_enclosing_call(&self.source[..byte])
    }

    fn enclosing_callable(&self, byte_offset: u32) -> Option<&str> {
        self.symbols
            .iter()
            .filter(|symbol| {
                matches!(
                    symbol.kind,
                    LocalSymbolKind::Function | LocalSymbolKind::Method
                ) && symbol.full_span.start <= byte_offset
                    && byte_offset <= symbol.full_span.end
            })
            .min_by_key(|symbol| symbol.full_span.len())
            .map(|symbol| symbol.name.as_str())
    }

    fn preceded_by_dot(&self, byte_offset: u32) -> bool {
        self.source
            .get(..byte_offset as usize)
            .is_some_and(|before| before.trim_end().ends_with('.'))
    }

    fn visible_for_completion(&self, symbol: &LocalSymbol, byte_offset: u32) -> bool {
        match symbol.kind {
            LocalSymbolKind::Function
            | LocalSymbolKind::Method
            | LocalSymbolKind::Type
            | LocalSymbolKind::Module => true,
            LocalSymbolKind::Variable | LocalSymbolKind::Parameter => {
                symbol.span.start <= byte_offset
                    && symbol.scope.contains(byte_offset)
                    && (symbol.container.is_none()
                        || symbol.container.as_deref() == self.enclosing_callable(byte_offset))
            }
            LocalSymbolKind::Field | LocalSymbolKind::EnumMember => false,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum CompletionCandidate<'a> {
    Local(&'a LocalSymbol),
    Builtin(&'a LanguageItem),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSite {
    pub callee: String,
    pub argument: usize,
}

fn same_declaration(left: &LocalSymbol, right: &LocalSymbol) -> bool {
    left.kind == right.kind
        && left.span == right.span
        && left.container == right.container
        && left.name == right.name
}

pub fn language_item(name: &str) -> Option<&'static LanguageItem> {
    language_items()
        .iter()
        .find(|item| item.name == name && item.kind != LanguageItemKind::Function)
        .or_else(|| language_function(name))
}

/// Names the compiler reserves as immutable built-in series declarations.
/// Keeping rename validation on the semantic registry avoids duplicating a
/// surface list that would drift as the compiler gains new built-ins.
pub fn is_builtin_series_name(name: &str) -> bool {
    BuiltinRegistry::standard_ref()
        .lookup_series(name)
        .is_some()
}

/// Match the semantic analyzer's `collect_functions` reservation rule exactly:
/// user functions cannot replace a registered callable/value or a cast.
pub fn is_reserved_user_function_name(name: &str) -> bool {
    let registry = BuiltinRegistry::standard_ref();
    registry.lookup_function(name).is_some()
        || registry.lookup_series(name).is_some()
        || registry.lookup_constant(name).is_some()
        || cast_result_type(name).is_some()
}

/// Returns the callable form of a built-in name.
///
/// Pine exposes a small number of names, such as `time`, as both a value and a
/// function. Keeping this lookup distinct prevents value hover information from
/// suppressing signature help for the callable form.
pub fn language_function(name: &str) -> Option<&'static LanguageItem> {
    language_items()
        .iter()
        .find(|item| item.name == name && item.kind == LanguageItemKind::Function)
}

pub fn language_items() -> &'static [LanguageItem] {
    static ITEMS: OnceLock<Vec<LanguageItem>> = OnceLock::new();
    ITEMS.get_or_init(build_language_items)
}

fn build_language_items() -> Vec<LanguageItem> {
    let mut items: Vec<LanguageItem> = KEYWORDS
        .iter()
        .map(|keyword| LanguageItem {
            name: (*keyword).to_string(),
            kind: LanguageItemKind::Keyword,
            detail: "Pine Script® v6 keyword".to_string(),
            signature: None,
            parameters: Vec::new(),
            insert_text: (*keyword).to_string(),
        })
        .collect();

    for series in BuiltinSeries::ALL {
        let ty = builtin_series_type(series);
        items.push(LanguageItem {
            name: series.name().to_string(),
            kind: LanguageItemKind::Series,
            detail: format!("series {}", ty.describe()),
            signature: Some(format!("{}: series {}", series.name(), ty.describe())),
            parameters: Vec::new(),
            insert_text: series.name().to_string(),
        });
    }

    let registry = BuiltinRegistry::standard_ref();
    for function in registry.functions() {
        items.push(function_item(function));
    }
    for constant in registry.constants() {
        items.push(LanguageItem {
            name: constant.name.to_string(),
            kind: LanguageItemKind::Constant,
            detail: format!("const {}", constant.ty.describe()),
            signature: Some(format!(
                "{}: const {}",
                constant.name,
                constant.ty.describe()
            )),
            parameters: Vec::new(),
            insert_text: constant.name.to_string(),
        });
    }

    let mut namespaces = HashSet::new();
    for item in &items {
        if let Some((namespace, _)) = item.name.split_once('.') {
            namespaces.insert(namespace.to_string());
        }
    }
    for namespace in namespaces {
        items.push(LanguageItem {
            name: namespace.clone(),
            kind: LanguageItemKind::Namespace,
            detail: "Pine Script v6 namespace".to_string(),
            signature: None,
            parameters: Vec::new(),
            insert_text: namespace,
        });
    }
    items.sort_by(|left, right| {
        left.name.cmp(&right.name).then_with(|| {
            language_item_priority(left.kind).cmp(&language_item_priority(right.kind))
        })
    });
    items.dedup_by(|left, right| left.name == right.name && left.kind == right.kind);
    items
}

fn language_item_priority(kind: LanguageItemKind) -> u8 {
    match kind {
        // Prefer the callable form in completion lists when Pine also exposes a
        // same-named value. `language_item` still deliberately prefers values
        // for hover and semantic-token queries.
        LanguageItemKind::Function => 0,
        LanguageItemKind::Series => 1,
        LanguageItemKind::Constant => 2,
        LanguageItemKind::Namespace => 3,
        LanguageItemKind::Keyword => 4,
    }
}

fn function_item(function: &BuiltinFunction) -> LanguageItem {
    let parameters: Vec<ParameterInfo> = if let Some(engine) = &function.ta_engine {
        engine.iter().map(ta_parameter).collect()
    } else {
        function
            .params
            .iter()
            .map(|parameter| ParameterInfo {
                label: parameter.name.to_string(),
                detail: format!(
                    "{} {}{}",
                    parameter.max_qualifier.describe(),
                    parameter_type(parameter.type_rule, &parameter.ty),
                    parameter
                        .default_int
                        .map(|value| format!(" = {value}"))
                        .or_else(|| parameter.default_float.map(|value| format!(" = {value}")))
                        .unwrap_or_default()
                ),
                required: parameter.required,
            })
            .collect()
    };
    let rendered = parameters
        .iter()
        .map(|parameter| {
            if parameter.required {
                format!("{}: {}", parameter.label, parameter.detail)
            } else {
                format!("[{}: {}]", parameter.label, parameter.detail)
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let signature = format!(
        "{}({rendered}) -> {}",
        function.name,
        function.ret.describe()
    );
    let insert_text = if parameters.is_empty() {
        format!("{}()", function.name)
    } else {
        format!("{}(", function.name)
    };
    LanguageItem {
        name: function.name.to_string(),
        kind: LanguageItemKind::Function,
        detail: signature.clone(),
        signature: Some(signature),
        parameters,
        insert_text,
    }
}

fn ta_parameter(parameter: &TaParam) -> ParameterInfo {
    let (detail, required) = match parameter.kind {
        TaParamKind::Source => ("series float".to_string(), true),
        TaParamKind::SourceDefaulted(series) => {
            (format!("series float = {}", series.name()), false)
        }
        TaParamKind::Condition => ("series bool".to_string(), true),
        TaParamKind::ConstInt { min, max, default } => (
            format!(
                "const int ({min}..={max}){}",
                default
                    .map(|value| format!(" = {value}"))
                    .unwrap_or_default()
            ),
            default.is_none(),
        ),
        TaParamKind::ConstFloat { positive, optional } => (
            if positive {
                "const float (> 0)".to_string()
            } else {
                "const float".to_string()
            },
            !optional,
        ),
        TaParamKind::ConstPercent => ("const float (0..=100)".to_string(), true),
        TaParamKind::ConstBool { default } => (
            format!(
                "const bool{}",
                default
                    .map(|value| format!(" = {value}"))
                    .unwrap_or_default()
            ),
            default.is_none(),
        ),
        TaParamKind::ConstTimeframe { default } => (
            format!(
                "const timeframe{}",
                default
                    .map(|value| format!(" = {value}s"))
                    .unwrap_or_default()
            ),
            default.is_none(),
        ),
    };
    ParameterInfo {
        label: parameter.name.to_string(),
        detail,
        required,
    }
}

fn parameter_type(rule: ParamTypeRule, ty: &Type) -> String {
    match rule {
        ParamTypeRule::Declared => ty.describe(),
        _ => rule.describe().to_string(),
    }
}

fn builtin_series_type(series: BuiltinSeries) -> Type {
    series.value_type()
}

struct SymbolCollector {
    symbols: Vec<LocalSymbol>,
    next_scope_id: u32,
}

impl SymbolCollector {
    fn new() -> Self {
        Self {
            symbols: Vec::new(),
            next_scope_id: 1,
        }
    }

    fn nested_scope(
        &mut self,
        parent: LexicalScope,
        statements: &[Stmt],
        fallback: Span,
    ) -> LexicalScope {
        let id = self.next_scope_id;
        self.next_scope_id += 1;
        LexicalScope {
            id,
            depth: parent.depth.saturating_add(1),
            range: Some(block_span(statements).unwrap_or(fallback)),
        }
    }
}

fn block_span(statements: &[Stmt]) -> Option<Span> {
    Some(statements.first()?.span().to(statements.last()?.span()))
}

fn collect_symbols(program: &ast::Program) -> Vec<LocalSymbol> {
    let mut collector = SymbolCollector::new();
    for item in &program.items {
        match item {
            Item::Import(import) => collector.symbols.push(LocalSymbol {
                name: import.alias.name.clone(),
                kind: LocalSymbolKind::Module,
                detail: format!(
                    "import {}/{}/{}",
                    import.publisher.name, import.library.name, import.version
                ),
                span: import.alias.span,
                full_span: import.span,
                container: None,
                signature: None,
                scope: LexicalScope::GLOBAL,
            }),
            Item::FuncDef(function) => {
                let kind = if function.receiver.is_some() {
                    LocalSymbolKind::Method
                } else {
                    LocalSymbolKind::Function
                };
                let signature = user_function_signature(function);
                collector.symbols.push(LocalSymbol {
                    name: function.name.name.clone(),
                    kind,
                    detail: if function.exported {
                        "exported user-defined callable".to_string()
                    } else {
                        "user-defined callable".to_string()
                    },
                    span: function.name.span,
                    full_span: function.span,
                    container: None,
                    signature: Some(signature),
                    scope: LexicalScope::GLOBAL,
                });
                let callable_scope = collector.nested_scope(
                    LexicalScope::GLOBAL,
                    &function.body,
                    Span::new(function.span.source, function.span.end, function.span.end),
                );
                for parameter in &function.params {
                    collector.symbols.push(LocalSymbol {
                        name: parameter.name.name.clone(),
                        kind: LocalSymbolKind::Parameter,
                        detail: parameter.ty.as_ref().map_or_else(
                            || "function parameter".to_string(),
                            pinewasm_syntax::FieldType::describe,
                        ),
                        span: parameter.name.span,
                        full_span: parameter.span,
                        container: Some(function.name.name.clone()),
                        signature: None,
                        scope: callable_scope,
                    });
                }
                collect_stmt_symbols(
                    &function.body,
                    Some(&function.name.name),
                    callable_scope,
                    &mut collector,
                );
            }
            Item::EnumDef(enumeration) => {
                collector.symbols.push(LocalSymbol {
                    name: enumeration.name.name.clone(),
                    kind: LocalSymbolKind::Type,
                    detail: "user-defined enum".to_string(),
                    span: enumeration.name.span,
                    full_span: enumeration.span,
                    container: None,
                    signature: None,
                    scope: LexicalScope::GLOBAL,
                });
                for variant in &enumeration.variants {
                    collector.symbols.push(LocalSymbol {
                        name: variant.name.name.clone(),
                        kind: LocalSymbolKind::EnumMember,
                        detail: variant
                            .title
                            .clone()
                            .unwrap_or_else(|| variant.name.name.clone()),
                        span: variant.name.span,
                        full_span: variant.span,
                        container: Some(enumeration.name.name.clone()),
                        signature: None,
                        scope: LexicalScope::GLOBAL,
                    });
                }
            }
            Item::TypeDef(ty) => {
                collector.symbols.push(LocalSymbol {
                    name: ty.name.name.clone(),
                    kind: LocalSymbolKind::Type,
                    detail: "user-defined type".to_string(),
                    span: ty.name.span,
                    full_span: ty.span,
                    container: None,
                    signature: None,
                    scope: LexicalScope::GLOBAL,
                });
                for field in &ty.fields {
                    collector.symbols.push(LocalSymbol {
                        name: field.name.name.clone(),
                        kind: LocalSymbolKind::Field,
                        detail: field.ty.describe(),
                        span: field.name.span,
                        full_span: field.span,
                        container: Some(ty.name.name.clone()),
                        signature: None,
                        scope: LexicalScope::GLOBAL,
                    });
                }
            }
            Item::Stmt(statement) => {
                collect_stmt_symbols(
                    std::slice::from_ref(statement),
                    None,
                    LexicalScope::GLOBAL,
                    &mut collector,
                );
            }
            Item::Indicator(_) | Item::Strategy(_) | Item::Library(_) => {}
        }
    }
    collector
        .symbols
        .sort_by_key(|symbol| (symbol.span.start, symbol.span.end));
    collector.symbols
}

/// Parse-only symbol collection for cheap workspace-wide symbol search.
/// Sources beyond the compiler's UTF-8 byte limit yield no navigation symbols.
pub fn symbols_from_source(source: &str) -> Vec<LocalSymbol> {
    if source.len() > MAX_SOURCE_LEN {
        return Vec::new();
    }
    collect_symbols(&parse(source).program)
}

/// Resolve only exported top-level library declarations by their source name.
/// A parameter or local with the same name must never win merely because it
/// appears earlier in the parsed symbol list.
pub(crate) fn exported_symbol_from_source(source: &str, name: &str) -> Option<LocalSymbol> {
    if source.len() > MAX_SOURCE_LEN {
        return None;
    }
    let program = parse(source).program;
    let exported_spans = program
        .items
        .iter()
        .filter_map(|item| match item {
            Item::FuncDef(function) if function.exported && function.receiver.is_none() => {
                Some(function.name.span)
            }
            Item::EnumDef(enumeration) if enumeration.exported => Some(enumeration.name.span),
            Item::TypeDef(ty) if ty.exported => Some(ty.name.span),
            _ => None,
        })
        .collect::<HashSet<_>>();
    collect_symbols(&program).into_iter().find(|symbol| {
        symbol.name == name && symbol.container.is_none() && exported_spans.contains(&symbol.span)
    })
}

fn collect_stmt_symbols(
    statements: &[Stmt],
    container: Option<&str>,
    scope: LexicalScope,
    collector: &mut SymbolCollector,
) {
    for statement in statements {
        match statement {
            Stmt::Assign {
                name,
                annotation,
                span,
                is_var,
                is_varip,
                ..
            } => collector.symbols.push(LocalSymbol {
                name: name.name.clone(),
                kind: LocalSymbolKind::Variable,
                detail: annotation.as_ref().map_or_else(
                    || {
                        if *is_varip {
                            "varip series".to_string()
                        } else if *is_var {
                            "persistent series".to_string()
                        } else {
                            "series variable".to_string()
                        }
                    },
                    |annotation| annotation.kind.describe(),
                ),
                span: name.span,
                full_span: *span,
                container: container.map(str::to_string),
                signature: None,
                scope,
            }),
            Stmt::Destructure { targets, span, .. } => {
                for target in targets {
                    collector.symbols.push(LocalSymbol {
                        name: target.name.clone(),
                        kind: LocalSymbolKind::Variable,
                        detail: "tuple element".to_string(),
                        span: target.span,
                        full_span: *span,
                        container: container.map(str::to_string),
                        signature: None,
                        scope,
                    });
                }
            }
            Stmt::For(loop_) => {
                let loop_scope = collector.nested_scope(
                    scope,
                    &loop_.body,
                    Span::new(loop_.span.source, loop_.span.end, loop_.span.end),
                );
                collector.symbols.push(LocalSymbol {
                    name: loop_.counter.name.clone(),
                    kind: LocalSymbolKind::Variable,
                    detail: "loop counter".to_string(),
                    span: loop_.counter.span,
                    full_span: loop_.span,
                    container: container.map(str::to_string),
                    signature: None,
                    scope: loop_scope,
                });
                collect_stmt_symbols(&loop_.body, container, loop_scope, collector);
            }
            Stmt::ForIn(loop_) => {
                let loop_scope = collector.nested_scope(
                    scope,
                    &loop_.body,
                    Span::new(loop_.span.source, loop_.span.end, loop_.span.end),
                );
                if let Some(index) = &loop_.index {
                    collector.symbols.push(LocalSymbol {
                        name: index.name.clone(),
                        kind: LocalSymbolKind::Variable,
                        detail: "loop index".to_string(),
                        span: index.span,
                        full_span: loop_.span,
                        container: container.map(str::to_string),
                        signature: None,
                        scope: loop_scope,
                    });
                }
                collector.symbols.push(LocalSymbol {
                    name: loop_.element.name.clone(),
                    kind: LocalSymbolKind::Variable,
                    detail: "loop element".to_string(),
                    span: loop_.element.span,
                    full_span: loop_.span,
                    container: container.map(str::to_string),
                    signature: None,
                    scope: loop_scope,
                });
                collect_stmt_symbols(&loop_.body, container, loop_scope, collector);
            }
            Stmt::If(if_) => {
                let then_scope = collector.nested_scope(
                    scope,
                    &if_.then_branch,
                    Span::new(if_.span.source, if_.span.end, if_.span.end),
                );
                collect_stmt_symbols(&if_.then_branch, container, then_scope, collector);
                if let Some(else_) = &if_.else_branch {
                    let else_scope = collector.nested_scope(
                        scope,
                        else_,
                        Span::new(if_.span.source, if_.span.end, if_.span.end),
                    );
                    collect_stmt_symbols(else_, container, else_scope, collector);
                }
            }
            Stmt::Once(once) => {
                let once_scope = collector.nested_scope(
                    scope,
                    &once.body,
                    Span::new(once.span.source, once.span.end, once.span.end),
                );
                collect_stmt_symbols(&once.body, container, once_scope, collector);
            }
            Stmt::While(loop_) => {
                let loop_scope = collector.nested_scope(
                    scope,
                    &loop_.body,
                    Span::new(loop_.span.source, loop_.span.end, loop_.span.end),
                );
                collect_stmt_symbols(&loop_.body, container, loop_scope, collector);
            }
            Stmt::Switch(switch) => {
                for arm in &switch.arms {
                    let arm_scope = collector.nested_scope(
                        scope,
                        &arm.body,
                        Span::new(arm.span.source, arm.span.end, arm.span.end),
                    );
                    collect_stmt_symbols(&arm.body, container, arm_scope, collector);
                }
            }
            Stmt::Reassign { .. }
            | Stmt::Expr { .. }
            | Stmt::Break { .. }
            | Stmt::Continue { .. } => {}
        }
    }
}

fn user_function_signature(function: &ast::FuncDef) -> String {
    let params = function
        .params
        .iter()
        .map(|parameter| {
            let ty = parameter
                .ty
                .as_ref()
                .map(|ty| format!("{} ", ty.describe()))
                .unwrap_or_default();
            let default = if parameter.default.is_some() {
                " = …"
            } else {
                ""
            };
            format!("{ty}{}{default}", parameter.name.name)
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("{}({params})", function.name.name)
}

fn completion_namespace(source: &str, byte_offset: usize) -> Option<String> {
    let byte = byte_offset.min(source.len());
    let line_start = source[..byte].rfind('\n').map_or(0, |index| index + 1);
    let before = &source[line_start..byte];
    let bytes = before.as_bytes();
    let mut start = bytes.len();
    while start > 0 && is_name_byte(bytes[start - 1]) {
        start -= 1;
    }
    let candidate = &before[start..];
    candidate
        .rfind('.')
        .map(|dot| candidate[..dot].to_string())
        .filter(|value| !value.is_empty())
}

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'.'
}

/// Locate the innermost open call while ignoring strings and comments.
pub fn find_enclosing_call(source: &str) -> Option<CallSite> {
    #[derive(Clone, Copy)]
    struct Frame {
        delimiter: u8,
        callee_start: Option<usize>,
        callee_end: Option<usize>,
        argument: usize,
    }

    let bytes = source.as_bytes();
    let mut stack: Vec<Frame> = Vec::new();
    let mut string_quote = None;
    let mut line_comment = false;
    let mut block_comment = false;
    let mut escaped = false;
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        let next = bytes.get(index + 1).copied();
        if line_comment {
            if byte == b'\n' {
                line_comment = false;
            }
            index += 1;
            continue;
        }
        if block_comment {
            if byte == b'*' && next == Some(b'/') {
                block_comment = false;
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        if let Some(quote) = string_quote {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == quote {
                string_quote = None;
            }
            index += 1;
            continue;
        }
        if byte == b'/' && next == Some(b'/') {
            line_comment = true;
            index += 2;
            continue;
        }
        if byte == b'/' && next == Some(b'*') {
            block_comment = true;
            index += 2;
            continue;
        }
        if matches!(byte, b'"' | b'\'') {
            string_quote = Some(byte);
            index += 1;
            continue;
        }
        match byte {
            b'(' => {
                let mut end = index;
                while end > 0 && source.as_bytes()[end - 1].is_ascii_whitespace() {
                    end -= 1;
                }
                let mut start = end;
                while start > 0 && is_name_byte(source.as_bytes()[start - 1]) {
                    start -= 1;
                }
                stack.push(Frame {
                    delimiter: b'(',
                    callee_start: (start < end).then_some(start),
                    callee_end: (start < end).then_some(end),
                    argument: 0,
                });
            }
            b'[' | b'{' => stack.push(Frame {
                delimiter: byte,
                callee_start: None,
                callee_end: None,
                argument: 0,
            }),
            b')' | b']' | b'}' => {
                let expected = match byte {
                    b')' => b'(',
                    b']' => b'[',
                    _ => b'{',
                };
                while let Some(frame) = stack.pop() {
                    if frame.delimiter == expected {
                        break;
                    }
                }
            }
            b',' => {
                if let Some(frame) = stack.last_mut() {
                    if frame.delimiter == b'(' {
                        frame.argument += 1;
                    }
                }
            }
            _ => {}
        }
        index += 1;
    }

    stack.iter().rev().find_map(|frame| {
        Some(CallSite {
            callee: source
                .get(frame.callee_start?..frame.callee_end?)?
                .to_string(),
            argument: frame.argument,
        })
    })
}

pub fn diagnostic_summary(diagnostics: &[Diagnostic]) -> (usize, usize) {
    diagnostics
        .iter()
        .fold((0, 0), |(errors, warnings), diagnostic| {
            match diagnostic.severity {
                Severity::Error => (errors + 1, warnings),
                Severity::Warning => (errors, warnings + 1),
            }
        })
}

pub fn is_keyword(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::KwIf
            | TokenKind::KwElse
            | TokenKind::KwTrue
            | TokenKind::KwFalse
            | TokenKind::KwAnd
            | TokenKind::KwOr
            | TokenKind::KwNot
            | TokenKind::KwVar
            | TokenKind::KwVarip
            | TokenKind::KwInt
            | TokenKind::KwFloat
            | TokenKind::KwBool
            | TokenKind::KwString
            | TokenKind::KwNa
            | TokenKind::KwFor
            | TokenKind::KwTo
            | TokenKind::KwBy
            | TokenKind::KwIn
            | TokenKind::KwWhile
            | TokenKind::KwBreak
            | TokenKind::KwContinue
            | TokenKind::KwSwitch
            | TokenKind::KwEnum
            | TokenKind::KwType
            | TokenKind::KwMethod
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = "//@version=6\nindicator(\"T\")\nlength = 14\nf(src, n = 2) => ta.sma(src, n)\nplot(f(close, length))\n";

    #[test]
    fn bundles_compiler_diagnostics_and_symbols() {
        let analysis = DocumentAnalysis::new("test.pine", SOURCE, Vec::new());
        assert!(analysis.success(), "{:?}", analysis.diagnostics());
        assert!(analysis
            .symbols()
            .iter()
            .any(|symbol| symbol.name == "length"));
        assert!(analysis.symbols().iter().any(|symbol| symbol.name == "f"));
        assert!(analysis.symbols().iter().any(|symbol| symbol.name == "src"));
    }

    #[test]
    fn compiler_errors_and_warnings_remain_structured() {
        let analysis = DocumentAnalysis::new("bad.pine", "x = missing\n", Vec::new());
        let codes: Vec<_> = analysis
            .diagnostics()
            .iter()
            .filter_map(|diagnostic| diagnostic.code.as_deref())
            .collect();
        assert!(codes.contains(&"E0201"));
        assert!(codes.contains(&"W0001"));
    }

    #[test]
    fn oversized_document_reports_the_compiler_limit_without_navigation_parsing() {
        let mut source = "variable = 1\n".to_string();
        source.push_str(&" ".repeat(MAX_SOURCE_LEN));
        let analysis = DocumentAnalysis::new("oversized.pine", source.clone(), Vec::new());
        assert!(!analysis.success());
        assert_eq!(analysis.diagnostics().len(), 1);
        assert_eq!(analysis.diagnostics()[0].code.as_deref(), Some("E0001"));
        assert!(analysis.symbols().is_empty());
        assert!(analysis.tokens().is_empty());
        assert!(symbols_from_source(&source).is_empty());
    }

    #[test]
    fn compiler_registry_drives_builtin_intelligence() {
        let sma = language_item("ta.sma").expect("ta.sma registered");
        assert_eq!(sma.kind, LanguageItemKind::Function);
        assert!(sma.signature.as_deref().unwrap().contains("length"));
        assert_eq!(language_item("close").unwrap().detail, "series float");
        assert_eq!(
            language_item("time").unwrap().kind,
            LanguageItemKind::Series
        );
        assert_eq!(
            language_function("time").unwrap().kind,
            LanguageItemKind::Function
        );

        let dual = DocumentAnalysis::new("dual.pine", "x = time\ny = time(\"D\")\n", Vec::new());
        let value_offset = dual.source().find("time").unwrap() as u32;
        let call_offset = dual.source().rfind("time").unwrap() as u32;
        assert_eq!(
            dual.language_item_at(value_offset).unwrap().0.kind,
            LanguageItemKind::Series
        );
        assert_eq!(
            dual.language_item_at(call_offset).unwrap().0.kind,
            LanguageItemKind::Function
        );
    }

    #[test]
    fn references_respect_function_parameter_shadowing() {
        let source = "x = 1\nf(x) => x + 1\nplot(x)\n";
        let analysis = DocumentAnalysis::new("scope.pine", source, Vec::new());
        let global = analysis
            .symbols()
            .iter()
            .find(|symbol| symbol.name == "x" && symbol.container.is_none())
            .unwrap();
        let parameter = analysis
            .symbols()
            .iter()
            .find(|symbol| symbol.name == "x" && symbol.kind == LocalSymbolKind::Parameter)
            .unwrap();
        assert_eq!(analysis.references_to(global).len(), 2);
        assert_eq!(analysis.references_to(parameter).len(), 2);

        let inside_f = source.find("x + 1").unwrap() as u32;
        let visible = analysis
            .completion_items(inside_f)
            .into_iter()
            .filter_map(|candidate| match candidate {
                CompletionCandidate::Local(symbol) => Some(symbol),
                CompletionCandidate::Builtin(_) => None,
            })
            .filter(|symbol| symbol.name == "x")
            .collect::<Vec<_>>();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].kind, LocalSymbolKind::Parameter);
    }

    #[test]
    fn references_respect_if_branch_and_else_branch_shadowing() {
        let source = concat!(
            "x = 1\n",
            "if close > open\n",
            "    x = 2\n",
            "    then_value = x\n",
            "else\n",
            "    x = 3\n",
            "    else_value = x\n",
            "plot(x)\n",
        );
        let analysis = DocumentAnalysis::new("branches.pine", source, Vec::new());
        let declarations = analysis
            .symbols()
            .iter()
            .filter(|symbol| symbol.name == "x")
            .collect::<Vec<_>>();
        assert_eq!(declarations.len(), 3);
        assert_eq!(analysis.references_to(declarations[0]).len(), 2);
        assert_eq!(analysis.references_to(declarations[1]).len(), 2);
        assert_eq!(analysis.references_to(declarations[2]).len(), 2);

        let final_use = source.rfind("x)").unwrap() as u32;
        assert!(same_declaration(
            analysis.local_symbol_at(final_use).unwrap(),
            declarations[0]
        ));
    }

    #[test]
    fn references_respect_loop_counter_shadowing_and_header_scope() {
        let source = concat!(
            "i = 10\n",
            "for i = i to 2\n",
            "    loop_value = i\n",
            "plot(i)\n",
        );
        let analysis = DocumentAnalysis::new("loop.pine", source, Vec::new());
        let declarations = analysis
            .symbols()
            .iter()
            .filter(|symbol| symbol.name == "i")
            .collect::<Vec<_>>();
        assert_eq!(declarations.len(), 2);
        assert_eq!(analysis.references_to(declarations[0]).len(), 3);
        assert_eq!(analysis.references_to(declarations[1]).len(), 2);

        let bound_use = source.find("i to").unwrap() as u32;
        let body_use = source.find("i\nplot").unwrap() as u32;
        let final_use = source.rfind("i)").unwrap() as u32;
        assert!(same_declaration(
            analysis.local_symbol_at(bound_use).unwrap(),
            declarations[0]
        ));
        assert!(same_declaration(
            analysis.local_symbol_at(body_use).unwrap(),
            declarations[1]
        ));
        assert!(same_declaration(
            analysis.local_symbol_at(final_use).unwrap(),
            declarations[0]
        ));
    }

    #[test]
    fn references_respect_switch_arm_shadowing() {
        let source = concat!(
            "x = 1\n",
            "switch\n",
            "    close > open =>\n",
            "        x = 2\n",
            "        arm_value = x\n",
            "    =>\n",
            "        default_value = x\n",
            "plot(x)\n",
        );
        let analysis = DocumentAnalysis::new("switch.pine", source, Vec::new());
        let declarations = analysis
            .symbols()
            .iter()
            .filter(|symbol| symbol.name == "x")
            .collect::<Vec<_>>();
        assert_eq!(declarations.len(), 2);
        assert_eq!(analysis.references_to(declarations[0]).len(), 3);
        assert_eq!(analysis.references_to(declarations[1]).len(), 2);

        let other_arm_use = source.find("x\nplot").unwrap() as u32;
        let final_use = source.rfind("x)").unwrap() as u32;
        assert!(same_declaration(
            analysis.local_symbol_at(other_arm_use).unwrap(),
            declarations[0]
        ));
        assert!(same_declaration(
            analysis.local_symbol_at(final_use).unwrap(),
            declarations[0]
        ));
    }

    #[test]
    fn finds_nested_signature_help_calls() {
        assert_eq!(
            find_enclosing_call("plot(ta.sma(close, "),
            Some(CallSite {
                callee: "ta.sma".to_string(),
                argument: 1
            })
        );
        assert_eq!(
            find_enclosing_call("plot('commas, stay, quoted', "),
            Some(CallSite {
                callee: "plot".to_string(),
                argument: 1
            })
        );
    }

    #[test]
    fn inspection_exposes_every_pipeline_stage_and_timings() {
        let analysis = DocumentAnalysis::new("test.pine", SOURCE, Vec::new());
        let inspection = analysis.inspect(InspectionStage::All);
        assert!(inspection.success);
        assert!(inspection.ast.as_deref().unwrap().contains("Program"));
        assert!(inspection.hir.is_some());
        assert!(inspection.mir.is_some());
        assert!(inspection.csil.as_deref().unwrap().contains("CSIL"));
        assert!(inspection.timings.is_some());
    }
}
