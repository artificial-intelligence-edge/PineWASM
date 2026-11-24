//! Recursive-descent parser with a Pratt expression core.
//!
//! Error strategy: report a structured diagnostic, then resynchronize at the
//! next statement boundary (Newline/Dedent). The parser always returns a
//! `Program` — possibly partial. Malformed input is expected to produce
//! diagnostics; a panic is a defect rather than part of the error contract.

use pinewasm_span::{codes, Diagnostic, Diagnostics, SourceId, Span};
use pinewasm_syntax::{
    AnnotatedType, AnnotationKind, BinaryOp, CallArg, EnumDecl, EnumVariant, Expr, FieldType,
    FieldTypeKind, ForInStmt, ForStmt, FuncDef, FuncParam, Ident, IfExpr, IfStmt, ImportDecl,
    IndicatorDecl, Item, LibraryDecl, OnceStmt, ParamQualifier, Program, Stmt, StrategyDecl,
    SwitchArm, SwitchStmt, Token, TokenKind, TypeAnnotation, TypeDecl, TypeField, TypeQualifier,
    UnaryOp, VersionDirective, WhileStmt,
};

use crate::lexer::{lex_with_source_id, MAX_SOURCE_LEN};
use pratt::PrattHost as _;

/// Hard cap on expression nesting depth so adversarial input (e.g. ten
/// thousand `(`) cannot overflow the parser's call stack.
///
/// The same number bounds the height of the expression tree the parser
/// builds. Recursion depth alone does not: a left-associative operator chain
/// (`a + a + a ...`) and a postfix chain (`a.b().c() ...`, `a[1][1] ...`) are
/// parsed by loops that wrap the tree built so far, one level per step,
/// without recursing. Every later pass walks the tree recursively (semantic
/// analysis, lowering, serialization, drop), so an unbounded chain overflows
/// their stack instead of the parser's. The chain loops therefore count the
/// levels they add (see `Parser::expr_reach`) and refuse an expression whose
/// tree would be taller than this cap.
pub(crate) const MAX_EXPR_DEPTH: u32 = 200;

/// Hard cap on statement-block nesting depth. The `MAX_EXPR_DEPTH` guard only
/// bounds the *expression* axis; `if`/`for`/`while`/`switch` bodies recurse
/// through `parse_block` on a separate axis with no such bound, so a script of
/// deeply nested blocks could overflow the native call stack here (aborting the
/// process) or in any downstream pass that walks the tree (semantics/MIR/CSIL).
/// Capping at parse time transitively protects every later stage, exactly as
/// `MAX_EXPR_DEPTH` does for expressions.
const MAX_BLOCK_DEPTH: u32 = 200;

/// Hard cap on how deeply a declared type nests collection type arguments
/// (`array<array<...>>`, `map<string, matrix<...>>`). Types are a third
/// recursion axis beside expressions and blocks: every `<` re-enters
/// `parse_field_type`, and the passes that follow walk the resulting
/// [`FieldType`] recursively (resolution, description, drop). Capping at parse
/// time bounds all of them, as `MAX_EXPR_DEPTH` and `MAX_BLOCK_DEPTH` do on
/// their axes. A collection of collections is already rare, so the cap is far
/// above any type a script declares.
pub(crate) const MAX_TYPE_DEPTH: u32 = 200;

/// The most tokens one source may lex to and still be parsed.
///
/// The parser allocates in proportion to its tokens: the tree it builds has
/// a bounded number of nodes per token. Bytes alone do not fix that number,
/// because the lexer adds `Newline`, `Indent` and `Dedent` tokens that no
/// byte spells, so a source at the size cap can lex to more tokens than it
/// has bytes. The budget is one token per byte of the largest source. Written
/// code spends several bytes on a token, so only a source that is one-byte
/// tokens almost from end to end reaches it, and that source is refused with
/// `E0001` before any of it is parsed.
pub const MAX_TOKENS: usize = MAX_SOURCE_LEN;

pub struct ParseResult {
    pub program: Program,
    pub diagnostics: Vec<Diagnostic>,
    /// Exact contextual Once headers, including inside expression-owned
    /// blocks. Their tokens remain identifiers in ordinary name positions.
    pub once_keyword_spans: Vec<Span>,
}

/// Parse `source`, attributing every span to `SourceId::MAIN` (the
/// single-source entry point).
pub fn parse(source: &str) -> ParseResult {
    parse_with_source_id(source, SourceId::MAIN)
}

/// Parse `source`, attributing every token/diagnostic span to `source_id`. An
/// imported library is parsed under its own distinct source id so its parse and
/// (downstream) analysis diagnostics land on the library buffer, not clamped
/// onto the main source (see docs/browser.md).
///
/// A source longer than [`MAX_SOURCE_LEN`] bytes, or one that lexes to more
/// than [`MAX_TOKENS`] tokens, is not parsed: the result is an empty program
/// and one `E0001` diagnostic.
///
/// The diagnostics, the lexer's and the parser's together, hold at most
/// [`pinewasm_span::MAX_ERRORS`] errors and [`pinewasm_span::MAX_WARNINGS`]
/// warnings. When there are more, an `E0002` or a `W0007` after them says
/// that the rest are left out; the program is the same either way.
pub fn parse_with_source_id(source: &str, source_id: SourceId) -> ParseResult {
    parse_into(source, source_id, Diagnostics::new())
}

/// [`parse_with_source_id`], reporting into `diagnostics`. A test passes a
/// list with other limits to check that nothing but the list depends on
/// them.
pub(crate) fn parse_into(
    source: &str,
    source_id: SourceId,
    mut diagnostics: Diagnostics,
) -> ParseResult {
    // The lexer returns an oversized source unread, as its end-of-input token
    // and the `E0001` that says so; that parses to an empty program below.
    let mut lexed = lex_with_source_id(source, source_id);
    if lexed.tokens.len() > MAX_TOKENS {
        return ParseResult {
            program: Program {
                version: None,
                strategy_alert_message: None,
                items: Vec::new(),
                span: Span::new(source_id, 0, 0),
            },
            diagnostics: vec![Diagnostic::error(
                codes::E0001,
                format!(
                    "source has {} tokens; the maximum supported count is {MAX_TOKENS}",
                    lexed.tokens.len()
                ),
            )],
            once_keyword_spans: Vec::new(),
        };
    }
    demote_contextual_method(&mut lexed.tokens);
    // The lexer's list may end in its own `E0002`; reported into this one it
    // counts as errors left out, and the list says so once, at the end.
    diagnostics.extend(lexed.diagnostics);
    let mut parser = Parser::new(source, lexed.tokens, diagnostics);
    let program = parser.parse_program();
    ParseResult {
        program,
        diagnostics: parser.diagnostics.finish(),
        once_keyword_spans: parser.once_keyword_spans,
    }
}

/// `method` is a keyword only where it opens a method definition
/// (`method name(`, optionally after `export`; see [`is_method_name_kind`]).
/// Everywhere else PineWASM treats it as an ordinary name
/// (`method = 1`, a parameter called `method`), so retype
/// those tokens as identifiers before parsing.
fn demote_contextual_method(tokens: &mut [Token]) {
    for i in 0..tokens.len() {
        if tokens[i].kind != TokenKind::KwMethod {
            continue;
        }
        let next = i + 1;
        let opens_def = if next < tokens.len() {
            let next_kind = tokens[next].kind;
            if is_method_name_kind(next_kind) {
                let after = i + 2;
                if after < tokens.len() {
                    tokens[after].kind == TokenKind::LParen
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        };
        if !opens_def {
            tokens[i].kind = TokenKind::Ident;
        }
    }
}

/// Token kinds that can name a method: an identifier, or a type keyword
/// (as in `method float(int id)` or `method type(...)`).
fn is_method_name_kind(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Ident
            | TokenKind::KwType
            | TokenKind::KwInt
            | TokenKind::KwFloat
            | TokenKind::KwBool
            | TokenKind::KwString
    )
}

/// What [`Parser::try_parse_named_annotated_stmt`] found at a statement start.
enum NamedDecl {
    /// Not a type-annotated declaration. Nothing was consumed or reported, so
    /// the caller goes on to the other statement forms.
    Absent,
    /// A declaration that parsed.
    Parsed(Stmt),
    /// A declaration that was reported and skipped. The cursor is already
    /// past it, so the caller must not try another statement form on what
    /// follows.
    Skipped,
}

struct Parser<'s> {
    source: &'s str,
    tokens: Vec<Token>,
    pos: usize,
    diagnostics: Diagnostics,
    expr_depth: u32,
    /// How deep the expressions parsed since this was last zeroed reach: the
    /// largest `expr_depth` they were parsed at, raised by a chain loop to
    /// cover the levels it added by wrapping. A sub-expression that starts at
    /// depth `d` is at most `max(expr_reach, d) - d + 1` nodes tall, because
    /// every level of the tree is either one recursion level or one counted
    /// wrap. The chain loops zero it around each operand to measure that
    /// operand alone (`spine_begin`, `spine_height`, `spine_end`).
    expr_reach: u32,
    /// Current statement-block nesting depth, bounded by `MAX_BLOCK_DEPTH`.
    block_depth: u32,
    /// Number of collection type-argument lists (`array<`, `matrix<`, `map<`)
    /// the type being parsed is currently inside, bounded by `MAX_TYPE_DEPTH`.
    type_depth: u32,
    /// Set when a declared type was refused for nesting past
    /// `MAX_TYPE_DEPTH`. The speculative type parse at statement start
    /// (`try_parse_named_annotated_stmt`) clears it before parsing and reads
    /// it afterwards, so that this refusal stands instead of being retried
    /// as an expression.
    type_too_deep: bool,
    /// Set true when the most recently parsed expression ended with an
    /// indented block (an `if`/`switch` expression), whose closing `Dedent`
    /// already terminated the logical line. `expect_end_of_stmt` consumes it
    /// so it does not then demand a `Newline` that the block already passed.
    last_expr_block_terminated: bool,
    /// Set true when the most recent simple statement ended with a `,`
    /// separator rather than a line end: another statement follows on the
    /// same logical line (`a = 1, b = 2`). Inline bodies (`f() => a = 1, a`,
    /// a switch arm's `=> x := 1, y := 2`) read it to keep collecting
    /// statements; block and top-level loops simply parse the next one.
    stmt_continues_on_line: bool,
    /// Checked against the resolved script version after all directives have
    /// been parsed; PineWASM permits the version annotation to follow code.
    once_keyword_spans: Vec<Span>,
}

impl<'s> Parser<'s> {
    /// A parser at the first of `tokens`, reporting into `diagnostics` (which
    /// already holds what the lexer reported).
    fn new(source: &'s str, tokens: Vec<Token>, diagnostics: Diagnostics) -> Self {
        Parser {
            source,
            tokens,
            pos: 0,
            diagnostics,
            expr_depth: 0,
            expr_reach: 0,
            block_depth: 0,
            type_depth: 0,
            type_too_deep: false,
            last_expr_block_terminated: false,
            stmt_continues_on_line: false,
            once_keyword_spans: Vec::new(),
        }
    }

    // -- token access -------------------------------------------------------

    fn peek(&self) -> Token {
        self.tokens[self.pos.min(self.tokens.len() - 1)]
    }

    fn peek_kind(&self) -> TokenKind {
        self.peek().kind
    }

    /// The kind of the token `n` positions ahead of the cursor (0 == current).
    fn nth_kind(&self, n: usize) -> TokenKind {
        self.tokens[(self.pos + n).min(self.tokens.len() - 1)].kind
    }

    fn bump(&mut self) -> Token {
        let t = self.peek();
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
        t
    }

    // Pine type words are contextual: declarations use them as types, while
    // variable and parameter positions may use the same spelling as a name.
    fn is_ident_kind(kind: TokenKind) -> bool {
        matches!(
            kind,
            TokenKind::Ident
                | TokenKind::KwType
                | TokenKind::KwInt
                | TokenKind::KwFloat
                | TokenKind::KwBool
                | TokenKind::KwString
        )
    }

    fn at(&self, kind: TokenKind) -> bool {
        self.peek_kind() == kind
            || (kind == TokenKind::Ident && Self::is_ident_kind(self.peek_kind()))
    }

    fn eat(&mut self, kind: TokenKind) -> Option<Token> {
        if self.at(kind) {
            Some(self.bump())
        } else {
            None
        }
    }

    /// Consume one namespace segment after a `.`: a plain identifier, or a
    /// type keyword (`int`/`float`/`bool`/`string`/`type`) that doubles as a
    /// member name (`input.int`, `syminfo.type`, ...). Returns the consumed token so
    /// the caller can read its source text; `None` (no consumption) otherwise.
    fn eat_namespace_segment(&mut self) -> Option<Token> {
        if matches!(
            self.peek_kind(),
            TokenKind::Ident
                | TokenKind::KwInt
                | TokenKind::KwFloat
                | TokenKind::KwBool
                | TokenKind::KwString
                // `enum` doubles as the member name in `input.enum`.
                | TokenKind::KwEnum
                // `type` doubles as the member name in `syminfo.type`.
                | TokenKind::KwType
        ) {
            Some(self.bump())
        } else {
            None
        }
    }

    fn expect(&mut self, kind: TokenKind) -> Option<Token> {
        if let Some(t) = self.eat(kind) {
            return Some(t);
        }
        let found = self.peek();
        // Error tokens were already diagnosed by the lexer; a second
        // "expected X, found invalid token" at the same span is noise.
        if found.kind != TokenKind::Error {
            self.error(
                found.span,
                format!(
                    "expected {}, found {}",
                    kind.describe(),
                    found.kind.describe()
                ),
            );
        }
        None
    }

    fn error(&mut self, span: Span, message: impl Into<String>) {
        self.diagnostics
            .push(Diagnostic::error(codes::E0110, message).with_label(span, "unexpected here"));
    }

