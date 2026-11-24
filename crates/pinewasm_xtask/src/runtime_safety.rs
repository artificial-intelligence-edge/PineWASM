use crate::command::{output, pinned_tool, remove_rust_tool_overrides, repo_root, run};
use crate::TaskResult;
use proc_macro2::{Delimiter, Span, TokenTree};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::visit::Visit;

const RUNTIME_SOURCE: &str = "crates/pinewasm_aot/runtime/src";
const INVENTORY_DOC: &str = "docs/aot-runtime-safety.md";
const TOOLCHAIN_FILE: &str = ".github/runtime-safety-toolchain.toml";
const MIRI_TARGET: &str = "x86_64-unknown-linux-gnu";
const MIRIFLAGS: &str =
    "-Zmiri-tree-borrows -Zmiri-strict-provenance -Zmiri-symbolic-alignment-check";
const DIRECT_NATIVE_RUNNER: &str = "/usr/bin/env";
const SAFETY_FLAG_OVERRIDES: [&str; 6] = [
    "CARGO_ENCODED_RUSTFLAGS",
    "MIRIFLAGS",
    "MIRI_LIB_SRC",
    "MIRI_SYSROOT",
    "RUSTDOCFLAGS",
    "RUSTFLAGS",
];
const BEGIN: &str = "<!-- BEGIN GENERATED UNSAFE INVENTORY -->";
const END: &str = "<!-- END GENERATED UNSAFE INVENTORY -->";
const UNSUPPORTED_SOURCE: &str = "unsupported external source inclusion";

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Site {
    path: String,
    line: usize,
    column: usize,
    kind: &'static str,
    source: String,
}

struct UnsafeVisitor<'a> {
    path: &'a str,
    source: &'a str,
    sites: Vec<Site>,
}

type MacroFragments = BTreeMap<String, String>;

struct MacroRule {
    transcriber: proc_macro2::TokenStream,
    fragments: MacroFragments,
}

impl UnsafeVisitor<'_> {
    fn record(&mut self, span: Span, kind: &'static str) {
        let start = span.start();
        let source = self
            .source
            .lines()
            .nth(start.line.saturating_sub(1))
            .unwrap_or_default()
            .trim()
            .replace('|', "\\|");
        self.sites.push(Site {
            path: self.path.to_string(),
            line: start.line,
            column: start.column + 1,
            kind,
            source,
        });
    }

    /// `syn` deliberately treats a macro body as opaque tokens. Walk those
    /// tokens as well so source-level unsafe keywords, mutable statics,
    /// foreign blocks, and symbol/linkage attributes inside a `macro_rules!`
    /// definition cannot evade the inventory merely because rustc expands
    /// them later.
    fn record_macro_tokens(&mut self, tokens: proc_macro2::TokenStream) {
        self.record_macro_tokens_with_fragments(tokens, &MacroFragments::new());
    }

    fn record_macro_tokens_with_fragments(
        &mut self,
        tokens: proc_macro2::TokenStream,
        fragments: &MacroFragments,
    ) {
        let tokens = tokens.into_iter().collect::<Vec<_>>();
        for (index, token) in tokens.iter().enumerate() {
            match token {
                TokenTree::Group(group) if macro_group_is_rules_body(&tokens, index) => {
                    self.record_macro_rules_tokens(group.stream(), group.span());
                }
                TokenTree::Group(group) if !macro_group_is_attribute(&tokens, index) => {
                    self.record_macro_tokens_with_fragments(group.stream(), fragments);
                }
                TokenTree::Ident(ident) if ident == "unsafe" => {
                    if let Some(kind) = macro_unsafe_construct_kind(&tokens[index + 1..]) {
                        self.record(ident.span(), kind);
                    }
                }
                TokenTree::Punct(dollar)
                    if dollar.as_char() == '$'
                        && macro_prefix_can_be_unsafe(&tokens[index..], fragments)
                        && !tokens
                            .get(index.wrapping_sub(1))
                            .is_some_and(|previous| token_is_ident(previous, "extern")) =>
                {
                    if let Some(kind) = macro_opaque_qualifier_kind(&tokens[index..], fragments) {
                        self.record(dollar.span(), kind);
                    }
                }
                _ => {}
            }

            if token_is_punct(token, '#') {
                if let Some(group) = macro_attribute_group(&tokens[index + 1..]) {
                    self.record_macro_attribute(group);
                }
            }

            if matches!(token, TokenTree::Ident(ident) if include_identifier(ident))
                && tokens
                    .get(index + 1)
                    .is_some_and(|next| token_is_punct(next, '!'))
            {
                self.record(token.span(), UNSUPPORTED_SOURCE);
            }

            if macro_metavariable_end(&tokens[index..]).is_some_and(|length| {
                tokens
                    .get(index + length)
                    .is_some_and(|next| token_is_punct(next, '!'))
                    && macro_name_can_include_source(&tokens[index..], fragments)
            }) {
                // A supplied function-like macro name can materialize include!
                // even when the file has a non-Rust extension. Source closure
                // cannot be established by recording an ordinary review site.
                self.record(token.span(), UNSUPPORTED_SOURCE);
            }

            if token_is_ident(token, "use") && macro_import_may_include_source(&tokens[index..]) {
                self.record(token.span(), UNSUPPORTED_SOURCE);
            }

            if token_is_ident(token, "static")
                && tokens
                    .get(index + 1)
                    .is_some_and(|next| token_is_ident(next, "mut"))
            {
                self.record(token.span(), "mutable static in macro");
            } else if token_is_ident(token, "static")
                && macro_tokens_start_metavariable(&tokens[index + 1..])
            {
                // A metavariable in this grammar position can expand to
                // `mut`; conservatively retain the expansion point for
                // review because the definition alone cannot disambiguate it
                // from an immutable static's generated name.
                self.record(token.span(), "opaque static mutability in macro");
            }

            if token_is_ident(token, "extern")
                && macro_tokens_start_foreign_block(&tokens[index + 1..])
            {
                self.record(token.span(), "extern block in macro");
            }
        }
    }

    /// Macro matchers describe accepted input; only transcribers can emit an
    /// unsafe boundary. Keep malformed or future rule syntax fail-closed as a
    /// single opaque review site rather than scanning matchers as expansions.
    fn record_macro_rules_tokens(&mut self, tokens: proc_macro2::TokenStream, span: Span) {
        let Some(transcribers) = macro_rule_transcribers(tokens) else {
            self.record(span, "opaque macro_rules definition");
            return;
        };
        for rule in transcribers {
            self.record_macro_tokens_with_fragments(rule.transcriber, &rule.fragments);
        }
    }

    fn record_macro_attribute(&mut self, group: &proc_macro2::Group) {
        // A literal or metavariable path value may keep the attribute opaque
        // to syn. Source redirection must fail closed in either spelling.
        if macro_attribute_may_redirect_source(group.stream()) {
            self.record(group.span(), UNSUPPORTED_SOURCE);
            return;
        }
        let Ok(meta) = syn::parse2::<syn::Meta>(group.stream()) else {
            // A macro metavariable may stand for the entire attribute. Keep
            // that opaque expansion point visible for manual review.
            self.record(group.span(), "opaque attribute in macro");
            return;
        };
        if attribute_path_is(meta.path(), "path") {
            self.record(meta.span(), UNSUPPORTED_SOURCE);
        } else if unsafe_attribute_path(meta.path()) {
            self.record(meta.path().span(), "unsafe attribute in macro");
        } else if attribute_path_is(meta.path(), "cfg_attr") {
            let span = meta.span();
            if let syn::Meta::List(list) = meta {
                self.record_cfg_attr_tokens(
                    list.tokens,
                    span,
                    "conditional unsafe attribute in macro",
                    "opaque conditional attribute in macro",
                );
            }
        }
    }

    /// `cfg_attr` can materialize an unsafe attribute after parsing, including
    /// the pre-2024 spelling of linkage attributes. Parse the attribute list
    /// recursively so those conditional boundaries cannot disappear into the
    /// otherwise-opaque meta token stream.
    fn record_cfg_attr_tokens(
        &mut self,
        tokens: proc_macro2::TokenStream,
        span: Span,
        unsafe_kind: &'static str,
        opaque_kind: &'static str,
    ) {
        let parser = Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated;
        let Ok(items) = parser.parse2(tokens) else {
            // Valid `cfg_attr` syntax is a comma-separated meta list. If syn
            // ever accepts a newer shape opaquely, keep it review-visible
            // instead of silently assuming that shape is safe.
            self.record(span, opaque_kind);
            return;
        };
        for meta in items.iter().skip(1) {
            if attribute_path_is(meta.path(), "path") {
                self.record(meta.span(), UNSUPPORTED_SOURCE);
            } else if unsafe_attribute_path(meta.path()) {
                self.record(meta.span(), unsafe_kind);
            } else if attribute_path_is(meta.path(), "cfg_attr") {
                if let syn::Meta::List(nested) = meta {
                    self.record_cfg_attr_tokens(
                        nested.tokens.clone(),
                        meta.span(),
                        unsafe_kind,
                        opaque_kind,
                    );
                } else {
                    self.record(meta.span(), opaque_kind);
                }
            }
        }
    }
}

