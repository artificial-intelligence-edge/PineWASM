//! Language Server Protocol transport and feature adapters.

use crate::analysis::{
    exported_symbol_from_source, is_keyword, language_function, symbols_from_source,
    CompletionCandidate, DocumentAnalysis, InspectionStage, LanguageItemKind, LocalSymbol,
    LocalSymbolKind, SERVER_NAME,
};
use crate::query;
use crate::query::{builtin_markdown, local_markdown, valid_identifier};
use crate::text::LineIndex;
use crate::workspace::{read_bounded_text, Workspace};
use crate::ToolResult;
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, Response};
use lsp_types::*;
use pinewasm_span::{Diagnostic as CompilerDiagnostic, Severity, SourceId, Span};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

const SEMANTIC_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::NAMESPACE,
    SemanticTokenType::TYPE,
    SemanticTokenType::FUNCTION,
    SemanticTokenType::VARIABLE,
    SemanticTokenType::PARAMETER,
    SemanticTokenType::PROPERTY,
    SemanticTokenType::ENUM_MEMBER,
    SemanticTokenType::KEYWORD,
];

const INSPECT_COMMANDS: &[&str] = &[
    "pinewasm.inspectAst",
    "pinewasm.inspectHir",
    "pinewasm.inspectMir",
    "pinewasm.inspectCsil",
    "pinewasm.inspectPipeline",
    "pinewasm.profile",
];

struct OpenDocument {
    version: i32,
    /// Canonical on-disk path, present only when the document resolved inside
    /// the configured workspace at the time it was last analyzed. Documents
    /// supplied by the client can still be analyzed without this authority,
    /// but they may not become workspace-edit targets.
    path: Option<PathBuf>,
    analysis: DocumentAnalysis,
    /// The v6 rewrite of a `//@version=5` document, computed with the
    /// analysis so diagnostics and code actions agree.
    migration: Option<pinewasm_migrate::Migration>,
}

struct ServerState {
    workspace: Workspace,
    documents: HashMap<Url, OpenDocument>,
    /// Libraries the client resolved itself (`pinewasm/setLibraries`), such as
    /// library sources it keeps outside the workspace. They replace a
    /// discovered library with the same import path, because the client's
    /// resolution is what its compiler runs.
    client_libraries: BTreeMap<String, String>,
}

/// Parameters of the `pinewasm/setLibraries` notification. Each notification
/// replaces the whole client-supplied set.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetLibrariesParams {
    libraries: Vec<ClientLibrary>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClientLibrary {
    import_path: String,
    source: String,
}

impl ServerState {
    fn new(workspace: Workspace) -> Self {
        Self {
            workspace,
            documents: HashMap::new(),
            client_libraries: BTreeMap::new(),
        }
    }

    /// Replace the client-supplied libraries, within the compiler's catalog
    /// limits. Malformed or over-limit entries are skipped, never fatal.
    fn set_client_libraries(&mut self, libraries: Vec<ClientLibrary>) {
        let mut accepted = BTreeMap::new();
        let mut source_bytes = 0usize;
        for library in libraries {
            let parts: Vec<_> = library.import_path.split('/').collect();
            if library.import_path.len() > pinewasm_compiler::MAX_LIBRARY_PATH_LEN
                || parts.len() != 3
                || parts.iter().any(|part| part.is_empty())
                || library.source.len() > pinewasm_compiler::MAX_SOURCE_LEN
            {
                eprintln!(
                    "pinewasm: skipping client library `{}`: invalid path or source",
                    library.import_path
                );
                continue;
            }
            // A repeated path replaces its earlier entry, so only the
            // difference counts against the limits.
            let replaced = accepted.get(&library.import_path).map_or(0, String::len);
            if (replaced == 0 && accepted.len() >= pinewasm_compiler::MAX_LIBRARY_COUNT)
                || (source_bytes - replaced).saturating_add(library.source.len())
                    > pinewasm_compiler::MAX_LIBRARY_SOURCE_BYTES
            {
                eprintln!(
                    "pinewasm: skipping client library `{}`: library catalog limit reached",
                    library.import_path
                );
                continue;
            }
            source_bytes = source_bytes - replaced + library.source.len();
            accepted.insert(library.import_path, library.source);
        }
        self.client_libraries = accepted;
    }

    fn open_buffers(&self) -> HashMap<PathBuf, String> {
        self.documents
            .values()
            .filter_map(|document| {
                Some((
                    document.path.clone()?,
                    document.analysis.source().to_string(),
                ))
            })
            .collect()
    }

    fn libraries(&self) -> Vec<crate::analysis::LibrarySource> {
        let discovered = self.workspace.libraries(&self.open_buffers());
        let mut source_bytes: usize = discovered.iter().map(|(_, source)| source.len()).sum();
        let mut libraries: Vec<_> = discovered
            .into_iter()
            .enumerate()
            .map(|(index, (import_path, source))| {
                let file_name = self
                    .workspace
                    .library_file(index as u32 + 1)
                    .map(|library| library.file.display().to_string());
                match self.client_libraries.get(&import_path) {
                    // A different client text replaces the discovered one when
                    // the catalog still fits; it has no file to point at. The
                    // same text keeps its file, so go-to-definition still works.
                    Some(client)
                        if *client != source
                            && (source_bytes - source.len()).saturating_add(client.len())
                                <= pinewasm_compiler::MAX_LIBRARY_SOURCE_BYTES =>
                    {
                        source_bytes = source_bytes - source.len() + client.len();
                        crate::analysis::LibrarySource {
                            import_path,
                            source: client.clone(),
                            file_name: None,
                        }
                    }
                    _ => crate::analysis::LibrarySource {
                        import_path,
                        source,
                        file_name,
                    },
                }
            })
            .collect();
        let known: HashSet<_> = libraries
            .iter()
            .map(|library| library.import_path.clone())
            .collect();
        for (import_path, source) in &self.client_libraries {
            if libraries.len() >= pinewasm_compiler::MAX_LIBRARY_COUNT {
                break;
            }
            if !known.contains(import_path)
                && source_bytes.saturating_add(source.len())
                    <= pinewasm_compiler::MAX_LIBRARY_SOURCE_BYTES
            {
                source_bytes += source.len();
                libraries.push(crate::analysis::LibrarySource {
                    import_path: import_path.clone(),
                    source: source.clone(),
                    file_name: None,
                });
            }
        }
        libraries
    }

    /// The source the compiler sees for an import, and the workspace file it
    /// came from when that file's text is what is compiled (so positions in
    /// it are valid for navigation).
    fn library_source(&self, import_path: &str) -> Option<(Option<PathBuf>, String)> {
        let library = self
            .libraries()
            .into_iter()
            .find(|library| library.import_path == import_path)?;
        let file = library.file_name.map(PathBuf::from);
        Some((file, library.source))
    }

    fn update_document(&mut self, uri: Url, version: i32, source: String) {
        let path = uri
            .to_file_path()
            .ok()
            .and_then(|candidate| self.workspace.resolve_file(&candidate).ok());
        let file_name = path
            .as_deref()
            .map_or_else(|| uri.to_string(), |path| path.display().to_string());
        let migration = (source.len() <= pinewasm_compiler::MAX_SOURCE_LEN
            && declares_version_5(&source))
        .then(|| {
            pinewasm_migrate::migrate(&source, &pinewasm_migrate::MigrationOptions::default())
        });
        let analysis = DocumentAnalysis::new(file_name, source, self.libraries());
        self.documents.insert(
            uri,
            OpenDocument {
                version,
                path,
                analysis,
                migration,
            },
        );
    }

    fn reanalyze_all(&mut self) {
        let revisions: Vec<_> = self
            .documents
            .iter()
            .map(|(uri, document)| {
                (
                    uri.clone(),
                    document.version,
                    document.analysis.source().to_string(),
                )
            })
            .collect();
        for (uri, version, source) in revisions {
            self.update_document(uri, version, source);
        }
    }

    fn analysis(&self, uri: &Url) -> Option<&DocumentAnalysis> {
        self.documents.get(uri).map(|document| &document.analysis)
    }

    /// Return the canonical URI that may receive edits for an open document.
    ///
    /// Re-resolving the client URI on every request closes the symlink-retarget
    /// window between `didOpen` and a later rename/quick-fix request. Requiring
    /// it to match the canonical path recorded at analysis time also prevents a
    /// document from silently changing identity while it remains open.
    fn edit_uri(&self, uri: &Url) -> Option<Url> {
        let document = self.documents.get(uri)?;
        let requested = uri.to_file_path().ok()?;
        let resolved = self.workspace.resolve_file(&requested).ok()?;
        if document.path.as_deref() != Some(resolved.as_path()) {
            return None;
        }
        Url::from_file_path(resolved).ok()
    }
}

#[allow(deprecated)]
pub fn run_stdio(workspace_override: Option<PathBuf>) -> ToolResult<()> {
    let (connection, io_threads) = Connection::stdio();
    let (initialize_id, initialize_value) = connection.initialize_start()?;
    let initialize: InitializeParams = serde_json::from_value(initialize_value)?;
    let root = workspace_override
        .or_else(|| initialize.root_uri.as_ref().and_then(uri_to_path))
        .or_else(|| initialize.root_path.as_ref().map(PathBuf::from))
        .unwrap_or(std::env::current_dir()?);
    let workspace = Workspace::new(root)?;
    connection.initialize_finish(initialize_id, initialize_result())?;
    run_connection(connection, workspace)?;
    io_threads.join()?;
    Ok(())
}