    fn text(&self, token: Token) -> &'s str {
        token.text(self.source)
    }

    /// Skip to the start of the next statement (past the next Newline, or to
    /// a Dedent/Eof), consuming the tokens of the broken statement.
    fn synchronize(&mut self) {
        loop {
            match self.peek_kind() {
                TokenKind::Newline => {
                    self.bump();
                    // An indented block after the broken line belongs to the
                    // broken statement; consume it too, or it would surface
                    // as a phantom "unexpected indentation" error.
                    if self.at(TokenKind::Indent) {
                        self.skip_block();
                    }
                    return;
                }
                TokenKind::Dedent | TokenKind::Eof => return,
                // Skip nested blocks wholesale so we do not resync into the
                // middle of a block belonging to the broken statement.
                TokenKind::Indent => {
                    self.skip_block();
                }
                _ => {
                    self.bump();
                }
            }
        }
    }

    fn skip_block(&mut self) {
        debug_assert!(self.at(TokenKind::Indent));
        self.bump();
        let mut depth = 1u32;
        loop {
            match self.peek_kind() {
                TokenKind::Indent => {
                    depth += 1;
                    self.bump();
                }
                TokenKind::Dedent => {
                    depth -= 1;
                    self.bump();
                    if depth == 0 {
                        return;
                    }
                }
                TokenKind::Eof => return,
                _ => {
                    self.bump();
                }
            }
        }
    }

    // -- grammar ------------------------------------------------------------

    fn parse_program(&mut self) -> Program {
        let start_span = self.peek().span;
        let mut version = None;
        let mut strategy_alert_message = None;
        let mut items = Vec::new();

        while !self.at(TokenKind::Eof) {
            match self.peek_kind() {
                TokenKind::Newline => {
                    self.bump();
                }
                TokenKind::AlertMessageDirective => {
                    // `//@strategy_alert_message=...` — capture the rest-of-line
                    // message text as strategy metadata (last one wins). It may
                    // appear anywhere at top level.
                    let t = self.bump();
                    let text = self.text(t);
                    if let Some(msg) = text.strip_prefix("//@strategy_alert_message=") {
                        strategy_alert_message = Some(msg.trim().to_string());
                    }
                    self.eat(TokenKind::Newline);
                }
                TokenKind::VersionDirective => {
                    let t = self.bump();
                    // Compiler annotations may follow code, including an
                    // import. The version belongs to the complete script.
                    if version.is_some() {
                        // PineWASM accepts a repeated directive with warning
                        // W0005; the first one decides the version.
                        self.diagnostics.push(
                            Diagnostic::warning(codes::W0005, "duplicate version directive")
                                .with_label(t.span, "ignored; the first directive applies"),
                        );
                    } else {
                        version = self.parse_version_directive(t);
                    }
                    self.eat(TokenKind::Newline);
                }
                TokenKind::Indent => {
                    let t = self.bump();
                    self.error(t.span, "unexpected indentation at top level".to_string());
                    // Consume the stray block to keep going.
                    let mut depth = 1u32;
                    while depth > 0 && !self.at(TokenKind::Eof) {
                        match self.bump().kind {
                            TokenKind::Indent => depth += 1,
                            TokenKind::Dedent => depth -= 1,
                            _ => {}
                        }
                    }
                }
                TokenKind::Dedent => {
                    // Produced only after an Indent error path; ignore.
                    self.bump();
                }
                _ => {
                    if let Some(item) = self.parse_item() {
                        items.push(item);
                    }
                }
            }
        }

        if version.as_ref().is_some_and(|v| v.version < 6) {
            if let Some(span) = self.once_keyword_spans.first().copied() {
                self.error(span, "`once` blocks require version 6");
            }
        }
        let end_span = self.peek().span;
        Program {
            version,
            strategy_alert_message,
            items,
            span: start_span.to(end_span),
        }
    }

    fn parse_version_directive(&mut self, token: Token) -> Option<VersionDirective> {
        let text = self.text(token);
        let value = text.strip_prefix("//@version=").and_then(|rest| {
            let digits: String = rest
                .trim()
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if digits.len() == rest.trim().len() && !digits.is_empty() {
                digits.parse::<u32>().ok()
            } else {
                None
            }
        });
        match value {
            Some(version) => Some(VersionDirective {
                version,
                span: token.span,
            }),
            None => {
                self.error(
                    token.span,
                    "malformed version directive; expected `//@version=6`",
                );
                None
            }
        }
    }

    fn parse_item(&mut self) -> Option<Item> {
        // `export` prefixes a function/method/type/enum definition in a library
        // script. It is a contextual identifier (not a reserved keyword),
        // so it only takes effect when directly followed by a definition; a bare
        // `export` (or `export = 1`) falls through to ordinary statement parsing.
        if self.at(TokenKind::Ident)
            && self.text(self.peek()) == "export"
            && self.export_followed_by_def()
        {
            let export_kw = self.bump(); // `export`
            return self.parse_exported_item(export_kw.span);
        }
        // A function definition (`name(params) => body`) is a top-level item.
        // Detected by the `=>` that follows the balanced parameter list, which
        // distinguishes it from a call statement (`f(x)`) or `indicator(...)`.
        if self.is_func_def_ahead() {
            return self.parse_func_def(false).map(Item::FuncDef);
        }
        // `enum Name` at top level is an enum declaration.
        if self.at(TokenKind::KwEnum) {
            return self.parse_enum_def(false).map(Item::EnumDef);
        }
        // `type Name` at top level is a user-defined type declaration.
        if self.at(TokenKind::KwType) && Self::is_ident_kind(self.nth_kind(1)) {
            return self.parse_type_def(false).map(Item::TypeDef);
        }
        // `method f(...) => ...` at top level is a user-defined method
        // declaration (a function with a typed receiver first parameter).
        if self.at(TokenKind::KwMethod) {
            return self.parse_method_def(false).map(Item::FuncDef);
        }
        // `indicator(...)` at top level is a declaration item. Only when the
        // name is *called* (`indicator(`) — an `indicator.foo` member access is
        // an ordinary statement, not a declaration.
        let next_is_lparen =
            self.tokens.get(self.pos + 1).map(|t| t.kind) == Some(TokenKind::LParen);
        if self.at(TokenKind::Ident) && self.text(self.peek()) == "indicator" && next_is_lparen {
            return self.parse_indicator_decl().map(Item::Indicator);
        }
        // `strategy(...)` at top level is a declaration item. A
        // `strategy.entry(...)`-style order primitive (name followed by `.`) is
        // an ordinary statement, so require a `(` here.
        if self.at(TokenKind::Ident) && self.text(self.peek()) == "strategy" && next_is_lparen {
            return self.parse_strategy_decl().map(Item::Strategy);
        }
        // `library("Title", overlay?)` at top level is a declaration item,
        // recognized exactly like `indicator(...)`: the name *called*.
        if self.at(TokenKind::Ident) && self.text(self.peek()) == "library" && next_is_lparen {
            return self.parse_library_decl().map(Item::Library);
        }
        // `import Publisher/LibName/Version as alias`: a contextual
        // statement-leading identifier followed by an identifier (the publisher).
        // A bare `import` used as a variable falls through to statement parsing.
        if self.at(TokenKind::Ident)
            && self.text(self.peek()) == "import"
            && self.nth_kind(1) == TokenKind::Ident
        {
            return self.parse_import_decl().map(Item::Import);
        }
        self.parse_stmt().map(Item::Stmt)
    }

    /// True when an `export` at the current position is immediately followed by
    /// a definition it can modify (a `name(...) =>` function, a `method`, a
    /// `type`, or an `enum`). Used to keep `export` a contextual identifier: a
    /// bare `export`/`export = 1` is an ordinary statement.
    fn export_followed_by_def(&self) -> bool {
        match self.tokens.get(self.pos + 1).map(|t| t.kind) {
            Some(TokenKind::KwMethod | TokenKind::KwType | TokenKind::KwEnum) => true,
            Some(TokenKind::Ident) => {
                // `export name(...) =>` — a function definition after the
                // `export` token. Reuse the func-def lookahead one position on.
                self.is_func_def_ahead_at(self.pos + 1)
            }
            _ => false,
        }
    }

    /// Parse the definition following an `export` modifier (its span begins at
    /// `export_span`). `export` is legal only in a library script; the analyzer
    /// enforces that (`E0243`), so the parser accepts it structurally here.
    fn parse_exported_item(&mut self, export_span: Span) -> Option<Item> {
        if self.at(TokenKind::KwEnum) {
            return self.parse_enum_def(true).map(|mut d| {
                d.span = export_span.to(d.span);
                Item::EnumDef(d)
            });
        }
        if self.at(TokenKind::KwType) && Self::is_ident_kind(self.nth_kind(1)) {
            return self.parse_type_def(true).map(|mut d| {
                d.span = export_span.to(d.span);
                Item::TypeDef(d)
            });
        }
        if self.at(TokenKind::KwMethod) {
            return self.parse_method_def(true).map(|mut d| {
                d.span = export_span.to(d.span);
                Item::FuncDef(d)
            });
        }
        // Otherwise a function definition (the lookahead guaranteed one).
        self.parse_func_def(true).map(|mut d| {
            d.span = export_span.to(d.span);
            Item::FuncDef(d)
        })
    }

    /// Peek for a function-definition shape at the current position:
    /// `IDENT ( ... ) =>`, where `( ... )` is a balanced parenthesized list.
    /// Used to tell a definition from a call. The scan is bounded by the
    /// token stream length, so it never loops unboundedly.
    fn is_func_def_ahead(&self) -> bool {
        self.is_func_def_ahead_at(self.pos)
    }

    /// Like [`Self::is_func_def_ahead`] but anchored at an arbitrary token
    /// position `start` (used to look one token past an `export` modifier).
    fn is_func_def_ahead_at(&self, start: usize) -> bool {
        if !self
            .tokens
            .get(start)
            .is_some_and(|t| Self::is_ident_kind(t.kind))
        {
            return false;
        }
        if self.tokens.get(start + 1).map(|t| t.kind) != Some(TokenKind::LParen) {
            return false;
        }
        // Skip the balanced parameter list.
        let mut i = start + 2;
        let mut depth = 1u32;
        while let Some(tok) = self.tokens.get(i) {
            match tok.kind {
                TokenKind::LParen => depth += 1,
                TokenKind::RParen => {
                    depth -= 1;
                    if depth == 0 {
                        i += 1;
                        break;
                    }
                }
                // A newline/EOF inside the parens means this is not a
                // single-line definition header; bail out. Bailing on Newline
                // (not just Eof) also bounds this lookahead to the current
                // logical line: without it, an unclosed `IDENT(` makes every
                // statement rescan the rest of the file, which is O(n^2) over
                // a source full of such lines (mirrors `is_destructure_ahead`).
                TokenKind::Newline | TokenKind::Eof => return false,
                _ => {}
            }
            i += 1;
        }
        if depth != 0 {
            return false;
        }
        self.tokens.get(i).map(|t| t.kind) == Some(TokenKind::FatArrow)
    }

    /// True when `text` is one of Pine's type qualifiers. Contextual: the
    /// words stay ordinary identifiers everywhere else, and a qualifier in a
    /// parameter position is retained on the AST.
    fn is_type_qualifier(text: &str) -> bool {
        matches!(text, "series" | "simple" | "const" | "input")
    }

    /// True when the tokens at `at` begin a declared parameter type: a scalar
    /// type keyword, or an identifier that is itself followed by another
    /// identifier (`FeatureSeries fs`, `color c`), a generic argument list
    /// (`array<float> xs`, `matrix<float> xs`, `map<string, float> xs`), a dotted segment (`chart.point p`), or the `[]`
    /// array suffix (`int[] xs`).
    fn starts_param_type(&self, at: usize) -> bool {
        let Some(first) = self.tokens.get(at) else {
            return false;
        };
        match first.kind {
            TokenKind::KwInt | TokenKind::KwFloat | TokenKind::KwBool | TokenKind::KwString => {
                if self
                    .tokens
                    .get(at + 1)
                    .is_some_and(|t| Self::is_ident_kind(t.kind))
                {
                    return true;
                }
                self.tokens.get(at + 1).map(|t| t.kind) == Some(TokenKind::LBracket)
                    && self.tokens.get(at + 2).map(|t| t.kind) == Some(TokenKind::RBracket)
                    && self
                        .tokens
                        .get(at + 3)
                        .is_some_and(|t| Self::is_ident_kind(t.kind))
            }
            TokenKind::Ident | TokenKind::KwType => {
                let base = self.text(*first);
                let mut end = at + 1;
                while self.tokens.get(end).map(|t| t.kind) == Some(TokenKind::Dot)
                    && self
                        .tokens
                        .get(end + 1)
                        .is_some_and(|t| t.kind == TokenKind::Ident)
                {
                    end += 2;
                }
                if self.tokens.get(end).map(|t| t.kind) == Some(TokenKind::Lt)
                    && matches!(base, "array" | "matrix" | "map")
                {
                    return true;
                }
                if self.tokens.get(end).map(|t| t.kind) == Some(TokenKind::LBracket)
                    && self.tokens.get(end + 1).map(|t| t.kind) == Some(TokenKind::RBracket)
                {
                    end += 2;
                }
                self.tokens
                    .get(end)
                    .is_some_and(|t| Self::is_ident_kind(t.kind))
            }
            _ => false,
        }
    }

    /// Parse one function/method parameter: `[qualifier] [Type] IDENT
    /// [= default]`. The optional qualifier (`series`/`simple`/`const`/
    /// `input`) is consumed only when a type or a name still follows, so a
    /// parameter actually *named* `series` (`f(series)`) keeps working. On a
    /// malformed parameter this reports, resynchronizes, and returns `None`.
    fn parse_param(&mut self) -> Option<FuncParam> {
        let lead_span = self.peek().span;
        // Optional qualifier.
        let qualifier = if self.at(TokenKind::Ident)
            && Self::is_type_qualifier(self.text(self.peek()))
            && (self.starts_param_type(self.pos + 1) || self.nth_kind(1) == TokenKind::Ident)
        {
            let token = self.bump();
            Some(match self.text(token) {
                "const" => ParamQualifier::Const,
                "input" => ParamQualifier::Input,
                "simple" => ParamQualifier::Simple,
                "series" => ParamQualifier::Series,
                _ => unreachable!(),
            })
        } else {
            None
        };
        // Optional declared type.
        let ty = if self.starts_param_type(self.pos) {
            Some(self.parse_field_type()?)
        } else {
            None
        };
        let param_token = match self.expect(TokenKind::Ident) {
            Some(t) => t,
            None => {
                self.synchronize();
                return None;
            }
        };
        let param_name = Ident {
            name: self.text(param_token).to_string(),
            span: param_token.span,
        };
        let mut param_span = lead_span.to(param_token.span);
        let default = if self.eat(TokenKind::Eq).is_some() {
            match self.parse_expr() {
                Some(e) => {
                    param_span = param_span.to(e.span());
                    Some(e)
                }
                None => {
                    self.synchronize();
                    return None;
                }
            }
        } else {
            None
        };
        Some(FuncParam {
            name: param_name,
            qualifier,
            ty,
            default,
            span: param_span,
        })
    }

    /// True when the `[` at the current position opens a destructuring
    /// pattern rather than a tuple-expression statement: scan to the matching
    /// `]` (tracking bracket depth) and check for a `=` (not `:=`) directly
    /// after it. A `[a, b] = ...` is a destructure; a bare `[a, b]` (a
    /// function body's return value) is an expression statement.
    fn is_destructure_ahead(&self) -> bool {
        debug_assert_eq!(self.peek_kind(), TokenKind::LBracket);
        let mut i = self.pos + 1;
        let mut depth = 1u32;
        while let Some(tok) = self.tokens.get(i) {
            match tok.kind {
                TokenKind::LBracket => depth += 1,
                TokenKind::RBracket => {
                    depth -= 1;
                    if depth == 0 {
                        i += 1;
                        break;
                    }
                }
                // A newline/EOF before the pattern closes means this is not a
                // single-line destructuring header.
                TokenKind::Newline | TokenKind::Eof => return false,
                _ => {}
            }
            i += 1;
        }
        if depth != 0 {
            return false;
        }
        self.tokens.get(i).map(|t| t.kind) == Some(TokenKind::Eq)
    }

    /// Parse a user-defined function definition: `name(p1, p2 = default) =>`
    /// followed by a single inline statement (single-line body) or a
    /// NEWLINE + indented block. The caller has confirmed the
    /// `IDENT ( ... ) =>` shape via [`Self::is_func_def_ahead`].
    fn parse_func_def(&mut self, exported: bool) -> Option<FuncDef> {
        let name_token = self.bump(); // the function name
        let name = Ident {
            name: self.text(name_token).to_string(),
            span: name_token.span,
        };
        self.bump(); // `(`

        // Parameter list: comma-separated `[qualifier] [Type] IDENT [= default]`.
        let mut params = Vec::new();
        let mut param_failed = false;
        if !self.at(TokenKind::RParen) {
            loop {
                match self.parse_param() {
                    Some(param) => {
                        params.push(param);
                        if self.eat(TokenKind::Comma).is_none() {
                            break;
                        }
                    }
                    None => {
                        param_failed = true;
                        break;
                    }
                }
            }
        }
        if param_failed {
            return None;
        }
        if self.expect(TokenKind::RParen).is_none() {
            self.synchronize();
            return None;
        }
        if self.expect(TokenKind::FatArrow).is_none() {
            self.synchronize();
            return None;
        }

        // Body: one or more comma-separated inline statements, or a
        // NEWLINE + indented block.
        let body = if self.at(TokenKind::Newline) {
            self.bump();
            self.parse_block()?
        } else {
            self.parse_inline_stmts()?
        };
        let end_span = body.last().map_or(name_token.span, Stmt::span);
        Some(FuncDef {
            name,
            params,
            body,
            receiver: None,
            exported,
            span: name_token.span.to(end_span),
        })
    }

    /// Parse a user-defined method definition:
    /// `method f(TypeName this, p2, ...) => body`. The `method` keyword is at
    /// the current position. Identical to a function definition except the
    /// first parameter carries an explicit receiver type (`FieldType`), which
    /// is recorded on `FuncDef::receiver`; the first parameter is otherwise a
    /// normal parameter (its name binds the receiver inside the body). A method
    /// with no parameters is a parse error (there is no receiver to type).
    fn parse_method_def(&mut self, exported: bool) -> Option<FuncDef> {
        let method_kw = self.bump(); // `method`
        let name_token = if is_method_name_kind(self.peek_kind()) {
            self.bump()
        } else {
            self.expect(TokenKind::Ident);
            self.synchronize();
            return None;
        };
        let name = Ident {
            name: self.text(name_token).to_string(),
            span: name_token.span,
        };
        if self.expect(TokenKind::LParen).is_none() {
            self.synchronize();
            return None;
        }

        // First parameter: the receiver, `[qualifier] TypeName this
        // [= default]` (or `array<T> this`, etc.). It parses like any
        // parameter; its declared type moves to `FuncDef::receiver`.
        let receiver_start = self.peek().span;
        let mut receiver = self.parse_param()?;
        let Some(receiver_ty) = receiver.ty.take() else {
            self.error(
                receiver_start.to(receiver.span),
                "a method's first parameter must declare its receiver type (`method f(Type this)`)",
            );
            self.synchronize();
            return None;
        };
        let mut params = vec![receiver];

        // Remaining parameters: comma-separated `[qualifier] [Type] IDENT
        // [= default]`, exactly as in a plain function definition.
        let mut param_failed = false;
        while self.eat(TokenKind::Comma).is_some() {
            match self.parse_param() {
                Some(param) => params.push(param),
                None => {
                    param_failed = true;
                    break;
                }
            }
        }
        if param_failed {
            return None;
        }
        if self.expect(TokenKind::RParen).is_none() {
            self.synchronize();
            return None;
        }
        if self.expect(TokenKind::FatArrow).is_none() {
            self.synchronize();
            return None;
        }

        // Body: one or more comma-separated inline statements, or a
        // NEWLINE + indented block.
        let body = if self.at(TokenKind::Newline) {
            self.bump();
            self.parse_block()?
        } else {
            self.parse_inline_stmts()?
        };
        let end_span = body.last().map_or(name_token.span, Stmt::span);
        Some(FuncDef {
            name,
            params,
            body,
            receiver: Some(receiver_ty),
            exported,
            span: method_kw.span.to(end_span),
        })
    }

    /// Parse an enum declaration: `enum Name` NEWLINE INDENT member* DEDENT,
    /// where each member is `IDENT [= "title"]` on its own line. The caller has
    /// confirmed the current token is `enum`. An empty member list is a parse
    /// error (`enum Name` with no members is meaningless); duplicate member
    /// names are a semantic check (`E0239`), not a parse error.
    fn parse_enum_def(&mut self, exported: bool) -> Option<EnumDecl> {
        let enum_token = self.bump(); // `enum`
        let name_token = match self.expect(TokenKind::Ident) {
            Some(t) => t,
            None => {
                self.synchronize();
                return None;
            }
        };
        let name = Ident {
            name: self.text(name_token).to_string(),
            span: name_token.span,
        };
        if self.expect(TokenKind::Newline).is_none() {
            self.synchronize();
            return None;
        }
        if self.expect(TokenKind::Indent).is_none() {
            self.synchronize();
            return None;
        }

        let mut variants = Vec::new();
        while !self.at(TokenKind::Dedent) && !self.at(TokenKind::Eof) {
            if self.at(TokenKind::Newline) {
                self.bump();
                continue;
            }
            match self.parse_enum_variant() {
                Some(v) => variants.push(v),
                None => {
                    // The member parser already resynchronized past its line.
                    continue;
                }
            }
        }
        self.eat(TokenKind::Dedent);

        if variants.is_empty() {
            self.error(name.span, "an `enum` must declare at least one member");
        }
        let end_span = variants.last().map_or(name.span, |v| v.span);
        Some(EnumDecl {
            name,
            variants,
            exported,
            span: enum_token.span.to(end_span),
        })
    }

    /// Parse one enum member on its own line: `IDENT [= "title"]`. On error
    /// resynchronizes past the line and returns `None`.
    fn parse_enum_variant(&mut self) -> Option<EnumVariant> {
        let name_token = match self.expect(TokenKind::Ident) {
            Some(t) => t,
            None => {
                self.synchronize();
                return None;
            }
        };
        let name = Ident {
            name: self.text(name_token).to_string(),
            span: name_token.span,
        };
        let mut span = name_token.span;
        let title = if self.eat(TokenKind::Eq).is_some() {
            let str_token = match self.expect(TokenKind::Str) {
                Some(t) => t,
                None => {
                    self.error(
                        self.peek().span,
                        "an enum member title must be a string literal",
                    );
                    self.synchronize();
                    return None;
                }
            };
            let raw = self.text(str_token);
            let value = self.decode_string(raw, str_token.span)?;
            span = span.to(str_token.span);
            Some(value)
        } else {
            None
        };
        self.expect_end_of_line();
        Some(EnumVariant { name, title, span })
    }

    /// Parse a type declaration: `type Name` NEWLINE INDENT field* DEDENT,
    /// where each field is `TYPE IDENT [= default]` on its own line. The caller
    /// has confirmed the current token is `type`. An empty field list is a
    /// parse error; duplicate field names and unknown field types are semantic
    /// checks (`E0236`), not parse errors.
    fn parse_type_def(&mut self, exported: bool) -> Option<TypeDecl> {
        let type_token = self.bump(); // `type`
        let name_token = match self.expect(TokenKind::Ident) {
            Some(t) => t,
            None => {
                self.synchronize();
                return None;
            }
        };
        let name = Ident {
            name: self.text(name_token).to_string(),
            span: name_token.span,
        };
        if self.expect(TokenKind::Newline).is_none() {
            self.synchronize();
            return None;
        }
        if self.expect(TokenKind::Indent).is_none() {
            self.synchronize();
            return None;
        }

        let mut fields = Vec::new();
        while !self.at(TokenKind::Dedent) && !self.at(TokenKind::Eof) {
            if self.at(TokenKind::Newline) {
                self.bump();
                continue;
            }
            match self.parse_type_field() {
                Some(f) => fields.push(f),
                None => continue,
            }
        }
        self.eat(TokenKind::Dedent);

        if fields.is_empty() {
            self.error(name.span, "a `type` must declare at least one field");
        }
        let end_span = fields.last().map_or(name.span, |f| f.span);
        Some(TypeDecl {
            name,
            fields,
            exported,
            span: type_token.span.to(end_span),
        })
    }

    /// Parse one field of a type declaration: `TYPE IDENT [= default]` on its
    /// own line. `TYPE` is a scalar keyword (`int`/`float`/`bool`/`string`),
    /// `color`, `array<T>`, `matrix<T>`, `map<K, V>`, or a bare identifier naming another type. On error
    /// resynchronizes past the line and returns `None`.
    fn parse_type_field(&mut self) -> Option<TypeField> {
        let ty = self.parse_field_type()?;
        let name_token = match self.expect(TokenKind::Ident) {
            Some(t) => t,
            None => {
                self.synchronize();
                return None;
            }
        };
        let name = Ident {
            name: self.text(name_token).to_string(),
            span: name_token.span,
        };
        let mut span = ty.span.to(name_token.span);
        let default = if self.eat(TokenKind::Eq).is_some() {
            match self.parse_expr() {
                Some(e) => {
                    span = span.to(e.span());
                    Some(e)
                }
                None => {
                    self.synchronize();
                    return None;
                }
            }
        } else {
            None
        };
        self.expect_end_of_line();
        Some(TypeField {
            ty,
            name,
            default,
            span,
        })
    }

    /// Parse a declared type: a scalar keyword, `color`, `array<T>`, `matrix<T>`, `map<K, V>`,
    /// a bare (possibly dotted, e.g. `chart.point`) identifier naming another
    /// user-defined type or enum, or any of those with the `[]` array suffix
    /// (`int[]`, `color[]`). `color` and type names are lexed as identifiers
    /// (they are not keywords); the resolver validates a named type exists
    /// (`E0236`).
    fn parse_field_type(&mut self) -> Option<FieldType> {
        let tok = self.peek();
        let base = match tok.kind {
            TokenKind::KwInt | TokenKind::KwFloat | TokenKind::KwBool | TokenKind::KwString => {
                let a = match tok.kind {
                    TokenKind::KwInt => AnnotatedType::Int,
                    TokenKind::KwFloat => AnnotatedType::Float,
                    TokenKind::KwBool => AnnotatedType::Bool,
                    TokenKind::KwString => AnnotatedType::String,
                    _ => unreachable!(),
                };
                self.bump();
                FieldType {
                    kind: FieldTypeKind::Scalar(a),
                    span: tok.span,
                }
            }
            TokenKind::Ident => {
                let mut text = self.text(tok).to_string();
                let mut span = tok.span;
                self.bump();
                if matches!(text.as_str(), "array" | "matrix" | "map") && self.at(TokenKind::Lt) {
                    return self.parse_collection_field_type(&text, tok.span);
                }
                // A dotted type name (`chart.point`).
                while self.at(TokenKind::Dot) && self.nth_kind(1) == TokenKind::Ident {
                    self.bump(); // `.`
                    let seg = self.bump();
                    text.push('.');
                    text.push_str(self.text(seg));
                    span = span.to(seg.span);
                }
                let kind = if text == "color" {
                    FieldTypeKind::Color
                } else {
                    FieldTypeKind::Named(text)
                };
                FieldType { kind, span }
            }
            _ => {
                self.error(
                    tok.span,
                    "expected a field type (int, float, bool, string, color, array<T>, matrix<T>, map<K, V>, or a type name)",
                );
                self.synchronize();
                return None;
            }
        };
        Some(self.with_array_suffix(base))
    }

    /// Wrap `base` into an array type when the `[]` suffix follows
    /// (`int[] xs`, `MyType[] items` — sugar for `array<T>`).
    fn with_array_suffix(&mut self, base: FieldType) -> FieldType {
        if self.at(TokenKind::LBracket) && self.nth_kind(1) == TokenKind::RBracket {
            self.bump(); // `[`
            let close = self.bump(); // `]`
            let span = base.span.to(close.span);
            return FieldType {
                kind: FieldTypeKind::Array(Box::new(base)),
                span,
            };
        }
        base
    }

    /// Parse the type arguments of a collection field type. `base_span`
    /// covers the `array`, `matrix`, or `map` keyword; the caller has
    /// confirmed the next token is `<`.
    ///
    /// This is the one place a type recurses (each argument is itself a
    /// type), so it is where nesting is counted against `MAX_TYPE_DEPTH`.
    fn parse_collection_field_type(&mut self, base: &str, base_span: Span) -> Option<FieldType> {
        // Refuse to descend past the type-nesting cap: a deeper type would
        // risk overflowing the stack here or in a later pass that walks it.
        // Report once and skip the rest of the statement.
        if self.type_depth >= MAX_TYPE_DEPTH {
            self.report_type_too_deep(base_span);
            self.synchronize();
            return None;
        }
        self.type_depth += 1;
        let ty = self.parse_collection_type_args(base, base_span);
        self.type_depth -= 1;
        ty
    }

    /// Report a declared type that nests past `MAX_TYPE_DEPTH`, labelling the
    /// collection keyword that opens the level too many.
    fn report_type_too_deep(&mut self, span: Span) {
        self.type_too_deep = true;
        self.diagnostics.push(
            Diagnostic::error(codes::E0110, "type is nested too deeply")
                .with_label(
                    span,
                    format!("the type is more than {MAX_TYPE_DEPTH} levels deep here"),
                )
                .with_note(
                    "each `array<...>`, `matrix<...>` or `map<...>` counts one level; hold an \
                     inner collection in a field of a user-defined type",
                ),
        );
    }

    /// The body of [`Self::parse_collection_field_type`]: the `<...>`
    /// argument list of one collection level.
    fn parse_collection_type_args(&mut self, base: &str, base_span: Span) -> Option<FieldType> {
        self.bump(); // `<`
        let first = self.parse_field_type()?;
        if base == "map" {
            self.expect(TokenKind::Comma)?;
            let value = self.parse_field_type()?;
            let close = self.expect(TokenKind::Gt)?;
            return Some(FieldType {
                kind: FieldTypeKind::Map(Box::new(first), Box::new(value)),
                span: base_span.to(close.span),
            });
        }
        let close = match self.expect(TokenKind::Gt) {
            Some(t) => t,
            None => {
                self.synchronize();
                return None;
            }
        };
        let kind = if base == "matrix" {
            FieldTypeKind::Matrix(Box::new(first))
        } else {
            FieldTypeKind::Array(Box::new(first))
        };
        Some(FieldType {
            kind,
            span: base_span.to(close.span),
        })
    }

    fn parse_indicator_decl(&mut self) -> Option<IndicatorDecl> {
        let name_token = self.bump(); // `indicator`
        if self.expect(TokenKind::LParen).is_none() {
            self.synchronize();
            return None;
        }
        let args = self.parse_call_args()?;
        let close = match self.expect(TokenKind::RParen) {
            Some(t) => t,
            None => {
                self.synchronize();
                return None;
            }
        };
        let span = name_token.span.to(close.span);
        self.expect_end_of_line();
        Some(IndicatorDecl { args, span })
    }

    /// Parse `strategy("Title", ...)` at top level. Structurally the same
    /// as [`Self::parse_indicator_decl`]: a name, a balanced argument list, and
    /// end-of-statement. Argument validation happens in semantic analysis.
    fn parse_strategy_decl(&mut self) -> Option<StrategyDecl> {
        let name_token = self.bump(); // `strategy`
        if self.expect(TokenKind::LParen).is_none() {
            self.synchronize();
            return None;
        }
        let args = self.parse_call_args()?;
        let close = match self.expect(TokenKind::RParen) {
            Some(t) => t,
            None => {
                self.synchronize();
                return None;
            }
        };
        let span = name_token.span.to(close.span);
        self.expect_end_of_line();
        Some(StrategyDecl { args, span })
    }

    /// Parse `library("Title", overlay?)` at top level. Structurally the
    /// same as [`Self::parse_indicator_decl`]; argument validation (a constant
    /// string title, an optional `overlay` boolean) happens in semantic
    /// analysis.
    fn parse_library_decl(&mut self) -> Option<LibraryDecl> {
        let name_token = self.bump(); // `library`
        if self.expect(TokenKind::LParen).is_none() {
            self.synchronize();
            return None;
        }
        let args = self.parse_call_args()?;
        let close = match self.expect(TokenKind::RParen) {
            Some(t) => t,
            None => {
                self.synchronize();
                return None;
            }
        };
        let span = name_token.span.to(close.span);
        self.expect_end_of_line();
        Some(LibraryDecl { args, span })
    }

    /// Parse `import Publisher/LibName/Version [as alias]`. The
    /// publisher/library are identifiers and the version is an
    /// identifier-or-number token; they are joined by `/`. A malformed path
    /// (missing a `/` component or a stray trailing token) is `E0243`, reported
    /// by the analyzer — here the parser reports only structural gaps and
    /// resynchronizes. When `as alias` is omitted the alias defaults to the
    /// library name.
    fn parse_import_decl(&mut self) -> Option<ImportDecl> {
        let import_kw = self.bump(); // `import`
                                     // A publisher name in an import path may contain hyphens (e.g.
                                     // `some-name/Lib/1`). The ordinary lexer correctly emits `-`
                                     // as an operator, so reassemble adjacent identifier segments only in
                                     // this path component.
        let publisher = self.parse_import_publisher()?;
        if self.eat(TokenKind::Slash).is_none() {
            self.error_here("an import path is `Publisher/LibName/Version`");
            self.synchronize();
            return None;
        }
        let library = self.parse_ident()?;
        if self.eat(TokenKind::Slash).is_none() {
            self.error_here("an import path needs a version: `Publisher/LibName/Version`");
            self.synchronize();
            return None;
        }
        // The version component: an integer (the common `.../1` form) or an
        // identifier. Carried verbatim as text for the map lookup.
        let version_token = if self.at(TokenKind::Number) || self.at(TokenKind::Ident) {
            self.bump()
        } else {
            self.error_here("an import version must be a number or name");
            self.synchronize();
            return None;
        };
        let version = self.text(version_token).to_string();
        let mut end = version_token.span;

        // Optional `as alias`. `as` is a contextual identifier here.
        let alias = if self.at(TokenKind::Ident) && self.text(self.peek()) == "as" {
            self.bump(); // `as`
            let alias_token = self.parse_ident()?;
            end = alias_token.span;
            alias_token
        } else {
            // Without `as alias`, the alias is the library name.
            library.clone()
        };
        let span = import_kw.span.to(end);
        self.expect_end_of_line();
        Some(ImportDecl {
            publisher,
            library,
            version,
            alias,
            span,
        })
    }

    fn parse_import_publisher(&mut self) -> Option<Ident> {
        let mut publisher = self.parse_ident()?;
        loop {
            if self.peek_kind() != TokenKind::Minus || self.nth_kind(1) != TokenKind::Ident {
                break;
            }
            let hyphen = self.peek();
            // A hyphenated name is one path component, not an arithmetic
            // expression with spaces around the minus sign.
            if publisher.span.end != hyphen.span.start {
                break;
            }
            self.bump();
            let segment = self.parse_ident()?;
            if hyphen.span.end != segment.span.start {
                self.error(
                    segment.span,
                    "publisher-name segments separated by `-` must be adjacent".to_string(),
                );
                return None;
            }
            publisher.name.push('-');
            publisher.name.push_str(&segment.name);
            publisher.span = publisher.span.to(segment.span);
        }
        Some(publisher)
    }

    /// Parse a bare identifier into an [`Ident`], reporting + resynchronizing on
    /// a non-identifier token.
    fn parse_ident(&mut self) -> Option<Ident> {
        let token = self.expect(TokenKind::Ident)?;
        Some(Ident {
            name: self.text(token).to_string(),
            span: token.span,
        })
    }

    /// Report an error at the current token's span with `message`.
    fn error_here(&mut self, message: &str) {
        let span = self.peek().span;
        self.error(span, message);
    }

    /// Parse a comma-separated argument list positioned just after the opening
    /// `(`, up to (but not consuming) the closing `)`. Each argument is either
    /// positional (`expr`) or named (`name = expr`). Argument names may be
    /// source keywords (for example `type =` or `to =`). A positional argument
    /// after any named argument is a parse error. On an unrecoverable error the
    /// parser synchronizes and returns `None`.
    fn parse_call_args(&mut self) -> Option<Vec<CallArg>> {
        let mut args = Vec::new();
        if self.at(TokenKind::RParen) {
            return Some(args);
        }
        let mut seen_named = false;
        loop {
            // A named argument is an identifier or keyword immediately followed
            // by `=`. Keywords are valid here, so `type` and `to` parse as
            // argument names. (`==` is a distinct token, so `a == b` never
            // matches.)
            let current_kind = self.peek().kind;
            let can_name_argument = self.at(TokenKind::Ident)
                || TokenKind::KEYWORDS
                    .iter()
                    .any(|(_, kind)| *kind == current_kind);
            let name = if can_name_argument && self.nth_kind(1) == TokenKind::Eq {
                let ident_token = self.bump(); // name
                self.bump(); // `=`
                seen_named = true;
                Some(Ident {
                    name: self.text(ident_token).to_string(),
                    span: ident_token.span,
                })
            } else {
                if seen_named {
                    let span = self.peek().span;
                    self.error(span, "positional argument cannot follow a named argument");
                    self.synchronize();
                    return None;
                }
                None
            };
            let value = match self.parse_expr() {
                Some(expr) => expr,
                None => {
                    self.synchronize();
                    return None;
                }
            };
            let span = match &name {
                Some(n) => n.span.to(value.span()),
                None => value.span(),
            };
            args.push(CallArg { name, value, span });
            if self.eat(TokenKind::Comma).is_none() {
                break;
            }
        }
        Some(args)
    }

    /// Parse the statements of an inline body (after `=>` on the same line):
    /// one statement, plus any further ones joined by the `,` separator
    /// (`f(x) => y = x * 2, y + 1`). The last statement is the body's value.
    fn parse_inline_stmts(&mut self) -> Option<Vec<Stmt>> {
        let mut body = vec![self.parse_stmt()?];
        while std::mem::take(&mut self.stmt_continues_on_line) {
            body.push(self.parse_stmt()?);
        }
        Some(body)
    }

    fn parse_stmt(&mut self) -> Option<Stmt> {
        self.stmt_continues_on_line = false;
        // A function definition nested inside a block is `E0224`: definitions
        // are top-level only. Parse and discard it (after reporting) so the
        // rest of the block still parses.
        if self.is_func_def_ahead() {
            if let Some(def) = self.parse_func_def(false) {
                self.diagnostics.push(
                    Diagnostic::error(
                        codes::E0224,
                        "a function definition must appear at the top level of the script",
                    )
                    .with_label(def.name.span, "not allowed inside a block")
                    .with_note("move this `name(...) => ...` definition to the top level"),
                );
            }
            return None;
        }
        // `once` is contextual: variables, parameters, functions and members
        // can use that name. Only a statement followed by an indented block
        // has the conditional-structure meaning.
        if self.at(TokenKind::Ident) && self.text(self.peek()) == "once" && self.once_block_ahead()
        {
            return self.parse_once_stmt();
        }
        match self.peek_kind() {
            TokenKind::KwIf => self.parse_if_stmt(),
            TokenKind::KwSwitch => self.parse_switch().map(Stmt::Switch),
            TokenKind::KwFor => self.parse_for_stmt(),
            TokenKind::KwWhile => self.parse_while_stmt(),
            TokenKind::KwBreak => self.parse_break_or_continue(true),
            TokenKind::KwContinue => self.parse_break_or_continue(false),
            TokenKind::KwVar => self.parse_var_stmt(),
            TokenKind::KwVarip => self.parse_varip_stmt(),
            // A leading type keyword followed by `(` is a type-cast call used as
            // an expression statement (`int(x)`), not a type-annotated
            // declaration (`int x = …`). Route it to the expression path.
            TokenKind::KwInt | TokenKind::KwFloat | TokenKind::KwBool | TokenKind::KwString
                if self.starts_param_type(self.pos) =>
            {
                self.parse_annotated_stmt(false, false)
            }
            // A `[` at statement start is either a destructuring declaration
            // (`[a, b] = expr`) or a bare tuple **expression** statement (a
            // function body's trailing `[hi, lo]` return value). Disambiguate
            // by looking for a `=` right after the closing `]`: a destructure
            // has one, a tuple-expression statement does not. (The
            // `target[offset]` history postfix needs a preceding primary, so
            // it never begins a statement.)
            TokenKind::LBracket if self.is_destructure_ahead() => self.parse_destructure_stmt(),
            TokenKind::LBracket => self.parse_expr_stmt(),
            kind if Self::is_ident_kind(kind) => self.parse_ident_stmt(),
            // `na = ...` / `na := ...`: `na` is a literal, not a variable.
            TokenKind::KwNa
                if matches!(
                    self.tokens.get(self.pos + 1).map(|t| t.kind),
                    Some(TokenKind::Eq | TokenKind::ColonEq)
                ) =>
            {
                let t = self.peek();
                self.error(
                    t.span,
                    "cannot assign to `na`: it is a literal, not a variable",
                );
                self.synchronize();
                None
            }
            _ => self.parse_expr_stmt(),
        }
    }

    /// A statement starting with an identifier: assignment (`x = e`),
    /// reassignment (`x := e`), or an expression statement. A *dotted* name
    /// followed by `=`/`:=` (`ta.sma = 1`) gets a targeted parse error:
    /// assignment targets must be plain identifiers.
    fn parse_ident_stmt(&mut self) -> Option<Stmt> {
        if self.variable_qualifier_ahead().is_some() {
            return self.parse_qualified_stmt(false, false);
        }
        // A drawing-type-name annotation at statement start (`line l = na`,
        // `chart.point p = ...`): the leading identifier (possibly dotted, for
        // `chart.point`) names a drawing object type and is immediately
        // followed by a target identifier and `=`. Recognized before the
        // assignment/expression disambiguation so the type-name-then-name shape
        // parses as an annotated declaration.
        match self.try_parse_named_annotated_stmt(false, false) {
            NamedDecl::Parsed(stmt) => return Some(stmt),
            NamedDecl::Skipped => return None,
            NamedDecl::Absent => {}
        }
        // Look ahead across `IDENT (Dot IDENT)*`. A `type` segment is a
        // field name here (`zone.type := x`).
        let mut end = self.pos + 1;
        while self.tokens.get(end).map(|t| t.kind) == Some(TokenKind::Dot)
            && matches!(
                self.tokens.get(end + 1).map(|t| t.kind),
                Some(TokenKind::Ident | TokenKind::KwType)
            )
        {
            end += 2;
        }
        let next = self.tokens.get(end).map(|t| t.kind);
        let is_reassign_op =
            next.is_some_and(|k| k == TokenKind::ColonEq || Self::compound_op(k).is_some());
        if next == Some(TokenKind::Eq) || is_reassign_op {
            let dotted = end > self.pos + 1;
            if dotted {
                // A dotted target with `=` is always an error (declarations use
                // a plain identifier). A dotted target with `:=` (or a compound
                // operator) is a field-write reassignment (`obj.field := value`)
                // — the resolver decides whether the head is an object
                // variable (field write) or a namespaced builtin (`E0217`).
                if next == Some(TokenKind::Eq) {
                    let span = self.peek().span.to(self.tokens[end - 1].span);
                    self.diagnostics.push(
                        Diagnostic::error(codes::E0110, "cannot assign to a namespaced name")
                            .with_label(span, "declaration targets must be plain identifiers"),
                    );
                    self.synchronize();
                    return None;
                }
                return self.parse_dotted_reassign_stmt(end);
            }
            return if next == Some(TokenKind::Eq) {
                self.parse_assign_stmt()
            } else {
                self.parse_reassign_stmt()
            };
        }
        self.parse_expr_stmt()
    }

    fn parse_assign_stmt(&mut self) -> Option<Stmt> {
        let name_token = self.bump();
        let name = Ident {
            name: self.text(name_token).to_string(),
            span: name_token.span,
        };
        self.bump(); // `=`
        let value = match self.parse_expr() {
            Some(e) => e,
            None => {
                self.synchronize();
                return None;
            }
        };
        let span = name_token.span.to(value.span());
        self.expect_end_of_stmt();
        Some(Stmt::Assign {
            name,
            value,
            is_var: false,
            is_varip: false,
            annotation: None,
            span,
        })
    }

    /// Map a parsed surface [`FieldType`] to the equivalent variable-annotation
    /// [`AnnotationKind`]. The two AST shapes coexist because annotations
    /// predate general field types; they resolve to the same semantic types.
    fn field_type_to_annotation(ty: FieldType) -> TypeAnnotation {
        let span = ty.span;
        let kind = match ty.kind {
            FieldTypeKind::Scalar(a) => AnnotationKind::Scalar(a),
            FieldTypeKind::Color => AnnotationKind::Color,
            FieldTypeKind::Array(elem) => AnnotationKind::Array(elem),
            FieldTypeKind::Matrix(elem) => AnnotationKind::Matrix(elem),
            FieldTypeKind::Map(key, value) => AnnotationKind::Map(key, value),
            FieldTypeKind::Named(name) => AnnotationKind::Named(name),
        };
        TypeAnnotation {
            kind,
            qualifier: None,
            span,
        }
    }

    /// Qualifier words remain usable as identifiers unless a type follows.
    fn variable_qualifier_ahead(&self) -> Option<TypeQualifier> {
        if !self.at(TokenKind::Ident) || !self.starts_param_type(self.pos + 1) {
            return None;
        }
        match self.text(self.peek()) {
            "const" => Some(TypeQualifier::Const),
            "simple" => Some(TypeQualifier::Simple),
            "series" => Some(TypeQualifier::Series),
            _ => None,
        }
    }

    fn parse_qualified_stmt(&mut self, is_var: bool, is_varip: bool) -> Option<Stmt> {
        let qualifier = self.variable_qualifier_ahead()?;
        let start = self.bump().span;
        let mut stmt = self.parse_annotated_stmt(is_var, is_varip)?;
        if let Stmt::Assign {
            annotation: Some(annotation),
            span,
            ..
        } = &mut stmt
        {
            annotation.qualifier = Some(qualifier);
            annotation.span = start.to(annotation.span);
            *span = start.to(*span);
        }
        Some(stmt)
    }

    /// Try to parse a type-name annotated declaration at the current position:
    /// `<Type> IDENT = expr` where `<Type>` is any non-scalar-keyword surface
    /// type — a drawing object type (`line l = na`), `color`, a user-defined
    /// type or enum name (`Settings s = …`), a dotted name (`chart.point p`),
    /// `array<T>`, or the `T[]` sugar. Parses the type speculatively and
    /// backtracks — leaving position and diagnostics unchanged — unless the
    /// type is followed by `IDENT =`, so a call statement (`line.new(...)`),
    /// an assignment (`x = 1`), or a comparison (`a < b`) still falls through
    /// to the ordinary statement paths. `is_var`/`is_varip` carry a leading
    /// `var`/`varip` already consumed by the caller. The [`NamedDecl`] it
    /// returns tells the caller whether the statement is settled.
    fn try_parse_named_annotated_stmt(&mut self, is_var: bool, is_varip: bool) -> NamedDecl {
        if !self.at(TokenKind::Ident) {
            return NamedDecl::Absent;
        }
        let save_pos = self.pos;
        let save_diags = self.diagnostics.mark();
        self.type_too_deep = false;
        let ty = match self.parse_field_type() {
            Some(t) => t,
            // Nesting past `MAX_TYPE_DEPTH` is a type that went too far, not
            // an expression that happens to start like one: the diagnostic
            // stands, and the statement has already been skipped.
            None if self.type_too_deep => return NamedDecl::Skipped,
            None => {
                self.pos = save_pos;
                self.diagnostics.rollback(save_diags);
                return NamedDecl::Absent;
            }
        };
        // The type must be followed by `IDENT` and then `=` (or the `:=`
        // mistake diagnosed below) for this to be a declaration.
        let is_decl = self.at(TokenKind::Ident)
            && matches!(self.nth_kind(1), TokenKind::Eq | TokenKind::ColonEq);
        if !is_decl {
            self.pos = save_pos;
            self.diagnostics.rollback(save_diags);
            return NamedDecl::Absent;
        }
        let ty_span = ty.span;
        let annotation = Self::field_type_to_annotation(ty);
        let name_token = self.bump(); // target IDENT
        let name = Ident {
            name: self.text(name_token).to_string(),
            span: name_token.span,
        };
        if self.at(TokenKind::ColonEq) {
            let t = self.peek();
            self.error(t.span, "a type-annotated declaration uses `=`, not `:=`");
            self.synchronize();
            return NamedDecl::Skipped;
        }
        if self.expect(TokenKind::Eq).is_none() {
            self.synchronize();
            return NamedDecl::Skipped;
        }
        let value = match self.parse_expr() {
            Some(e) => e,
            None => {
                self.synchronize();
                return NamedDecl::Skipped;
            }
        };
        let span = ty_span.to(value.span());
        self.expect_end_of_stmt();
        NamedDecl::Parsed(Stmt::Assign {
            name,
            value,
            is_var,
            is_varip,
            annotation: Some(annotation),
            span,
        })
    }

    /// `[var|varip] <type> IDENT = expr` — an annotated declaration. `is_var`
    /// is true when a `var` (or `varip`) keyword has already been consumed by
    /// the caller; `is_varip` additionally marks the `varip` form (`is_varip`
    /// implies `is_var`). `start` is the span of the leading keyword so the
    /// statement span covers the whole declaration.
    fn parse_annotated_stmt(&mut self, is_var: bool, is_varip: bool) -> Option<Stmt> {
        let start = self.peek().span;
        let annotation = Self::field_type_to_annotation(self.parse_field_type()?);
        let name_token = match self.expect(TokenKind::Ident) {
            Some(t) => t,
            None => {
                self.synchronize();
                return None;
            }
        };
        let name = Ident {
            name: self.text(name_token).to_string(),
            span: name_token.span,
        };
        // A `:=` here is a declaration-with-reassignment mistake: annotations
        // always declare with `=`.
        if self.at(TokenKind::ColonEq) {
            let t = self.peek();
            self.error(t.span, "a type-annotated declaration uses `=`, not `:=`");
            self.synchronize();
            return None;
        }
        if self.expect(TokenKind::Eq).is_none() {
            self.synchronize();
            return None;
        }
        let value = match self.parse_expr() {
            Some(e) => e,
            None => {
                self.synchronize();
                return None;
            }
        };
        let span = start.to(value.span());
        self.expect_end_of_stmt();
        Some(Stmt::Assign {
            name,
            value,
            is_var,
            is_varip,
            annotation: Some(annotation),
            span,
        })
    }

    /// `varip IDENT = expr` — intrabar-persistent declaration. Lowers
    /// exactly like `var` but marks the slot as `varip` so it survives
    /// realtime-tick rollback. `varip x := e` is a parse error (like `var`).
    fn parse_varip_stmt(&mut self) -> Option<Stmt> {
        self.parse_var_like_stmt(true)
    }

    /// `var IDENT = expr` — persistent declaration. `var x := e` is a parse
    /// error: `var` always declares (with `=`); `:=` reassigns existing
    /// variables.
    fn parse_var_stmt(&mut self) -> Option<Stmt> {
        self.parse_var_like_stmt(false)
    }

    /// Shared body of `var`/`varip` declaration parsing. `is_varip` selects the
    /// `varip` form; both consume the leading keyword and produce an
    /// `Stmt::Assign` with `is_var = true`.
    fn parse_var_like_stmt(&mut self, is_varip: bool) -> Option<Stmt> {
        let var_token = self.bump(); // `var` or `varip`
        if self.variable_qualifier_ahead().is_some() {
            let mut stmt = self.parse_qualified_stmt(true, is_varip)?;
            if let Stmt::Assign { span, .. } = &mut stmt {
                *span = var_token.span.to(*span);
            }
            return Some(stmt);
        }
        // `var int x = 1` — a persistent, type-annotated declaration. The
        // annotated path handles the type keyword, name, `=`, and value; we
        // only fix up the statement span to start at the keyword.
        if matches!(
            self.peek_kind(),
            TokenKind::KwInt | TokenKind::KwFloat | TokenKind::KwBool | TokenKind::KwString
        ) && self.starts_param_type(self.pos)
        {
            let stmt = self.parse_annotated_stmt(true, is_varip)?;
            if let Stmt::Assign {
                name,
                value,
                is_var,
                is_varip,
                annotation,
                span,
            } = stmt
            {
                return Some(Stmt::Assign {
                    name,
                    value,
                    is_var,
                    is_varip,
                    annotation,
                    span: var_token.span.to(span),
                });
            }
            return Some(stmt);
        }
        // `var line l = na` — a persistent drawing-typed declaration. Reuse the
        // named-annotation path, fixing up the statement span to start at the
        // `var`/`varip` keyword.
        match self.try_parse_named_annotated_stmt(true, is_varip) {
            NamedDecl::Parsed(stmt) => {
                if let Stmt::Assign {
                    name,
                    value,
                    is_var,
                    is_varip,
                    annotation,
                    span,
                } = stmt
                {
                    return Some(Stmt::Assign {
                        name,
                        value,
                        is_var,
                        is_varip,
                        annotation,
                        span: var_token.span.to(span),
                    });
                }
                return Some(stmt);
            }
            NamedDecl::Skipped => return None,
            NamedDecl::Absent => {}
        }
        let name_token = match self.expect(TokenKind::Ident) {
            Some(t) => t,
            None => {
                self.synchronize();
                return None;
            }
        };
        let name = Ident {
            name: self.text(name_token).to_string(),
            span: name_token.span,
        };
        if self.at(TokenKind::ColonEq) {
            let t = self.peek();
            let kw = if is_varip { "`varip`" } else { "`var`" };
            self.error(t.span, format!("{kw} declares with `=`, not `:=`"));
            self.synchronize();
            return None;
        }
        if self.expect(TokenKind::Eq).is_none() {
            self.synchronize();
            return None;
        }
        let value = match self.parse_expr() {
            Some(e) => e,
            None => {
                self.synchronize();
                return None;
            }
        };
        let span = var_token.span.to(value.span());
        self.expect_end_of_stmt();
        Some(Stmt::Assign {
            name,
            value,
            is_var: true,
            is_varip,
            annotation: None,
            span,
        })
    }

    /// The binary operation a compound reassignment token applies, or `None`
    /// for the plain `:=`.
    fn compound_op(kind: TokenKind) -> Option<BinaryOp> {
        Some(match kind {
            TokenKind::PlusEq => BinaryOp::Add,
            TokenKind::MinusEq => BinaryOp::Sub,
            TokenKind::StarEq => BinaryOp::Mul,
            TokenKind::SlashEq => BinaryOp::Div,
            TokenKind::PercentEq => BinaryOp::Rem,
            _ => return None,
        })
    }

    /// Desugar a compound reassignment (`x += rhs` → `x := x + rhs`): wrap
    /// `rhs` in the binary op with the target as the left operand. A plain
    /// `:=` passes `rhs` through unchanged.
    fn desugar_compound(name: &Ident, op_kind: TokenKind, rhs: Expr) -> Expr {
        match Self::compound_op(op_kind) {
            Some(op) => {
                let span = name.span.to(rhs.span());
                Expr::Binary {
                    op,
                    lhs: Box::new(Expr::Ident(name.clone())),
                    rhs: Box::new(rhs),
                    span,
                }
            }
            None => rhs,
        }
    }

    /// `IDENT := expr` — reassignment — or the compound form (`IDENT += expr`
    /// and friends), which desugars to `IDENT := IDENT op expr`. Whether the
    /// name exists is a semantic question (`E0217`), not a parse error.
    fn parse_reassign_stmt(&mut self) -> Option<Stmt> {
        let name_token = self.bump();
        let name = Ident {
            name: self.text(name_token).to_string(),
            span: name_token.span,
        };
        let op_token = self.bump(); // `:=` or a compound operator
        let value = match self.parse_expr() {
            Some(e) => e,
            None => {
                self.synchronize();
                return None;
            }
        };
        let value = Self::desugar_compound(&name, op_token.kind, value);
        let span = name_token.span.to(value.span());
        self.expect_end_of_stmt();
        Some(Stmt::Reassign { name, value, span })
    }

    /// `IDENT (Dot IDENT)+ := expr` — a field-write reassignment (`obj.field :=
    /// value`). The caller has scanned the dotted target to token index
    /// `end` (the `:=`). Folds the path into one dotted [`Ident`] name — the
    /// same shape a field read uses — so the resolver can split it. Whether the
    /// head is an object variable is a semantic question.
    fn parse_dotted_reassign_stmt(&mut self, end: usize) -> Option<Stmt> {
        let start_token = self.peek();
        let mut name = self.text(start_token).to_string();
        let mut span = start_token.span;
        self.bump(); // head IDENT
        while self.pos < end {
            self.bump(); // `.`
            let seg = self.bump(); // segment IDENT
            name.push('.');
            name.push_str(self.text(seg));
            span = span.to(seg.span);
        }
        let name = Ident { name, span };
        let op_token = self.bump(); // `:=` or a compound operator
        let value = match self.parse_expr() {
            Some(e) => e,
            None => {
                self.synchronize();
                return None;
            }
        };
        let value = Self::desugar_compound(&name, op_token.kind, value);
        let full_span = span.to(value.span());
        self.expect_end_of_stmt();
        Some(Stmt::Reassign {
            name,
            value,
            span: full_span,
        })
    }

    fn parse_expr_stmt(&mut self) -> Option<Stmt> {
        let expr = match self.parse_expr() {
            Some(e) => e,
            None => {
                self.synchronize();
                return None;
            }
        };
        let span = expr.span();
        self.expect_end_of_stmt();
        Some(Stmt::Expr { expr, span })
    }

    /// `[a, b, c] = expr` — a destructuring declaration. The targets are
    /// plain identifiers (a namespaced or non-identifier target is a parse
    /// error); a single-element pattern (`[a]`) parses (arity is a semantic
    /// check, `E0225`). The `= expr` is required — `[a, b]` in statement
    /// position without an assignment, or with `:=`, is a parse error.
    fn parse_destructure_stmt(&mut self) -> Option<Stmt> {
        let open = self.bump(); // `[`
        let mut targets = Vec::new();
        loop {
            let tok = match self.expect(TokenKind::Ident) {
                Some(t) => t,
                None => {
                    self.synchronize();
                    return None;
                }
            };
            // A dotted target (`ta.sma`) cannot be a destructuring binding.
            if self.at(TokenKind::Dot) {
                self.error(
                    tok.span,
                    "a destructuring target must be a plain identifier",
                );
                self.synchronize();
                return None;
            }
            targets.push(Ident {
                name: self.text(tok).to_string(),
                span: tok.span,
            });
            if self.eat(TokenKind::Comma).is_none() {
                break;
            }
        }
        if self.expect(TokenKind::RBracket).is_none() {
            self.synchronize();
            return None;
        }
        if self.at(TokenKind::ColonEq) {
            let t = self.peek();
            self.error(t.span, "a destructuring declaration uses `=`, not `:=`");
            self.synchronize();
            return None;
        }
        if self.expect(TokenKind::Eq).is_none() {
            self.synchronize();
            return None;
        }
        let value = match self.parse_expr() {
            Some(e) => e,
            None => {
                self.synchronize();
                return None;
            }
        };
        let span = open.span.to(value.span());
        self.expect_end_of_stmt();
        Some(Stmt::Destructure {
            targets,
            value,
            span,
        })
    }

    /// Consume the Newline that ends a simple statement, diagnosing trailing
    /// garbage such as `plot(x) plot(y)`.
    fn expect_end_of_stmt(&mut self) {
        // A block-form `if`/`switch` expression at the tail of the statement
        // already consumed its terminating `Dedent`; the parser is now at the
        // next statement's first token, so no further `Newline` is required.
        if std::mem::take(&mut self.last_expr_block_terminated) {
            return;
        }
        // Several one-line statements may share a line, separated by a
        // comma (`a = 1, b = 2`). The parser also accepts a trailing comma
        // before the line end; it simply ends the statement (`x := 1,`).
        if self.eat(TokenKind::Comma).is_some()
            && !matches!(
                self.peek_kind(),
                TokenKind::Newline | TokenKind::Eof | TokenKind::Dedent
            )
        {
            self.stmt_continues_on_line = true;
            return;
        }
        self.expect_end_of_line();
    }

    /// Consume the Newline that ends a line-oriented construct (a statement,
    /// a declaration, a type field, an enum member), diagnosing trailing
    /// garbage. Unlike [`Self::expect_end_of_stmt`], a `,` is not accepted.
    fn expect_end_of_line(&mut self) {
        match self.peek_kind() {
            TokenKind::Newline => {
                self.bump();
            }
            TokenKind::Eof | TokenKind::Dedent => {}
            _ => {
                let t = self.peek();
                // Error tokens are already diagnosed by the lexer.
                if t.kind != TokenKind::Error {
                    self.error(
                        t.span,
                        format!("expected end of line, found {}", t.kind.describe()),
                    );
                }
                self.synchronize();
            }
        }
    }

    /// Recognize the logical header without consuming it. A top-level
    /// assignment or comma belongs to an identifier statement; nested calls
    /// may contain both, so keep their delimiters balanced during the scan.
    fn once_block_ahead(&self) -> bool {
        let mut depth = 0usize;
        for (offset, token) in self.tokens[self.pos + 1..].iter().enumerate() {
            match token.kind {
                TokenKind::LParen | TokenKind::LBracket => depth += 1,
                TokenKind::RParen | TokenKind::RBracket => depth = depth.saturating_sub(1),
                TokenKind::Newline => {
                    return self.nth_kind(offset + 2) == TokenKind::Indent;
                }
                TokenKind::Eof | TokenKind::Dedent | TokenKind::Indent => return false,
                TokenKind::Comma | TokenKind::Eq | TokenKind::ColonEq | TokenKind::FatArrow
                    if depth == 0 =>
                {
                    return false
                }
                kind if depth == 0 && Self::compound_op(kind).is_some() => return false,
                _ => {}
            }
        }
        false
    }

    fn parse_once_stmt(&mut self) -> Option<Stmt> {
        let token = self.bump();
        self.once_keyword_spans.push(token.span);
        let cond = if self.at(TokenKind::Newline) {
            None
        } else {
            match self.parse_expr() {
                Some(cond) => Some(cond),
                None => {
                    self.synchronize();
                    return None;
                }
            }
        };
        if self.expect(TokenKind::Newline).is_none() {
            self.synchronize();
            return None;
        }
        let body = self.parse_block()?;
        let end = body.last().map_or(token.span, Stmt::span);
        Some(Stmt::Once(OnceStmt {
            cond,
            body,
            span: token.span.to(end),
        }))
    }

    fn parse_if_stmt(&mut self) -> Option<Stmt> {
        let if_expr = self.parse_if_expr()?;
        Some(Stmt::If(IfStmt {
            cond: if_expr.cond,
            then_branch: if_expr.then_branch,
            else_branch: if_expr.else_branch,
            span: if_expr.span,
        }))
    }

    /// Parse `if cond <block> [else <block> | else if ...]`. Shared by the
    /// statement form (`parse_if_stmt`) and the expression form (an `if`
    /// consumed as a value); the two differ only in how the branch bodies are
    /// interpreted during semantic analysis.
    fn parse_if_expr(&mut self) -> Option<IfExpr> {
        let if_token = self.bump(); // `if`
        let cond = match self.parse_expr() {
            Some(c) => c,
            None => {
                self.synchronize();
                return None;
            }
        };
        if self.expect(TokenKind::Newline).is_none() {
            self.synchronize();
            return None;
        }
        let then_branch = self.parse_block()?;
        let mut end_span = then_branch.last().map_or(cond.span(), Stmt::span);

        let mut else_branch = None;
        if self.at(TokenKind::KwElse) {
            self.bump();
            if self.at(TokenKind::KwIf) {
                // `else if ...` chains as an else-branch containing one If
                // statement wrapping the nested if. This recursion goes
                // through `parse_if_expr` again — NOT `parse_block` — so it
                // must be counted against the block-depth cap here directly,
                // or a long `if / else if / else if / ...` chain (a flat chain,
                // not nested indentation) would overflow the native stack here
                // or in a downstream tree walk (semantics/MIR/CSIL).
                if self.block_depth >= MAX_BLOCK_DEPTH {
                    let span = self.peek().span;
                    self.error(span, "block is nested too deeply");
                    return None;
                }
                self.block_depth += 1;
                let nested = self.parse_if_expr();
                self.block_depth -= 1;
                let nested = nested?;
                end_span = nested.span;
                else_branch = Some(vec![Stmt::If(IfStmt {
                    cond: nested.cond,
                    then_branch: nested.then_branch,
                    else_branch: nested.else_branch,
                    span: nested.span,
                })]);
            } else {
                if self.expect(TokenKind::Newline).is_none() {
                    self.synchronize();
                    return None;
                }
                let stmts = self.parse_block()?;
                if let Some(last) = stmts.last() {
                    end_span = last.span();
                }
                else_branch = Some(stmts);
            }
        }

        Some(IfExpr {
            cond,
            then_branch,
            else_branch,
            span: if_token.span.to(end_span),
        })
    }

    /// Parse a `switch` (statement or expression form; both share syntax).
    /// `switch [subject]` on its own line, then an indented block of arms.
    /// Each arm is `label => body` or `=> body` (the `default`), where `body`
    /// is either a single inline expression on the same line or an indented
    /// block.
    fn parse_switch(&mut self) -> Option<SwitchStmt> {
        let switch_token = self.bump(); // `switch`

        // An optional subject on the same line as `switch`. A bare `switch`
        // (subjectless) has arms that are boolean conditions.
        let subject = if self.at(TokenKind::Newline) {
            None
        } else {
            match self.parse_expr() {
                Some(e) => Some(e),
                None => {
                    self.synchronize();
                    return None;
                }
            }
        };

        if self.expect(TokenKind::Newline).is_none() {
            self.synchronize();
            return None;
        }
        if self.expect(TokenKind::Indent).is_none() {
            self.synchronize();
            return None;
        }

        let mut arms = Vec::new();
        while !self.at(TokenKind::Dedent) && !self.at(TokenKind::Eof) {
            if self.at(TokenKind::Newline) {
                self.bump();
                continue;
            }
            match self.parse_switch_arm() {
                Some(arm) => arms.push(arm),
                None => {
                    // The arm parser already resynchronized past its line.
                    continue;
                }
            }
        }
        self.eat(TokenKind::Dedent);

        let end_span = arms.last().map_or_else(
            || subject.as_ref().map_or(switch_token.span, Expr::span),
            |a| a.span,
        );
        Some(SwitchStmt {
            subject,
            arms,
            span: switch_token.span.to(end_span),
        })
    }

    /// Parse one `switch` arm: `label => body`, or `=> body` for the default
    /// arm. `body` is a single inline expression statement (`=> expr`) or an
    /// indented block (`=>` followed by a newline and an indented block).
    fn parse_switch_arm(&mut self) -> Option<SwitchArm> {
        let start_span = self.peek().span;
        // A `default` arm has no label: the `=>` comes first.
        let label = if self.at(TokenKind::FatArrow) {
            None
        } else {
            match self.parse_expr() {
                Some(e) => Some(e),
                None => {
                    self.synchronize();
                    return None;
                }
            }
        };
        if self.expect(TokenKind::FatArrow).is_none() {
            self.synchronize();
            return None;
        }

        // Inline body (`=> stmt`) or an indented block (`=>` then a newline).
        // A single inline statement covers both a value expression (for a
        // switch expression) and an effect/assignment (for a switch
        // statement); the two are distinguished during analysis.
        if self.at(TokenKind::Newline) {
            self.bump();
            let body = self.parse_block()?;
            let end_span = body
                .last()
                .map(Stmt::span)
                .or_else(|| label.as_ref().map(Expr::span))
                .unwrap_or(start_span);
            Some(SwitchArm {
                label,
                body,
                span: start_span.to(end_span),
            })
        } else {
            let body = self.parse_inline_stmts()?;
            let end_span = body.last().map_or(start_span, Stmt::span);
            Some(SwitchArm {
                label,
                body,
                span: start_span.to(end_span),
            })
        }
    }

    /// `for counter = start to end [by step]` (counted) or
    /// `for element in arr` / `for [index, element] in arr` (iteration). The
    /// two are distinguished right after `for`: a leading `[` opens the tuple
    /// iteration form, and an `IDENT` followed by `in` opens the bare
    /// iteration form; otherwise it is the counted form.
    fn parse_for_stmt(&mut self) -> Option<Stmt> {
        // `for [index, element] in arr` — the only `for` form starting with
        // `[`. (The counted form's counter is always a plain identifier.)
        if self.nth_kind(1) == TokenKind::LBracket {
            return self.parse_for_in_stmt();
        }
        // `for element in arr` — an identifier directly followed by `in`.
        if self.nth_kind(1) == TokenKind::Ident && self.nth_kind(2) == TokenKind::KwIn {
            return self.parse_for_in_stmt();
        }
        let for_token = self.bump(); // `for`
                                     // `for int i = ...`: the parser accepts an `int` keyword before the
                                     // counter. A counted loop's counter is an `int` (or a `float` when
                                     // the bounds or step are fractional, which the checker derives), so
                                     // the keyword adds nothing and is dropped.
        if self.at(TokenKind::KwInt) && self.nth_kind(1) == TokenKind::Ident {
            self.bump();
        }
        let counter_token = match self.expect(TokenKind::Ident) {
            Some(t) => t,
            None => {
                self.synchronize();
                return None;
            }
        };
        let counter = Ident {
            name: self.text(counter_token).to_string(),
            span: counter_token.span,
        };
        // The counter is declared with `=`; `:=` here is a mistake.
        if self.at(TokenKind::ColonEq) {
            let t = self.peek();
            self.error(
                t.span,
                "a `for` loop counter is declared with `=`, not `:=`",
            );
            self.synchronize();
            return None;
        }
        if self.expect(TokenKind::Eq).is_none() {
            self.synchronize();
            return None;
        }
        let start = match self.parse_expr() {
            Some(e) => e,
            None => {
                self.synchronize();
                return None;
            }
        };
        if self.expect(TokenKind::KwTo).is_none() {
            self.synchronize();
            return None;
        }
        let end = match self.parse_expr() {
            Some(e) => e,
            None => {
                self.synchronize();
                return None;
            }
        };
        let step = if self.eat(TokenKind::KwBy).is_some() {
            match self.parse_expr() {
                Some(e) => Some(e),
                None => {
                    self.synchronize();
                    return None;
                }
            }
        } else {
            None
        };
        if self.expect(TokenKind::Newline).is_none() {
            self.synchronize();
            return None;
        }
        let body = self.parse_block()?;
        let end_span = body.last().map_or_else(
            || step.as_ref().map_or_else(|| end.span(), Expr::span),
            Stmt::span,
        );
        Some(Stmt::For(Box::new(ForStmt {
            counter,
            start,
            end,
            step,
            body,
            span: for_token.span.to(end_span),
        })))
    }

    /// `for element in arr` / `for [index, element] in arr` followed by an
    /// indented body. The loop variable(s) must be plain identifiers; the
    /// iterable is a full expression. The caller has confirmed the `in` shape.
    fn parse_for_in_stmt(&mut self) -> Option<Stmt> {
        let for_token = self.bump(); // `for`

        // The binding pattern: either `[index, element]` or a bare `element`.
        let (index, element) = if self.at(TokenKind::LBracket) {
            self.bump(); // `[`
            let index_token = match self.expect(TokenKind::Ident) {
                Some(t) => t,
                None => {
                    self.synchronize();
                    return None;
                }
            };
            let index = Ident {
                name: self.text(index_token).to_string(),
                span: index_token.span,
            };
            if self.expect(TokenKind::Comma).is_none() {
                self.synchronize();
                return None;
            }
            let element_token = match self.expect(TokenKind::Ident) {
                Some(t) => t,
                None => {
                    self.synchronize();
                    return None;
                }
            };
            let element = Ident {
                name: self.text(element_token).to_string(),
                span: element_token.span,
            };
            if self.expect(TokenKind::RBracket).is_none() {
                self.synchronize();
                return None;
            }
            (Some(index), element)
        } else {
            let element_token = match self.expect(TokenKind::Ident) {
                Some(t) => t,
                None => {
                    self.synchronize();
                    return None;
                }
            };
            let element = Ident {
                name: self.text(element_token).to_string(),
                span: element_token.span,
            };
            (None, element)
        };

        if self.expect(TokenKind::KwIn).is_none() {
            self.synchronize();
            return None;
        }
        let iterable = match self.parse_expr() {
            Some(e) => e,
            None => {
                self.synchronize();
                return None;
            }
        };
        if self.expect(TokenKind::Newline).is_none() {
            self.synchronize();
            return None;
        }
        let body = self.parse_block()?;
        let end_span = body.last().map_or_else(|| iterable.span(), Stmt::span);
        Some(Stmt::ForIn(Box::new(ForInStmt {
            index,
            element,
            iterable,
            body,
            span: for_token.span.to(end_span),
        })))
    }

    /// `while cond` followed by an indented body.
    fn parse_while_stmt(&mut self) -> Option<Stmt> {
        let while_token = self.bump(); // `while`
        let cond = match self.parse_expr() {
            Some(c) => c,
            None => {
                self.synchronize();
                return None;
            }
        };
        if self.expect(TokenKind::Newline).is_none() {
            self.synchronize();
            return None;
        }
        let body = self.parse_block()?;
        let end_span = body.last().map_or_else(|| cond.span(), Stmt::span);
        Some(Stmt::While(WhileStmt {
            cond,
            body,
            span: while_token.span.to(end_span),
        }))
    }

    /// `break` / `continue` — a keyword on its own line. Whether it is inside
    /// a loop is a semantic question (`E0223`), not a parse error.
    fn parse_break_or_continue(&mut self, is_break: bool) -> Option<Stmt> {
        let token = self.bump();
        self.expect_end_of_stmt();
        Some(if is_break {
            Stmt::Break { span: token.span }
        } else {
            Stmt::Continue { span: token.span }
        })
    }

    fn parse_block(&mut self) -> Option<Vec<Stmt>> {
        if self.expect(TokenKind::Indent).is_none() {
            self.synchronize();
            return None;
        }
        // Refuse to descend past the block-nesting cap: a deeper tree would
        // risk overflowing the stack here or in a later tree-walking pass.
        // Report once and skip the rest of this block (its `Indent` is already
        // consumed) instead of recursing into it.
        if self.block_depth >= MAX_BLOCK_DEPTH {
            let span = self.peek().span;
            self.error(span, "block is nested too deeply");
            self.skip_to_block_end();
            return None;
        }
        self.block_depth += 1;
        let mut stmts = Vec::new();
        while !self.at(TokenKind::Dedent) && !self.at(TokenKind::Eof) {
            if self.at(TokenKind::Newline) {
                self.bump();
                continue;
            }
            if let Some(stmt) = self.parse_stmt() {
                stmts.push(stmt);
            }
        }
        self.block_depth -= 1;
        self.eat(TokenKind::Dedent);
        Some(stmts)
    }

    /// After a block's opening `Indent` has been consumed, skip the rest of it
    /// — including any nested `Indent`/`Dedent` pairs — up to and including its
    /// matching `Dedent`, without building any AST. Used to bail out of an
    /// over-deep block (`MAX_BLOCK_DEPTH`) while still making forward progress,
    /// so the enclosing block's loop terminates.
    fn skip_to_block_end(&mut self) {
        let mut depth = 1u32;
        while depth > 0 {
            match self.peek_kind() {
                TokenKind::Indent => depth += 1,
                TokenKind::Dedent => depth -= 1,
                TokenKind::Eof => break,
                _ => {}
            }
            self.bump();
        }
    }

    // -- expressions (Pratt) ------------------------------------------------

    fn parse_expr(&mut self) -> Option<Expr> {
        pratt::parse_ternary(self)
    }

    fn parse_postfix(&mut self) -> Option<Expr> {
        let outer_reach = self.spine_begin();
        let expr = self.parse_postfix_chain(outer_reach);
        if expr.is_none() {
            // Nothing was built, but expressions that parsed before this one
            // (earlier statements of an enclosing `if` block, say) still
            // count toward the enclosing measure.
            self.spine_end(outer_reach, 1);
        }
        expr
    }

    fn parse_postfix_chain(&mut self, outer_reach: u32) -> Option<Expr> {
        let mut expr = self.parse_primary()?;
        // Postfix chaining is a loop, not recursion, so it never grows the
        // parser stack — but each `[...]`, `(...)`, `.method(...)` and
        // `.field` wraps `expr` in another AST node, and a long chain like
        // `a[1][1][1]...` or `a.b().c()...` builds a tree deep enough to
        // overflow the stack when it is later dropped or walked. `height`
        // (a local, so it can't leak across the `?` early returns below) is
        // the height of `expr`; together with the ambient recursion depth it
        // is bounded by `MAX_EXPR_DEPTH`.
        let mut height = self.spine_height();
        loop {
            if self.last_expr_block_terminated {
                break;
            }
            if matches!(
                self.peek_kind(),
                TokenKind::LBracket | TokenKind::LParen | TokenKind::Dot
            ) && !self.spine_fits(height + 1)
            {
                self.report_too_deep();
                return None;
            }
            match self.peek_kind() {
                TokenKind::LBracket => {
                    self.bump();
                    let offset = self.parse_expr()?;
                    let close = self.expect(TokenKind::RBracket)?;
                    height = (height + 1).max(self.spine_height());
                    let span = expr.span().to(close.span);
                    expr = Expr::History {
                        target: Box::new(expr),
                        offset: Box::new(offset),
                        span,
                    };
                }
                TokenKind::LParen => {
                    // Calls only chain directly off an identifier.
                    let Expr::Ident(callee) = expr else {
                        let t = self.peek();
                        self.error(t.span, "only named functions can be called");
                        return None;
                    };
                    self.bump();
                    let args = self.parse_call_args()?;
                    let close = self.expect(TokenKind::RParen)?;
                    height = self.spine_height();
                    let span = callee.span.to(close.span);
                    expr = Expr::Call {
                        callee,
                        args,
                        type_args: Vec::new(),
                        span,
                    };
                }
                // A `.method(args)` or `.field` chained off any expression,
                // including call and history results. Bare-identifier paths
                // such as `obj.child.value` are folded into Expr::Ident by
                // parse_primary; this arm covers the remaining suffix.
                TokenKind::Dot => {
                    self.bump(); // `.`
                    let Some(field_token) = self.eat_namespace_segment() else {
                        let found = self.peek();
                        self.error(
                            found.span,
                            format!(
                                "expected an identifier after `.`, found {}",
                                found.kind.describe()
                            ),
                        );
                        return None;
                    };
                    let field = Ident {
                        name: self.text(field_token).to_string(),
                        span: field_token.span,
                    };
                    if self.eat(TokenKind::LParen).is_some() {
                        let args = self.parse_call_args()?;
                        let close = self.expect(TokenKind::RParen)?;
                        height = (height + 1).max(self.spine_height());
                        let span = expr.span().to(close.span);
                        expr = Expr::MethodCall {
                            receiver: Box::new(expr),
                            method: field,
                            args,
                            span,
                        };
                    } else {
                        height += 1;
                        let span = expr.span().to(field.span);
                        expr = Expr::FieldAccess {
                            receiver: Box::new(expr),
                            field,
                            span,
                        };
                    }
                }
                _ => break,
            }
            // The suffix's own operand (an offset or an argument) can be the
            // taller side, so check again now that its height is known.
            if !self.spine_fits(height) {
                self.report_too_deep();
                return None;
            }
        }
        self.spine_end(outer_reach, height);
        Some(expr)
    }

    /// `[e0, e1, ...]` — a tuple expression. At least one element; a trailing
    /// comma is not allowed. Whether the tuple appears in a legal position
    /// (destructuring RHS / function-body tail) is a semantic check.
    fn parse_tuple_expr(&mut self) -> Option<Expr> {
        let open = self.bump(); // `[`
        let mut elements = Vec::new();
        loop {
            match self.parse_expr() {
                Some(e) => elements.push(e),
                None => {
                    self.synchronize();
                    return None;
                }
            }
            if self.eat(TokenKind::Comma).is_none() {
                break;
            }
        }
        let close = self.expect(TokenKind::RBracket)?;
        Some(Expr::Tuple {
            elements,
            span: open.span.to(close.span),
        })
    }

    fn parse_primary(&mut self) -> Option<Expr> {
        // Reset the block-terminated flag: only a block-form `if`/`switch`
        // expression (below) sets it, and it must reflect the *last* leaf
        // parsed for the enclosing statement expression.
        self.last_expr_block_terminated = false;
        let token = self.peek();
        match token.kind {
            // `if`/`switch` in expression position (an `if`/`switch` consumed
            // as a value). Their branch bodies are indented blocks whose last
            // statement supplies the branch value; semantic analysis extracts
            // it and desugars the whole form to a hidden result slot. The
            // closing `Dedent` terminates the logical line, so flag it.
            TokenKind::KwIf => {
                let e = self.parse_if_expr().map(|e| Expr::If(Box::new(e)));
                self.last_expr_block_terminated = e.is_some();
                e
            }
            TokenKind::KwSwitch => {
                let e = self.parse_switch().map(|s| Expr::Switch(Box::new(s)));
                self.last_expr_block_terminated = e.is_some();
                e
            }
            // Loops return the value of their final body expression, so
            // a loop can be the value of an assignment or argument. Reuse the
            // statement parser to preserve the same headers and block shape.
            TokenKind::KwFor | TokenKind::KwWhile => {
                let stmt = if token.kind == TokenKind::KwFor {
                    self.parse_for_stmt()
                } else {
                    self.parse_while_stmt()
                };
                let e = stmt.map(|stmt| Expr::Loop(Box::new(stmt)));
                self.last_expr_block_terminated = e.is_some();
                e
            }
            // `[e0, e1, ...]` — a tuple expression. Legal only as a
            // destructuring RHS or a function body's trailing value; using it
            // elsewhere is a semantic error (`E0225`), but it always parses
            // here so those positions can consume it.
            TokenKind::LBracket => self.parse_tuple_expr(),
            TokenKind::Number => {
                self.bump();
                let text = self.text(token);
                // A fraction or an exponent makes the literal a float
                // (`1.5`, `8.`, `10e10`); otherwise it is an exact int.
                if text.contains(['.', 'e', 'E']) {
                    match text.parse::<f64>() {
                        Ok(value) => Some(Expr::Float {
                            value,
                            span: token.span,
                        }),
                        Err(_) => {
                            self.error(token.span, format!("malformed number literal `{text}`"));
                            None
                        }
                    }
                } else {
                    // Int-form literals parse directly to i64 so the exact
                    // value reaches the runtime — no f64 round-trip.
                    match crate::integer_literal::parse(text.as_bytes()) {
                        Some(value) => Some(Expr::Int {
                            value,
                            span: token.span,
                        }),
                        None => {
                            self.error(token.span, "integer literal out of range");
                            None
                        }
                    }
                }
            }
            TokenKind::KwTrue => {
                self.bump();
                Some(Expr::Bool {
                    value: true,
                    span: token.span,
                })
            }
            TokenKind::KwFalse => {
                self.bump();
                Some(Expr::Bool {
                    value: false,
                    span: token.span,
                })
            }
            TokenKind::KwNa => {
                self.bump();
                if self.at(TokenKind::LParen) {
                    // `na(...)` is a call to the `na` builtin: yield an
                    // identifier so the postfix call machinery applies.
                    Some(Expr::Ident(Ident {
                        name: "na".to_string(),
                        span: token.span,
                    }))
                } else {
                    // Bare `na` is the not-available literal.
                    Some(Expr::Na { span: token.span })
                }
            }
            TokenKind::Str => {
                self.bump();
                let raw = self.text(token);
                let value = self.decode_string(raw, token.span)?;
                Some(Expr::Str {
                    value,
                    span: token.span,
                })
            }
            TokenKind::Color => {
                self.bump();
                // Lexer guarantees `#` + exactly 6 or 8 hex digits.
                let raw = self.text(token);
                let (r, g, b, t) = decode_color(raw);
                Some(Expr::Color {
                    r,
                    g,
                    b,
                    t,
                    span: token.span,
                })
            }
            TokenKind::Ident | TokenKind::KwType => {
                self.bump();
                // Fold `IDENT (Dot IDENT)*` into one identifier whose name
                // joins the parts with dots (`ta.sma`) and whose span covers
                // the whole path. Dotted names are only meaningful as
                // namespaced built-in references; resolution is semantic.
                let mut name = self.text(token).to_string();
                let mut span = token.span;
                while self.at(TokenKind::Dot) {
                    self.bump(); // `.`
                                 // A namespace segment is an identifier, or one of the type
                                 // keywords (`int`/`float`/`bool`/`string`/`enum`) — those double
                                 // as member names in the `input.*` namespace (`input.int`,
                                 // `input.float`, ..., `input.enum`). The keyword's raw source
                                 // text is the segment name.
                    match self.eat_namespace_segment() {
                        Some(part) => {
                            name.push('.');
                            name.push_str(self.text(part));
                            span = span.to(part.span);
                        }
                        None => {
                            let found = self.peek();
                            // Error tokens were already diagnosed by the lexer.
                            if found.kind != TokenKind::Error {
                                self.error(
                                    found.span,
                                    format!(
                                        "expected an identifier after `.`, found {}",
                                        found.kind.describe()
                                    ),
                                );
                            }
                            return None;
                        }
                    }
                }
                // Generic-constructor sugar: `array.new<float>()`,
                // `matrix.new<int>(r, c)`, `map.new<string, float>()`. A `<`
                // that immediately follows one of the registered
                // generic-constructor base names is a type-argument list, not
                // a comparison. An all-scalar list rewrites the name to the
                // typed registry constructor (`array.new_float`,
                // `map.new_string_float`); a list naming a user-defined type
                // carries the type arguments structurally on the call node.
                // Everywhere else `<`/`>` remain comparison operators.
                if self.at(TokenKind::Lt) {
                    if let Some((rewritten, type_args, gen_span)) =
                        self.parse_generic_constructor(&name, span)
                    {
                        if type_args.is_empty() {
                            // All-scalar: the rewritten name IS a registry
                            // constructor; the postfix loop wraps the call.
                            return Some(Expr::Ident(Ident {
                                name: rewritten,
                                span: gen_span,
                            }));
                        }
                        // A UDT element type: build the call here so the type
                        // arguments live on the node itself (a generic
                        // constructor is only meaningful when called).
                        if self.at(TokenKind::LParen) {
                            self.bump();
                            let args = self.parse_call_args()?;
                            let close = self.expect(TokenKind::RParen)?;
                            let call_span = gen_span.to(close.span);
                            return Some(Expr::Call {
                                callee: Ident {
                                    name: rewritten,
                                    span: gen_span,
                                },
                                args,
                                type_args,
                                span: call_span,
                            });
                        }
                        self.generic_type_arg_error(
                            &name,
                            gen_span,
                            format!("generic constructor `{name}<...>` must be called"),
                        );
                        return None;
                    } else if is_generic_constructor_base(&name) {
                        // A bad/missing type argument on a generic constructor:
                        // `parse_generic_constructor` reported E0234 and
                        // consumed what it could; do not fall through to treat
                        // the `<` as a comparison.
                        return None;
                    }
                }
                Some(Expr::Ident(Ident { name, span }))
            }
            // A type keyword immediately followed by `(` is a type-cast call
            // (`int(x)`, `float(x)`, `bool(x)`, `string(x)`). The keyword's
            // source text becomes the callee name; the postfix loop wraps this
            // `Expr::Ident` into an `Expr::Call`. Not followed by `(`, a type
            // keyword is not a value expression (it is a type annotation, which
            // the statement parser handles), so it falls through to the error
            // arm below.
            kind if Self::is_ident_kind(kind) => {
                self.bump();
                Some(Expr::Ident(Ident {
                    name: self.text(token).to_string(),
                    span: token.span,
                }))
            }
            TokenKind::LParen => {
                let open = self.bump();
                let inner = self.parse_expr()?;
                let close = self.expect(TokenKind::RParen)?;
                Some(Expr::Group {
                    inner: Box::new(inner),
                    span: open.span.to(close.span),
                })
            }
            // The lexer already diagnosed Error tokens; consume silently so
            // one bad character produces exactly one diagnostic.
            TokenKind::Error => {
                self.bump();
                None
            }
            // A stray `:` (e.g. `x ?: y`, or a lone `:` where a value is
            // expected) gets a targeted message: it is only valid between
            // the branches of a ternary.
            TokenKind::Colon => {
                self.diagnostics.push(
                    Diagnostic::error(codes::E0110, "expected an expression, found `:`")
                        .with_label(
                            token.span,
                            "`:` is only valid between the branches of `cond ? a : b` \
                         (or as part of `:=`)",
                        ),
                );
                None
            }
            _ => {
                self.error(
                    token.span,
                    format!("expected an expression, found {}", token.kind.describe()),
                );
                None
            }
        }
    }

    /// Parse a generic-constructor type-argument list following a base name
    /// (`array.new`, `matrix.new`, `map.new`). The caller has confirmed the
    /// current token is `<`. On success returns `(name, type_args, span)`
    /// covering base-name-through-`>`: an all-scalar list yields the typed
    /// registry-constructor name (`array.new_float`, `map.new_string_float`,
    /// ...) with **empty** `type_args`; a list naming a user-defined type
    /// yields the unmodified base name plus the structured type-argument
    /// idents for the caller to attach to the call node. Returns `None`
    /// (with an `E0234` reported when the base *is* a generic constructor)
    /// when the type-argument list is malformed. When the base name is not a
    /// generic constructor at all, returns `None` without consuming or
    /// reporting, so the `<` stays a comparison operator.
    fn parse_generic_constructor(
        &mut self,
        base: &str,
        base_span: Span,
    ) -> Option<(String, Vec<Ident>, Span)> {
        let expected_args = match base {
            "array.new" | "matrix.new" => 1,
            "map.new" => 2,
            // Not a generic constructor: leave `<` as a comparison operator.
            _ => return None,
        };
        // From here the base name IS a generic constructor, so any failure is
        // reported as E0234 and consumes tokens up to the closing `>` (or a
        // statement boundary) to resynchronize.
        self.bump(); // `<`
        let mut type_args: Vec<Ident> = Vec::new();
        loop {
            let tok = self.peek();
            let seg_start = tok.span;
            let mut seg_end = tok.span;
            let mut seg = match tok.kind {
                TokenKind::KwFloat => "float".to_string(),
                TokenKind::KwInt => "int".to_string(),
                TokenKind::KwBool => "bool".to_string(),
                TokenKind::KwString => "string".to_string(),
                // A bare identifier is `color` or a user-defined-type name (a
                // collection-of-UDTs element type). The name is validated
                // against the collected types in semantics; the parser accepts
                // any identifier here so the `<` is consumed as a type-argument
                // list rather than a comparison.
                TokenKind::Ident => self.text(tok).to_string(),
                _ => {
                    self.generic_type_arg_error(
                        base,
                        base_span.to(tok.span),
                        format!("expected a type argument, found {}", tok.kind.describe()),
                    );
                    self.recover_generic_args();
                    return None;
                }
            };
            self.bump();
            // A dotted name (`chart.point`) is a synthetic drawing type — append
            // the `.Ident` tail so the element type carries its namespaced name.
            while self.peek_kind() == TokenKind::Dot {
                self.bump(); // `.`
                let part = self.peek();
                if part.kind != TokenKind::Ident {
                    self.generic_type_arg_error(
                        base,
                        base_span.to(part.span),
                        format!(
                            "expected a type-name segment after `.`, found {}",
                            part.kind.describe()
                        ),
                    );
                    self.recover_generic_args();
                    return None;
                }
                seg.push('.');
                seg.push_str(self.text(part));
                seg_end = part.span;
                self.bump();
            }
            type_args.push(Ident {
                name: seg,
                span: seg_start.to(seg_end),
            });
            if self.eat(TokenKind::Comma).is_some() {
                continue;
            }
            break;
        }
        let close = match self.expect(TokenKind::Gt) {
            Some(t) => t,
            None => {
                self.generic_type_arg_error(
                    base,
                    base_span,
                    "unterminated `<...>` type-argument list".to_string(),
                );
                self.recover_generic_args();
                return None;
            }
        };
        if type_args.len() != expected_args {
            self.generic_type_arg_error(
                base,
                base_span.to(close.span),
                format!(
                    "`{base}` expects {expected_args} type argument{}, found {}",
                    if expected_args == 1 { "" } else { "s" },
                    type_args.len()
                ),
            );
            return None;
        }
        // When every type argument is a scalar element type, produce the
        // registry constructor name (`array.new_float`, `map.new_string_float`)
        // — those ARE registered functions, so the call carries no type args.
        // When any argument names a user-defined type (a collection-of-UDTs
        // element type), the constructor is not a registry entry: return the
        // base name plus the structured type-argument idents, which the
        // caller attaches to the call node so semantics dispatches on node
        // shape (no in-band string encoding).
        let full_span = base_span.to(close.span);
        let all_scalar = type_args.iter().all(|a| is_scalar_type_arg(&a.name));
        if all_scalar {
            let mangled = format!(
                "{base}_{}",
                type_args
                    .iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>()
                    .join("_")
            );
            Some((mangled, Vec::new(), full_span))
        } else {
            Some((base.to_string(), type_args, full_span))
        }
    }

    /// Report an `E0234` collection type-argument error at `span`.
    fn generic_type_arg_error(&mut self, _base: &str, span: Span, message: String) {
        self.diagnostics.push(
            Diagnostic::error(codes::E0234, message)
                .with_label(span, "in this generic constructor's type arguments"),
        );
    }

    /// After a malformed generic type-argument list, skip to the closing `>`
    /// (or a statement boundary) so the parser can resynchronize.
    fn recover_generic_args(&mut self) {
        loop {
            match self.peek_kind() {
                TokenKind::Gt => {
                    self.bump();
                    return;
                }
                TokenKind::Newline | TokenKind::Dedent | TokenKind::Eof | TokenKind::RParen => {
                    return;
                }
                _ => {
                    self.bump();
                }
            }
        }
    }

    /// Decode a raw string literal (with quotes) into its value. Both quote
    /// kinds delimit strings (`"…"` and `'…'`); the lexer guarantees the
    /// closing quote matches the opening one.
    fn decode_string(&mut self, raw: &str, span: Span) -> Option<String> {
        // A `"""..."""`/`'''...'''` multiline string keeps its line breaks
        // and indentation as text (a CRLF break becomes `\n`).
        let triple = raw
            .strip_prefix("\"\"\"")
            .and_then(|s| s.strip_suffix("\"\"\""))
            .or_else(|| raw.strip_prefix("'''").and_then(|s| s.strip_suffix("'''")));
        let body = triple
            .or_else(|| raw.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
            .or_else(|| raw.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
            .unwrap_or(raw);
        let mut value = String::with_capacity(body.len());
        let mut chars = body.chars().peekable();
        while let Some(c) = chars.next() {
            if triple.is_some() && matches!(c, '\n' | '\r') {
                if c == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                value.push('\n');
                continue;
            }
            if matches!(c, '\n' | '\r') {
                if c == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                // A line break inside a single-line string literal becomes one
                // space; spaces and tabs at the start of the continuation line
                // are dropped.
                while matches!(chars.peek(), Some(' ' | '\t')) {
                    chars.next();
                }
                value.push(' ');
                continue;
            }
            if c != '\\' {
                value.push(c);
                continue;
            }
            match chars.next() {
                Some('n') => value.push('\n'),
                Some('t') => value.push('\t'),
                Some('"') => value.push('"'),
                Some('\'') => value.push('\''),
                Some('\\') => value.push('\\'),
                // Only the escapes matched above have special meaning.
                // Other backslash-prefixed characters retain both characters
                // literally (e.g. `"\\s"` contains a backslash and `s`).
                Some(other) => {
                    value.push('\\');
                    value.push(other);
                }
                None => {
                    self.error(span, "string ends with a lone backslash".to_string());
                    return None;
                }
            }
        }
        Some(value)
    }
}

/// Whether `name` is a base name that takes generic type arguments
/// (`array.new<T>`, `matrix.new<T>`, `map.new<K, V>`). Only these positions
/// trigger `<type>` parsing; `<`/`>` remain comparison operators everywhere
/// else.
fn is_generic_constructor_base(name: &str) -> bool {
    matches!(name, "array.new" | "matrix.new" | "map.new")
}

/// True when a generic type argument names a built-in scalar element type
/// (as opposed to a user-defined type name).
fn is_scalar_type_arg(arg: &str) -> bool {
    matches!(arg, "int" | "float" | "bool" | "string" | "color")
}

/// Decode a validated `#RRGGBB`/`#RRGGBBAA` color literal into `(r, g, b, t)`.
/// The lexer guarantees the `#` prefix and exactly 6 or 8 hex digits, so this
/// cannot fail. The optional alpha byte maps to Pine transparency by
/// `t = 100 * (1 - alpha / 255)` (alpha 255 ⇒ t 0 opaque, alpha 0 ⇒
/// t 100 transparent); absent alpha is opaque (`t = 0`).
fn decode_color(raw: &str) -> (u8, u8, u8, f64) {
    let hex = raw.strip_prefix('#').unwrap_or(raw);
    let byte = |i: usize| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap_or(0);
    let r = byte(0);
    let g = byte(1);
    let b = byte(2);
    let t = if hex.len() == 8 {
        let alpha = f64::from(byte(3));
        100.0 * (1.0 - alpha / 255.0)
    } else {
        0.0
    };
    (r, g, b, t)
}

/// The parser state as the Pratt core ([`pratt`]) sees it: token kinds, the
/// expression-depth counter, diagnostics, and the AST constructors. Each
/// method is one step of the expression parser, called by the Pratt core.
impl pratt::PrattHost<Expr, Token> for Parser<'_> {
    fn kind_here(&self) -> TokenKind {
        self.peek_kind()
    }

    fn advance(&mut self) -> Token {
        self.bump()
    }

    fn block_ended(&self) -> bool {
        self.last_expr_block_terminated
    }

    fn depth(&self) -> u32 {
        self.expr_depth
    }

    fn set_depth(&mut self, depth: u32) {
        self.expr_depth = depth;
        self.expr_reach = self.expr_reach.max(depth);
    }

    fn report_too_deep(&mut self) {
        let span = self.peek().span;
        self.diagnostics.push(
            Diagnostic::error(codes::E0110, "expression is nested too deeply")
                .with_label(
                    span,
                    format!("the expression is more than {MAX_EXPR_DEPTH} levels deep here"),
                )
                .with_note(
                    "a long chain of operators or calls counts one level per link; assign part \
                     of the expression to a variable",
                ),
        );
    }

    fn spine_begin(&mut self) -> u32 {
        std::mem::take(&mut self.expr_reach)
    }

    fn spine_height(&mut self) -> u32 {
        let reach = std::mem::take(&mut self.expr_reach);
        reach.saturating_sub(self.expr_depth) + 1
    }

    fn spine_fits(&self, height: u32) -> bool {
        self.expr_depth.saturating_add(height) <= MAX_EXPR_DEPTH
    }

    fn spine_end(&mut self, outer_reach: u32, height: u32) {
        // `height >= 1`: the chain's result stands where its first operand
        // started, at the current depth.
        self.expr_reach = outer_reach.max(self.expr_depth + height - 1);
    }

    fn expect_colon(&mut self) -> bool {
        // `a ? b` with no `:` — expect() reports "expected `:`, found ...".
        self.expect(TokenKind::Colon).is_some()
    }

    fn operand(&mut self) -> Option<Expr> {
        self.parse_postfix()
    }

    fn mk_binary(op: BinaryOp, lhs: Expr, rhs: Expr) -> Expr {
        let span = lhs.span().to(rhs.span());
        Expr::Binary {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
            span,
        }
    }

    fn mk_unary(op: UnaryOp, op_token: Token, operand: Expr) -> Expr {
        let span = op_token.span.to(operand.span());
        Expr::Unary {
            op,
            operand: Box::new(operand),
            span,
        }
    }

    fn mk_ternary(cond: Expr, then_e: Expr, else_e: Expr) -> Expr {
        let span = cond.span().to(else_e.span());
        Expr::Ternary {
            cond: Box::new(cond),
            then_e: Box::new(then_e),
            else_e: Box::new(else_e),
            span,
        }
    }
}