fn unsafe_attribute_path(path: &syn::Path) -> bool {
    path.is_ident("unsafe")
        || ["no_mangle", "export_name", "link_section", "naked"]
            .iter()
            .any(|name| attribute_path_is(path, name))
}

fn attribute_path_is(path: &syn::Path, expected: &str) -> bool {
    path.get_ident()
        .is_some_and(|ident| identifier_has_name(ident, expected))
}

/// Raw identifiers preserve resolution-based attribute and macro names.
/// Keep this separate from keyword checks: `r#unsafe` is an ordinary name,
/// not an unsafe grammar qualifier or the `unsafe(...)` attribute wrapper.
fn identifier_has_name(ident: &proc_macro2::Ident, expected: &str) -> bool {
    if ident == expected {
        return true;
    }
    ident
        .to_string()
        .strip_prefix("r#")
        .is_some_and(|name| name == expected)
}

fn token_is_ident(token: &TokenTree, expected: &str) -> bool {
    matches!(token, TokenTree::Ident(ident) if ident == expected)
}

fn token_is_punct(token: &TokenTree, expected: char) -> bool {
    matches!(token, TokenTree::Punct(punct) if punct.as_char() == expected)
}

fn macro_attribute_may_redirect_source(tokens: proc_macro2::TokenStream) -> bool {
    let tokens = tokens.into_iter().collect::<Vec<_>>();
    tokens.windows(2).any(|pair| {
        matches!(&pair[0], TokenTree::Ident(ident) if identifier_has_name(ident, "path"))
            && token_is_punct(&pair[1], '=')
    }) || tokens.iter().any(|token| {
        matches!(token, TokenTree::Group(group) if macro_attribute_may_redirect_source(group.stream()))
    })
}

fn macro_attribute_group(tokens: &[TokenTree]) -> Option<&proc_macro2::Group> {
    match tokens {
        [TokenTree::Group(group), ..] if group.delimiter() == Delimiter::Bracket => Some(group),
        [TokenTree::Punct(bang), TokenTree::Group(group), ..]
            if bang.as_char() == '!' && group.delimiter() == Delimiter::Bracket =>
        {
            Some(group)
        }
        _ => None,
    }
}

fn macro_group_is_attribute(tokens: &[TokenTree], index: usize) -> bool {
    (index >= 1 && token_is_punct(&tokens[index - 1], '#'))
        || (index >= 2
            && token_is_punct(&tokens[index - 2], '#')
            && token_is_punct(&tokens[index - 1], '!'))
}

fn macro_group_is_rules_body(tokens: &[TokenTree], index: usize) -> bool {
    (index >= 3
        && token_is_ident(&tokens[index - 3], "macro_rules")
        && token_is_punct(&tokens[index - 2], '!')
        && matches!(&tokens[index - 1], TokenTree::Ident(_)))
        || (index >= 4
            && token_is_ident(&tokens[index - 4], "macro_rules")
            && token_is_punct(&tokens[index - 3], '!')
            && token_is_punct(&tokens[index - 2], '$')
            && matches!(&tokens[index - 1], TokenTree::Ident(_)))
}

fn macro_rule_transcribers(tokens: proc_macro2::TokenStream) -> Option<Vec<MacroRule>> {
    let tokens = tokens.into_iter().collect::<Vec<_>>();
    let mut transcribers = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        if !matches!(&tokens[index], TokenTree::Group(_))
            || !tokens
                .get(index + 1)
                .is_some_and(|token| token_is_punct(token, '='))
            || !tokens
                .get(index + 2)
                .is_some_and(|token| token_is_punct(token, '>'))
        {
            return None;
        }
        let TokenTree::Group(transcriber) = tokens.get(index + 3)? else {
            return None;
        };
        let TokenTree::Group(matcher) = &tokens[index] else {
            return None;
        };
        let mut fragments = MacroFragments::new();
        collect_macro_fragments(matcher.stream(), &mut fragments);
        transcribers.push(MacroRule {
            transcriber: transcriber.stream(),
            fragments,
        });
        index += 4;
        if index < tokens.len() {
            if !token_is_punct(&tokens[index], ';') {
                return None;
            }
            index += 1;
        }
    }
    (!transcribers.is_empty()).then_some(transcribers)
}

fn collect_macro_fragments(tokens: proc_macro2::TokenStream, fragments: &mut MacroFragments) {
    let tokens = tokens.into_iter().collect::<Vec<_>>();
    for window in tokens.windows(4) {
        if let [TokenTree::Punct(dollar), TokenTree::Ident(name), TokenTree::Punct(colon), TokenTree::Ident(kind)] =
            window
        {
            if dollar.as_char() == '$' && colon.as_char() == ':' {
                fragments.insert(name.to_string(), kind.to_string());
            }
        }
    }
    for token in tokens {
        if let TokenTree::Group(group) = token {
            collect_macro_fragments(group.stream(), fragments);
        }
    }
}

/// Only token-tree/identifier fragments can supply a bare `unsafe` qualifier.
/// Unknown fragments and opaque invocations remain conservative review sites.
fn macro_prefix_can_be_unsafe(tokens: &[TokenTree], fragments: &MacroFragments) -> bool {
    let name = match tokens.get(1) {
        Some(TokenTree::Ident(name)) => Some(name.to_string()),
        Some(TokenTree::Group(group)) => {
            let nested = group.stream().into_iter().collect::<Vec<_>>();
            // Every binding in a repeated prefix matters: a safe visibility
            // fragment may precede an unsafe-capable qualifier. Literal
            // `unsafe` tokens can also join the construct outside the group.
            return nested.iter().enumerate().any(|(index, token)| {
                token_is_ident(token, "unsafe")
                    || (token_is_punct(token, '$')
                        && macro_prefix_can_be_unsafe(&nested[index..], fragments))
            });
        }
        _ => None,
    };
    let kind = name.as_ref().and_then(|name| fragments.get(name));
    !matches!(
        kind.map(String::as_str),
        Some(
            "expr"
                | "expr_2021"
                | "vis"
                | "ty"
                | "block"
                | "pat"
                | "pat_param"
                | "literal"
                | "path"
                | "lifetime"
                | "meta"
                | "stmt"
                | "item"
        )
    )
}

fn macro_unsafe_construct_kind(tokens: &[TokenTree]) -> Option<&'static str> {
    match tokens {
        [TokenTree::Group(group), ..] if group.delimiter() == Delimiter::Brace => {
            Some("unsafe block in macro")
        }
        [TokenTree::Ident(ident), ..] if ident == "fn" => Some("unsafe function in macro"),
        [TokenTree::Ident(ident), ..] if ident == "trait" => Some("unsafe trait in macro"),
        [TokenTree::Ident(ident), ..] if ident == "impl" => Some("unsafe impl in macro"),
        [TokenTree::Ident(ident), ..] if ident == "extern" => Some("unsafe extern in macro"),
        [TokenTree::Ident(ident), ..] if ident == "static" => Some("unsafe static in macro"),
        _ if macro_tokens_start_metavariable(tokens) => Some("opaque unsafe construct in macro"),
        _ => None,
    }
}

fn macro_name_can_include_source(tokens: &[TokenTree], fragments: &MacroFragments) -> bool {
    match tokens.get(1) {
        Some(TokenTree::Ident(name)) => matches!(
            fragments.get(&name.to_string()).map(String::as_str),
            None | Some("ident" | "tt" | "path")
        ),
        Some(TokenTree::Group(group)) => {
            let nested = group.stream().into_iter().collect::<Vec<_>>();
            nested.iter().enumerate().any(|(index, token)| {
                matches!(token, TokenTree::Ident(ident) if include_identifier(ident))
                    || (token_is_punct(token, '$')
                        && macro_name_can_include_source(&nested[index..], fragments))
            })
        }
        _ => false,
    }
}

fn include_identifier(ident: &proc_macro2::Ident) -> bool {
    identifier_has_name(ident, "include")
}

fn import_includes_source(tree: &syn::UseTree) -> bool {
    import_includes_source_at(tree, true)
}