fn initialize_result() -> Value {
    let capabilities = ServerCapabilities {
        position_encoding: Some(PositionEncodingKind::UTF16),
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::FULL),
                will_save: Some(false),
                will_save_wait_until: Some(false),
                save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                    include_text: Some(false),
                })),
            },
        )),
        completion_provider: Some(CompletionOptions {
            resolve_provider: Some(false),
            trigger_characters: Some(vec![".".to_string()]),
            ..Default::default()
        }),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        signature_help_provider: Some(SignatureHelpOptions {
            trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
            retrigger_characters: Some(vec![")".to_string()]),
            ..Default::default()
        }),
        definition_provider: Some(OneOf::Left(true)),
        references_provider: Some(OneOf::Left(true)),
        code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
        rename_provider: Some(OneOf::Right(RenameOptions {
            prepare_provider: Some(true),
            work_done_progress_options: WorkDoneProgressOptions::default(),
        })),
        document_symbol_provider: Some(OneOf::Left(true)),
        workspace_symbol_provider: Some(OneOf::Left(true)),
        folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
        semantic_tokens_provider: Some(
            SemanticTokensOptions {
                work_done_progress_options: WorkDoneProgressOptions::default(),
                legend: SemanticTokensLegend {
                    token_types: SEMANTIC_TYPES.to_vec(),
                    token_modifiers: Vec::new(),
                },
                range: Some(false),
                full: Some(SemanticTokensFullOptions::Bool(true)),
            }
            .into(),
        ),
        execute_command_provider: Some(ExecuteCommandOptions {
            commands: INSPECT_COMMANDS
                .iter()
                .map(|command| (*command).to_string())
                .collect(),
            work_done_progress_options: WorkDoneProgressOptions::default(),
        }),
        workspace: Some(WorkspaceServerCapabilities {
            workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                // Analysis is deliberately rooted in one deterministic
                // workspace. Restart the server to select a different root.
                supported: Some(false),
                change_notifications: None,
            }),
            file_operations: None,
        }),
        ..Default::default()
    };
    json!({
        "capabilities": capabilities,
        "serverInfo": {
            "name": SERVER_NAME,
            "version": pinewasm_compiler::COMPILER_VERSION
        }
    })
}

fn run_connection(connection: Connection, workspace: Workspace) -> ToolResult<()> {
    let mut state = ServerState::new(workspace);
    for message in &connection.receiver {
        match message {
            Message::Request(request) => {
                if connection.handle_shutdown(&request)? {
                    return Ok(());
                }
                let response = handle_request(&state, request);
                connection.sender.send(Message::Response(response))?;
            }
            Message::Notification(notification) => {
                if let Err(error) = handle_notification(&connection, &mut state, notification) {
                    // A malformed or transient client notification must not
                    // tear down the server. Stderr is safe for LSP stdio logs.
                    eprintln!("pinewasm: notification failed: {error}");
                }
            }
            Message::Response(_) => {
                // This server does not currently issue client requests.
            }
        }
    }
    Ok(())
}

fn handle_request(state: &ServerState, request: Request) -> Response {
    let id = request.id.clone();
    let result =
        match request.method.as_str() {
            "textDocument/completion" => parse_params::<CompletionParams>(&request)
                .and_then(|params| completion(state, params)),
            "textDocument/hover" => {
                parse_params::<HoverParams>(&request).and_then(|params| hover(state, params))
            }
            "textDocument/signatureHelp" => parse_params::<SignatureHelpParams>(&request)
                .and_then(|params| signature_help(state, params)),
            "textDocument/definition" => parse_params::<GotoDefinitionParams>(&request)
                .and_then(|params| definition(state, params)),
            "textDocument/references" => parse_params::<ReferenceParams>(&request)
                .and_then(|params| references(state, params)),
            "textDocument/codeAction" => parse_params::<CodeActionParams>(&request)
                .and_then(|params| code_actions(state, params)),
            "textDocument/prepareRename" => parse_params::<TextDocumentPositionParams>(&request)
                .and_then(|params| prepare_rename(state, params)),
            "textDocument/rename" => {
                parse_params::<RenameParams>(&request).and_then(|params| rename(state, params))
            }
            "textDocument/documentSymbol" => parse_params::<DocumentSymbolParams>(&request)
                .and_then(|params| document_symbols(state, params)),
            "workspace/symbol" => parse_params::<WorkspaceSymbolParams>(&request)
                .and_then(|params| workspace_symbols(state, params)),
            "textDocument/foldingRange" => parse_params::<FoldingRangeParams>(&request)
                .and_then(|params| folding_ranges(state, params)),
            "textDocument/semanticTokens/full" => parse_params::<SemanticTokensParams>(&request)
                .and_then(|params| semantic_tokens(state, params)),
            "workspace/executeCommand" => parse_params::<ExecuteCommandParams>(&request)
                .and_then(|params| execute_command(state, params)),
            _ => Err((
                ErrorCode::MethodNotFound as i32,
                format!("unsupported request `{}`", request.method),
            )),
        };
    match result {
        Ok(value) => Response::new_ok(id, value),
        Err((code, message)) => Response::new_err(id, code, message),
    }
}

fn handle_notification(
    connection: &Connection,
    state: &mut ServerState,
    notification: Notification,
) -> ToolResult<()> {
    match notification.method.as_str() {
        "textDocument/didOpen" => {
            let params: DidOpenTextDocumentParams = serde_json::from_value(notification.params)?;
            state.update_document(
                params.text_document.uri.clone(),
                params.text_document.version,
                params.text_document.text,
            );
            state.reanalyze_all();
            publish_all(connection, state)?;
        }
        "textDocument/didChange" => {
            let params: DidChangeTextDocumentParams = serde_json::from_value(notification.params)?;
            if let Some(change) = params.content_changes.into_iter().next_back() {
                state.update_document(
                    params.text_document.uri,
                    params.text_document.version,
                    change.text,
                );
                state.reanalyze_all();
                publish_all(connection, state)?;
            }
        }
        "textDocument/didSave" => {
            let _: DidSaveTextDocumentParams = serde_json::from_value(notification.params)?;
            // Refresh disk-backed libraries even when the client did not
            // register watched-file notifications. Open buffers still win.
            state.workspace.refresh()?;
            state.reanalyze_all();
            publish_all(connection, state)?;
        }
        "textDocument/didClose" => {
            let params: DidCloseTextDocumentParams = serde_json::from_value(notification.params)?;
            state.documents.remove(&params.text_document.uri);
            send_notification(
                connection,
                "textDocument/publishDiagnostics",
                PublishDiagnosticsParams {
                    uri: params.text_document.uri,
                    diagnostics: Vec::new(),
                    version: None,
                },
            )?;
            // Importers immediately fall back from a closed unsaved library
            // buffer to the latest revision on disk.
            state.workspace.refresh()?;
            state.reanalyze_all();
            publish_all(connection, state)?;
        }
        "workspace/didChangeWatchedFiles" => {
            state.workspace.refresh()?;
            state.reanalyze_all();
            publish_all(connection, state)?;
        }
        "pinewasm/setLibraries" => {
            let params: SetLibrariesParams = serde_json::from_value(notification.params)?;
            state.set_client_libraries(params.libraries);
            state.reanalyze_all();
            publish_all(connection, state)?;
        }
        "workspace/didChangeWorkspaceFolders" => {}
        "$/cancelRequest" | "$/setTrace" | "initialized" | "exit" => {}
        _ => {}
    }
    Ok(())
}

type RequestResult = Result<Value, (i32, String)>;

fn query_error_response(message: String) -> (i32, String) {
    let code = if message == query::RESOLUTION_WORK_BUDGET_EXCEEDED {
        ErrorCode::RequestFailed
    } else {
        ErrorCode::InvalidParams
    };
    (code as i32, message)
}

fn parse_params<T: DeserializeOwned>(request: &Request) -> Result<T, (i32, String)> {
    serde_json::from_value(request.params.clone()).map_err(|error| {
        (
            ErrorCode::InvalidParams as i32,
            format!("invalid parameters for `{}`: {error}", request.method),
        )
    })
}

fn document_position<'a>(
    state: &'a ServerState,
    params: &TextDocumentPositionParams,
) -> Result<(&'a DocumentAnalysis, u32), (i32, String)> {
    let analysis = state.analysis(&params.text_document.uri).ok_or_else(|| {
        (
            ErrorCode::InvalidParams as i32,
            format!("document is not open: {}", params.text_document.uri),
        )
    })?;
    let offset = analysis.line_index().byte_offset(params.position);
    Ok((analysis, offset))
}