/// The Pratt expression core: ternary, binary loop and prefix operators.
///
/// It is generic over the parser state ([`pratt::PrattHost`]) so that the
/// exact code the parser runs can be extracted with Charon/Aeneas. `Parser`
/// is the only production host. A postfix/primary expression (including a
/// parenthesized group, a call or an `if`/`switch` expression) is one opaque
/// operand here.
///
/// It is written without `?` (which lowers to the `Try` trait machinery) and
/// without `==` on `TokenKind` (`matches!` instead), to stay inside the
/// subset Aeneas translates without external models.
pub(crate) mod pratt {
    #![allow(clippy::question_mark)]

    use super::MAX_EXPR_DEPTH;
    use crate::precedence::{infix_binding_power, prefix_binding_power};
    use pinewasm_syntax::{BinaryOp, TokenKind, UnaryOp};

    /// What the Pratt core needs from the parser. `N` is the expression node
    /// type, `T` the token type.
    pub(crate) trait PrattHost<N, T> {
        /// The kind of the current token.
        fn kind_here(&self) -> TokenKind;
        /// Consume the current token and return it.
        fn advance(&mut self) -> T;
        /// The last operand ended with an indented block (`if`/`switch`
        /// expression), so the next token starts the next statement.
        fn block_ended(&self) -> bool;
        /// The expression nesting depth.
        fn depth(&self) -> u32;
        /// Set the expression nesting depth.
        fn set_depth(&mut self, depth: u32);
        /// Report "expression is nested too deeply" at the current token.
        fn report_too_deep(&mut self);
        /// Start measuring a chain: a tree that a loop builds by wrapping what
        /// it has so far, so that its height grows without any recursion.
        /// Returns the measure of the enclosing expression, for `spine_end`.
        fn spine_begin(&mut self) -> u32;
        /// An upper bound on the height, in nodes, of a tree whose root
        /// stands at the current depth and whose children are everything
        /// parsed since the last `spine_begin` or `spine_height`.
        fn spine_height(&mut self) -> u32;
        /// Whether a tree `height` nodes tall, rooted at the current depth,
        /// stays within `MAX_EXPR_DEPTH`.
        fn spine_fits(&self, height: u32) -> bool;
        /// Finish a chain whose result is `height` nodes tall, restoring the
        /// enclosing measure returned by `spine_begin`. A chain that failed
        /// ends with height 1, which only restores.
        fn spine_end(&mut self, outer_reach: u32, height: u32);
        /// Consume a `:`, or report its absence and return `false`.
        fn expect_colon(&mut self) -> bool;
        /// A postfix expression: a primary followed by calls, indexing and
        /// method calls.
        fn operand(&mut self) -> Option<N>;
        /// `lhs op rhs`.
        fn mk_binary(op: BinaryOp, lhs: N, rhs: N) -> N;
        /// `op operand`, where `op_token` is the operator's token.
        fn mk_unary(op: UnaryOp, op_token: T, operand: N) -> N;
        /// `cond ? then_e : else_e`.
        fn mk_ternary(cond: N, then_e: N, else_e: N) -> N;
    }