/// `root` is true while the first path segment is inspected. A path rooted at
/// `crate`, `self`, or `super` names an item defined in the scanned crate,
/// whose definition site this inventory also visits, so a first-party
/// function, constant, or macro merely named `include` is not source
/// inclusion. Every other root (`core`, `std`, a global `::` path, or an
/// external crate) keeps failing closed on a terminal `include`.
fn import_includes_source_at(tree: &syn::UseTree, root: bool) -> bool {
    match tree {
        syn::UseTree::Path(path) => {
            if root && ["crate", "self", "super"].iter().any(|k| path.ident == k) {
                return false;
            }
            import_includes_source_at(&path.tree, false)
        }
        syn::UseTree::Name(name) => include_identifier(&name.ident),
        syn::UseTree::Rename(rename) => include_identifier(&rename.ident),
        syn::UseTree::Group(group) => group
            .items
            .iter()
            .any(|item| import_includes_source_at(item, root)),
        syn::UseTree::Glob(_) => false,
    }
}

fn macro_import_may_include_source(tokens: &[TokenTree]) -> bool {
    let Some(end) = tokens.iter().position(|token| token_is_punct(token, ';')) else {
        return true;
    };
    let source = tokens[..=end].iter().cloned().collect();
    match syn::parse2::<syn::ItemUse>(source) {
        Ok(import) => import_includes_source(&import.tree),
        // Macro-supplied imports have no resolved source spelling here. They
        // can alias include! just as a literal import can; require ordinary,
        // reviewable imports rather than treating this as a waivable site.
        Err(_) => true,
    }
}

fn macro_opaque_qualifier_kind(
    tokens: &[TokenTree],
    fragments: &MacroFragments,
) -> Option<&'static str> {
    let mut end = macro_metavariable_end(tokens)?;
    // An optional or empty repetition between a qualifier and its construct
    // disappears on expansion. Keep looking across the whole adjacent prefix
    // sequence instead of losing the earlier unsafe-capable binding.
    while let Some(length) = macro_metavariable_end(&tokens[end..]) {
        end += length;
    }
    match tokens.get(end) {
        Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Brace => {
            Some("opaque unsafe block qualifier in macro")
        }
        Some(TokenTree::Ident(ident)) if ident == "fn" => {
            Some("opaque unsafe function qualifier in macro")
        }
        Some(TokenTree::Ident(ident)) if ident == "trait" => {
            Some("opaque unsafe trait qualifier in macro")
        }
        Some(TokenTree::Ident(ident)) if ident == "impl" => {
            Some("opaque unsafe impl qualifier in macro")
        }
        Some(TokenTree::Ident(ident)) if ident == "extern" => {
            Some("opaque unsafe extern qualifier in macro")
        }
        Some(TokenTree::Ident(ident)) if ident == "static" => {
            Some("opaque unsafe static qualifier in macro")
        }
        Some(TokenTree::Ident(ident)) if ident == "mut" => {
            Some("opaque mutable static keyword in macro")
        }
        Some(TokenTree::Ident(_))
            if matches!(tokens.get(end + 1), Some(TokenTree::Punct(colon)) if colon.as_char() == ':')
                && macro_prefix_token_capacity(&tokens[..end], fragments) >= 2 =>
        {
            // The prefix can supply both `static` and `mut`. Treat a following
            // named declaration conservatively: token scanning cannot prove
            // that the expanded declaration is only a safe local or field.
            Some("opaque static declaration prefix in macro")
        }
        Some(TokenTree::Punct(colon))
            if colon.as_char() == ':'
                && macro_prefix_token_capacity(&tokens[..end], fragments) >= 3 =>
        {
            // The prefix may supply `static`, `mut`, and the binding name.
            // A bare or singly optional field/parameter name cannot do so.
            Some("opaque static declaration prefix in macro")
        }
        _ if macro_tokens_start_foreign_block(&tokens[end..]) => {
            Some("opaque extern keyword in macro")
        }
        _ => None,
    }
}

/// Count possible token positions in an opaque prefix, capped at the three
/// needed for `static mut NAME`. A tt/ident binding occupies one position;
/// typed safe fragments cannot supply these bare tokens and contribute zero.
/// unbounded repetition can supply all three, while `?` retains its body's
/// capacity. This is conservative token inspection, not macro expansion.
fn macro_prefix_token_capacity(tokens: &[TokenTree], fragments: &MacroFragments) -> usize {
    let mut capacity = 0;
    let mut index = 0;
    while index < tokens.len() && capacity < 3 {
        let (length, positions) = if let Some(length) = macro_repetition_end(&tokens[index..]) {
            let TokenTree::Group(body) = &tokens[index + 1] else {
                unreachable!("a recognized repetition has a group body")
            };
            let nested = macro_prefix_token_capacity(
                &body.stream().into_iter().collect::<Vec<_>>(),
                fragments,
            );
            let positions = if token_is_punct(&tokens[index + length - 1], '?') {
                nested
            } else if nested == 0 {
                0
            } else {
                3
            };
            (length, positions)
        } else if let Some(length) = macro_metavariable_end(&tokens[index..]) {
            (
                length,
                usize::from(macro_prefix_can_be_unsafe(&tokens[index..], fragments)),
            )
        } else {
            (1, 1)
        };
        capacity = (capacity + positions).min(3);
        index += length;
    }
    capacity
}

fn macro_tokens_start_metavariable(tokens: &[TokenTree]) -> bool {
    macro_metavariable_end(tokens).is_some()
}

fn macro_metavariable_end(tokens: &[TokenTree]) -> Option<usize> {
    match tokens {
        [TokenTree::Punct(dollar), TokenTree::Ident(ident), ..]
            if dollar.as_char() == '$' && ident != "crate" =>
        {
            Some(2)
        }
        _ => macro_repetition_end(tokens),
    }
}

fn macro_tokens_start_foreign_block(tokens: &[TokenTree]) -> bool {
    match tokens {
        [TokenTree::Group(group), ..] => group.delimiter() == Delimiter::Brace,
        [TokenTree::Literal(abi), TokenTree::Group(group), ..] => {
            syn::parse_str::<syn::LitStr>(&abi.to_string()).is_ok()
                && group.delimiter() == Delimiter::Brace
        }
        [TokenTree::Punct(dollar), TokenTree::Ident(_), TokenTree::Group(group), ..] => {
            dollar.as_char() == '$' && group.delimiter() == Delimiter::Brace
        }
        _ => macro_repetition_end(tokens).is_some_and(|end| {
            tokens
                .get(end)
                .is_some_and(|token| matches!(token, TokenTree::Group(body) if body.delimiter() == Delimiter::Brace))
        }),
    }
}

/// Repetition delimiters disappear during expansion. Preserve the following
/// token even for multi-binding prefixes; classification is deliberately
/// conservative and does not attempt to materialize the expanded sequence.
fn macro_repetition_end(tokens: &[TokenTree]) -> Option<usize> {
    let [TokenTree::Punct(dollar), TokenTree::Group(repetition), rest @ ..] = tokens else {
        return None;
    };
    if dollar.as_char() != '$' || repetition.delimiter() != Delimiter::Parenthesis {
        return None;
    }
    match rest {
        [TokenTree::Punct(operator), ..] if matches!(operator.as_char(), '?' | '*' | '+') => {
            Some(3)
        }
        [separator, TokenTree::Punct(operator), ..]
            if matches!(operator.as_char(), '*' | '+')
                && macro_repetition_separator_is_valid(separator) =>
        {
            Some(4)
        }
        _ => None,
    }
}

fn macro_repetition_separator_is_valid(token: &TokenTree) -> bool {
    match token {
        TokenTree::Group(_) => false,
        TokenTree::Punct(punct) => !matches!(punct.as_char(), '$' | '?' | '*' | '+'),
        TokenTree::Ident(_) | TokenTree::Literal(_) => true,
    }
}