fn completion(state: &ServerState, params: CompletionParams) -> RequestResult {
    let (analysis, offset) = document_position(state, &params.text_document_position)?;
    let namespace = analysis.dotted_name_at(offset).and_then(|(name, _)| {
        name.rsplit_once('.')
            .map(|(namespace, _)| namespace.to_string())
    });
    let mut items = analysis
        .completion_items(offset)
        .into_iter()
        .map(|candidate| match candidate {
            CompletionCandidate::Local(symbol) => CompletionItem {
                label: symbol.name.clone(),
                kind: Some(local_completion_kind(symbol.kind)),
                detail: Some(
                    symbol
                        .signature
                        .clone()
                        .unwrap_or_else(|| symbol.detail.clone()),
                ),
                documentation: Some(Documentation::MarkupContent(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: local_markdown(symbol, analysis.line_index()),
                })),
                insert_text: Some(
                    if matches!(
                        symbol.kind,
                        LocalSymbolKind::Function | LocalSymbolKind::Method
                    ) {
                        format!("{}(", symbol.name)
                    } else {
                        symbol.name.clone()
                    },
                ),
                ..Default::default()
            },
            CompletionCandidate::Builtin(item) => {
                let label = namespace.as_deref().map_or_else(
                    || item.name.clone(),
                    |namespace| {
                        item.name
                            .strip_prefix(&format!("{namespace}."))
                            .unwrap_or(&item.name)
                            .split('.')
                            .next()
                            .unwrap_or(&item.name)
                            .to_string()
                    },
                );
                CompletionItem {
                    label,
                    kind: Some(language_completion_kind(item.kind)),
                    detail: Some(item.detail.clone()),
                    documentation: Some(Documentation::MarkupContent(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: builtin_markdown(item),
                    })),
                    insert_text: Some(if namespace.is_some() {
                        item.insert_text.rsplit_once('.').map_or_else(
                            || item.insert_text.clone(),
                            |(_, suffix)| suffix.to_string(),
                        )
                    } else {
                        item.insert_text.clone()
                    }),
                    ..Default::default()
                }
            }
        })
        .collect::<Vec<_>>();
    if let Some(namespace) = namespace.as_deref() {
        if let Some(module) = analysis
            .symbols()
            .iter()
            .find(|symbol| symbol.kind == LocalSymbolKind::Module && symbol.name == namespace)
        {
            if let Some(import_path) = module.detail.strip_prefix("import ") {
                if let Some((_, library_source)) = state.library_source(import_path) {
                    let mut seen = items
                        .iter()
                        .map(|item| item.label.clone())
                        .collect::<HashSet<_>>();
                    for symbol in
                        symbols_from_source(&library_source)
                            .into_iter()
                            .filter(|symbol| {
                                symbol.detail.starts_with("exported ")
                                    && matches!(
                                        symbol.kind,
                                        LocalSymbolKind::Function | LocalSymbolKind::Method
                                    )
                            })
                    {
                        if !seen.insert(symbol.name.clone()) {
                            continue;
                        }
                        items.push(CompletionItem {
                            label: symbol.name.clone(),
                            kind: Some(local_completion_kind(symbol.kind)),
                            detail: symbol.signature.clone(),
                            documentation: Some(Documentation::String(format!(
                                "Exported by Pine library `{import_path}`."
                            ))),
                            insert_text: Some(format!("{}(", symbol.name)),
                            ..Default::default()
                        });
                    }
                }
            }
        }
    }
    to_value(CompletionResponse::Array(items))
}

fn hover(state: &ServerState, params: HoverParams) -> RequestResult {
    let (analysis, offset) = document_position(state, &params.text_document_position_params)?;
    to_value(query::hover(analysis, offset).map(|value| Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: value.markdown,
        }),
        range: Some(analysis.line_index().range(value.span)),
    }))
}

fn signature_help(state: &ServerState, params: SignatureHelpParams) -> RequestResult {
    let (analysis, offset) = document_position(state, &params.text_document_position_params)?;
    let Some(call) = analysis.enclosing_call(offset) else {
        return to_value(Option::<SignatureHelp>::None);
    };
    if let Some(item) = language_function(&call.callee) {
        let parameters = item
            .parameters
            .iter()
            .map(|parameter| ParameterInformation {
                label: ParameterLabel::Simple(parameter.label.clone()),
                documentation: Some(Documentation::String(parameter.detail.clone())),
            })
            .collect::<Vec<_>>();
        let active = call.argument.min(parameters.len().saturating_sub(1)) as u32;
        return to_value(Some(SignatureHelp {
            signatures: vec![SignatureInformation {
                label: item.signature.clone().unwrap_or_else(|| item.name.clone()),
                documentation: Some(Documentation::MarkupContent(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: builtin_markdown(item),
                })),
                parameters: Some(parameters),
                active_parameter: Some(active),
            }],
            active_signature: Some(0),
            active_parameter: Some(active),
        }));
    }
    if let Some(symbol) = analysis
        .symbols()
        .iter()
        .find(|symbol| symbol.name == call.callee && symbol.signature.is_some())
    {
        return to_value(Some(SignatureHelp {
            signatures: vec![SignatureInformation {
                label: symbol.signature.clone().unwrap_or_default(),
                documentation: Some(Documentation::String(symbol.detail.clone())),
                parameters: None,
                active_parameter: Some(call.argument as u32),
            }],
            active_signature: Some(0),
            active_parameter: Some(call.argument as u32),
        }));
    }
    to_value(Option::<SignatureHelp>::None)
}

fn definition(state: &ServerState, params: GotoDefinitionParams) -> RequestResult {
    let uri = params
        .text_document_position_params
        .text_document
        .uri
        .clone();
    let (analysis, offset) = document_position(state, &params.text_document_position_params)?;
    // `alias.member` navigates to the imported library file when one follows
    // the workspace's library mapping convention. Member-level positioning is
    // resolved by analyzing the library source.
    if let Some((dotted, _)) = analysis.dotted_name_at(offset) {
        if let Some((alias, member)) = dotted.split_once('.') {
            if let Some(module) = analysis
                .symbols()
                .iter()
                .find(|symbol| symbol.kind == LocalSymbolKind::Module && symbol.name == alias)
            {
                if let Some(import_path) = module.detail.strip_prefix("import ") {
                    if let Some((Some(file), library_source)) = state.library_source(import_path) {
                        if let Some(symbol) = exported_symbol_from_source(&library_source, member) {
                            if let Ok(uri) = Url::from_file_path(&file) {
                                let index = LineIndex::new(library_source.clone());
                                return to_value(Some(GotoDefinitionResponse::Scalar(
                                    Location::new(uri, index.range(symbol.span)),
                                )));
                            }
                        }
                    }
                }
            }
        }
    }
    if let Some(symbol) = analysis.local_symbol_at(offset) {
        return to_value(Some(GotoDefinitionResponse::Scalar(Location::new(
            uri,
            analysis.line_index().range(symbol.span),
        ))));
    }
    to_value(Option::<GotoDefinitionResponse>::None)
}

fn references(state: &ServerState, params: ReferenceParams) -> RequestResult {
    let uri = params.text_document_position.text_document.uri.clone();
    let (analysis, offset) = document_position(state, &params.text_document_position)?;
    let locations = query::references(analysis, offset, params.context.include_declaration)
        .map_err(|error| query_error_response(error.code().to_owned()))?
        .into_iter()
        .map(|span| Location::new(uri.clone(), analysis.line_index().range(span)))
        .collect::<Vec<_>>();
    to_value(locations)
}

fn code_actions(state: &ServerState, params: CodeActionParams) -> RequestResult {
    let Some(edit_uri) = state.edit_uri(&params.text_document.uri) else {
        // Client-supplied source outside the workspace remains analyzable, but
        // it must never induce a filesystem edit outside the configured root.
        return to_value(Vec::<CodeActionOrCommand>::new());
    };
    let Some(analysis) = state.analysis(&params.text_document.uri) else {
        return to_value(Vec::<CodeActionOrCommand>::new());
    };
    let mut actions = Vec::new();
    if let Some(migration) = state
        .documents
        .get(&params.text_document.uri)
        .and_then(|document| document.migration.as_ref())
    {
        migration_actions(analysis, migration, &edit_uri, &params, &mut actions);
    }
    let selection = query::Range {
        start: query::Position {
            line: params.range.start.line,
            character: params.range.start.character,
        },
        end: query::Position {
            line: params.range.end.line,
            character: params.range.end.character,
        },
    };
    actions.extend(
        query::code_actions(analysis, selection)
            .into_iter()
            .map(|action| {
                let mut changes = HashMap::new();
                changes.insert(
                    edit_uri.clone(),
                    vec![TextEdit::new(
                        analysis.line_index().range(action.span),
                        action.text,
                    )],
                );
                CodeActionOrCommand::CodeAction(CodeAction {
                    title: action.title,
                    kind: Some(CodeActionKind::QUICKFIX),
                    diagnostics: None,
                    edit: Some(WorkspaceEdit {
                        changes: Some(changes),
                        document_changes: None,
                        change_annotations: None,
                    }),
                    is_preferred: Some(true),
                    disabled: None,
                    command: None,
                    data: action.diagnostic_code.map(|code| json!({ "code": code })),
                })
            })
            .collect::<Vec<_>>(),
    );
    to_value(actions)
}