    /// The ternary conditional `cond ? a : b` — the loosest-binding
    /// expression form (below `or`), right-associative:
    /// `a ? b : c ? d : e` is `a ? b : (c ? d : e)`. Each ternary level
    /// counts against `MAX_EXPR_DEPTH` like any other nesting.
    pub(crate) fn parse_ternary<N, T, H: PrattHost<N, T>>(h: &mut H) -> Option<N> {
        if h.depth() >= MAX_EXPR_DEPTH {
            h.report_too_deep();
            return None;
        }
        h.set_depth(h.depth() + 1);
        let result = parse_ternary_inner(h);
        h.set_depth(h.depth() - 1);
        result
    }

    fn parse_ternary_inner<N, T, H: PrattHost<N, T>>(h: &mut H) -> Option<N> {
        let cond = match parse_expr_bp(h, 0) {
            Some(e) => e,
            None => return None,
        };
        // A block-form `if`/`switch` ended with its dedent: the next token
        // starts the next statement, so it cannot continue this expression.
        if h.block_ended() || !matches!(h.kind_here(), TokenKind::Question) {
            return Some(cond);
        }
        h.advance(); // `?`
                     // The then-branch is a full expression (it may itself be a
                     // ternary): the `:` separator delimits it.
        let then_e = match parse_ternary(h) {
            Some(e) => e,
            None => return None,
        };
        if !h.expect_colon() {
            return None;
        }
        let else_e = match parse_ternary(h) {
            Some(e) => e,
            None => return None,
        };
        Some(H::mk_ternary(cond, then_e, else_e))
    }