impl<'ast> Visit<'ast> for UnsafeVisitor<'_> {
    fn visit_item_use(&mut self, node: &'ast syn::ItemUse) {
        if import_includes_source(&node.tree) {
            self.record(node.span(), UNSUPPORTED_SOURCE);
        }
        syn::visit::visit_item_use(self, node);
    }

    fn visit_attribute(&mut self, node: &'ast syn::Attribute) {
        if attribute_path_is(node.path(), "path") {
            self.record(node.span(), UNSUPPORTED_SOURCE);
        } else if unsafe_attribute_path(node.path()) {
            self.record(node.span(), "unsafe attribute");
        } else if attribute_path_is(node.path(), "cfg_attr") {
            if let syn::Meta::List(list) = &node.meta {
                self.record_cfg_attr_tokens(
                    list.tokens.clone(),
                    node.span(),
                    "conditional unsafe attribute",
                    "opaque conditional attribute",
                );
            } else {
                self.record(node.span(), "opaque conditional attribute");
            }
        }
        syn::visit::visit_attribute(self, node);
    }

    fn visit_expr_unsafe(&mut self, node: &'ast syn::ExprUnsafe) {
        self.record(node.unsafe_token.span(), "unsafe block");
        syn::visit::visit_expr_unsafe(self, node);
    }

    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        if let Some(token) = &node.sig.unsafety {
            self.record(token.span(), "unsafe function");
        }
        syn::visit::visit_item_fn(self, node);
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        if let Some(token) = &node.sig.unsafety {
            self.record(token.span(), "unsafe method");
        }
        syn::visit::visit_impl_item_fn(self, node);
    }

    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        if let Some(token) = &node.unsafety {
            self.record(token.span(), "unsafe impl");
        }
        syn::visit::visit_item_impl(self, node);
    }

    fn visit_item_trait(&mut self, node: &'ast syn::ItemTrait) {
        if let Some(token) = &node.unsafety {
            self.record(token.span(), "unsafe trait");
        }
        syn::visit::visit_item_trait(self, node);
    }

    fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
        if let Some(token) = &node.sig.unsafety {
            self.record(token.span(), "unsafe trait method");
        }
        syn::visit::visit_trait_item_fn(self, node);
    }

    fn visit_item_static(&mut self, node: &'ast syn::ItemStatic) {
        if let syn::StaticMutability::Mut(token) = &node.mutability {
            self.record(token.span(), "mutable static");
        }
        syn::visit::visit_item_static(self, node);
    }

    fn visit_item_foreign_mod(&mut self, node: &'ast syn::ItemForeignMod) {
        self.record(node.abi.extern_token.span(), "extern block");
        syn::visit::visit_item_foreign_mod(self, node);
    }

    fn visit_foreign_item_fn(&mut self, node: &'ast syn::ForeignItemFn) {
        if let Some(token) = &node.sig.unsafety {
            self.record(token.span(), "unsafe foreign function");
        }
        syn::visit::visit_foreign_item_fn(self, node);
    }

    fn visit_foreign_item_static(&mut self, node: &'ast syn::ForeignItemStatic) {
        if let syn::StaticMutability::Mut(token) = &node.mutability {
            self.record(token.span(), "mutable foreign static");
        }
        syn::visit::visit_foreign_item_static(self, node);
    }

    fn visit_type_bare_fn(&mut self, node: &'ast syn::TypeBareFn) {
        if let Some(token) = &node.unsafety {
            self.record(token.span(), "unsafe bare function type");
        }
        syn::visit::visit_type_bare_fn(self, node);
    }

    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        if node
            .path
            .segments
            .last()
            .is_some_and(|segment| include_identifier(&segment.ident))
        {
            self.record(node.path.span(), UNSUPPORTED_SOURCE);
        }
        if node.path.is_ident("macro_rules") {
            self.record_macro_rules_tokens(node.tokens.clone(), node.path.span());
        } else {
            self.record_macro_tokens(node.tokens.clone());
        }
        syn::visit::visit_macro(self, node);
    }
}

pub(crate) fn check(args: &[String]) -> TaskResult {
    let write = match args {
        [] => false,
        [flag] if flag == "--write" => true,
        _ => return Err("usage: aot-unsafe-inventory [--write]".into()),
    };
    let root = repo_root()?;
    let sites = collect(&root)?;
    let generated = render(&sites);
    let document = root.join(INVENTORY_DOC);
    let source = fs::read_to_string(&document)
        .map_err(|error| format!("cannot read {}: {error}", document.display()))?;
    let updated = replace_generated(&source, &generated)?;
    if write {
        if source != updated {
            fs::write(&document, updated)
                .map_err(|error| format!("cannot write {}: {error}", document.display()))?;
        }
        println!("Wrote {} unsafe sites to {INVENTORY_DOC}.", sites.len());
        return Ok(());
    }
    if source != updated {
        return Err(format!(
            "{INVENTORY_DOC} is stale; run `cargo xtask aot-unsafe-inventory --write`"
        ));
    }
    println!(
        "AOT runtime unsafe inventory is complete and current ({} sites).",
        sites.len()
    );
    Ok(())
}

pub(crate) fn miri(args: &[String]) -> TaskResult {
    no_args("runtime-safety-miri", args)?;
    let root = repo_root()?;
    let toolchain = safety_toolchain(&root)?;
    let mut setup = safety_cargo(&root, &toolchain)?;
    run(setup.args(["miri", "setup", "--target", MIRI_TARGET]))?;
    for cargo_args in [
        vec![
            "miri",
            "test",
            "--locked",
            "--target",
            MIRI_TARGET,
            "-p",
            "pinewasm_num",
            "--features",
            "miri",
            "--test",
            "pinned_baseline",
            "--",
            "--test-threads=1",
        ],
        vec![
            "miri",
            "test",
            "--locked",
            "--target",
            MIRI_TARGET,
            "-p",
            "pinewasm_ta",
            "--features",
            "miri",
            "--test",
            "contract",
            "--",
            "--test-threads=1",
        ],
        vec![
            "miri",
            "test",
            "--locked",
            "--target",
            MIRI_TARGET,
            "-p",
            "pinewasm_broker",
            "--no-default-features",
            "--features",
            "miri",
            "--lib",
            "--",
            "--test-threads=1",
        ],
    ] {
        let mut command = safety_cargo(&root, &toolchain)?;
        run(command
            .env("MIRIFLAGS", MIRIFLAGS)
            .env("CARGO_TARGET_DIR", root.join("target/runtime-safety/miri"))
            .args(cargo_args))?;
    }
    let runtime_manifest = root.join("crates/pinewasm_aot/runtime/Cargo.toml");
    let mut runtime = safety_cargo(&root, &toolchain)?;
    run(runtime
        .env("MIRIFLAGS", MIRIFLAGS)
        .env("CARGO_TARGET_DIR", root.join("target/runtime-safety/miri"))
        .args(["miri", "test", "--manifest-path"])
        .arg(runtime_manifest)
        .args([
            "--features",
            "standalone_wasm,miri",
            "--lib",
            "--locked",
            "--target",
            MIRI_TARGET,
            "--",
            "--test-threads=1",
        ]))?;
    Ok(())
}

pub(crate) fn address_sanitizer(args: &[String]) -> TaskResult {
    no_args("runtime-safety-asan", args)?;
    let root = repo_root()?;
    let toolchain = safety_toolchain(&root)?;
    let host = safety_host(&root, &toolchain)?;
    if !matches!(
        host.as_str(),
        "aarch64-apple-darwin"
            | "aarch64-unknown-linux-gnu"
            | "x86_64-apple-darwin"
            | "x86_64-unknown-linux-gnu"
    ) {
        return Err(format!(
            "AddressSanitizer is not supported by this lane on host target {host}"
        ));
    }
    let target_dir = root.join("target/runtime-safety/address");
    for cargo_args in [
        vec![
            "test",
            "-Zbuild-std",
            "--locked",
            "--target",
            &host,
            "-p",
            "pinewasm_ta",
            "--test",
            "contract",
            "--",
            "--test-threads=1",
        ],
        vec![
            "test",
            "-Zbuild-std",
            "--locked",
            "--target",
            &host,
            "-p",
            "pinewasm_broker",
            "--no-default-features",
            "--lib",
            "--",
            "--test-threads=1",
        ],
    ] {
        let mut command = safety_cargo(&root, &toolchain)?;
        force_direct_native_runner(&mut command, &host);
        run(command
            .env("RUSTFLAGS", "-Zsanitizer=address")
            .env("RUSTDOCFLAGS", "-Zsanitizer=address")
            .env(
                "ASAN_OPTIONS",
                "detect_leaks=1:detect_stack_use_after_return=1:halt_on_error=1",
            )
            .env("CARGO_TARGET_DIR", &target_dir)
            .args(cargo_args))?;
    }
    let runtime_manifest = root.join("crates/pinewasm_aot/runtime/Cargo.toml");
    let mut runtime = safety_cargo(&root, &toolchain)?;
    force_direct_native_runner(&mut runtime, &host);
    run(runtime
        .env("RUSTFLAGS", "-Zsanitizer=address")
        .env("RUSTDOCFLAGS", "-Zsanitizer=address")
        .env(
            "ASAN_OPTIONS",
            "detect_leaks=1:detect_stack_use_after_return=1:halt_on_error=1",
        )
        .env("CARGO_TARGET_DIR", &target_dir)
        .args(["test", "-Zbuild-std", "--manifest-path"])
        .arg(runtime_manifest)
        .args([
            "--features",
            "standalone_wasm",
            "--lib",
            "--locked",
            "--target",
            &host,
            "--",
            "--test-threads=1",
        ]))
}

fn no_args(command: &str, args: &[String]) -> TaskResult {
    if args.is_empty() {
        Ok(())
    } else {
        Err(format!("usage: {command}"))
    }
}

fn safety_toolchain(root: &Path) -> TaskResult<String> {
    let source = fs::read_to_string(root.join(TOOLCHAIN_FILE))
        .map_err(|error| format!("cannot read {TOOLCHAIN_FILE}: {error}"))?;
    source
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once('=')?;
            (key.trim() == "channel").then(|| value.trim().trim_matches('"').to_string())
        })
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{TOOLCHAIN_FILE} has no pinned channel"))
}