/// Whether the first version directive in `source` is `//@version=5`.
fn declares_version_5(source: &str) -> bool {
    source
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("//@version="))
        .is_some_and(|line| line.trim_start_matches("//@version=").trim() == "5")
}

/// The most edits one code-action request offers as individual quick fixes.
const MAX_MIGRATION_QUICK_FIXES: usize = 12;

/// Code actions that migrate a v5 document: the whole-file conversion, and
/// one quick fix per rewrite whose site overlaps the requested range.
fn migration_actions(
    analysis: &DocumentAnalysis,
    migration: &pinewasm_migrate::Migration,
    edit_uri: &Url,
    params: &CodeActionParams,
    actions: &mut Vec<CodeActionOrCommand>,
) {
    use pinewasm_migrate::{migrate_range, MigrationOptions, Status};
    if migration.status != Status::Migrated {
        return;
    }
    let index = analysis.line_index();
    let to_edits = |edits: &[pinewasm_migrate::TextEdit]| -> Vec<TextEdit> {
        edits
            .iter()
            .map(|edit| {
                TextEdit::new(
                    index.range(Span::new(SourceId::MAIN, edit.start, edit.end)),
                    edit.new_text.clone(),
                )
            })
            .collect()
    };
    let action = |title: String, kind: CodeActionKind, edits: Vec<TextEdit>, preferred: bool| {
        let mut changes = HashMap::new();
        changes.insert(edit_uri.clone(), edits);
        CodeActionOrCommand::CodeAction(CodeAction {
            title,
            kind: Some(kind),
            diagnostics: None,
            edit: Some(WorkspaceEdit {
                changes: Some(changes),
                document_changes: None,
                change_annotations: None,
            }),
            is_preferred: Some(preferred),
            disabled: None,
            command: None,
            data: None,
        })
    };
    let review = migration
        .findings
        .iter()
        .filter(|finding| finding.severity >= pinewasm_migrate::Severity::Warning)
        .count();
    let title = if review == 0 {
        "Convert script to Pine v6".to_string()
    } else {
        format!("Convert script to Pine v6 ({review} sites need review)")
    };
    let whole = to_edits(&migration.edits);
    actions.push(action(
        title.clone(),
        CodeActionKind::QUICKFIX,
        whole.clone(),
        true,
    ));
    actions.push(action(
        title,
        CodeActionKind::new("source.convertToPineV6"),
        whole,
        false,
    ));
    let source = analysis.source();
    let mut offered = HashSet::new();
    for finding in migration
        .findings
        .iter()
        .filter(|f| f.applied && f.rule != "A1")
    {
        if offered.len() >= MAX_MIGRATION_QUICK_FIXES {
            break;
        }
        let range = index.range(Span::new(SourceId::MAIN, finding.start, finding.end));
        if range.end < params.range.start || range.start > params.range.end {
            continue;
        }
        let single = migrate_range(
            source,
            &MigrationOptions::default(),
            finding.start,
            finding.end,
        );
        if single.edits.is_empty() || !offered.insert(single.output.clone()) {
            continue;
        }
        actions.push(action(
            format!(
                "Migrate to v6 [{}]: {}",
                finding.rule,
                first_sentence(&finding.message)
            ),
            CodeActionKind::QUICKFIX,
            to_edits(&single.edits),
            false,
        ));
    }
}

fn first_sentence(message: &str) -> &str {
    message.split(": ").next().unwrap_or(message)
}

fn prepare_rename(state: &ServerState, params: TextDocumentPositionParams) -> RequestResult {
    if state.edit_uri(&params.text_document.uri).is_none() {
        return to_value(Option::<PrepareRenameResponse>::None);
    }
    let (analysis, offset) = document_position(state, &params)?;
    let result = query::prepare_rename(analysis, offset)
        .map(|span| PrepareRenameResponse::Range(analysis.line_index().range(span)));
    to_value(result)
}

fn rename(state: &ServerState, params: RenameParams) -> RequestResult {
    if !valid_identifier(&params.new_name) {
        return Err((
            ErrorCode::InvalidParams as i32,
            format!("`{}` is not a valid Pine identifier", params.new_name),
        ));
    }
    let uri = params.text_document_position.text_document.uri.clone();
    let edit_uri = state.edit_uri(&uri).ok_or_else(|| {
        (
            ErrorCode::InvalidParams as i32,
            format!("document is not an editable file inside the workspace: {uri}"),
        )
    })?;
    let (analysis, offset) = document_position(state, &params.text_document_position)?;
    let edits = query::checked_rename(analysis, offset, &params.new_name)
        .map_err(query_error_response)?
        .into_iter()
        .map(|span| TextEdit::new(analysis.line_index().range(span), params.new_name.clone()))
        .collect();
    let mut changes = HashMap::new();
    changes.insert(edit_uri, edits);
    to_value(WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    })
}

fn document_symbols(state: &ServerState, params: DocumentSymbolParams) -> RequestResult {
    let Some(analysis) = state.analysis(&params.text_document.uri) else {
        return to_value(Vec::<DocumentSymbol>::new());
    };
    #[allow(deprecated)]
    let symbols = analysis
        .symbols()
        .iter()
        .map(|symbol| DocumentSymbol {
            name: symbol.name.clone(),
            detail: Some(
                symbol
                    .signature
                    .clone()
                    .unwrap_or_else(|| symbol.detail.clone()),
            ),
            kind: local_symbol_kind(symbol.kind),
            tags: None,
            deprecated: None,
            range: analysis.line_index().range(symbol.full_span),
            selection_range: analysis.line_index().range(symbol.span),
            children: None,
        })
        .collect::<Vec<_>>();
    to_value(DocumentSymbolResponse::Nested(symbols))
}

#[allow(deprecated)]
fn workspace_symbols(state: &ServerState, params: WorkspaceSymbolParams) -> RequestResult {
    let query = params.query.to_ascii_lowercase();
    let mut results = Vec::new();
    let mut open_documents = state
        .documents
        .values()
        .filter_map(|document| Some((document.path.as_ref()?, document)))
        .collect::<Vec<_>>();
    open_documents.sort_by(|(left_path, left), (right_path, right)| {
        left_path
            .cmp(right_path)
            .then_with(|| right.version.cmp(&left.version))
    });
    let mut open_paths = HashSet::new();
    for (path, document) in open_documents {
        if !open_paths.insert(path.clone()) {
            continue;
        }
        let Ok(uri) = Url::from_file_path(path) else {
            continue;
        };
        if append_workspace_symbols(
            &mut results,
            &query,
            &uri,
            document.analysis.line_index(),
            document.analysis.symbols().iter(),
        ) {
            return to_value(results);
        }
    }

    for file in state.workspace.pine_files().iter().take(2_000) {
        if open_paths.contains(file) {
            continue;
        }
        let Ok(source) = read_bounded_text(file, pinewasm_compiler::MAX_SOURCE_LEN) else {
            continue;
        };
        let index = LineIndex::new(source.clone());
        let Ok(uri) = Url::from_file_path(file) else {
            continue;
        };
        let symbols = symbols_from_source(&source);
        if append_workspace_symbols(&mut results, &query, &uri, &index, symbols.iter()) {
            return to_value(results);
        }
    }
    to_value(results)
}

#[allow(deprecated)]
fn append_workspace_symbols<'a>(
    results: &mut Vec<SymbolInformation>,
    query: &str,
    uri: &Url,
    index: &LineIndex,
    symbols: impl IntoIterator<Item = &'a LocalSymbol>,
) -> bool {
    for symbol in symbols {
        if !query.is_empty() && !symbol.name.to_ascii_lowercase().contains(query) {
            continue;
        }
        results.push(SymbolInformation {
            name: symbol.name.clone(),
            kind: local_symbol_kind(symbol.kind),
            tags: None,
            deprecated: None,
            location: Location::new(uri.clone(), index.range(symbol.span)),
            container_name: symbol.container.clone(),
        });
        if results.len() >= 200 {
            return true;
        }
    }
    false
}

fn folding_ranges(state: &ServerState, params: FoldingRangeParams) -> RequestResult {
    let Some(analysis) = state.analysis(&params.text_document.uri) else {
        return to_value(Vec::<FoldingRange>::new());
    };
    let mut ranges = Vec::new();
    let mut seen = HashSet::new();
    for symbol in analysis.symbols().iter().filter(|symbol| {
        matches!(
            symbol.kind,
            LocalSymbolKind::Function | LocalSymbolKind::Method | LocalSymbolKind::Type
        )
    }) {
        let range = analysis.line_index().range(symbol.full_span);
        if range.end.line > range.start.line && seen.insert((range.start.line, range.end.line)) {
            ranges.push(FoldingRange {
                start_line: range.start.line,
                start_character: Some(range.start.character),
                end_line: range.end.line,
                end_character: Some(range.end.character),
                kind: Some(FoldingRangeKind::Region),
                collapsed_text: Some(symbol.name.clone()),
            });
        }
    }
    to_value(ranges)
}