    /// Binding powers, loosest to tightest:
    /// or(1) < and(3) < ==/!=(5) < </<=/>/>=(7) < +/-(9) < */"/"/%(11)
    /// — see [`infix_binding_power`] and [`prefix_binding_power`].
    fn parse_expr_bp<N, T, H: PrattHost<N, T>>(h: &mut H, min_bp: u8) -> Option<N> {
        if h.depth() >= MAX_EXPR_DEPTH {
            h.report_too_deep();
            return None;
        }
        h.set_depth(h.depth() + 1);
        let outer_reach = h.spine_begin();
        let result = parse_expr_bp_inner(h, min_bp, outer_reach);
        if result.is_none() {
            h.spine_end(outer_reach, 1);
        }
        h.set_depth(h.depth() - 1);
        result
    }

    fn parse_expr_bp_inner<N, T, H: PrattHost<N, T>>(
        h: &mut H,
        min_bp: u8,
        outer_reach: u32,
    ) -> Option<N> {
        let mut lhs = match parse_unary(h) {
            Some(e) => e,
            None => return None,
        };
        // Each turn of the loop wraps `lhs` in one more binary node without
        // recursing, so a left-associative chain (`a + a + a ...`) grows the
        // tree by one level per operator. `height` is the height of `lhs`;
        // the recursion-depth guard alone would not bound it.
        let mut height = h.spine_height();
        loop {
            // A block-form `if`/`switch` ended with its dedent: the next
            // token starts the next statement.
            if h.block_ended() {
                break;
            }
            let Some((op, l_bp, r_bp)) = infix_binding_power(h.kind_here()) else {
                break;
            };
            if l_bp < min_bp {
                break;
            }
            if !h.spine_fits(height + 1) {
                h.report_too_deep();
                return None;
            }
            h.advance();
            let rhs = match parse_expr_bp(h, r_bp) {
                Some(e) => e,
                None => return None,
            };
            // The new node is one taller than `lhs`, unless the right
            // operand is the taller side.
            height += 1;
            let rhs_height = h.spine_height();
            if rhs_height > height {
                height = rhs_height;
            }
            if !h.spine_fits(height) {
                h.report_too_deep();
                return None;
            }
            lhs = H::mk_binary(op, lhs, rhs);
        }
        h.spine_end(outer_reach, height);
        Some(lhs)
    }