fn safety_cargo(root: &Path, toolchain: &str) -> TaskResult<Command> {
    let cargo = pinned_tool(toolchain, "cargo")?;
    let bin = cargo
        .parent()
        .ok_or_else(|| format!("nightly Cargo has no parent: {}", cargo.display()))?;
    let mut paths = vec![bin.to_path_buf()];
    if let Some(inherited) = env::var_os("PATH") {
        paths.extend(env::split_paths(&inherited));
    }
    let path = env::join_paths(paths)
        .map_err(|error| format!("cannot construct runtime-safety PATH: {error}"))?;
    let mut command = Command::new(&cargo);
    command
        .current_dir(root)
        .env("PATH", path)
        .env("RUSTUP_TOOLCHAIN", toolchain)
        .env("CARGO", &cargo);
    remove_rust_tool_overrides(&mut command);
    remove_safety_flag_overrides(&mut command);
    Ok(command)
}

fn remove_safety_flag_overrides(command: &mut Command) {
    // `CARGO_ENCODED_RUSTFLAGS` takes precedence over `RUSTFLAGS`; leaving it
    // inherited could silently remove AddressSanitizer instrumentation. The
    // Miri sysroot/source overrides can likewise bypass the pinned setup.
    for variable in SAFETY_FLAG_OVERRIDES {
        command.env_remove(variable);
    }
}

fn force_direct_native_runner(command: &mut Command, target: &str) {
    // Cargo runner configuration wraps every test executable. Override both
    // inherited environment and configuration-file values with a fixed
    // identity runner so `/bin/true`-style wrappers cannot report tests as
    // passing without executing them. Supported ASan hosts all provide this
    // absolute POSIX path; its absence fails the command closed.
    command.env(cargo_target_runner_variable(target), DIRECT_NATIVE_RUNNER);
}

fn cargo_target_runner_variable(target: &str) -> String {
    let normalized = target
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    format!("CARGO_TARGET_{normalized}_RUNNER")
}

fn safety_host(root: &Path, toolchain: &str) -> TaskResult<String> {
    let rustc = pinned_tool(toolchain, "rustc")?;
    let output = output(Command::new(rustc).current_dir(root).arg("-vV"))?;
    let text = String::from_utf8(output.stdout)
        .map_err(|error| format!("nightly rustc -vV was not UTF-8: {error}"))?;
    text.lines()
        .find_map(|line| line.strip_prefix("host: ").map(ToString::to_string))
        .ok_or_else(|| "nightly rustc -vV did not report a host target".into())
}

fn collect(root: &Path) -> TaskResult<Vec<Site>> {
    let directory = root.join(RUNTIME_SOURCE);
    let mut files = Vec::new();
    collect_rust_files(&directory, &mut files)?;
    files.sort();
    let mut sites = Vec::new();
    for file in files {
        let relative = file
            .strip_prefix(root)
            .map_err(|error| format!("cannot relativize {}: {error}", file.display()))?
            .to_string_lossy()
            .replace('\\', "/");
        let source = fs::read_to_string(&file)
            .map_err(|error| format!("cannot read {}: {error}", file.display()))?;
        sites.extend(source_sites(&relative, &source)?);
    }
    sites.sort();
    Ok(sites)
}

fn source_sites(path: &str, source: &str) -> TaskResult<Vec<Site>> {
    let syntax = syn::parse_file(source)
        .map_err(|error| format!("cannot parse {path} for unsafe inventory: {error}"))?;
    let mut visitor = UnsafeVisitor {
        path,
        source,
        sites: Vec::new(),
    };
    visitor.visit_file(&syntax);
    if let Some(site) = visitor
        .sites
        .iter()
        .find(|site| site.kind == UNSUPPORTED_SOURCE)
    {
        return Err(format!(
            "{}:{}:{}: include! and #[path] are not supported by the runtime source inventory; keep runtime code in ordinary modules under {RUNTIME_SOURCE}",
            site.path, site.line, site.column
        ));
    }
    Ok(visitor.sites)
}

fn collect_rust_files(directory: &Path, files: &mut Vec<PathBuf>) -> TaskResult {
    let entries = fs::read_dir(directory)
        .map_err(|error| format!("cannot read {}: {error}", directory.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("cannot inspect directory entry: {error}"))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
        if file_type.is_dir() {
            collect_rust_files(&path, files)?;
        } else if file_type.is_file() && path.extension().is_some_and(|value| value == "rs") {
            files.push(path);
        }
    }
    Ok(())
}

fn render(sites: &[Site]) -> String {
    let mut output = String::from("| Location | Construct | Source |\n|---|---|---|\n");
    for site in sites {
        output.push_str(&format!(
            "| [`{}:{}:{}`](../{}#L{}) | {} | `{}` |\n",
            site.path,
            site.line,
            site.column,
            site.path,
            site.line,
            site.kind,
            site.source.replace('`', "\\`")
        ));
    }
    output
}