fn semantic_tokens(state: &ServerState, params: SemanticTokensParams) -> RequestResult {
    let Some(analysis) = state.analysis(&params.text_document.uri) else {
        return to_value(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data: Vec::new(),
        }));
    };
    let mut absolute: Vec<(u32, u32, u32, u32)> = Vec::new();
    let mut covered = HashSet::<(u32, u32)>::new();
    for token in analysis.tokens() {
        if is_keyword(token.kind) {
            add_semantic_span(analysis, token.span, 7, &mut absolute, &mut covered);
            continue;
        }
        if token.kind != pinewasm_syntax::TokenKind::Ident {
            continue;
        }
        let middle = token.span.start + token.span.len() / 2;
        if let Some((dotted, span)) = analysis.dotted_name_at(middle) {
            if dotted.contains('.') {
                if let Some((item, _)) = analysis.language_item_at(middle) {
                    add_semantic_span(
                        analysis,
                        span,
                        semantic_type_for_language_item(item.kind),
                        &mut absolute,
                        &mut covered,
                    );
                    continue;
                }
            }
        }
        if let Some(symbol) =
            analysis.resolve_local(token.text(analysis.source()), token.span.start)
        {
            add_semantic_span(
                analysis,
                token.span,
                semantic_type_for_local(symbol.kind),
                &mut absolute,
                &mut covered,
            );
        } else if let Some((item, _)) = analysis.language_item_at(middle) {
            add_semantic_span(
                analysis,
                token.span,
                semantic_type_for_language_item(item.kind),
                &mut absolute,
                &mut covered,
            );
        }
    }
    absolute.sort_unstable();
    let mut previous_line = 0;
    let mut previous_start = 0;
    let mut data = Vec::with_capacity(absolute.len());
    for (line, start, length, token_type) in absolute {
        let delta_line = line - previous_line;
        let delta_start = if delta_line == 0 {
            start - previous_start
        } else {
            start
        };
        data.push(SemanticToken {
            delta_line,
            delta_start,
            length,
            token_type,
            token_modifiers_bitset: 0,
        });
        previous_line = line;
        previous_start = start;
    }
    to_value(SemanticTokensResult::Tokens(SemanticTokens {
        result_id: None,
        data,
    }))
}

fn execute_command(state: &ServerState, params: ExecuteCommandParams) -> RequestResult {
    let Some(stage) = inspection_stage_for_command(&params.command) else {
        return Err((
            ErrorCode::InvalidParams as i32,
            format!("unknown PineWASM command `{}`", params.command),
        ));
    };
    let uri = params
        .arguments
        .first()
        .and_then(|value| {
            value
                .as_str()
                .map(str::to_string)
                .or_else(|| value.get("uri")?.as_str().map(str::to_string))
        })
        .ok_or_else(|| {
            (
                ErrorCode::InvalidParams as i32,
                "inspection commands require the document URI as their first argument".to_string(),
            )
        })?;
    let uri = Url::parse(&uri).map_err(|error| {
        (
            ErrorCode::InvalidParams as i32,
            format!("invalid document URI: {error}"),
        )
    })?;
    let analysis = state.analysis(&uri).ok_or_else(|| {
        (
            ErrorCode::InvalidParams as i32,
            format!("document is not open: {uri}"),
        )
    })?;
    to_value(analysis.inspect(stage))
}

/// Resolve the inspection command ids advertised by PineWASM.
fn inspection_stage_for_command(command: &str) -> Option<InspectionStage> {
    match command {
        "pinewasm.inspectAst" => Some(InspectionStage::Ast),
        "pinewasm.inspectHir" => Some(InspectionStage::Hir),
        "pinewasm.inspectMir" => Some(InspectionStage::Mir),
        "pinewasm.inspectCsil" => Some(InspectionStage::Csil),
        "pinewasm.inspectPipeline" | "pinewasm.profile" => Some(InspectionStage::All),
        _ => None,
    }
}

fn publish_all(connection: &Connection, state: &ServerState) -> ToolResult<()> {
    for (uri, document) in &state.documents {
        publish_one(connection, uri, document)?;
    }
    Ok(())
}

fn publish_one(connection: &Connection, uri: &Url, document: &OpenDocument) -> ToolResult<()> {
    let mut diagnostics: Vec<Diagnostic> = document
        .analysis
        .diagnostics_for_source(SourceId::MAIN)
        .map(|diagnostic| lsp_diagnostic(&document.analysis, uri, diagnostic))
        .collect();
    // Sites the v6 migration cannot convert exactly, so they are visible
    // before the conversion is applied.
    if let Some(migration) = &document.migration {
        let index = document.analysis.line_index();
        for finding in migration
            .findings
            .iter()
            .filter(|f| f.severity >= pinewasm_migrate::Severity::Warning)
        {
            diagnostics.push(Diagnostic {
                range: index.range(Span::new(SourceId::MAIN, finding.start, finding.end)),
                severity: Some(if finding.severity == pinewasm_migrate::Severity::Error {
                    DiagnosticSeverity::WARNING
                } else {
                    DiagnosticSeverity::INFORMATION
                }),
                code: Some(NumberOrString::String(format!("migrate {}", finding.rule))),
                code_description: None,
                source: Some("pinewasm migrate".to_string()),
                message: finding.message.clone(),
                related_information: None,
                tags: None,
                data: None,
            });
        }
    }
    send_notification(
        connection,
        "textDocument/publishDiagnostics",
        PublishDiagnosticsParams {
            uri: uri.clone(),
            diagnostics,
            version: Some(document.version),
        },
    )
}

fn lsp_diagnostic(
    analysis: &DocumentAnalysis,
    document_uri: &Url,
    diagnostic: &CompilerDiagnostic,
) -> Diagnostic {
    let primary = diagnostic.primary_span().unwrap_or_else(Span::dummy);
    let mut message = diagnostic.message.clone();
    if let Some(label) = diagnostic.labels.first() {
        if !label.message.is_empty() && label.message != message {
            message.push_str("\n\n");
            message.push_str(&label.message);
        }
    }
    for note in &diagnostic.notes {
        message.push_str("\n\nNote: ");
        message.push_str(note);
    }
    let related_information = diagnostic
        .labels
        .iter()
        .skip(1)
        .filter_map(|label| {
            let (file_name, source) = analysis.source_for_id(label.span.source)?;
            let uri = if label.span.source == SourceId::MAIN {
                document_uri.clone()
            } else {
                Url::from_file_path(PathBuf::from(file_name)).ok()?
            };
            Some(DiagnosticRelatedInformation {
                location: Location::new(uri, LineIndex::new(source.to_string()).range(label.span)),
                message: label.message.clone(),
            })
        })
        .collect::<Vec<_>>();
    Diagnostic {
        range: analysis.line_index().range(primary),
        severity: Some(match diagnostic.severity {
            Severity::Error => DiagnosticSeverity::ERROR,
            Severity::Warning => DiagnosticSeverity::WARNING,
        }),
        code: diagnostic
            .code
            .as_ref()
            .map(|code| NumberOrString::String(code.clone())),
        code_description: None,
        source: Some("pinewasm".to_string()),
        message,
        related_information: (!related_information.is_empty()).then_some(related_information),
        tags: None,
        data: serde_json::to_value(diagnostic).ok(),
    }
}

fn send_notification<P: serde::Serialize>(
    connection: &Connection,
    method: &str,
    params: P,
) -> ToolResult<()> {
    connection
        .sender
        .send(Message::Notification(Notification::new(
            method.to_string(),
            params,
        )))?;
    Ok(())
}

fn add_semantic_span(
    analysis: &DocumentAnalysis,
    span: Span,
    token_type: u32,
    output: &mut Vec<(u32, u32, u32, u32)>,
    covered: &mut HashSet<(u32, u32)>,
) {
    if !covered.insert((span.start, span.end)) {
        return;
    }
    let range = analysis.line_index().range(span);
    if range.start.line == range.end.line && range.end.character > range.start.character {
        output.push((
            range.start.line,
            range.start.character,
            range.end.character - range.start.character,
            token_type,
        ));
    }
}

fn local_completion_kind(kind: LocalSymbolKind) -> CompletionItemKind {
    match kind {
        LocalSymbolKind::Variable | LocalSymbolKind::Parameter => CompletionItemKind::VARIABLE,
        LocalSymbolKind::Function | LocalSymbolKind::Method => CompletionItemKind::FUNCTION,
        LocalSymbolKind::Type => CompletionItemKind::CLASS,
        LocalSymbolKind::Field => CompletionItemKind::FIELD,
        LocalSymbolKind::EnumMember => CompletionItemKind::ENUM_MEMBER,
        LocalSymbolKind::Module => CompletionItemKind::MODULE,
    }
}

fn language_completion_kind(kind: LanguageItemKind) -> CompletionItemKind {
    match kind {
        LanguageItemKind::Keyword => CompletionItemKind::KEYWORD,
        LanguageItemKind::Function => CompletionItemKind::FUNCTION,
        LanguageItemKind::Series => CompletionItemKind::VARIABLE,
        LanguageItemKind::Constant => CompletionItemKind::CONSTANT,
        LanguageItemKind::Namespace => CompletionItemKind::MODULE,
    }
}