    /// A prefix `-`, `+` or `not` (operand parsed at the operator's binding
    /// power from [`prefix_binding_power`]), else a postfix expression.
    fn parse_unary<N, T, H: PrattHost<N, T>>(h: &mut H) -> Option<N> {
        let Some((op, r_bp)) = prefix_binding_power(h.kind_here()) else {
            return h.operand();
        };
        let op_token = h.advance();
        let operand = match parse_expr_bp(h, r_bp) {
            Some(e) => e,
            None => return None,
        };
        Some(H::mk_unary(op, op_token, operand))
    }
}

/// The production Pratt core ([`pratt`]) instantiated with a token-kind host
/// whose operand parser is the shape parser's primary (atoms and groups),
/// compared with the shape parser in [`crate::pratt_shape`].
/// Together with the parser-level differential tests in `pratt_shape`, this
/// ties the generic core to the shape model on the same inputs.
#[cfg(test)]
mod pratt_host_tests {
    use super::pratt::{parse_ternary, PrattHost};
    use crate::pratt_shape::{pratt_shape, ShapeNode};
    use pinewasm_syntax::{BinaryOp, TokenKind, UnaryOp};
    use proptest::prelude::*;

    /// Token kinds in, post-order node lists out (`N` is a subtree's list).
    struct ShapeHost<'a> {
        toks: &'a [TokenKind],
        pos: usize,
        depth: u32,
    }

    impl PrattHost<Vec<ShapeNode>, ()> for ShapeHost<'_> {
        fn kind_here(&self) -> TokenKind {
            self.toks.get(self.pos).copied().unwrap_or(TokenKind::Eof)
        }
        fn advance(&mut self) {
            self.pos += 1;
        }
        fn block_ended(&self) -> bool {
            false
        }
        fn depth(&self) -> u32 {
            self.depth
        }
        fn set_depth(&mut self, depth: u32) {
            self.depth = depth;
        }
        fn report_too_deep(&mut self) {}
        // The shape model bounds recursion depth only, so this host never
        // refuses a chain; the differential inputs are far shorter than the
        // parser's chain bound.
        fn spine_begin(&mut self) -> u32 {
            0
        }
        fn spine_height(&mut self) -> u32 {
            1
        }
        fn spine_fits(&self, _height: u32) -> bool {
            true
        }
        fn spine_end(&mut self, _outer_reach: u32, _height: u32) {}
        fn expect_colon(&mut self) -> bool {
            if self.kind_here() == TokenKind::Colon {
                self.pos += 1;
                true
            } else {
                false
            }
        }
        fn operand(&mut self) -> Option<Vec<ShapeNode>> {
            match self.kind_here() {
                TokenKind::Ident | TokenKind::Number | TokenKind::KwTrue | TokenKind::KwFalse => {
                    self.pos += 1;
                    Some(vec![ShapeNode::Atom])
                }
                TokenKind::LParen => {
                    self.pos += 1;
                    let mut inner = parse_ternary(self)?;
                    if self.kind_here() != TokenKind::RParen {
                        return None;
                    }
                    self.pos += 1;
                    inner.push(ShapeNode::Group);
                    Some(inner)
                }
                _ => None,
            }
        }
        fn mk_binary(op: BinaryOp, mut lhs: Vec<ShapeNode>, rhs: Vec<ShapeNode>) -> Vec<ShapeNode> {
            lhs.extend(rhs);
            lhs.push(ShapeNode::Binary(op));
            lhs
        }
        fn mk_unary(op: UnaryOp, (): (), mut operand: Vec<ShapeNode>) -> Vec<ShapeNode> {
            operand.push(ShapeNode::Unary(op));
            operand
        }
        fn mk_ternary(
            mut cond: Vec<ShapeNode>,
            then_e: Vec<ShapeNode>,
            else_e: Vec<ShapeNode>,
        ) -> Vec<ShapeNode> {
            cond.extend(then_e);
            cond.extend(else_e);
            cond.push(ShapeNode::Ternary);
            cond
        }
    }

    fn core_shape(toks: &[TokenKind]) -> Option<Vec<ShapeNode>> {
        let mut host = ShapeHost {
            toks,
            pos: 0,
            depth: 0,
        };
        let out = parse_ternary(&mut host)?;
        assert_eq!(host.depth, 0, "the core restores the depth counter");
        (host.pos == toks.len()).then_some(out)
    }

    fn model_shape(toks: &[TokenKind]) -> Option<Vec<ShapeNode>> {
        let mut out = Vec::new();
        pratt_shape(toks, &mut out).then_some(out)
    }

    const ALPHABET: &[TokenKind] = &[
        TokenKind::Ident,
        TokenKind::Number,
        TokenKind::KwAnd,
        TokenKind::KwOr,
        TokenKind::KwNot,
        TokenKind::Plus,
        TokenKind::Minus,
        TokenKind::Star,
        TokenKind::Slash,
        TokenKind::Percent,
        TokenKind::Lt,
        TokenKind::Le,
        TokenKind::Gt,
        TokenKind::Ge,
        TokenKind::EqEq,
        TokenKind::BangEq,
        TokenKind::Question,
        TokenKind::Colon,
        TokenKind::LParen,
        TokenKind::RParen,
        TokenKind::Comma,
        TokenKind::Eof,
    ];

    #[test]
    fn depth_limit_matches_shape_model() {
        use TokenKind as K;
        for n in 95..=105 {
            let mut toks = vec![K::LParen; n];
            toks.push(K::Ident);
            toks.extend(std::iter::repeat_n(K::RParen, n));
            assert_eq!(core_shape(&toks), model_shape(&toks), "{n} parentheses");
        }
        for n in 195..=202 {
            let mut toks = vec![K::Minus; n];
            toks.push(K::Ident);
            assert_eq!(core_shape(&toks), model_shape(&toks), "{n} prefix minus");
        }
    }

    #[test]
    fn long_flat_chain_is_a_loop() {
        // 100k operators: the binary loop does not recurse per operator (the
        // shape model's tail-recursive loop would exhaust the test thread's
        // stack here, so only the length and the last node are checked).
        let mut toks = vec![TokenKind::Ident];
        for _ in 0..100_000 {
            toks.push(TokenKind::Plus);
            toks.push(TokenKind::Ident);
        }
        let out = core_shape(&toks).expect("a flat chain parses");
        assert_eq!(out.len(), toks.len());
        assert_eq!(out.last(), Some(&ShapeNode::Binary(BinaryOp::Add)));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(4096))]

        /// Arbitrary token strings: the production core with the shape host
        /// accepts exactly when the shape model does, with the same output.
        #[test]
        fn core_matches_shape_model(
            toks in prop::collection::vec(prop::sample::select(ALPHABET), 0..24)
        ) {
            prop_assert_eq!(core_shape(&toks), model_shape(&toks), "tokens: {:?}", toks);
        }
    }
}