fn replace_generated(source: &str, generated: &str) -> TaskResult<String> {
    let begin = source
        .find(BEGIN)
        .ok_or_else(|| format!("{INVENTORY_DOC} is missing {BEGIN}"))?;
    let content_start = begin + BEGIN.len();
    let end = source[content_start..]
        .find(END)
        .map(|offset| content_start + offset)
        .ok_or_else(|| format!("{INVENTORY_DOC} is missing {END}"))?;
    if source.matches(BEGIN).count() != 1 || source.matches(END).count() != 1 {
        return Err(format!(
            "{INVENTORY_DOC} must contain exactly one generated inventory region"
        ));
    }
    Ok(format!(
        "{}\n{}{}{}",
        &source[..content_start],
        generated,
        END,
        &source[end + END.len()..]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safety_commands_remove_inherited_flag_and_miri_sysroot_overrides() {
        let mut command = Command::new("cargo");
        for variable in SAFETY_FLAG_OVERRIDES {
            command.env(variable, "hostile");
        }
        remove_safety_flag_overrides(&mut command);

        for variable in SAFETY_FLAG_OVERRIDES {
            assert!(
                command
                    .get_envs()
                    .any(|(key, value)| key == variable && value.is_none()),
                "{variable} was not removed"
            );
        }
    }

    #[test]
    fn native_runner_override_executes_the_test_binary_directly() {
        let target = "x86_64-unknown-linux-gnu";
        let variable = cargo_target_runner_variable(target);
        assert_eq!(variable, "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER");

        let mut command = Command::new("cargo");
        command.env(&variable, "/bin/true");
        force_direct_native_runner(&mut command, target);

        assert!(command.get_envs().any(|(key, value)| {
            key == std::ffi::OsStr::new(&variable)
                && value == Some(std::ffi::OsStr::new(DIRECT_NATIVE_RUNNER))
        }));
    }

    #[test]
    fn syntax_inventory_covers_each_explicit_unsafe_boundary() {
        let source = r#"
static mut VALUE: u8 = 0;
extern "C" {
    static FOREIGN: u8;
    static mut FOREIGN_MUT: u8;
    unsafe fn explicitly_unsafe_foreign();
}
unsafe trait Marker { unsafe fn call(); }
type Callback = unsafe extern "C" fn(*mut u8);
struct T;
#[no_mangle]
pub extern "C" fn exported() {}
unsafe impl Marker for T {
    unsafe fn call() { unsafe { VALUE = FOREIGN; } }
}
macro_rules! generated_export {
    ($attribute:meta) => {
        #[no_mangle]
        pub unsafe extern "C" fn generated() {}
        static mut GENERATED_VALUE: u8 = 0;
        static $attribute GENERATED_FROM_META: u8 = 0;
        extern "C" { static GENERATED_FOREIGN: u8; }
        #[cfg_attr(target_arch = "wasm32", link_section = ".generated")]
        #[$attribute]
        fn generated_attributes() {}
    };
}
"#;
        let syntax = syn::parse_file(source).expect("fixture parses");
        let mut visitor = UnsafeVisitor {
            path: "fixture.rs",
            source,
            sites: Vec::new(),
        };
        visitor.visit_file(&syntax);
        let kinds = visitor
            .sites
            .iter()
            .map(|site| site.kind)
            .collect::<Vec<_>>();
        for expected in [
            "mutable static",
            "extern block",
            "mutable foreign static",
            "unsafe foreign function",
            "unsafe trait",
            "unsafe trait method",
            "unsafe bare function type",
            "unsafe impl",
            "unsafe method",
            "unsafe block",
            "unsafe attribute",
            "unsafe attribute in macro",
            "conditional unsafe attribute in macro",
            "opaque attribute in macro",
            "unsafe extern in macro",
            "mutable static in macro",
            "opaque static mutability in macro",
            "extern block in macro",
        ] {
            assert!(kinds.contains(&expected), "missing {expected}: {kinds:?}");
        }
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == "mutable static in macro")
                .count(),
            1
        );
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == "extern block in macro")
                .count(),
            1
        );
    }

    #[test]
    fn macro_inventory_requires_complete_boundary_token_sequences() {
        let source = r#"
macro_rules! identifiers_are_not_boundaries {
    ($static_name:ident, $mutable_name:ident, $external_name:ident) => {
        fn static_mutable_externally() {}
        fn no_mangle(export_name: u8, link_section: u8, naked: u8) {}
        stringify!(unsafe);
        stringify!(no_mangle);
        #[cfg_attr(feature = "no_mangle", inline)]
        pub extern "C" fn abi_function_is_not_a_foreign_block() {}
    };
}
macro_rules! optional_abi_function_is_not_a_foreign_block {
    ($($abi:literal)?) => {
        pub extern $($abi)? fn parameterized_abi_function() {}
    };
}
macro_rules! matcher_tokens_are_not_expansions {
    ($(#[$attribute:meta])* unsafe extern static mut) => {
        fn generated_safe_item() {}
    };
}
macro_rules! crate_metavariable_is_not_a_qualifier {
    () => { $crate::safe_function() };
}
macro_rules! expression_is_not_an_unsafe_qualifier {
    ($condition:expr) => { if $condition { 1u8 } else { 2u8 } };
}
macro_rules! visibility_is_not_an_unsafe_qualifier {
    ($visibility:vis) => { $visibility fn safe_function() {} };
}
"#;
        let syntax = syn::parse_file(source).expect("fixture parses");
        let mut visitor = UnsafeVisitor {
            path: "fixture.rs",
            source,
            sites: Vec::new(),
        };
        visitor.visit_file(&syntax);

        assert!(
            visitor.sites.is_empty(),
            "arbitrary identifiers or extern functions are not inventory sites: {:?}",
            visitor.sites
        );
    }

    #[test]
    fn inventory_rejects_external_source_inclusion_in_every_position() {
        for source in [
            "include!(\"../hidden.in\");",
            "fn nested() { include!(\"../hidden.in\"); }",
            "#[path = \"../hidden.rs\"] mod hidden;",
            "#[cfg_attr(any(), path = \"../hidden.rs\")] mod hidden;",
            "#[cfg_attr(any(), cfg_attr(any(), path = \"../hidden.rs\"))] mod hidden;",
            "macro_rules! external { () => { include!(\"../hidden.in\"); }; }",
            "macro_rules! external { () => { #[path = \"../hidden.rs\"] mod hidden; }; }",
            "macro_rules! external { () => { #[cfg_attr(any(), path = \"../hidden.rs\")] mod hidden; }; }",
            "macro_rules! external { ($file:literal) => { #[path = $file] mod hidden; }; }",
            "macro_rules! external { ($file:literal) => { #[cfg_attr(any(), path = $file)] mod hidden; }; }",
        ] {
            let error = source_sites("fixture.rs", source)
                .expect_err("external code must not silently escape the source inventory");
            assert!(error.contains("not supported by the runtime source inventory"), "{error}");
        }
    }

    #[test]
    fn macro_inventory_scans_every_ast_position_and_parameterized_abis() {
        let source = r#"
fn nested() {
    emit_statement! { static mut LOCAL: u8 = 0; }
    let _ = emit_expression!(unsafe { 1 });
}
trait Generated {
    emit_trait_item! { unsafe fn generated(); }
}
impl Generated for T {
    emit_impl_item! { unsafe fn generated() {} }
}
macro_rules! generated_foreign {
    ($abi:literal) => { extern $abi { static FOREIGN: u8; } };
}
macro_rules! generated_optional_foreign {
    ($($abi:literal)?) => { extern $($abi)? { static OPTIONAL_FOREIGN: u8; } };
}
macro_rules! generated_zero_or_more_foreign {
    ($($abi:literal)*) => { extern $($abi)* { static ZERO_OR_MORE_FOREIGN: u8; } };
}
macro_rules! generated_one_or_more_foreign {
    ($($abi:literal)+) => { extern $($abi)+ { static ONE_OR_MORE_FOREIGN: u8; } };
}
macro_rules! generated_separated_foreign {
    ($($abi:literal),*) => { extern $($abi),* { static SEPARATED_FOREIGN: u8; } };
}
macro_rules! generated_unsafe_function_qualifier {
    ($qualifier:tt) => { $qualifier fn GENERATED_UNSAFE_FUNCTION() {} };
}
macro_rules! generated_unsafe_extern_qualifier {
    ($($qualifier:tt)?) => { $($qualifier)? extern "C" { static GENERATED_EXTERN: u8; } };
}
macro_rules! generated_mutable_static {
    ($mutability:ident) => { static $mutability VALUE: u8 = 0; };
}
macro_rules! generated_optional_mutable_static {
    ($($mutability:ident)?) => { static $($mutability)? OPTIONAL_VALUE: u8 = 0; };
}
"#;
        let syntax = syn::parse_file(source).expect("fixture parses");
        let mut visitor = UnsafeVisitor {
            path: "fixture.rs",
            source,
            sites: Vec::new(),
        };
        visitor.visit_file(&syntax);
        let count = |kind| {
            visitor
                .sites
                .iter()
                .filter(|site| site.kind == kind)
                .count()
        };

        assert_eq!(count("mutable static in macro"), 1);
        assert_eq!(count("opaque static mutability in macro"), 2);
        assert_eq!(count("extern block in macro"), 6);
        assert_eq!(count("unsafe block in macro"), 1);
        assert_eq!(count("unsafe function in macro"), 2);
        assert_eq!(count("opaque unsafe function qualifier in macro"), 1);
        assert_eq!(count("opaque unsafe extern qualifier in macro"), 1);
    }

    #[test]
    fn nested_repetition_qualifiers_remain_inventory_sites() {
        for (tail, expected) in [
            (
                "fn generated() {}",
                "opaque unsafe function qualifier in macro",
            ),
            (
                "trait Generated {}",
                "opaque unsafe trait qualifier in macro",
            ),
            (
                "impl Marker for T {}",
                "opaque unsafe impl qualifier in macro",
            ),
            ("extern \"C\" {}", "opaque unsafe extern qualifier in macro"),
            (
                "static FOREIGN: u8;",
                "opaque unsafe static qualifier in macro",
            ),
            ("{ 1u8 }", "opaque unsafe block qualifier in macro"),
        ] {
            for qualifier in ["$($($q)*)*", "$($($q)+)+", "$($($q)?),*", "$($($($q)*)*)*"] {
                let source = format!(
                    "macro_rules! generated {{ ($(($($q:tt)*)),*) => {{ {qualifier} {tail} }}; }}"
                );
                let sites = source_sites("nested.rs", &source).expect("fixture parses");
                assert!(
                    sites.iter().any(|site| site.kind == expected),
                    "nested qualifier {qualifier} before {tail} escaped the inventory: {sites:?}"
                );
            }
        }
        for (source, expected) in [
            (
                "macro_rules! mutable { ($(($($q:ident)*)),*) => { static $($($q)*)* VALUE: u8 = 0; }; }",
                "opaque static mutability in macro",
            ),
            (
                "macro_rules! foreign { ($(($($abi:literal)*)),*) => { extern $($($abi)*)* { fn foreign(); } }; }",
                "extern block in macro",
            ),
        ] {
            let sites = source_sites("nested_prefix.rs", source).expect("fixture parses");
            assert!(
                sites.iter().any(|site| site.kind == expected),
                "nested repetition after a boundary prefix escaped the inventory: {sites:?}"
            );
        }

        let safe = r"
macro_rules! safe_visibility {
    ($([$(($visibility:vis))*]),*) => { $($($visibility)*)* fn safe_function() {} };
}
macro_rules! safe_expression {
    ($(($($condition:expr)*)),*) => { if $($($condition)*)* { 1u8 } else { 2u8 } };
}
macro_rules! safe_mixed_expression {
    ($(($first:expr, $second:expr)),*) => { if $($first && $second)* { 1u8 } else { 2u8 } };
}
";
        assert!(
            source_sites("safe_nested.rs", safe)
                .expect("safe fixture parses")
                .is_empty(),
            "nested safe fragment kinds must retain their non-qualifier meaning"
        );
    }

    #[test]
    fn nested_repetition_regression_compiles_to_an_unsafe_function() {
        let source = r"
macro_rules! make {
    ($(($($q:tt)*)),*) => { $($($q)*)* fn generated() {} };
}
make!((unsafe));
macro_rules! safe_visibility {
    ($([$(($visibility:vis))*]),*) => { $($($visibility)*)* fn safe_function() {} };
}
safe_visibility!([(pub)]);
macro_rules! safe_expression {
    ($(($($condition:expr)*)),*) => { if $($($condition)*)* { 1u8 } else { 2u8 } };
}
pub fn safe_expression_control() -> u8 { safe_expression!((true)) }
macro_rules! safe_mixed_expression {
    ($(($first:expr, $second:expr)),*) => { if $($first && $second)* { 1u8 } else { 2u8 } };
}
pub fn safe_mixed_expression_control() -> u8 { safe_mixed_expression!((true, false)) }
";
        assert_compiled_unsafe_function_is_inventoried(source);
    }

    #[test]
    fn mixed_repetition_regressions_compile_to_unsafe_functions() {
        for source in [
            r"
macro_rules! make {
    ($(($qualifier:tt $visibility:vis,)),*) => { $($visibility $qualifier)* fn generated() {} };
}
make!((unsafe pub,));
",
            r"
macro_rules! make {
    ($(($visibility:vis,)),*) => { $($visibility unsafe)* fn generated() {} };
}
make!((pub,));
",
            r"
macro_rules! make {
    ($qualifier:tt $(, $optional:literal)?) => { $qualifier $($optional)? fn generated() {} };
}
make!(unsafe);
",
        ] {
            assert_compiled_unsafe_function_is_inventoried(source);
        }
    }

    #[test]
    fn macro_supplied_foreign_block_keyword_is_inventoried() {
        let source = r#"
macro_rules! make {
    ($keyword:ident) => { $keyword "C" { static FOREIGN: u8; } };
}
make!(extern);
"#;
        assert_compiled_boundary_is_inventoried(
            source,
            "let _ = FOREIGN;",
            "opaque extern keyword in macro",
        );
    }

    #[test]
    fn macro_supplied_mutable_static_keywords_are_inventoried() {
        for (source, expected_kind) in [
            (
                r"
macro_rules! make {
    ($keyword:ident) => { pub $keyword mut VALUE: u8 = 0; };
}
make!(static);
",
                "opaque mutable static keyword in macro",
            ),
            (
                r"
macro_rules! make {
    ($keyword:ident, $mutability:ident) => { pub $keyword $mutability VALUE: u8 = 0; };
}
make!(static, mut);
",
                "opaque static declaration prefix in macro",
            ),
            (
                r"
macro_rules! make {
    ($(($keyword:ident, $mutability:ident)),*) => { pub $($keyword $mutability)* VALUE: u8 = 0; };
}
make!((static, mut));
",
                "opaque static declaration prefix in macro",
            ),
        ] {
            assert_compiled_boundary_is_inventoried(source, "let _ = VALUE;", expected_kind);
        }
    }

    #[test]
    fn macro_supplied_mutable_static_names_are_inventoried() {
        let safe = r"
macro_rules! safe_names {
    ($visibility:vis, $field:ident, $argument:ident) => {
        pub struct Named { $visibility $field: u8 }
        pub fn identity($argument: u8) -> u8 { $argument }
    };
}
safe_names!(pub, field, argument);
macro_rules! safe_optional_name {
    ($($field:ident)?) => { pub struct Optional { pub $($field)?: u8 } };
}
safe_optional_name!(field);
";
        assert!(
            source_sites("safe_names.rs", safe)
                .expect("safe name fixture parses")
                .is_empty(),
            "bare field/parameter names and visibility must not imply a mutable static"
        );
        for source in [
            r"
macro_rules! make {
    ($keyword:ident, $mutability:ident, $name:ident) => {
        pub $keyword $mutability $name: u8 = 0;
    };
}
make!(static, mut, VALUE);
",
            r"
macro_rules! make {
    ($($prefix:ident),*) => { pub $($prefix)*: u8 = 0; };
}
make!(static, mut, VALUE);
",
            r"
macro_rules! make {
    ($(($keyword:ident, $mutability:ident, $name:ident))?) => {
        pub $($keyword $mutability $name)?: u8 = 0;
    };
}
make!((static, mut, VALUE));
",
        ] {
            assert_compiled_boundary_is_inventoried(
                &format!("{source}\n{safe}"),
                "let _ = VALUE;",
                "opaque static declaration prefix in macro",
            );
        }
    }

    #[test]
    fn safe_visibility_cannot_supply_a_mutable_static_prefix() {
        for source in [
            r"
macro_rules! make {
    ($keyword:ident, $visibility:vis, $name:ident) => {
        $visibility $keyword $name: u8 = 0;
    };
}
make!(static, , VALUE);
",
            r"
macro_rules! make {
    ($(($keyword:ident, $visibility:vis, $name:ident))?) => {
        $($visibility $keyword $name)?: u8 = 0;
    };
}
make!((static, , VALUE));
",
            r"
macro_rules! make {
    ($keyword:ident, $visibility:vis,) => { $visibility $keyword VALUE: u8 = 0; };
}
make!(static, ,);
",
        ] {
            assert_compiled_control(source, "let _ = VALUE;", None, false);
            assert!(
                source_sites("safe_static.rs", source)
                    .expect("safe static fixture parses")
                    .is_empty(),
                "a visibility fragment cannot supply static, mut, or a binding name: {source}"
            );
        }
    }

    #[test]
    fn macro_supplied_include_names_fail_closed() {
        for source in [
            r#"
macro_rules! load { ($name:ident) => { $name!("hidden.in"); }; }
load!(include);
"#,
            r#"
macro_rules! load { ($name:tt) => { $name!("hidden.in"); }; }
load!(include);
"#,
            r#"
macro_rules! load { ($($name:ident)?) => { $($name)?!("hidden.in"); }; }
load!(include);
"#,
            r#"
macro_rules! load { ($(($($name:ident)*)),*) => { $($($name)*)*!("hidden.in"); }; }
load!((include));
"#,
        ] {
            assert_compiled_control(source, "hidden();", Some("pub unsafe fn hidden() {}"), true);
            let error = source_sites("dynamic_include.rs", source)
                .expect_err("a supplied include! name must not hide external source");
            assert!(
                error.contains("not supported by the runtime source inventory"),
                "{error}"
            );
        }
    }

    #[test]
    fn aliased_source_inclusion_is_rejected() {
        let ordinary_imports = r"
#![no_std]
use core::mem::{self as memory, size_of};
macro_rules! imported_identity {
    () => {
        use core::convert::identity as copy_value;
        pub fn safe_import_control() -> u8 {
            let _ = memory::size_of::<u8>() + size_of::<u8>();
            copy_value(1u8)
        }
    };
}
imported_identity!();
";
        assert_compiled_control(
            ordinary_imports,
            "let _ = safe_import_control();",
            None,
            false,
        );
        assert!(
            source_sites("ordinary_imports.rs", ordinary_imports)
                .expect("ordinary imports parse")
                .is_empty(),
            "ordinary explicit and literal macro-generated imports must remain supported"
        );
        for source in [
            r#"
#![no_std]
use core::include as load;
load!("hidden.in");
"#,
            r#"
#![no_std]
use core::{include as first};
use first as load;
load!("hidden.in");
"#,
            r#"
#![no_std]
use core::r#include as load;
load!("hidden.in");
"#,
            r#"
#![no_std]
macro_rules! load {
    () => { use core::include as imported; imported!("hidden.in"); };
}
load!();
"#,
            r#"
#![no_std]
macro_rules! load {
    ($name:ident) => { use core::$name as imported; imported!("hidden.in"); };
}
load!(include);
"#,
            r#"
#![no_std]
macro_rules! load { () => { core::r#include!("hidden.in"); }; }
load!();
"#,
        ] {
            assert_compiled_control(source, "hidden();", Some("pub unsafe fn hidden() {}"), true);
            let error = source_sites("aliased_include.rs", source)
                .expect_err("an imported include! alias must not hide external source");
            assert!(
                error.contains("not supported by the runtime source inventory"),
                "{error}"
            );
        }
    }

    #[test]
    fn first_party_items_named_include_remain_supported() {
        let source = r"
#![no_std]
mod helpers {
    pub fn include() -> u8 { 1 }
    pub const INCLUDE: u8 = 2;
}
use crate::helpers::include as load;
use self::helpers::{include as again, INCLUDE};
pub fn total() -> u8 { load() + again() + INCLUDE }
";
        assert!(
            source_sites("first_party_include.rs", source)
                .expect("first-party items named include parse")
                .is_empty(),
            "a crate-rooted path to a first-party item named include is not source inclusion"
        );
        for source in [
            "#![no_std]\nuse ::core::include as load;\nload!(\"hidden.in\");\n",
            "#![no_std]\nuse std::include as load;\nload!(\"hidden.in\");\n",
        ] {
            assert!(
                source_sites("external_include.rs", source).is_err(),
                "a non-first-party terminal include must keep failing closed: {source}"
            );
        }
    }

    #[test]
    fn raw_attribute_identifiers_preserve_source_and_linkage_boundaries() {
        for source in [
            r#"#![no_std]
#[r#path = "hidden.in"] mod loaded;
"#,
            r#"#![no_std]
#[r#cfg_attr(all(), r#path = "hidden.in")] mod loaded;
"#,
            r#"#![no_std]
macro_rules! load { () => { #[r#path = "hidden.in"] mod loaded; }; }
load!();
"#,
            r#"#![no_std]
macro_rules! load {
    ($file:literal) => { #[r#cfg_attr(all(), r#path = $file)] mod loaded; };
}
load!("hidden.in");
"#,
        ] {
            assert_compiled_control(
                source,
                "loaded::hidden();",
                Some("pub unsafe fn hidden() {}"),
                true,
            );
            let error = source_sites("raw_path.rs", source)
                .expect_err("raw attributes must not hide external source");
            assert!(
                error.contains("not supported by the runtime source inventory"),
                "{error}"
            );
        }

        let safe = r"
macro_rules! ordinary_raw_names {
    () => {
        fn r#unsafe() {}
        fn r#no_mangle() {}
        fn r#path() {}
        pub fn safe_control() { r#unsafe(); r#no_mangle(); r#path(); }
    };
}
ordinary_raw_names!();
#[r#cfg_attr(all(), inline)] pub fn safe_attribute() {}
";
        assert_compiled_control(safe, "safe_control(); safe_attribute();", None, false);
        assert!(
            source_sites("safe_raw_names.rs", safe)
                .expect("safe raw names parse")
                .is_empty(),
            "raw names must not become unsafe grammar keywords or linkage attributes"
        );

        let link_attribute = if cfg!(target_vendor = "apple") {
            r#"#[r#link_section = "__TEXT,__text"] pub fn exposed() {}"#
        } else {
            r#"#[r#link_section = ".text"] pub fn exposed() {}"#
        };
        for (source, expected_kind) in [
            (r"#[r#no_mangle] pub fn exposed() {}", "unsafe attribute"),
            (
                r#"#[r#export_name = "exported"] pub fn exposed() {}"#,
                "unsafe attribute",
            ),
            (link_attribute, "unsafe attribute"),
            (
                r"#[unsafe(r#no_mangle)] pub fn exposed() {}",
                "unsafe attribute",
            ),
            (
                r"#[r#cfg_attr(all(), r#no_mangle)] pub fn exposed() {}",
                "conditional unsafe attribute",
            ),
            (
                r"macro_rules! item { () => { #[r#cfg_attr(all(), r#no_mangle)] pub fn exposed() {} }; } item!();",
                "conditional unsafe attribute in macro",
            ),
        ] {
            assert_compiled_control(source, "exposed();", None, false);
            let sites = source_sites("raw_linkage.rs", source).expect("raw attribute parses");
            assert!(
                sites.iter().any(|site| site.kind == expected_kind),
                "raw attribute escaped its boundary: {source}; {sites:?}"
            );
        }
    }

    fn assert_compiled_unsafe_function_is_inventoried(source: &str) {
        assert_compiled_boundary_is_inventoried(
            source,
            "generated();",
            "opaque unsafe function qualifier in macro",
        );
    }

    fn assert_compiled_boundary_is_inventoried(
        source: &str,
        unsafe_control: &str,
        expected_kind: &str,
    ) {
        assert_compiled_control(source, unsafe_control, None, true);
        let sites = source_sites("fixture.rs", source).expect("fixture parses");
        assert!(
            sites.iter().any(|site| site.kind == expected_kind),
            "the real compiler expansion must have an inventory review site: {sites:?}"
        );
    }

    fn assert_compiled_control(
        source: &str,
        control_body: &str,
        included_source: Option<&str>,
        requires_unsafe: bool,
    ) {
        let fixture = crate::command::temp_dir("unsafe-qualifier-repetition")
            .expect("temporary compiler fixture");
        if let Some(included) = included_source {
            fs::write(fixture.path().join("hidden.in"), included).expect("write included source");
        }
        let path = fixture.path().join("fixture.rs");
        fs::write(&path, source).expect("write compiler fixture");
        let root = repo_root().expect("repository root");
        let toolchain = crate::command::pinned_toolchain(&root).expect("pinned toolchain");
        let rustc = pinned_tool(&toolchain, "rustc").expect("pinned rustc");
        let compile = || {
            Command::new(&rustc)
                .current_dir(fixture.path())
                .args([
                    "--edition=2021",
                    "--crate-type=lib",
                    "--emit=metadata",
                    "--error-format=json",
                ])
                .arg(&path)
                .output()
                .expect("run fixture compiler")
        };
        let valid = compile();
        assert!(
            valid.status.success(),
            "reported boundary-generating macro must compile: {}",
            String::from_utf8_lossy(&valid.stderr)
        );

        fs::write(
            &path,
            format!("{source}\nfn operation_control() {{ {control_body} }}"),
        )
        .expect("write operation control");
        let control = compile();
        if requires_unsafe {
            assert!(
                !control.status.success()
                    && String::from_utf8_lossy(&control.stderr).contains("\"code\":\"E0133\""),
                "the control must prove the expansion requires unsafe: {}",
                String::from_utf8_lossy(&control.stderr)
            );
        } else {
            assert!(
                control.status.success(),
                "the control must prove ordinary safe access compiles: {}",
                String::from_utf8_lossy(&control.stderr)
            );
        }
    }

    #[test]
    fn macro_rules_inventory_scans_only_transcribers_in_every_arm() {
        let source = r"
macro_rules! generated_boundaries {
    ($(#[$matcher_attribute:meta])* unsafe extern static mut) => {
        #[no_mangle]
        fn generated_export() {}
    };
    ($safe:ident) => {
        static mut GENERATED: u8 = 0;
        unsafe static GENERATED_FOREIGN: u8;
    };
}
macro_rules! outer {
    () => {
        macro_rules! nested {
            ($(#[$matcher_attribute:meta])* unsafe extern static mut) => {
                unsafe fn generated_nested() {}
            };
        }
    };
}
";
        let syntax = syn::parse_file(source).expect("fixture parses");
        let mut visitor = UnsafeVisitor {
            path: "fixture.rs",
            source,
            sites: Vec::new(),
        };
        visitor.visit_file(&syntax);
        let kinds = visitor
            .sites
            .iter()
            .map(|site| site.kind)
            .collect::<Vec<_>>();

        assert_eq!(
            kinds,
            [
                "unsafe attribute in macro",
                "mutable static in macro",
                "unsafe static in macro",
                "unsafe function in macro",
            ]
        );
    }

    #[test]
    fn unrecognized_macro_rules_grammar_is_fail_closed() {
        let mut visitor = UnsafeVisitor {
            path: "fixture.rs",
            source: "macro_rules! future {}",
            sites: Vec::new(),
        };

        visitor.record_macro_rules_tokens(proc_macro2::TokenStream::new(), Span::call_site());

        assert_eq!(visitor.sites.len(), 1);
        assert_eq!(visitor.sites[0].kind, "opaque macro_rules definition");
    }

    #[test]
    fn conditional_unsafe_attributes_are_inventory_sites_not_cfg_predicates() {
        let source = r#"
#[cfg_attr(target_arch = "wasm32", no_mangle)]
fn legacy_conditional_export() {}
#[cfg_attr(target_arch = "wasm32", unsafe(export_name = "conditional_export"))]
fn wrapped_conditional_export() {}
#[cfg_attr(
    target_arch = "wasm32",
    cfg_attr(feature = "special-section", link_section = ".special")
)]
static CONDITIONAL_SECTION: u8 = 0;
#[cfg_attr(feature = "no_mangle", inline)]
fn predicate_name_is_not_an_attribute() {}
#[cfg_attr(target_arch = "wasm32", project::no_mangle)]
fn qualified_lookalike_is_not_builtin() {}
"#;
        let syntax = syn::parse_file(source).expect("fixture parses");
        let mut visitor = UnsafeVisitor {
            path: "fixture.rs",
            source,
            sites: Vec::new(),
        };
        visitor.visit_file(&syntax);

        assert_eq!(
            visitor
                .sites
                .iter()
                .filter(|site| site.kind == "conditional unsafe attribute")
                .count(),
            3,
            "only expanded built-in unsafe attributes are inventory sites: {:?}",
            visitor.sites
        );
        assert_eq!(visitor.sites.len(), 3);
    }

    #[test]
    fn generated_region_replacement_is_exact_and_bounded() {
        let source = format!("before\n{BEGIN}\nstale\n{END}\nafter\n");
        let updated = replace_generated(&source, "fresh\n").expect("replace");
        assert_eq!(updated, format!("before\n{BEGIN}\nfresh\n{END}\nafter\n"));
    }
}