fn local_symbol_kind(kind: LocalSymbolKind) -> SymbolKind {
    match kind {
        LocalSymbolKind::Variable => SymbolKind::VARIABLE,
        LocalSymbolKind::Function | LocalSymbolKind::Method => SymbolKind::FUNCTION,
        LocalSymbolKind::Parameter => SymbolKind::VARIABLE,
        LocalSymbolKind::Type => SymbolKind::CLASS,
        LocalSymbolKind::Field => SymbolKind::FIELD,
        LocalSymbolKind::EnumMember => SymbolKind::ENUM_MEMBER,
        LocalSymbolKind::Module => SymbolKind::MODULE,
    }
}

fn semantic_type_for_local(kind: LocalSymbolKind) -> u32 {
    match kind {
        LocalSymbolKind::Module => 0,
        LocalSymbolKind::Type => 1,
        LocalSymbolKind::Function | LocalSymbolKind::Method => 2,
        LocalSymbolKind::Variable => 3,
        LocalSymbolKind::Parameter => 4,
        LocalSymbolKind::Field => 5,
        LocalSymbolKind::EnumMember => 6,
    }
}

fn semantic_type_for_language_item(kind: LanguageItemKind) -> u32 {
    match kind {
        LanguageItemKind::Namespace => 0,
        LanguageItemKind::Function => 2,
        LanguageItemKind::Series | LanguageItemKind::Constant => 3,
        LanguageItemKind::Keyword => 7,
    }
}

fn uri_to_path(uri: &Url) -> Option<PathBuf> {
    let path = uri.to_file_path().ok()?;
    path.canonicalize().ok().or(Some(path))
}