#[cfg(test)]
mod param_loop_extraction_controls {
    use super::*;

    fn parser(source: &str) -> Parser<'_> {
        let lexed = lex_with_source_id(source, SourceId::MAIN);
        Parser::new(
            source,
            lexed.tokens,
            lexed.diagnostics.into_iter().collect(),
        )
    }

    #[test]
    fn param_loop_preserves_order_defaults_and_empty_list() {
        for (source, names) in [
            ("f() => 1", vec![]),
            ("f(a) => a", vec!["a"]),
            ("f(a, b = 2, c = 3) => a", vec!["a", "b", "c"]),
        ] {
            let mut p = parser(source);
            let def = p.parse_func_def(false).expect("function parses");
            assert_eq!(
                def.params
                    .iter()
                    .map(|p| p.name.name.as_str())
                    .collect::<Vec<_>>(),
                names
            );
            assert!(p.diagnostics.is_empty(), "{source}: {:?}", p.diagnostics);
            assert_eq!(p.peek_kind(), TokenKind::Eof);
            assert_eq!(p.expr_depth, 0);
            if def.params.len() == 3 {
                assert!(def.params[0].default.is_none());
                assert!(matches!(
                    def.params[1].default,
                    Some(Expr::Int { value: 2, .. })
                ));
                assert!(matches!(
                    def.params[2].default,
                    Some(Expr::Int { value: 3, .. })
                ));
            }
        }
    }

    #[test]
    fn param_failure_preserves_actual_cursor_and_diagnostics() {
        for source in ["f(,) => 1", "f(a,) => 1", "f(a = ) => 1", "f(a, b = ) => 1"] {
            let mut expected = parser(source);
            expected.bump();
            expected.bump();
            // Parsing parameters one at a time yields the exact failure frame;
            // later closing-token checks must not run.
            loop {
                if expected.parse_param().is_none() {
                    break;
                }
                assert!(
                    expected.eat(TokenKind::Comma).is_some(),
                    "expected malformed parameter"
                );
            }
            let expected_cursor = expected.pos;
            let expected_diags = format!("{:?}", expected.diagnostics);
            let mut actual = parser(source);
            assert!(actual.parse_func_def(false).is_none());
            assert_eq!(actual.pos, expected_cursor, "{source}");
            assert_eq!(
                format!("{:?}", actual.diagnostics),
                expected_diags,
                "{source}"
            );
            assert!(!actual.diagnostics.is_empty());
            assert_eq!(actual.expr_depth, expected.expr_depth);
        }
    }
}