fn to_value(value: impl serde::Serialize) -> RequestResult {
    serde_json::to_value(value).map_err(|error| {
        (
            ErrorCode::InternalError as i32,
            format!("failed to serialize response: {error}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{did_you_mean, valid_identifier};
    use lsp_server::RequestId;
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch_workspace(label: &str) -> (PathBuf, PathBuf) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock must follow the Unix epoch")
            .as_nanos();
        let sandbox = std::env::temp_dir().join(format!(
            "pinewasm-lsp-{label}-{}-{nonce}",
            std::process::id()
        ));
        let root = sandbox.join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        (sandbox, root)
    }

    fn rename_params(uri: Url) -> RenameParams {
        RenameParams {
            text_document_position: TextDocumentPositionParams::new(
                TextDocumentIdentifier::new(uri),
                Position::new(2, 1),
            ),
            new_name: "renamed".to_string(),
            work_done_progress_params: WorkDoneProgressParams::default(),
        }
    }

    #[test]
    fn language_facade_matches_native_protocol_responses() {
        let fixtures: Vec<Value> =
            serde_json::from_str(include_str!("../fixtures/language_queries.json")).unwrap();
        for fixture in fixtures {
            let request: query::Request =
                serde_json::from_value(fixture["request"].clone()).unwrap();
            let method = match request.capability {
                query::Capability::Hover => "textDocument/hover",
                query::Capability::Definition => "textDocument/definition",
                query::Capability::References => "textDocument/references",
                query::Capability::PrepareRename => "textDocument/prepareRename",
                query::Capability::Rename => "textDocument/rename",
                query::Capability::CodeActions => "textDocument/codeAction",
                query::Capability::Analyze => continue,
            };
            // Invalid UTF-16 offsets are intentionally stricter in the facade.
            if fixture["name"] == "reject-surrogate-position" {
                continue;
            }
            let (sandbox, root) = scratch_workspace(fixture["name"].as_str().unwrap());
            let file = root.join("main.pine");
            std::fs::write(&file, &request.document.source).unwrap();
            for library in &request.libraries {
                std::fs::write(
                    root.join(format!(
                        "{}.pine.local",
                        library.import_path.replace('/', "_")
                    )),
                    &library.source,
                )
                .unwrap();
            }
            let uri = Url::from_file_path(&file).unwrap();
            let workspace = Workspace::new(&root).unwrap();
            let (server, client) = Connection::memory();
            let server_thread = thread::spawn(move || run_connection(server, workspace).unwrap());
            client.sender.send(Message::Notification(Notification::new("textDocument/didOpen".into(), json!({
                "textDocument": { "uri": uri, "languageId": "pine", "version": 1, "text": request.document.source }
            })))).unwrap();
            assert!(matches!(
                client.receiver.recv().unwrap(),
                Message::Notification(_)
            ));
            let params = json!({ "textDocument": { "uri": uri }, "position": request.position,
                "range": request.range, "newName": request.new_name,
                "context": { "includeDeclaration": request.include_declaration, "diagnostics": [] } });
            client
                .sender
                .send(Message::Request(Request::new(
                    RequestId::from(1),
                    method.into(),
                    params,
                )))
                .unwrap();
            let Message::Response(response) = client.receiver.recv().unwrap() else {
                panic!("expected response");
            };
            let facade = query::query(request.clone());
            match (response.response_result, facade) {
                (Ok(native), Ok(facade)) => match request.capability {
                    query::Capability::Hover => {
                        assert_eq!(native["contents"]["value"], facade.data["markdown"]);
                        assert_eq!(native["range"], facade.data["range"]);
                    }
                    query::Capability::Definition => {
                        assert_eq!(native["range"], facade.data["range"]);
                    }
                    query::Capability::References => assert_eq!(
                        native
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|item| &item["range"])
                            .collect::<Vec<_>>(),
                        facade
                            .data
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|item| &item["range"])
                            .collect::<Vec<_>>()
                    ),
                    query::Capability::PrepareRename => assert_eq!(native, facade.data["range"]),
                    query::Capability::Rename => {
                        let index = LineIndex::new(&request.document.source);
                        let edits = native["changes"]
                            .as_object()
                            .unwrap()
                            .values()
                            .next()
                            .unwrap()
                            .as_array()
                            .unwrap();
                        let mapped = edits.iter().map(|edit| {
                            let location: Range = serde_json::from_value(edit["range"].clone()).unwrap();
                            let start = index.byte_offset(location.start) as usize;
                            let end = index.byte_offset(location.end) as usize;
                            json!({ "documentId": request.document.id, "documentVersion": request.document.version,
                                "startUtf16": request.document.source[..start].encode_utf16().count(),
                                "endUtf16": request.document.source[..end].encode_utf16().count(), "text": edit["newText"] })
                        }).collect::<Vec<_>>();
                        assert_eq!(json!(mapped), facade.data["edits"]);
                    }
                    query::Capability::CodeActions => assert_eq!(
                        native
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|item| &item["title"])
                            .collect::<Vec<_>>(),
                        facade
                            .data
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|item| &item["title"])
                            .collect::<Vec<_>>()
                    ),
                    query::Capability::Analyze => unreachable!(),
                },
                (Err(native), Err(facade)) => assert_eq!(native.message, facade.code()),
                (native, facade) => {
                    panic!("parity mismatch {}: {native:?} {facade:?}", fixture["name"])
                }
            }
            client
                .sender
                .send(Message::Request(Request::new(
                    RequestId::from(2),
                    "shutdown".into(),
                    (),
                )))
                .unwrap();
            client
                .sender
                .send(Message::Notification(Notification::new("exit".into(), ())))
                .unwrap();
            let _ = client.receiver.recv().unwrap();
            server_thread.join().unwrap();
            std::fs::remove_dir_all(sandbox).unwrap();
        }
    }

    #[test]
    fn advertises_rich_language_and_inspection_capabilities() {
        let value = initialize_result();
        let capabilities = &value["capabilities"];
        assert_eq!(capabilities["hoverProvider"], true);
        assert_eq!(capabilities["definitionProvider"], true);
        assert!(capabilities["semanticTokensProvider"].is_object());
        let commands = capabilities["executeCommandProvider"]["commands"]
            .as_array()
            .unwrap();
        assert!(commands
            .iter()
            .any(|command| command == "pinewasm.inspectCsil"));
    }

    #[test]
    fn rejects_invalid_rename_targets() {
        assert!(valid_identifier("signal_2"));
        assert!(!valid_identifier("2signal"));
        assert!(!valid_identifier("if"));
        assert!(!valid_identifier("foo.bar"));
        assert_eq!(did_you_mean("did you mean `ta.sma`?"), Some("ta.sma"));
    }

    #[test]
    fn resolution_work_refusal_uses_lsp_request_failed() {
        assert_eq!(
            query_error_response(query::RESOLUTION_WORK_BUDGET_EXCEEDED.to_string()),
            (
                ErrorCode::RequestFailed as i32,
                query::RESOLUTION_WORK_BUDGET_EXCEEDED.to_string()
            )
        );
        assert_eq!(
            query_error_response("invalid rename".to_string()).0,
            ErrorCode::InvalidParams as i32
        );
    }

    #[test]
    fn outside_file_is_analyzable_but_cannot_receive_workspace_edits() {
        let (sandbox, root) = scratch_workspace("outside-edit");
        let outside = sandbox.join("outside.pine");
        let source = "//@version=6\nindicator(\"T\")\nfoo = 1\nbar = foo\n";
        std::fs::write(&outside, source).unwrap();

        let mut state = ServerState::new(Workspace::new(&root).unwrap());
        let uri = Url::from_file_path(&outside).unwrap();
        state.update_document(uri.clone(), 1, source.to_string());

        assert!(state.analysis(&uri).is_some());
        assert!(state.edit_uri(&uri).is_none());
        let error = rename(&state, rename_params(uri)).expect_err("outside edit must fail");
        assert!(error.1.contains("inside the workspace"));

        let _ = std::fs::remove_dir_all(sandbox);
    }

    #[test]
    fn version_5_document_offers_conversion_and_per_site_fixes() {
        let (sandbox, root) = scratch_workspace("migrate");
        let path = root.join("old.pine");
        let source = "//@version=5\nindicator(\"T\")\nx = 5 / 2\nplot(x, offset = bar_index % 2)\n";
        std::fs::write(&path, source).unwrap();
        let mut state = ServerState::new(Workspace::new(&root).unwrap());
        let uri = Url::from_file_path(root.canonicalize().unwrap().join("old.pine")).unwrap();
        state.update_document(uri.clone(), 1, source.to_string());
        let migration = state.documents[&uri]
            .migration
            .as_ref()
            .expect("v5 document is migrated");
        assert!(migration.findings.iter().any(|f| f.rule == "D-2"));

        let params = CodeActionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            range: Range::new(Position::new(2, 4), Position::new(2, 9)),
            context: CodeActionContext::default(),
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        let actions: Vec<CodeActionOrCommand> =
            serde_json::from_value(code_actions(&state, params).unwrap()).unwrap();
        let titles: Vec<String> = actions
            .iter()
            .filter_map(|a| match a {
                CodeActionOrCommand::CodeAction(action) => Some(action.title.clone()),
                CodeActionOrCommand::Command(_) => None,
            })
            .collect();
        assert!(
            titles
                .iter()
                .any(|t| t.starts_with("Convert script to Pine v6 (1 sites need review)")),
            "{titles:?}"
        );
        let fix = actions
            .iter()
            .find_map(|a| match a {
                CodeActionOrCommand::CodeAction(action)
                    if action.title.starts_with("Migrate to v6 [D5]") =>
                {
                    Some(action.clone())
                }
                _ => None,
            })
            .expect("a per-site quick fix for the division");
        let edits = &fix.edit.unwrap().changes.unwrap()[&uri];
        assert_eq!(edits.len(), 1);
        assert!(edits[0].new_text.contains("int(5 / 2)"), "{edits:?}");
        assert!(!edits[0].new_text.contains("@version"), "{edits:?}");

        let _ = std::fs::remove_dir_all(sandbox);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_retargeted_outside_workspace_cannot_receive_edits() {
        use std::os::unix::fs::symlink;

        let (sandbox, root) = scratch_workspace("symlink-edit");
        let source = "//@version=6\nindicator(\"T\")\nfoo = 1\nbar = foo\n";
        let inside = root.join("inside.pine");
        let outside = sandbox.join("outside.pine");
        let alias = root.join("alias.pine");
        std::fs::write(&inside, source).unwrap();
        std::fs::write(&outside, source).unwrap();
        symlink(&inside, &alias).unwrap();

        let mut state = ServerState::new(Workspace::new(&root).unwrap());
        let uri = Url::from_file_path(&alias).unwrap();
        state.update_document(uri.clone(), 1, source.to_string());
        assert!(state.edit_uri(&uri).is_some());

        std::fs::remove_file(&alias).unwrap();
        symlink(&outside, &alias).unwrap();
        assert!(state.edit_uri(&uri).is_none());
        let error = rename(&state, rename_params(uri)).expect_err("escaped symlink edit must fail");
        assert!(error.1.contains("inside the workspace"));

        let _ = std::fs::remove_dir_all(sandbox);
    }

    #[test]
    fn rename_emits_the_canonical_in_workspace_uri() {
        let (sandbox, root) = scratch_workspace("inside-edit");
        let file = root.join("script.pine");
        let source = "//@version=6\nindicator(\"T\")\nfoo = 1\nbar = foo\n";
        std::fs::write(&file, source).unwrap();

        let mut state = ServerState::new(Workspace::new(&root).unwrap());
        let uri = Url::from_file_path(&file).unwrap();
        let canonical_uri = Url::from_file_path(file.canonicalize().unwrap()).unwrap();
        state.update_document(uri.clone(), 1, source.to_string());

        let value = rename(&state, rename_params(uri)).expect("inside rename must succeed");
        let edit: WorkspaceEdit = serde_json::from_value(value).unwrap();
        let changes = edit.changes.unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes.get(&canonical_uri).map(Vec::len), Some(2));

        let _ = std::fs::remove_dir_all(sandbox);
    }

    #[test]
    fn references_honor_include_declaration_context() {
        let (sandbox, root) = scratch_workspace("reference-context");
        let file = root.join("script.pine");
        let source = "x = 1\ny = x\n";
        std::fs::write(&file, source).unwrap();

        let mut state = ServerState::new(Workspace::new(&root).unwrap());
        let uri = Url::from_file_path(&file).unwrap();
        state.update_document(uri.clone(), 1, source.to_string());
        let params = |include_declaration| ReferenceParams {
            text_document_position: TextDocumentPositionParams::new(
                TextDocumentIdentifier::new(uri.clone()),
                Position::new(1, 4),
            ),
            context: ReferenceContext {
                include_declaration,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let without: Vec<Location> =
            serde_json::from_value(references(&state, params(false)).unwrap()).unwrap();
        assert_eq!(without.len(), 1);
        assert_eq!(without[0].range.start, Position::new(1, 4));

        let with: Vec<Location> =
            serde_json::from_value(references(&state, params(true)).unwrap()).unwrap();
        assert_eq!(with.len(), 2);
        assert_eq!(with[0].range.start, Position::new(0, 0));

        let _ = std::fs::remove_dir_all(sandbox);
    }

    #[test]
    fn rename_of_branch_shadow_does_not_edit_outer_binding() {
        let (sandbox, root) = scratch_workspace("branch-shadow-rename");
        let file = root.join("script.pine");
        let source = concat!(
            "x = 1\n",
            "if close > open\n",
            "    x = 2\n",
            "    branch_value = x\n",
            "plot(x)\n",
        );
        std::fs::write(&file, source).unwrap();

        let mut state = ServerState::new(Workspace::new(&root).unwrap());
        let uri = Url::from_file_path(&file).unwrap();
        state.update_document(uri.clone(), 1, source.to_string());
        let params = RenameParams {
            text_document_position: TextDocumentPositionParams::new(
                TextDocumentIdentifier::new(uri),
                Position::new(2, 4),
            ),
            new_name: "inner_x".to_string(),
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let value = rename(&state, params).expect("branch-local rename must succeed");
        let edit: WorkspaceEdit = serde_json::from_value(value).unwrap();
        let edits = edit.changes.unwrap().into_values().next().unwrap();
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0].range.start, Position::new(2, 4));
        assert_eq!(edits[1].range.start, Position::new(3, 19));

        let _ = std::fs::remove_dir_all(sandbox);
    }

    #[test]
    fn rename_rejects_descendant_and_ancestor_capture() {
        let cases = [
            (
                "descendant",
                concat!(
                    "x = 1\n",
                    "if close > open\n",
                    "    z = 2\n",
                    "    branch_value = x\n",
                    "plot(x)\n",
                ),
                Position::new(0, 0),
                "z",
            ),
            (
                "ancestor",
                concat!(
                    "z = 1\n",
                    "if close > open\n",
                    "    x = 2\n",
                    "    inherited_value = z\n",
                    "    local_value = x\n",
                ),
                Position::new(2, 4),
                "z",
            ),
        ];

        for (label, source, position, new_name) in cases {
            let (sandbox, root) = scratch_workspace(label);
            let file = root.join("script.pine");
            std::fs::write(&file, source).unwrap();
            let mut state = ServerState::new(Workspace::new(&root).unwrap());
            let uri = Url::from_file_path(&file).unwrap();
            state.update_document(uri.clone(), 1, source.to_string());
            let params = RenameParams {
                text_document_position: TextDocumentPositionParams::new(
                    TextDocumentIdentifier::new(uri),
                    position,
                ),
                new_name: new_name.to_string(),
                work_done_progress_params: WorkDoneProgressParams::default(),
            };

            let error = rename(&state, params).expect_err("capturing rename must fail closed");
            assert!(error.1.contains("capture or change"), "{label}: {error:?}");
            let _ = std::fs::remove_dir_all(sandbox);
        }
    }

    #[test]
    fn rename_rejects_compiler_reserved_builtin_series_names() {
        let (sandbox, root) = scratch_workspace("builtin-rename");
        let file = root.join("script.pine");
        let source = "x = 1\ny = x\n";
        std::fs::write(&file, source).unwrap();
        let mut state = ServerState::new(Workspace::new(&root).unwrap());
        let uri = Url::from_file_path(&file).unwrap();
        state.update_document(uri.clone(), 1, source.to_string());
        let params = RenameParams {
            text_document_position: TextDocumentPositionParams::new(
                TextDocumentIdentifier::new(uri),
                Position::new(0, 0),
            ),
            new_name: "close".to_string(),
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let error = rename(&state, params).expect_err("built-in rename must fail");
        assert!(error.1.contains("reserved by the compiler"));
        let _ = std::fs::remove_dir_all(sandbox);
    }

    #[test]
    fn rename_rejects_compiler_reserved_function_and_cast_names() {
        let (sandbox, root) = scratch_workspace("builtin-function-rename");
        let file = root.join("script.pine");
        let source = "f() => 1\nx = f()\n";
        std::fs::write(&file, source).unwrap();
        let mut state = ServerState::new(Workspace::new(&root).unwrap());
        let uri = Url::from_file_path(&file).unwrap();
        state.update_document(uri.clone(), 1, source.to_string());

        for new_name in ["plot", "color"] {
            let params = RenameParams {
                text_document_position: TextDocumentPositionParams::new(
                    TextDocumentIdentifier::new(uri.clone()),
                    Position::new(0, 0),
                ),
                new_name: new_name.to_string(),
                work_done_progress_params: WorkDoneProgressParams::default(),
            };

            let error =
                rename(&state, params).expect_err("built-in function and cast renames must fail");
            assert!(
                error.1.contains("built-in callable"),
                "{new_name}: {error:?}"
            );
        }
        let _ = std::fs::remove_dir_all(sandbox);
    }

    #[test]
    fn workspace_symbols_overlay_unsaved_open_documents() {
        let (sandbox, root) = scratch_workspace("workspace-symbol-overlay");
        let file = root.join("script.pine");
        std::fs::write(&file, "disk_only = 1\n").unwrap();

        let mut state = ServerState::new(Workspace::new(&root).unwrap());
        let uri = Url::from_file_path(&file).unwrap();
        state.update_document(uri, 2, "unsaved_only = 1\n".to_string());
        let params = |query: &str| WorkspaceSymbolParams {
            query: query.to_string(),
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let unsaved: Vec<SymbolInformation> =
            serde_json::from_value(workspace_symbols(&state, params("unsaved_only")).unwrap())
                .unwrap();
        assert_eq!(unsaved.len(), 1);
        assert_eq!(unsaved[0].name, "unsaved_only");

        let stale: Vec<SymbolInformation> =
            serde_json::from_value(workspace_symbols(&state, params("disk_only")).unwrap())
                .unwrap();
        assert!(stale.is_empty());

        let all: Vec<SymbolInformation> =
            serde_json::from_value(workspace_symbols(&state, params("")).unwrap()).unwrap();
        assert_eq!(all.len(), 1, "the on-disk revision must not be duplicated");

        let _ = std::fs::remove_dir_all(sandbox);
    }

    #[test]
    fn preserves_secondary_compiler_labels_as_related_information() {
        let analysis = DocumentAnalysis::new("test.pine", "x = 1\n", Vec::new());
        let compiler = CompilerDiagnostic::error("E9999", "duplicate")
            .with_label(Span::new(SourceId::MAIN, 0, 1), "declared again")
            .with_label(Span::new(SourceId::MAIN, 4, 5), "first declared here");
        let uri = Url::parse("file:///tmp/test.pine").unwrap();
        let diagnostic = lsp_diagnostic(&analysis, &uri, &compiler);
        let related = diagnostic.related_information.unwrap();
        assert_eq!(related.len(), 1);
        assert_eq!(related[0].message, "first declared here");
        assert_eq!(related[0].location.uri, uri);
    }

    #[test]
    fn completes_exports_from_a_mapped_pine_library() {
        let root =
            std::env::temp_dir().join(format!("pinewasm-lsp-completion-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("example_Signals_1.pine.local"),
            "//@version=6\nlibrary(\"Signals\")\nexport identity(float value) => value\n",
        )
        .unwrap();
        let mut state = ServerState::new(Workspace::new(&root).unwrap());
        let uri = Url::from_file_path(root.join("import-completion.pine")).unwrap();
        let source =
            "//@version=6\nimport example/Signals/1 as signals\nindicator(\"T\")\nx = signals.";
        state.update_document(uri.clone(), 1, source.to_string());
        let position = state
            .analysis(&uri)
            .unwrap()
            .line_index()
            .position(source.len() as u32);
        let value = completion(
            &state,
            CompletionParams {
                text_document_position: TextDocumentPositionParams::new(
                    TextDocumentIdentifier::new(uri),
                    position,
                ),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
                context: None,
            },
        )
        .unwrap();
        let CompletionResponse::Array(items) = serde_json::from_value(value).unwrap() else {
            panic!("expected completion array");
        };
        assert!(items.iter().any(|item| item.label == "identity"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn client_supplied_libraries_resolve_imports_and_replace_discovered_ones() {
        let root =
            std::env::temp_dir().join(format!("pinewasm-lsp-client-libs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("library/example/Signals")).unwrap();
        std::fs::write(
            root.join("library/example/Signals/1.pine"),
            "//@version=6\nlibrary(\"Signals\")\nexport onDisk(float value) => value\n",
        )
        .unwrap();
        let mut state = ServerState::new(Workspace::new(&root).unwrap());
        let uri = Url::from_file_path(root.join("main.pine")).unwrap();
        let source = "//@version=6\nimport example/Signals/1 as signals\nimport example/Remote/2 as remote\nindicator(\"T\")\nplot(signals.fromClient(close) + remote.cached(close))\n";

        state.update_document(uri.clone(), 1, source.to_string());
        let before = state.analysis(&uri).unwrap().diagnostics().len();
        assert!(
            before > 0,
            "the client-supplied import is unresolved before the client sends its set"
        );

        state.set_client_libraries(vec![
            ClientLibrary {
                import_path: "example/Signals/1".to_string(),
                source:
                    "//@version=6\nlibrary(\"Signals\")\nexport fromClient(float value) => value\n"
                        .to_string(),
            },
            ClientLibrary {
                import_path: "example/Remote/2".to_string(),
                source: "//@version=6\nlibrary(\"Remote\")\nexport cached(float value) => value\n"
                    .to_string(),
            },
            ClientLibrary {
                import_path: "example/Remote/2".to_string(),
                source: "//@version=6\nlibrary(\"Remote\")\nexport cached(float value) => value\n"
                    .to_string(),
            },
            ClientLibrary {
                import_path: "not-a-path".to_string(),
                source: String::new(),
            },
        ]);
        state.reanalyze_all();
        let diagnostics = state.analysis(&uri).unwrap().diagnostics();
        assert!(diagnostics.is_empty(), "{diagnostics:#?}");
        assert_eq!(state.client_libraries.len(), 2);
        let counted: usize = state.client_libraries.values().map(String::len).sum();
        assert!(counted < 300, "a repeated path is counted once");
        // Completion and navigation read the same merged catalog: the cached
        // library has no file, and the replaced one no longer points at disk.
        let (file, remote) = state.library_source("example/Remote/2").unwrap();
        assert!(file.is_none() && remote.contains("cached"));
        let (file, signals) = state.library_source("example/Signals/1").unwrap();
        assert!(file.is_none() && signals.contains("fromClient"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn in_memory_lsp_session_publishes_diagnostics_and_answers_hover() {
        let workspace = Workspace::new(env!("CARGO_MANIFEST_DIR")).unwrap();
        let (server, client) = Connection::memory();
        let server_thread = thread::spawn(move || run_connection(server, workspace).unwrap());
        let uri = Url::parse("file:///tmp/pinewasm-lsp-test.pine").unwrap();
        let source = "//@version=6\nindicator(\"T\")\nx = ta.sma(close, 14)\nplot(x)\n";
        client
            .sender
            .send(Message::Notification(Notification::new(
                "textDocument/didOpen".to_string(),
                DidOpenTextDocumentParams {
                    text_document: TextDocumentItem::new(
                        uri.clone(),
                        "pine".to_string(),
                        1,
                        source.to_string(),
                    ),
                },
            )))
            .unwrap();
        let published = client.receiver.recv().unwrap();
        let Message::Notification(published) = published else {
            panic!("expected diagnostics notification");
        };
        assert_eq!(published.method, "textDocument/publishDiagnostics");
        let diagnostics: PublishDiagnosticsParams =
            serde_json::from_value(published.params).unwrap();
        assert!(
            diagnostics.diagnostics.is_empty(),
            "{:#?}",
            diagnostics.diagnostics
        );

        client
            .sender
            .send(Message::Request(Request::new(
                RequestId::from(1),
                "textDocument/hover".to_string(),
                HoverParams {
                    text_document_position_params: TextDocumentPositionParams::new(
                        TextDocumentIdentifier::new(uri.clone()),
                        Position::new(2, 7),
                    ),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                },
            )))
            .unwrap();
        let Message::Response(response) = client.receiver.recv().unwrap() else {
            panic!("expected hover response");
        };
        let hover = response
            .response_result
            .expect("hover request should succeed");
        assert!(hover.to_string().contains("ta.sma"));

        client
            .sender
            .send(Message::Request(Request::new(
                RequestId::from(2),
                "shutdown".to_string(),
                (),
            )))
            .unwrap();
        client
            .sender
            .send(Message::Notification(Notification::new(
                "exit".to_string(),
                (),
            )))
            .unwrap();
        let Message::Response(shutdown) = client.receiver.recv().unwrap() else {
            panic!("expected shutdown response");
        };
        assert!(shutdown.response_result.is_ok());
        server_thread.join().unwrap();
    }
}