#[cfg(test)]
mod method_loop_extraction_controls {
    use super::*;

    fn parser(source: &str) -> Parser<'_> {
        let lexed = lex_with_source_id(source, SourceId::MAIN);
        Parser::new(
            source,
            lexed.tokens,
            lexed.diagnostics.into_iter().collect(),
        )
    }

    #[test]
    fn method_receiver_order_and_defaults() {
        for (source, names) in [
            ("method f(int this) => this", vec!["this"]),
            ("method f(int this, a, b = 2) => a", vec!["this", "a", "b"]),
        ] {
            let mut p = parser(source);
            let def = p.parse_method_def(true).expect("method parses");
            assert!(def.exported);
            assert!(def.receiver.is_some());
            assert!(def.params[0].ty.is_none());
            assert_eq!(
                def.params
                    .iter()
                    .map(|p| p.name.name.as_str())
                    .collect::<Vec<_>>(),
                names
            );
            if def.params.len() == 3 {
                assert!(def.params[1].default.is_none());
                assert!(matches!(
                    def.params[2].default,
                    Some(Expr::Int { value: 2, .. })
                ));
            }
            assert!(p.diagnostics.is_empty());
            assert_eq!(p.peek_kind(), TokenKind::Eof);
        }
    }

    #[test]
    fn method_tail_failure_preserves_parameter_frame() {
        for source in [
            "method f(int this,) => 1",
            "method f(int this, a,) => 1",
            "method f(int this, a = ) => 1",
            "method f(int this, a, b = ) => 1",
        ] {
            let mut expected = parser(source);
            expected.bump();
            expected.bump();
            expected.bump();
            let mut receiver = expected.parse_param().expect("receiver parses");
            assert!(receiver.ty.take().is_some());
            loop {
                assert!(
                    expected.eat(TokenKind::Comma).is_some(),
                    "malformed tail has comma"
                );
                if expected.parse_param().is_none() {
                    break;
                }
            }
            let mut actual = parser(source);
            assert!(actual.parse_method_def(false).is_none());
            assert_eq!(actual.pos, expected.pos, "{source}");
            assert_eq!(
                format!("{:?}", actual.diagnostics),
                format!("{:?}", expected.diagnostics),
                "{source}"
            );
            assert_eq!(actual.expr_depth, expected.expr_depth);
            assert_eq!(actual.block_depth, expected.block_depth);
            assert!(!actual.diagnostics.is_empty());
        }
    }

    #[test]
    fn method_requires_typed_receiver() {
        for source in ["method f() => 1", "method f(this) => 1"] {
            let mut p = parser(source);
            assert!(p.parse_method_def(false).is_none());
            assert!(!p.diagnostics.is_empty());
        }
    }
}

#[cfg(test)]
mod contextual_method_extraction_controls {
    use super::*;

    // Local copies of the demotion logic: these tests do not call the
    // production name predicate or share its lookahead implementation.
    fn reference_demote_contextual_method(tokens: &mut [Token]) {
        for i in 0..tokens.len() {
            if tokens[i].kind != TokenKind::KwMethod {
                continue;
            }
            let opens_def = tokens
                .get(i + 1)
                .is_some_and(|t| reference_is_method_name_kind(t.kind))
                && tokens.get(i + 2).map(|t| t.kind) == Some(TokenKind::LParen);
            if !opens_def {
                tokens[i].kind = TokenKind::Ident;
            }
        }
    }

    fn reference_is_method_name_kind(kind: TokenKind) -> bool {
        matches!(
            kind,
            TokenKind::Ident
                | TokenKind::KwType
                | TokenKind::KwInt
                | TokenKind::KwFloat
                | TokenKind::KwBool
                | TokenKind::KwString
        )
    }

    const ALL_KINDS: [TokenKind; 63] = [
        TokenKind::Ident,
        TokenKind::Number,
        TokenKind::Str,
        TokenKind::Color,
        TokenKind::KwIf,
        TokenKind::KwElse,
        TokenKind::KwTrue,
        TokenKind::KwFalse,
        TokenKind::KwAnd,
        TokenKind::KwOr,
        TokenKind::KwNot,
        TokenKind::KwVar,
        TokenKind::KwVarip,
        TokenKind::KwInt,
        TokenKind::KwFloat,
        TokenKind::KwBool,
        TokenKind::KwString,
        TokenKind::KwNa,
        TokenKind::KwFor,
        TokenKind::KwTo,
        TokenKind::KwBy,
        TokenKind::KwIn,
        TokenKind::KwWhile,
        TokenKind::KwBreak,
        TokenKind::KwContinue,
        TokenKind::KwSwitch,
        TokenKind::KwEnum,
        TokenKind::KwType,
        TokenKind::KwMethod,
        TokenKind::Plus,
        TokenKind::Minus,
        TokenKind::Star,
        TokenKind::Slash,
        TokenKind::Percent,
        TokenKind::PlusEq,
        TokenKind::MinusEq,
        TokenKind::StarEq,
        TokenKind::SlashEq,
        TokenKind::PercentEq,
        TokenKind::Lt,
        TokenKind::Le,
        TokenKind::Gt,
        TokenKind::Ge,
        TokenKind::EqEq,
        TokenKind::BangEq,
        TokenKind::Eq,
        TokenKind::ColonEq,
        TokenKind::FatArrow,
        TokenKind::Question,
        TokenKind::Colon,
        TokenKind::LParen,
        TokenKind::RParen,
        TokenKind::LBracket,
        TokenKind::RBracket,
        TokenKind::Comma,
        TokenKind::Dot,
        TokenKind::VersionDirective,
        TokenKind::AlertMessageDirective,
        TokenKind::Newline,
        TokenKind::Indent,
        TokenKind::Dedent,
        TokenKind::Eof,
        TokenKind::Error,
    ];

    fn tokens(kinds: &[TokenKind]) -> Vec<Token> {
        kinds
            .iter()
            .enumerate()
            .map(|(i, &kind)| Token {
                kind,
                span: Span::new(SourceId(17 + i as u32), (i * 7) as u32, (i * 7 + 3) as u32),
            })
            .collect()
    }

    fn assert_normalization(original: &[Token]) -> Vec<Token> {
        let mut expected = original.to_vec();
        reference_demote_contextual_method(&mut expected);
        let mut actual = original.to_vec();
        demote_contextual_method(&mut actual);
        assert_eq!(actual, expected, "local copy differs for {original:?}");
        assert_eq!(actual.len(), original.len());
        for (i, (before, after)) in original.iter().zip(&actual).enumerate() {
            assert_eq!(after.span, before.span, "span changed at {i}");
            let expected_kind = if before.kind == TokenKind::KwMethod {
                let opens = original.get(i + 1).is_some_and(|t| {
                    matches!(
                        t.kind,
                        TokenKind::Ident
                            | TokenKind::KwInt
                            | TokenKind::KwFloat
                            | TokenKind::KwBool
                            | TokenKind::KwString
                            | TokenKind::KwType
                    )
                }) && original
                    .get(i + 2)
                    .is_some_and(|t| t.kind == TokenKind::LParen);
                if opens {
                    TokenKind::KwMethod
                } else {
                    TokenKind::Ident
                }
            } else {
                before.kind
            };
            assert_eq!(
                after.kind, expected_kind,
                "original neighbors differ at {i}"
            );
        }
        let once = actual.clone();
        demote_contextual_method(&mut actual);
        assert_eq!(actual, once, "normalization is not idempotent");
        actual
    }

    #[test]
    fn all_kind_triples_and_short_streams() {
        assert_normalization(&[]);
        for a in ALL_KINDS {
            assert_normalization(&tokens(&[a]));
            for b in ALL_KINDS {
                assert_normalization(&tokens(&[a, b]));
                for c in ALL_KINDS {
                    assert_normalization(&tokens(&[a, b, c]));
                }
            }
        }
    }

    #[test]
    fn all_method_names_and_third_kind_contexts() {
        for name in ALL_KINDS {
            for third in ALL_KINDS {
                let actual = assert_normalization(&tokens(&[TokenKind::KwMethod, name, third]));
                let keep = matches!(
                    name,
                    TokenKind::Ident
                        | TokenKind::KwInt
                        | TokenKind::KwFloat
                        | TokenKind::KwBool
                        | TokenKind::KwString
                        | TokenKind::KwType
                ) && third == TokenKind::LParen;
                assert_eq!(actual[0].kind == TokenKind::KwMethod, keep);
            }
        }
    }

    #[test]
    fn future_original_tokens_and_adjacent_methods() {
        use TokenKind::{Ident, KwMethod, KwType, LParen, Newline, RParen};
        for kinds in [
            vec![KwMethod, KwMethod, Ident, LParen],
            vec![KwMethod, Ident, LParen, KwMethod],
            vec![KwMethod, KwType, LParen, RParen, KwMethod, Ident, LParen],
            vec![
                KwMethod, Ident, Newline, KwMethod, Ident, LParen, KwMethod, KwMethod,
            ],
        ] {
            assert_normalization(&tokens(&kinds));
        }
        let mut long = Vec::new();
        for kind in ALL_KINDS {
            long.extend([KwMethod, kind, LParen, KwMethod, KwMethod, Ident, LParen]);
        }
        assert_normalization(&tokens(&long));
    }

    #[test]
    fn tails_and_extreme_spans_are_unchanged() {
        let mut input = tokens(&[
            TokenKind::KwMethod,
            TokenKind::Ident,
            TokenKind::LParen,
            TokenKind::KwMethod,
            TokenKind::Ident,
            TokenKind::KwMethod,
        ]);
        input[0].span = Span::new(SourceId(u32::MAX), 0, u32::MAX);
        input[1].span = Span::new(SourceId(0), u32::MAX, u32::MAX);
        assert_normalization(&input);
        assert_normalization(&input[3..]);
        assert_normalization(&input[5..]);
    }

    fn normalize_source(source: &str) -> (Vec<Token>, Vec<Token>) {
        let lexed = lex_with_source_id(source, SourceId(91));
        let original = lexed.tokens;
        assert!(original.iter().all(|t| t.span.source == SourceId(91)));
        let actual = assert_normalization(&original);
        (original, actual)
    }

    #[test]
    fn actual_lexer_keeps_definition_contexts() {
        for name in ["f", "int", "float", "bool", "string", "type"] {
            for prefix in ["", "export "] {
                let source = format!("{prefix}method {name}(int this) => this");
                let (before, after) = normalize_source(&source);
                let index = before
                    .iter()
                    .position(|t| t.kind == TokenKind::KwMethod)
                    .unwrap();
                assert_eq!(after[index].kind, TokenKind::KwMethod, "{source}");
            }
        }
    }

    #[test]
    fn actual_lexer_demotes_nondefinition_contexts() {
        for source in [
            "method",
            "method f",
            "method f)",
            "method f\n(",
            "method = 1",
            "obj.method(1)",
            "method method(",
            "method if(",
            "method 1(",
            "method \"s\"(",
            "method [f](",
            "method\n",
        ] {
            let (before, after) = normalize_source(source);
            let method_indices: Vec<_> = before
                .iter()
                .enumerate()
                .filter_map(|(i, t)| (t.kind == TokenKind::KwMethod).then_some(i))
                .collect();
            assert!(!method_indices.is_empty(), "missing method in {source}");
            assert!(
                method_indices
                    .iter()
                    .all(|&i| after[i].kind == TokenKind::Ident),
                "{source}"
            );
        }
    }

    #[test]
    fn actual_lexer_mixed_lines_and_diagnostics_frame() {
        for source in [
            "method f(int this) => this\nmethod = 1\r\nobj.method(2)",
            "é method f(\n\tmethod = 2\n",
            "method @\nmethod type(\n",
            "method // tail",
        ] {
            let first = lex_with_source_id(source, SourceId(91));
            let original_diagnostics = first.diagnostics;
            let (_, after) = normalize_source(source);
            let second = lex_with_source_id(source, SourceId(91));
            assert_eq!(second.diagnostics, original_diagnostics);
            assert_eq!(after.last().unwrap().kind, TokenKind::Eof);
        }
    }

    #[test]
    fn actual_parse_entry_matches_frozen_normalization() {
        for source in [
            "method f(int this) => this\n",
            "export method float(int this) => this\n",
            "method = 1\nobj.method(2)\n",
            "method method(\n",
            "method f\n(\n",
            "method @\nmethod type(\n",
        ] {
            let source_id = SourceId(137);
            let mut lexed = lex_with_source_id(source, source_id);
            reference_demote_contextual_method(&mut lexed.tokens);
            let mut reference = Parser::new(
                source,
                lexed.tokens,
                lexed.diagnostics.into_iter().collect(),
            );
            let expected_program = reference.parse_program();
            let actual = parse_with_source_id(source, source_id);
            assert_eq!(actual.program, expected_program, "{source}");
            assert_eq!(
                actual.diagnostics,
                reference.diagnostics.finish(),
                "{source}"
            );
        }
    }
}
