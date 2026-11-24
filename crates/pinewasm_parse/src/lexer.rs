//! Hand-written, indentation-aware lexer.
//!
//! PineWASM (like Pine Script®) delimits blocks by indentation. The lexer follows
//! the classic Python model: it tracks a stack of indentation widths and
//! emits synthetic `Indent`/`Dedent` tokens at line starts, and a `Newline`
//! token at the end of every logical line. Blank lines and comment-only
//! lines produce no tokens.
//!
//! Tabs count as 4 columns (documented in `docs/language-support.md`).

use pinewasm_span::{codes, too_many_errors, Diagnostic, SourceId, Span, MAX_ERRORS};
use pinewasm_syntax::{Token, TokenKind};

const TAB_WIDTH: u32 = 4;

/// The most UTF-8 bytes one source may hold. Lexing allocates in proportion
/// to its input and every span is a pair of `u32` byte offsets, so the entry
/// points refuse a longer source with `E0001` before reading any of it. The
/// compiler applies the same cap to the main source and to each library.
pub const MAX_SOURCE_LEN: usize = 1 << 20;

// Build diagnostic text with explicit steps that stay in the Aeneas-extractable subset.
// A u64 has at most twenty decimal digits; each remainder is in 0..=9.
// Checked Rust indexing/arithmetic remains enabled in this numeric kernel.
fn unsigned_decimal_digits(mut value: u64) -> ([u8; 20], usize) {
    let mut digits = [0_u8; 20];
    let mut index = 20;
    while value >= 10 {
        index -= 1;
        digits[index] = b'0' + (value % 10) as u8;
        value /= 10;
    }
    index -= 1;
    digits[index] = b'0' + value as u8;
    (digits, index)
}

fn append_unsigned_decimal(text: &mut String, value: u64) {
    let (digits, mut index) = unsigned_decimal_digits(value);
    while index < 20 {
        text.push(char::from(digits[index]));
        index += 1;
    }
}

fn indentation_label(width: u32, landed: u32) -> String {
    let mut text = String::new();
    text.push_str("this line is indented ");
    append_unsigned_decimal(&mut text, u64::from(width));
    text.push_str(" columns; expected ");
    append_unsigned_decimal(&mut text, u64::from(landed));
    text
}

fn unexpected_character_message(character: &str) -> String {
    let mut text = String::new();
    text.push_str("unexpected character `");
    text.push_str(character);
    text.push('`');
    text
}

fn invalid_color_label(count: usize) -> String {
    let mut text = String::new();
    text.push_str("found ");
    // Exact on admitted targets with usize::BITS <= 64. Wider-pointer
    // extraction targets require a separate kernel rather than this cast.
    append_unsigned_decimal(&mut text, count as u64);
    text.push_str(" hex digit(s) after `#`");
    text
}

fn source_too_large_message(len: usize) -> String {
    let mut text = String::new();
    text.push_str("source is ");
    // Exact on admitted targets with usize::BITS <= 64, as above.
    append_unsigned_decimal(&mut text, len as u64);
    text.push_str(" bytes; the maximum supported size is ");
    append_unsigned_decimal(&mut text, MAX_SOURCE_LEN as u64);
    text
}

/// The `E0001` diagnostic for a source longer than [`MAX_SOURCE_LEN`]. It
/// carries no label, like the compiler's own input guards: nothing in a
/// buffer that was not read can be pointed at.
fn source_too_large_diagnostic(len: usize) -> Diagnostic {
    Diagnostic::error(codes::E0001, source_too_large_message(len))
}

// Construct an owned diagnostic before borrowing the destination vector.
fn inconsistent_indentation_diagnostic(
    source_id: SourceId,
    line_start: usize,
    indent_end: usize,
    width: u32,
    landed: u32,
) -> Diagnostic {
    let diagnostic = Diagnostic::error(
        codes::E0100,
        "inconsistent indentation: does not match any enclosing block",
    );
    let span = Span::new(source_id, line_start as u32, indent_end as u32);
    let label_message = indentation_label(width, landed);
    diagnostic.with_label(span, label_message)
}

// Report one diagnostic into a list that holds at most `MAX_ERRORS` of them.
// Every lexer diagnostic is an error, so the length of the list is the number
// of errors reported so far, up to the limit: the error that finds the list
// full is replaced by the one `E0002` that says the rest are left out, and
// every later one is dropped. A source of nothing but stray characters would
// otherwise hold a diagnostic for each of them.
//
// The conditional mutation works on an owned diagnostics vector, not on `Lexer<'s>`.
fn append_diagnostic(diagnostics: &mut Vec<Diagnostic>, diagnostic: Diagnostic) {
    let reported = diagnostics.len();
    if reported < MAX_ERRORS {
        diagnostics.push(diagnostic);
    } else if reported == MAX_ERRORS {
        diagnostics.push(too_many_errors(MAX_ERRORS));
    }
}

// The conditional mutation works on an owned diagnostics vector, not on `Lexer<'s>`.
fn append_inconsistent_indentation_diagnostic(
    diagnostics: &mut Vec<Diagnostic>,
    source_id: SourceId,
    line_start: usize,
    indent_end: usize,
    width: u32,
    landed: u32,
) {
    if landed != width {
        let diagnostic =
            inconsistent_indentation_diagnostic(source_id, line_start, indent_end, width, landed);
        append_diagnostic(diagnostics, diagnostic);
    }
}

pub struct LexResult {
    pub tokens: Vec<Token>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Lex `source`, attributing every token/diagnostic span to `SourceId::MAIN`.
pub fn lex(source: &str) -> LexResult {
    lex_with_source_id(source, SourceId::MAIN)
}

/// Lex `source`, attributing every token/diagnostic span to `source_id`. Used
/// so an imported library's spans carry the library's own source id rather than
/// being clamped onto the main buffer (see docs/browser.md).
///
/// A source longer than [`MAX_SOURCE_LEN`] is not read: the result is the
/// end-of-input token alone and one `E0001` diagnostic.
///
/// The diagnostics hold at most [`MAX_ERRORS`] errors. When the source has
/// more, an `E0002` after them says that the rest are left out; the tokens
/// are the same either way.
pub fn lex_with_source_id(source: &str, source_id: SourceId) -> LexResult {
    if source.len() > MAX_SOURCE_LEN {
        return LexResult {
            tokens: vec![Token::new(TokenKind::Eof, Span::new(source_id, 0, 0))],
            diagnostics: vec![source_too_large_diagnostic(source.len())],
        };
    }
    Lexer::new(source, source_id).run()
}

/// Lex without `"""..."""` multiline strings, which the byte kernel does not
/// know. The kernel differential tests compare this path against `RefLexer`,
/// which has no triple-quoted string rule either.
#[cfg(test)]
fn lex_without_triple_quoted_strings(source: &str, source_id: SourceId) -> LexResult {
    let mut lexer = Lexer::new(source, source_id);
    lexer.triple_quoted_strings = false;
    lexer.run()
}

struct Lexer<'s> {
    source: &'s str,
    bytes: &'s [u8],
    source_id: SourceId,
    pos: usize,
    tokens: Vec<Token>,
    diagnostics: Vec<Diagnostic>,
    indent_stack: Vec<u32>,
    /// Number of unmatched `(` tokens in the current statement. Wrapped lines
    /// inside parentheses may use any indentation, including block-width
    /// multiples and a column-zero closing delimiter.
    /// The parser remains responsible for reporting unmatched delimiters.
    paren_depth: u32,
    /// True while no real token has been produced on the current line.
    at_line_start: bool,
    /// `tokens.len()` right after line-start processing; used to detect
    /// whether a `//@version=` comment is the first thing on its line (a
    /// directive) or trailing a statement (a plain comment).
    line_start_token_count: usize,
    /// Whether `"""..."""`/`'''...'''` multiline strings are recognized.
    /// Always true outside the kernel differential tests.
    triple_quoted_strings: bool,
}

impl<'s> Lexer<'s> {
    fn new(source: &'s str, source_id: SourceId) -> Self {
        Lexer {
            source,
            bytes: source.as_bytes(),
            source_id,
            pos: 0,
            tokens: Vec::new(),
            diagnostics: Vec::new(),
            indent_stack: vec![0],
            paren_depth: 0,
            at_line_start: true,
            line_start_token_count: 0,
            triple_quoted_strings: true,
        }
    }

    fn span(&self, start: usize, end: usize) -> Span {
        Span::new(self.source_id, start as u32, end as u32)
    }

    fn push(&mut self, kind: TokenKind, start: usize, end: usize) {
        self.tokens.push(Token::new(kind, self.span(start, end)));
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.pos + offset).copied()
    }

    fn run(mut self) -> LexResult {
        loop {
            if self.at_line_start && !self.handle_line_start() {
                break;
            }
            if self.pos >= self.bytes.len() {
                break;
            }
            self.lex_token();
        }
        self.finish()
    }

    /// Measure indentation at the start of a line, skipping blank and
    /// comment-only lines entirely. Emits Indent/Dedent as needed.
    /// Returns false at end of input.
    fn handle_line_start(&mut self) -> bool {
        loop {
            let line_start = self.pos;
            let mut width: u32 = 0;
            while let Some(b) = self.peek() {
                match b {
                    b' ' => {
                        width += 1;
                        self.pos += 1;
                    }
                    b'\t' => {
                        width += TAB_WIDTH;
                        self.pos += 1;
                    }
                    _ => break,
                }
            }
            match self.peek() {
                None => return false,
                Some(b'\n') => {
                    self.pos += 1; // blank line
                    continue;
                }
                Some(b'\r') => {
                    self.pos += 1;
                    if self.peek() == Some(b'\n') {
                        self.pos += 1;
                    }
                    continue;
                }
                Some(b'/') if self.peek_at(1) == Some(b'/') => {
                    // Version and `//@strategy_alert_message` directives are real
                    // tokens even though they look like comments, and they
                    // participate in normal Indent/Dedent processing (an indented
                    // directive is a parse error, not a silently skipped line).
                    // Other comment-only lines are skipped entirely.
                    if !self.is_version_directive() && !self.is_alert_message_directive() {
                        self.skip_to_line_end();
                        continue;
                    }
                }
                Some(_) => {}
            }

            let indent_end = self.pos;
            // The lexer handles wrapped lines in two forms:
            //
            // * inside parentheses, wrapped lines may use any indentation;
            // * outside parentheses, the indentation must not be a multiple
            //   of the four-column block unit.
            //
            // Retract the Newline emitted by the preceding physical line and
            // keep lexing the same logical statement. Do not mutate the block
            // indent stack for either continuation form. With no preceding
            // Newline (start of input), ordinary indentation handling applies.
            if (self.paren_depth > 0 || width % TAB_WIDTH != 0)
                && self.tokens.last().map(|t| t.kind) == Some(TokenKind::Newline)
            {
                self.tokens.pop();
                break;
            }
            let current = *self.indent_stack.last().expect("indent stack never empty");
            match width.cmp(&current) {
                std::cmp::Ordering::Greater => {
                    self.indent_stack.push(width);
                    self.push(TokenKind::Indent, line_start, indent_end);
                }
                std::cmp::Ordering::Less => {
                    while *self.indent_stack.last().expect("indent stack never empty") > width {
                        self.indent_stack.pop();
                        self.push(TokenKind::Dedent, line_start, indent_end);
                    }
                    let landed = *self.indent_stack.last().expect("indent stack never empty");
                    append_inconsistent_indentation_diagnostic(
                        &mut self.diagnostics,
                        self.source_id,
                        line_start,
                        indent_end,
                        width,
                        landed,
                    );
                    // Recover by treating the line as being at the landed depth.
                }
                std::cmp::Ordering::Equal => {}
            }
            break;
        }
        self.at_line_start = false;
        self.line_start_token_count = self.tokens.len();
        true
    }

    fn is_version_directive(&self) -> bool {
        has_prefix(self.bytes, self.pos, &VERSION_DIRECTIVE)
    }

    fn is_alert_message_directive(&self) -> bool {
        has_prefix(self.bytes, self.pos, &ALERT_MESSAGE_DIRECTIVE)
    }

    fn skip_to_line_end(&mut self) {
        while let Some(b) = self.peek() {
            if b == b'\n' || b == b'\r' {
                break;
            }
            self.pos += 1;
        }
    }

    fn end_line(&mut self, newline_start: usize) {
        // Emit Newline only if the line held at least one real token.
        if !self.at_line_start {
            let end = self.pos.max(newline_start);
            self.push(TokenKind::Newline, newline_start, end);
        }
        self.at_line_start = true;
    }

    /// One `lex_token` step: the byte kernel [`tok_step`] decides how many
    /// bytes to consume, their class, the diagnostic code and the paren-depth
    /// update; this glue applies the decision (tokens, keyword resolution,
    /// diagnostic messages, spans).
    fn lex_token(&mut self) {
        let start = self.pos;
        if self.triple_quoted_strings && self.lex_triple_quoted_string(start) {
            return;
        }
        let first_on_line = self.tokens.len() == self.line_start_token_count;
        let step = tok_step(self.bytes, start, first_on_line, self.paren_depth);
        if step.len == 0 {
            return;
        }
        let end = start + step.len;
        match step.paren {
            ParenOp::Open => self.paren_depth += 1,
            ParenOp::Close => self.paren_depth = self.paren_depth.saturating_sub(1),
            ParenOp::Keep => {}
        }
        self.pos = end;
        match step.class {
            StepClass::Space | StepClass::Comment => {}
            StepClass::LineBreak => self.end_line(start),
            StepClass::Lexeme(TokenKind::Ident) => {
                let text = &self.source[start..end];
                let kind = TokenKind::keyword_from_str(text).unwrap_or(TokenKind::Ident);
                self.push(kind, start, end);
            }
            StepClass::Lexeme(kind) => self.push(kind, start, end),
        }
        match step.diag {
            None => {}
            Some(StepDiag::UnexpectedChar) => {
                let (message, label) = if self.bytes[start] == b'!' {
                    (unexpected_character_message("!"), "did you mean `!=`?")
                } else {
                    // One UTF-8 scalar: the span stays on a char boundary.
                    let ch = &self.source[start..end];
                    (unexpected_character_message(ch), "not valid in PineWASM")
                };
                let diagnostic = Diagnostic::error(codes::E0101, message)
                    .with_label(self.span(start, end), label);
                append_diagnostic(&mut self.diagnostics, diagnostic);
            }
            Some(StepDiag::UnterminatedString) => {
                let diagnostic = Diagnostic::error(codes::E0102, "unterminated string literal")
                    .with_label(self.span(start, end), "string starts here");
                append_diagnostic(&mut self.diagnostics, diagnostic);
            }
            Some(StepDiag::InvalidColor) => {
                let ndigits = step.len - 1;
                let diagnostic = Diagnostic::error(
                    codes::E0229,
                    "invalid color literal: expected `#RRGGBB` or `#RRGGBBAA`",
                )
                .with_label(self.span(start, end), invalid_color_label(ndigits));
                append_diagnostic(&mut self.diagnostics, diagnostic);
            }
        }
    }

    /// Lex a `"""..."""` (or `'''...'''`) multiline string if one starts at
    /// `start`, and report whether it did. Its line breaks and indentation are
    /// literal text. This lives outside the byte kernel, which only knows
    /// single-quoted strings (including their line-wrapped form).
    fn lex_triple_quoted_string(&mut self, start: usize) -> bool {
        let quote = self.bytes[start];
        if quote != b'"' && quote != b'\'' {
            return false;
        }
        if byte_is(self.bytes, start + 1, quote) && byte_is(self.bytes, start + 2, quote) {
            let end = match triple_string_end(self.bytes, start + 3, quote) {
                Some(end) => {
                    self.push(TokenKind::Str, start, end);
                    end
                }
                None => {
                    // Report the opening delimiter and resume after it.
                    let end = start + 3;
                    self.push(TokenKind::Error, start, end);
                    let diagnostic =
                        Diagnostic::error(codes::E0102, "unterminated multiline string literal")
                            .with_label(self.span(start, end), "string starts here");
                    append_diagnostic(&mut self.diagnostics, diagnostic);
                    end
                }
            };
            self.pos = end;
            return true;
        }
        false
    }

    fn finish(mut self) -> LexResult {
        let end = self.bytes.len();
        // Close the final line and any open blocks.
        if !self.at_line_start {
            self.push(TokenKind::Newline, end, end);
        }
        while self.indent_stack.len() > 1 {
            self.indent_stack.pop();
            self.push(TokenKind::Dedent, end, end);
        }
        self.push(TokenKind::Eof, end, end);
        LexResult {
            tokens: self.tokens,
            diagnostics: self.diagnostics,
        }
    }
}

// -- Byte kernel ---------------------------------------------------------------
//
// `tok_step` is the whole decision `lex_token` makes at one position, as a pure
// function of the source bytes. It is written in an Aeneas-extractable style.
// Everything that needs `&str` or `String` (keyword lookup, diagnostic
// messages) stays in `Lexer::lex_token`.

/// Class of the byte range one step consumes. Indentation has no class here:
/// only `handle_line_start` produces it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StepClass {
    /// A real token. Identifiers are reported as `Ident`; the glue resolves
    /// keywords with `TokenKind::keyword_from_str`.
    Lexeme(TokenKind),
    Space,
    LineBreak,
    Comment,
}

/// Diagnostic code of a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StepDiag {
    /// `E0101`
    UnexpectedChar,
    /// `E0102`
    UnterminatedString,
    /// `E0229`
    InvalidColor,
}

/// Paren-depth update of a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParenOp {
    Keep,
    Open,
    Close,
}

/// The decision of one `lex_token` step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TokStep {
    pub(crate) len: usize,
    pub(crate) class: StepClass,
    pub(crate) diag: Option<StepDiag>,
    pub(crate) paren: ParenOp,
}

/// Byte classes scanned by [`scan_while`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ByteClass {
    /// `0-9`
    Digit,
    /// `0-9a-fA-F`
    Hex,
    /// `A-Za-z0-9_`
    IdentCont,
    /// anything but `\n` and `\r`
    NotLineEnd,
    /// UTF-8 continuation bytes `0x80..=0xBF`
    Cont,
}

// Byte arrays and explicit comparisons (rather than byte-string literals and
// `RangeInclusive::contains`) stay within the std items that Aeneas extraction
// supports.
#[allow(clippy::byte_char_slices)]
const VERSION_DIRECTIVE: [u8; 11] = [
    b'/', b'/', b'@', b'v', b'e', b'r', b's', b'i', b'o', b'n', b'=',
];

#[allow(clippy::byte_char_slices)]
const ALERT_MESSAGE_DIRECTIVE: [u8; 26] = [
    b'/', b'/', b'@', b's', b't', b'r', b'a', b't', b'e', b'g', b'y', b'_', b'a', b'l', b'e', b'r',
    b't', b'_', b'm', b'e', b's', b's', b'a', b'g', b'e', b'=',
];

#[allow(clippy::manual_range_contains)]
fn is_digit_byte(b: u8) -> bool {
    b'0' <= b && b <= b'9'
}

#[allow(clippy::manual_range_contains)]
fn is_ident_start_byte(b: u8) -> bool {
    (b'A' <= b && b <= b'Z') || (b'a' <= b && b <= b'z') || b == b'_'
}

#[allow(clippy::manual_range_contains)]
fn in_class(class: ByteClass, b: u8) -> bool {
    match class {
        ByteClass::Digit => is_digit_byte(b),
        ByteClass::Hex => is_digit_byte(b) || (b'a' <= b && b <= b'f') || (b'A' <= b && b <= b'F'),
        ByteClass::IdentCont => is_ident_start_byte(b) || is_digit_byte(b),
        ByteClass::NotLineEnd => b != b'\n' && b != b'\r',
        ByteClass::Cont => 0x80 <= b && b < 0xC0,
    }
}

/// The first index `>= i` whose byte is not in `class` (or the length).
fn scan_while(bytes: &[u8], i: usize, class: ByteClass) -> usize {
    let mut j = i;
    while j < bytes.len() && in_class(class, bytes[j]) {
        j += 1;
    }
    j
}

/// `bytes[i] == c`, false past the end.
fn byte_is(bytes: &[u8], i: usize, c: u8) -> bool {
    i < bytes.len() && bytes[i] == c
}

/// `bytes[i]` is in `class`, false past the end.
fn byte_in(bytes: &[u8], i: usize, class: ByteClass) -> bool {
    i < bytes.len() && in_class(class, bytes[i])
}

/// `bytes[pos..]` starts with `lit` (`pos <= bytes.len()`).
fn has_prefix(bytes: &[u8], pos: usize, lit: &[u8]) -> bool {
    let avail = bytes.len() - pos;
    let mut k = 0;
    while k < lit.len() {
        if k >= avail || bytes[pos + k] != lit[k] {
            return false;
        }
        k += 1;
    }
    true
}

/// The fraction of `lex_number` at `i`: digits after a dot, or a trailing-dot
/// float (`8.`) unless an identifier or a second dot follows (`1.x`, `1..`).
/// Returns the end.
fn frac_end(bytes: &[u8], i: usize) -> usize {
    if !byte_is(bytes, i, b'.') {
        return i;
    }
    if byte_in(bytes, i + 1, ByteClass::Digit) {
        scan_while(bytes, i + 1, ByteClass::Digit)
    } else if i + 1 < bytes.len() && (is_ident_start_byte(bytes[i + 1]) || bytes[i + 1] == b'.') {
        i
    } else {
        i + 1
    }
}

/// The exponent of `lex_number` at `i`: `e`/`E`, an optional
/// sign, and at least one digit; otherwise nothing (`1e` / `1exit` keep the
/// `e` as the start of an identifier). Returns the end.
fn exp_end(bytes: &[u8], i: usize) -> usize {
    if !(byte_is(bytes, i, b'e') || byte_is(bytes, i, b'E')) {
        return i;
    }
    let mut k = i + 1;
    if byte_is(bytes, k, b'+') || byte_is(bytes, k, b'-') {
        k += 1;
    }
    if byte_in(bytes, k, ByteClass::Digit) {
        scan_while(bytes, k + 1, ByteClass::Digit)
    } else {
        i
    }
}

/// `lex_number` from `pos`: integer digits, an optional
/// fraction, an optional exponent. Returns the end.
/// Whether `b` can end an operand, so a `.` right after it is member access
/// rather than the start of a leading-dot float.
fn ends_operand(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b')' | b']')
}

fn number_end(bytes: &[u8], pos: usize) -> usize {
    let digits = scan_while(bytes, pos, ByteClass::Digit);
    let frac = frac_end(bytes, digits);
    exp_end(bytes, frac)
}

/// Return the first byte of a nonblank wrapped string line, consuming its
/// newline and indentation. Outside parentheses PineWASM requires continuation
/// indentation that is not a multiple of the four-column block width; inside
/// parentheses any indentation is valid. The string value receives one space
/// at this boundary, independent of the source indentation width.
fn wrapped_string_line(bytes: &[u8], newline: usize, paren_depth: u32) -> Option<usize> {
    let mut i = newline;
    match bytes.get(i).copied()? {
        b'\r' => {
            i += 1;
            if bytes.get(i) == Some(&b'\n') {
                i += 1;
            }
        }
        b'\n' => i += 1,
        _ => return None,
    }
    let mut indent = 0u32;
    while let Some(b) = bytes.get(i).copied() {
        match b {
            b' ' => {
                indent += 1;
                i += 1;
            }
            b'\t' => {
                indent += TAB_WIDTH;
                i += 1;
            }
            _ => break,
        }
    }
    if i == bytes.len() || matches!(bytes[i], b'\n' | b'\r') {
        return None;
    }
    (paren_depth > 0 || (indent > 0 && indent % TAB_WIDTH != 0)).then_some(i)
}

/// `lex_string` after the opening quote at `i`: the end of the literal and
/// whether the closing `quote` was found. PineWASM lets a quoted string
/// continue onto a wrapped source line; an ordinary statement
/// newline still terminates the string with a diagnostic.
fn string_end(bytes: &[u8], i: usize, quote: u8, paren_depth: u32) -> (usize, bool) {
    let mut j = i;
    while j < bytes.len() {
        let b = bytes[j];
        if matches!(b, b'\n' | b'\r') {
            if let Some(next) = wrapped_string_line(bytes, j, paren_depth) {
                j = next;
                continue;
            }
            return (j, false);
        }
        if b == b'\\' {
            if j + 1 >= bytes.len() || matches!(bytes[j + 1], b'\n' | b'\r') {
                return (j + 1, false);
            }
            j += 2;
        } else if b == quote {
            return (j + 1, true);
        } else {
            j += 1;
        }
    }
    (j, false)
}

/// The end of a `"""..."""` (or `'''...'''`) multiline string whose content
/// starts at `i`, or `None` when the closing triple quote is missing. A
/// backslash escapes the next byte, so `\"` never closes the string.
fn triple_string_end(bytes: &[u8], i: usize, quote: u8) -> Option<usize> {
    let mut j = i;
    while j < bytes.len() {
        if bytes[j] == b'\\' {
            j += 2;
        } else if bytes[j] == quote && byte_is(bytes, j + 1, quote) && byte_is(bytes, j + 2, quote)
        {
            return Some(j + 3);
        } else {
            j += 1;
        }
    }
    None
}

fn lexeme(len: usize, kind: TokenKind) -> TokStep {
    TokStep {
        len,
        class: StepClass::Lexeme(kind),
        diag: None,
        paren: ParenOp::Keep,
    }
}

fn one_or_two(bytes: &[u8], next: usize, one: TokenKind, two: TokenKind) -> TokStep {
    if byte_is(bytes, next, b'=') {
        lexeme(2, two)
    } else {
        lexeme(1, one)
    }
}

/// The decision `lex_token` makes at `pos`; `first_on_line` is
/// `tokens.len() == line_start_token_count`. At or past the end it consumes
/// nothing (`len == 0`); otherwise `len >= 1`.
pub(crate) fn tok_step(bytes: &[u8], pos: usize, first_on_line: bool, paren_depth: u32) -> TokStep {
    if pos >= bytes.len() {
        return TokStep {
            len: 0,
            class: StepClass::Space,
            diag: None,
            paren: ParenOp::Keep,
        };
    }
    let b = bytes[pos];
    let next = pos + 1;
    if b == b' ' || b == b'\t' {
        TokStep {
            len: 1,
            class: StepClass::Space,
            diag: None,
            paren: ParenOp::Keep,
        }
    } else if b == b'\n' {
        TokStep {
            len: 1,
            class: StepClass::LineBreak,
            diag: None,
            paren: ParenOp::Keep,
        }
    } else if b == b'\r' {
        let len = if byte_is(bytes, next, b'\n') { 2 } else { 1 };
        TokStep {
            len,
            class: StepClass::LineBreak,
            diag: None,
            paren: ParenOp::Keep,
        }
    } else if b == b'/' && byte_is(bytes, next, b'/') {
        // `//@version=` / `//@strategy_alert_message=` are directives only as
        // the first thing on their line; otherwise a plain comment.
        let len = scan_while(bytes, pos, ByteClass::NotLineEnd) - pos;
        if first_on_line && has_prefix(bytes, pos, &VERSION_DIRECTIVE) {
            lexeme(len, TokenKind::VersionDirective)
        } else if first_on_line && has_prefix(bytes, pos, &ALERT_MESSAGE_DIRECTIVE) {
            lexeme(len, TokenKind::AlertMessageDirective)
        } else {
            TokStep {
                len,
                class: StepClass::Comment,
                diag: None,
                paren: ParenOp::Keep,
            }
        }
    } else if is_digit_byte(b) {
        lexeme(number_end(bytes, pos) - pos, TokenKind::Number)
    } else if b == b'.'
        && bytes.get(next).copied().is_some_and(is_digit_byte)
        && !(pos > 0 && ends_operand(bytes[pos - 1]))
    {
        // A leading-dot float such as `.5`. Directly after an operand the dot
        // stays a member access, so `x.5` is still `x` `.` `5`.
        lexeme(number_end(bytes, pos) - pos, TokenKind::Number)
    } else if b == b'#' {
        // `#RRGGBB` / `#RRGGBBAA`; any other hex-digit count is E0229.
        let end = scan_while(bytes, next, ByteClass::Hex);
        let ndigits = end - next;
        if ndigits == 6 || ndigits == 8 {
            lexeme(end - pos, TokenKind::Color)
        } else {
            TokStep {
                len: end - pos,
                class: StepClass::Lexeme(TokenKind::Error),
                diag: Some(StepDiag::InvalidColor),
                paren: ParenOp::Keep,
            }
        }
    } else if b == b'"' || b == b'\'' {
        let (end, closed) = string_end(bytes, next, b, paren_depth);
        if closed {
            lexeme(end - pos, TokenKind::Str)
        } else {
            TokStep {
                len: end - pos,
                class: StepClass::Lexeme(TokenKind::Error),
                diag: Some(StepDiag::UnterminatedString),
                paren: ParenOp::Keep,
            }
        }
    } else if is_ident_start_byte(b) {
        lexeme(
            scan_while(bytes, pos, ByteClass::IdentCont) - pos,
            TokenKind::Ident,
        )
    } else if b == b'+' {
        one_or_two(bytes, next, TokenKind::Plus, TokenKind::PlusEq)
    } else if b == b'-' {
        one_or_two(bytes, next, TokenKind::Minus, TokenKind::MinusEq)
    } else if b == b'*' {
        one_or_two(bytes, next, TokenKind::Star, TokenKind::StarEq)
    } else if b == b'/' {
        one_or_two(bytes, next, TokenKind::Slash, TokenKind::SlashEq)
    } else if b == b'%' {
        one_or_two(bytes, next, TokenKind::Percent, TokenKind::PercentEq)
    } else if b == b'(' {
        TokStep {
            len: 1,
            class: StepClass::Lexeme(TokenKind::LParen),
            diag: None,
            paren: ParenOp::Open,
        }
    } else if b == b')' {
        TokStep {
            len: 1,
            class: StepClass::Lexeme(TokenKind::RParen),
            diag: None,
            paren: ParenOp::Close,
        }
    } else if b == b'[' {
        lexeme(1, TokenKind::LBracket)
    } else if b == b']' {
        lexeme(1, TokenKind::RBracket)
    } else if b == b',' {
        lexeme(1, TokenKind::Comma)
    } else if b == b'?' {
        lexeme(1, TokenKind::Question)
    } else if b == b'.' {
        // A dot reaches here only when the number rule did not consume it; it
        // joins namespaced names like `ta.sma`.
        lexeme(1, TokenKind::Dot)
    } else if b == b'<' {
        one_or_two(bytes, next, TokenKind::Lt, TokenKind::Le)
    } else if b == b'>' {
        one_or_two(bytes, next, TokenKind::Gt, TokenKind::Ge)
    } else if b == b'=' {
        // `==` (equality), `=>` (a switch arm), or `=` (declaration).
        if byte_is(bytes, next, b'=') {
            lexeme(2, TokenKind::EqEq)
        } else if byte_is(bytes, next, b'>') {
            lexeme(2, TokenKind::FatArrow)
        } else {
            lexeme(1, TokenKind::Eq)
        }
    } else if b == b'!' {
        if byte_is(bytes, next, b'=') {
            lexeme(2, TokenKind::BangEq)
        } else {
            TokStep {
                len: 1,
                class: StepClass::Lexeme(TokenKind::Error),
                diag: Some(StepDiag::UnexpectedChar),
                paren: ParenOp::Keep,
            }
        }
    } else if b == b':' {
        // `:=` always lexes as one token; a standalone `:` is the ternary
        // branch separator.
        one_or_two(bytes, next, TokenKind::Colon, TokenKind::ColonEq)
    } else {
        // One UTF-8 scalar: the lead byte and its continuation bytes, so a
        // multi-byte character produces a single diagnostic and the span
        // stays on a char boundary.
        TokStep {
            len: scan_while(bytes, next, ByteClass::Cont) - pos,
            class: StepClass::Lexeme(TokenKind::Error),
            diag: Some(StepDiag::UnexpectedChar),
            paren: ParenOp::Keep,
        }
    }
}

/// Differential tests: the lexer (byte kernel `tok_step` plus glue) against
/// `RefLexer`, a second lexer implementation used by these tests.
#[cfg(test)]
mod kernel_differential_tests {
    use super::{lex_without_triple_quoted_strings, tok_step, LexResult, StepClass, TAB_WIDTH};
    use pinewasm_span::{codes, Diagnostic, Diagnostics, SourceId, Span, MAX_ERRORS};
    use pinewasm_syntax::{Token, TokenKind};

    fn reference(source: &str, source_id: SourceId) -> LexResult {
        RefLexer::new(source, source_id).run()
    }

    fn assert_same(source: &str) {
        let id = SourceId::MAIN;
        let new = lex_without_triple_quoted_strings(source, id);
        let old = reference(source, id);
        assert_eq!(new.tokens, old.tokens, "tokens differ on {source:?}");
        assert_eq!(
            new.diagnostics, old.diagnostics,
            "diagnostics differ on {source:?}"
        );
    }

    const PIECES: &[&str] = &[
        " ",
        "  ",
        "\t",
        "\n",
        "\r",
        "\r\n",
        "    ",
        "//",
        "//@version=",
        "//@version=6",
        "//@strategy_alert_message=",
        "//@strategy_alert_message=x",
        "// c",
        "0",
        "12",
        "3.",
        "4.5",
        ".",
        "..",
        "e",
        "E",
        "e+",
        "e-",
        "1e5",
        "2.5e-3",
        "1exit",
        "#",
        "#ff00aa",
        "#FF00AA80",
        "#abc",
        "a",
        "Z",
        "_",
        "x1",
        "if",
        "else",
        "var",
        "varip",
        "method",
        "na",
        "\"",
        "'",
        "\\",
        "\\n",
        "\\\"",
        "+",
        "-",
        "*",
        "/",
        "%",
        "=",
        "==",
        "=>",
        "!",
        "!=",
        "<",
        "<=",
        ">",
        ">=",
        ":",
        ":=",
        "(",
        ")",
        "[",
        "]",
        ",",
        "?",
        "\u{a7}",
        "\u{e9}",
        "\u{4e2d}",
        "\u{1f600}",
        "\u{80}",
        "@",
        "$",
        "`",
        "~",
        "^",
        "&",
        "|",
        ";",
        "{",
        "}",
    ];

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    #[test]
    fn kernel_lexer_matches_reference_random() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for _ in 0..200_000 {
            let n = (rng.next() % 24) as usize;
            let mut s = String::new();
            for _ in 0..n {
                s.push_str(PIECES[(rng.next() % PIECES.len() as u64) as usize]);
            }
            assert_same(&s);
        }
    }

    #[test]
    fn kernel_lexer_matches_reference_random_scalars() {
        // Arbitrary Unicode scalars (any plane, combining
        // marks, BOM, controls) mixed with the lexer pieces.
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        for _ in 0..100_000 {
            let n = (rng.next() % 16) as usize;
            let mut s = String::new();
            for _ in 0..n {
                if rng.next() % 2 == 0 {
                    s.push_str(PIECES[(rng.next() % PIECES.len() as u64) as usize]);
                } else {
                    let c = match rng.next() % 3 {
                        0 => (rng.next() % 0x80) as u32,
                        1 => (rng.next() % 0x800) as u32,
                        _ => (rng.next() % 0x11_0000) as u32,
                    };
                    if let Some(c) = char::from_u32(c) {
                        s.push(c);
                    }
                }
            }
            assert_same(&s);
        }
    }

    #[test]
    fn kernel_lexer_matches_reference_all_short_ascii() {
        // Every string of length <= 3 over a 26-character alphabet that hits
        // every arm of `lex_token`.
        let alphabet: Vec<char> = " \t\n\r/@0.e+#a_\"'\\=!<:()%x9\u{e9}\u{1f600}"
            .chars()
            .collect();
        let mut stack = vec![String::new()];
        while let Some(s) = stack.pop() {
            assert_same(&s);
            if s.chars().count() < 3 {
                for c in &alphabet {
                    let mut t = s.clone();
                    t.push(*c);
                    stack.push(t);
                }
            }
        }
    }

    #[test]
    fn kernel_lexer_matches_reference_fixtures() {
        for src in [
            "//@version=6\nindicator(\"x\")\nx = close > open ? 1 : 0\n",
            "if a\n    b := 1\n  c\n",
            "f(a,\n  b)\n",
            "s = \"first line\n  continued line\"\n",
            "f(\"first line\r\n    continued line\")\n",
            "s = \"oops\nx = 1\n",
            "x = 1 \u{a7} 2",
            "x = 9223372036854775808",
            "//@strategy_alert_message=hi\n  //@version=6\n",
        ] {
            assert_same(src);
        }
    }

    /// `RefLexer` keeps every diagnostic. Past the limit the lexer
    /// must still produce its tokens exactly, and report what the shared
    /// bounded list would keep of its diagnostics: the first `MAX_ERRORS`
    /// and then one `E0002`.
    #[test]
    fn kernel_lexer_matches_reference_past_the_diagnostic_limit() {
        let id = SourceId::MAIN;
        for unit in [
            "$",
            "$ ",
            "\"\n",
            "'\n",
            "#12 ",
            "\u{a7}",
            "a\n        b\n                c\n            d\n",
            "$\n\"\n#1\na\n        b\n                c\n            d\n",
        ] {
            for count in [MAX_ERRORS - 1, MAX_ERRORS, MAX_ERRORS + 1, 3 * MAX_ERRORS] {
                let source = unit.repeat(count);
                let new = lex_without_triple_quoted_strings(&source, id);
                let old = reference(&source, id);
                assert!(old.diagnostics.len() >= count, "{unit:?}");
                assert!(old.diagnostics.iter().all(Diagnostic::is_error), "{unit:?}");
                assert_eq!(
                    new.tokens, old.tokens,
                    "tokens differ on {unit:?} x {count}"
                );
                let kept = old
                    .diagnostics
                    .into_iter()
                    .collect::<Diagnostics>()
                    .finish();
                assert_eq!(
                    new.diagnostics, kept,
                    "diagnostics differ on {unit:?} x {count}"
                );
                assert!(new.diagnostics.len() <= MAX_ERRORS + 1);
            }
        }
    }

    #[test]
    fn kernel_lexer_matches_reference_repo_pine_files() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if path.is_dir() {
                    if !name.starts_with('.') && !name.starts_with("target") {
                        walk(&path, out);
                    }
                } else if name.ends_with(".pine") {
                    out.push(path);
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut files = Vec::new();
        walk(&root, &mut files);
        assert!(!files.is_empty());
        for file in files {
            if let Ok(src) = std::fs::read_to_string(&file) {
                assert_same(&src);
            }
        }
    }

    #[test]
    fn kernel_step_consumes_at_least_one_byte_before_end() {
        let bytes = "x \u{1f600}\r\n#abc 'q".as_bytes();
        for pos in 0..=bytes.len() + 2 {
            for first in [false, true] {
                let step = tok_step(bytes, pos, first, 0);
                if pos < bytes.len() {
                    assert!(step.len >= 1 && pos + step.len <= bytes.len());
                } else {
                    assert_eq!(step.len, 0);
                    assert_eq!(step.class, StepClass::Space);
                }
            }
        }
    }

    // ---- RefLexer: second lexer implementation used for differential tests ----

    struct RefLexer<'s> {
        source: &'s str,
        bytes: &'s [u8],
        source_id: SourceId,
        pos: usize,
        tokens: Vec<Token>,
        diagnostics: Vec<Diagnostic>,
        indent_stack: Vec<u32>,
        /// Number of unmatched `(` tokens in the current statement. Wrapped
        /// lines inside parentheses may use any indentation, including
        /// block-width multiples and a column-zero closing delimiter.
        /// The parser remains responsible for reporting unmatched delimiters.
        paren_depth: u32,
        /// True while no real token has been produced on the current line.
        at_line_start: bool,
        /// `tokens.len()` right after line-start processing; used to detect
        /// whether a `//@version=` comment is the first thing on its line (a
        /// directive) or trailing a statement (a plain comment).
        line_start_token_count: usize,
    }

    impl<'s> RefLexer<'s> {
        fn new(source: &'s str, source_id: SourceId) -> Self {
            RefLexer {
                source,
                bytes: source.as_bytes(),
                source_id,
                pos: 0,
                tokens: Vec::new(),
                diagnostics: Vec::new(),
                indent_stack: vec![0],
                paren_depth: 0,
                at_line_start: true,
                line_start_token_count: 0,
            }
        }

        fn span(&self, start: usize, end: usize) -> Span {
            Span::new(self.source_id, start as u32, end as u32)
        }

        fn push(&mut self, kind: TokenKind, start: usize, end: usize) {
            self.tokens.push(Token::new(kind, self.span(start, end)));
        }

        fn peek(&self) -> Option<u8> {
            self.bytes.get(self.pos).copied()
        }

        fn peek_at(&self, offset: usize) -> Option<u8> {
            self.bytes.get(self.pos + offset).copied()
        }

        fn run(mut self) -> LexResult {
            loop {
                if self.at_line_start && !self.handle_line_start() {
                    break;
                }
                if self.pos >= self.bytes.len() {
                    break;
                }
                self.lex_token();
            }
            self.finish()
        }

        /// Measure indentation at the start of a line, skipping blank and
        /// comment-only lines entirely. Emits Indent/Dedent as needed.
        /// Returns false at end of input.
        fn handle_line_start(&mut self) -> bool {
            loop {
                let line_start = self.pos;
                let mut width: u32 = 0;
                while let Some(b) = self.peek() {
                    match b {
                        b' ' => {
                            width += 1;
                            self.pos += 1;
                        }
                        b'\t' => {
                            width += TAB_WIDTH;
                            self.pos += 1;
                        }
                        _ => break,
                    }
                }
                match self.peek() {
                    None => return false,
                    Some(b'\n') => {
                        self.pos += 1; // blank line
                        continue;
                    }
                    Some(b'\r') => {
                        self.pos += 1;
                        if self.peek() == Some(b'\n') {
                            self.pos += 1;
                        }
                        continue;
                    }
                    Some(b'/') if self.peek_at(1) == Some(b'/') => {
                        // Version and `//@strategy_alert_message` directives are real
                        // tokens even though they look like comments, and they
                        // participate in normal Indent/Dedent processing (an indented
                        // directive is a parse error, not a silently skipped line).
                        // Other comment-only lines are skipped entirely.
                        if !self.is_version_directive() && !self.is_alert_message_directive() {
                            self.skip_to_line_end();
                            continue;
                        }
                    }
                    Some(_) => {}
                }

                let indent_end = self.pos;
                // The lexer handles wrapped lines in two forms:
                //
                // * inside parentheses, wrapped lines may use any indentation;
                // * outside parentheses, the indentation must not be a multiple
                //   of the four-column block unit.
                //
                // Retract the Newline emitted by the preceding physical line and
                // keep lexing the same logical statement. Do not mutate the block
                // indent stack for either continuation form. With no preceding
                // Newline (start of input), ordinary indentation handling applies.
                if (self.paren_depth > 0 || width % TAB_WIDTH != 0)
                    && self.tokens.last().map(|t| t.kind) == Some(TokenKind::Newline)
                {
                    self.tokens.pop();
                    break;
                }
                let current = *self.indent_stack.last().expect("indent stack never empty");
                match width.cmp(&current) {
                    std::cmp::Ordering::Greater => {
                        self.indent_stack.push(width);
                        self.push(TokenKind::Indent, line_start, indent_end);
                    }
                    std::cmp::Ordering::Less => {
                        while *self.indent_stack.last().expect("indent stack never empty") > width {
                            self.indent_stack.pop();
                            self.push(TokenKind::Dedent, line_start, indent_end);
                        }
                        let landed = *self.indent_stack.last().expect("indent stack never empty");
                        if landed != width {
                            self.diagnostics.push(
                                Diagnostic::error(
                                    codes::E0100,
                                    "inconsistent indentation: does not match any enclosing block",
                                )
                                .with_label(
                                    self.span(line_start, indent_end),
                                    format!(
                                        "this line is indented {width} columns; expected {landed}"
                                    ),
                                ),
                            );
                            // Recover by treating the line as being at the landed depth.
                        }
                    }
                    std::cmp::Ordering::Equal => {}
                }
                break;
            }
            self.at_line_start = false;
            self.line_start_token_count = self.tokens.len();
            true
        }

        fn is_version_directive(&self) -> bool {
            self.source[self.pos..].starts_with("//@version=")
        }

        fn is_alert_message_directive(&self) -> bool {
            self.source[self.pos..].starts_with("//@strategy_alert_message=")
        }

        fn skip_to_line_end(&mut self) {
            while let Some(b) = self.peek() {
                if b == b'\n' || b == b'\r' {
                    break;
                }
                self.pos += 1;
            }
        }

        fn end_line(&mut self, newline_start: usize) {
            // Emit Newline only if the line held at least one real token.
            if !self.at_line_start {
                let end = self.pos.max(newline_start);
                self.push(TokenKind::Newline, newline_start, end);
            }
            self.at_line_start = true;
        }

        fn lex_token(&mut self) {
            let start = self.pos;
            let b = match self.peek() {
                Some(b) => b,
                None => return,
            };
            match b {
                b' ' | b'\t' => {
                    self.pos += 1;
                }
                b'\n' => {
                    self.pos += 1;
                    self.end_line(start);
                }
                b'\r' => {
                    self.pos += 1;
                    if self.peek() == Some(b'\n') {
                        self.pos += 1;
                    }
                    self.end_line(start);
                }
                b'/' if self.peek_at(1) == Some(b'/') => {
                    // `//@version=` counts as a directive only at the start of
                    // its line; trailing a statement it is an ordinary comment.
                    let first_on_line = self.tokens.len() == self.line_start_token_count;
                    if first_on_line && self.is_version_directive() {
                        self.skip_to_line_end();
                        self.push(TokenKind::VersionDirective, start, self.pos);
                    } else if first_on_line && self.is_alert_message_directive() {
                        self.skip_to_line_end();
                        self.push(TokenKind::AlertMessageDirective, start, self.pos);
                    } else {
                        self.skip_to_line_end();
                    }
                }
                b'0'..=b'9' => self.lex_number(),
                b'.' if matches!(self.peek_at(1), Some(b'0'..=b'9'))
                    && !(self.pos > 0 && crate::lexer::ends_operand(self.bytes[self.pos - 1])) =>
                {
                    self.lex_number();
                }
                b'#' => self.lex_color(),
                b'"' | b'\'' => self.lex_string(),
                b'A'..=b'Z' | b'a'..=b'z' | b'_' => self.lex_ident(),
                b'+' => self.one_or_two(TokenKind::Plus, b'=', TokenKind::PlusEq),
                b'-' => self.one_or_two(TokenKind::Minus, b'=', TokenKind::MinusEq),
                b'*' => self.one_or_two(TokenKind::Star, b'=', TokenKind::StarEq),
                b'/' => self.one_or_two(TokenKind::Slash, b'=', TokenKind::SlashEq),
                b'%' => self.one_or_two(TokenKind::Percent, b'=', TokenKind::PercentEq),
                b'(' => {
                    self.paren_depth += 1;
                    self.single(TokenKind::LParen);
                }
                b')' => {
                    self.paren_depth = self.paren_depth.saturating_sub(1);
                    self.single(TokenKind::RParen);
                }
                b'[' => self.single(TokenKind::LBracket),
                b']' => self.single(TokenKind::RBracket),
                b',' => self.single(TokenKind::Comma),
                b'?' => self.single(TokenKind::Question),
                // A dot reaches here only when the number rule did not consume
                // it (fractions are handled inside `lex_number`); it joins
                // namespaced names like `ta.sma`.
                b'.' => self.single(TokenKind::Dot),
                b'<' => self.one_or_two(TokenKind::Lt, b'=', TokenKind::Le),
                b'>' => self.one_or_two(TokenKind::Gt, b'=', TokenKind::Ge),
                b'=' => {
                    // `=` may open `==` (equality) or `=>` (a switch arm); a lone
                    // `=` is declaration/assignment.
                    match self.peek_at(1) {
                        Some(b'=') => {
                            self.pos += 2;
                            self.push(TokenKind::EqEq, start, self.pos);
                        }
                        Some(b'>') => {
                            self.pos += 2;
                            self.push(TokenKind::FatArrow, start, self.pos);
                        }
                        _ => self.single(TokenKind::Eq),
                    }
                }
                b'!' => {
                    if self.peek_at(1) == Some(b'=') {
                        self.pos += 2;
                        self.push(TokenKind::BangEq, start, self.pos);
                    } else {
                        self.pos += 1;
                        self.push(TokenKind::Error, start, self.pos);
                        self.diagnostics.push(
                            Diagnostic::error(codes::E0101, "unexpected character `!`")
                                .with_label(self.span(start, self.pos), "did you mean `!=`?"),
                        );
                    }
                }
                // `:=` always lexes as one token; a standalone `:` is the
                // ternary branch separator (valid only there — the parser
                // rejects strays with a targeted diagnostic).
                b':' => self.one_or_two(TokenKind::Colon, b'=', TokenKind::ColonEq),
                _ => {
                    // Consume one UTF-8 scalar so multi-byte characters produce a
                    // single diagnostic and the span stays on a char boundary.
                    let ch_len = self.source[self.pos..]
                        .chars()
                        .next()
                        .map_or(1, char::len_utf8);
                    let ch: String = self.source[self.pos..self.pos + ch_len].into();
                    self.pos += ch_len;
                    self.push(TokenKind::Error, start, self.pos);
                    self.diagnostics.push(
                        Diagnostic::error(codes::E0101, format!("unexpected character `{ch}`"))
                            .with_label(self.span(start, self.pos), "not valid in PineWASM"),
                    );
                }
            }
        }

        fn single(&mut self, kind: TokenKind) {
            let start = self.pos;
            self.pos += 1;
            self.push(kind, start, self.pos);
        }

        fn one_or_two(&mut self, one: TokenKind, second: u8, two: TokenKind) {
            let start = self.pos;
            self.pos += 1;
            if self.peek() == Some(second) {
                self.pos += 1;
                self.push(two, start, self.pos);
            } else {
                self.push(one, start, self.pos);
            }
        }

        fn lex_number(&mut self) {
            let start = self.pos;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
            // A fractional part: digits after the dot, or a trailing-dot float
            // (`8.`, which PineWASM reads as a float equal to `8.0`). The dot stays a separate
            // token when an identifier follows (`1.x` would otherwise swallow a
            // method/member name) or when a second dot follows.
            if self.peek() == Some(b'.') {
                let after = self.peek_at(1);
                if matches!(after, Some(b'0'..=b'9')) {
                    self.pos += 1;
                    while matches!(self.peek(), Some(b'0'..=b'9')) {
                        self.pos += 1;
                    }
                } else if !matches!(after, Some(b'A'..=b'Z' | b'a'..=b'z' | b'_' | b'.')) {
                    self.pos += 1; // trailing-dot float: `8.`
                }
            }
            // Optional exponent (`1e5`, `2.5e-3`, `10e+10`): consumed only when
            // digits follow the optional sign, so `1e` / `1exit` keep the `e` as
            // the start of an identifier.
            if matches!(self.peek(), Some(b'e' | b'E')) {
                let mut i = self.pos + 1;
                if matches!(self.bytes.get(i), Some(b'+' | b'-')) {
                    i += 1;
                }
                if matches!(self.bytes.get(i), Some(b'0'..=b'9')) {
                    self.pos = i + 1;
                    while matches!(self.peek(), Some(b'0'..=b'9')) {
                        self.pos += 1;
                    }
                }
            }
            self.push(TokenKind::Number, start, self.pos);
        }

        /// Lex a hex color literal: `#` followed by exactly 6 (`#RRGGBB`) or 8
        /// (`#RRGGBBAA`) hex digits. Any other run of hex-ish characters after `#`
        /// is a color-literal error (`E0229`). The token span covers the `#` and
        /// every consumed hex digit; the parser decodes the digits.
        fn lex_color(&mut self) {
            let start = self.pos;
            self.pos += 1; // the `#`
            let digits_start = self.pos;
            while matches!(self.peek(), Some(b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F')) {
                self.pos += 1;
            }
            let ndigits = self.pos - digits_start;
            if ndigits == 6 || ndigits == 8 {
                self.push(TokenKind::Color, start, self.pos);
            } else {
                self.push(TokenKind::Error, start, self.pos);
                self.diagnostics.push(
                    Diagnostic::error(
                        codes::E0229,
                        "invalid color literal: expected `#RRGGBB` or `#RRGGBBAA`",
                    )
                    .with_label(
                        self.span(start, self.pos),
                        format!("found {ndigits} hex digit(s) after `#`"),
                    ),
                );
            }
        }

        /// Lex a string literal delimited by the quote character at the current
        /// position — `"` or `'` (PineWASM accepts both; the closing quote must match
        /// the opening one, and the other quote kind is an ordinary character
        /// inside the literal).
        fn lex_string(&mut self) {
            let start = self.pos;
            let quote = self.bytes[self.pos];
            self.pos += 1; // opening quote
            loop {
                match self.peek() {
                    None => {
                        self.push(TokenKind::Error, start, self.pos);
                        self.diagnostics.push(
                            Diagnostic::error(codes::E0102, "unterminated string literal")
                                .with_label(self.span(start, self.pos), "string starts here"),
                        );
                        return;
                    }
                    Some(b'\n' | b'\r') => {
                        if let Some(next) =
                            super::wrapped_string_line(self.bytes, self.pos, self.paren_depth)
                        {
                            self.pos = next;
                        } else {
                            // Leave an ordinary statement newline for line handling.
                            self.push(TokenKind::Error, start, self.pos);
                            self.diagnostics.push(
                                Diagnostic::error(codes::E0102, "unterminated string literal")
                                    .with_label(self.span(start, self.pos), "string starts here"),
                            );
                            return;
                        }
                    }
                    Some(b'\\') => {
                        self.pos += 1;
                        if matches!(self.peek(), Some(b'\n' | b'\r')) {
                            self.push(TokenKind::Error, start, self.pos);
                            self.diagnostics.push(
                                Diagnostic::error(codes::E0102, "unterminated string literal")
                                    .with_label(self.span(start, self.pos), "string starts here"),
                            );
                            return;
                        }
                        // Skip the escaped character (validated during parsing).
                        if self.peek().is_some() {
                            self.pos += 1;
                        }
                    }
                    Some(b) if b == quote => {
                        self.pos += 1;
                        break;
                    }
                    Some(_) => {
                        self.pos += 1;
                    }
                }
            }
            self.push(TokenKind::Str, start, self.pos);
        }

        fn lex_ident(&mut self) {
            let start = self.pos;
            while matches!(
                self.peek(),
                Some(b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_')
            ) {
                self.pos += 1;
            }
            let text = &self.source[start..self.pos];
            let kind = TokenKind::keyword_from_str(text).unwrap_or(TokenKind::Ident);
            self.push(kind, start, self.pos);
        }

        fn finish(mut self) -> LexResult {
            let end = self.bytes.len();
            // Close the final line and any open blocks.
            if !self.at_line_start {
                self.push(TokenKind::Newline, end, end);
            }
            while self.indent_stack.len() > 1 {
                self.indent_stack.pop();
                self.push(TokenKind::Dedent, end, end);
            }
            self.push(TokenKind::Eof, end, end);
            LexResult {
                tokens: self.tokens,
                diagnostics: self.diagnostics,
            }
        }
    }
}

#[cfg(test)]
mod indentation_diagnostic_tests {
    use super::*;

    #[test]
    fn owned_diagnostic_matches_original_construction() {
        for (start, end, width, landed) in [
            (0, 12, 12, 8),
            (31, 35, 4, 0),
            (usize::MAX, usize::MAX, u32::MAX, 0),
        ] {
            let expected = Diagnostic::error(
                codes::E0100,
                "inconsistent indentation: does not match any enclosing block",
            )
            .with_label(
                Span::new(SourceId::MAIN, start as u32, end as u32),
                format!("this line is indented {width} columns; expected {landed}"),
            );
            assert_eq!(
                inconsistent_indentation_diagnostic(SourceId::MAIN, start, end, width, landed),
                expected,
            );
        }
    }

    #[test]
    fn mismatch_reports_landed_depth_but_continuation_does_not() {
        let mismatch = lex("a\n        b\n                c\n            d\n");
        assert_eq!(mismatch.diagnostics.len(), 1);
        assert_eq!(
            mismatch.diagnostics[0].labels[0].message,
            "this line is indented 12 columns; expected 8",
        );
        let matching = lex("a\n        b\n                c\n        d\n");
        assert!(matching.diagnostics.is_empty());
        let continuation = lex("a\n        b\n      c\n");
        assert!(continuation.diagnostics.is_empty());
    }

    #[test]
    fn append_boundary_preserves_false_branch_and_diagnostic_order() {
        let mut actual = vec![Diagnostic::error(codes::E0100, "earlier diagnostic")];
        actual.reserve(4);
        let expected_prefix = actual.clone();
        let pointer = actual.as_ptr();
        let capacity = actual.capacity();
        append_inconsistent_indentation_diagnostic(&mut actual, SourceId(17), 31, 35, 8, 8);
        assert_eq!(actual, expected_prefix);
        assert_eq!(actual.as_ptr(), pointer);
        assert_eq!(actual.capacity(), capacity);

        let mut expected = expected_prefix;
        for (start, end, width, landed) in [(31, 35, 4, 0), (51, 63, 12, 8)] {
            append_inconsistent_indentation_diagnostic(
                &mut actual,
                SourceId(17),
                start,
                end,
                width,
                landed,
            );
            expected.push(
                Diagnostic::error(
                    codes::E0100,
                    "inconsistent indentation: does not match any enclosing block",
                )
                .with_label(
                    Span::new(SourceId(17), start as u32, end as u32),
                    format!("this line is indented {width} columns; expected {landed}"),
                ),
            );
            assert_eq!(actual, expected);
        }
    }
}

#[cfg(test)]
mod diagnostic_text_tests {
    use super::*;

    #[test]
    fn decimal_text_matches_original_at_numeric_boundaries() {
        let mut values = vec![0_u64, 1, 9, 10, 11, u32::MAX as u64, u64::MAX];
        let mut power = 10_u64;
        loop {
            values.extend([power - 1, power, power + 1]);
            match power.checked_mul(10) {
                Some(next) => power = next,
                None => break,
            }
        }
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        for _ in 0..4096 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            values.push(state);
        }
        for value in values {
            let (digits, start) = unsigned_decimal_digits(value);
            assert!(start < 20);
            assert!(digits[start..].iter().all(u8::is_ascii_digit));
            let mut text = String::new();
            append_unsigned_decimal(&mut text, value);
            assert_eq!(text, value.to_string());
            assert_eq!(text.as_bytes(), &digits[start..]);
            if let Ok(count) = usize::try_from(value) {
                assert_eq!(
                    invalid_color_label(count),
                    format!("found {count} hex digit(s) after `#`")
                );
            }
        }
        let widths = [0_u32, 1, 9, 10, 99, 100, 999, 1000, u32::MAX - 1, u32::MAX];
        for width in widths {
            for landed in widths {
                assert_eq!(
                    indentation_label(width, landed),
                    format!("this line is indented {width} columns; expected {landed}")
                );
            }
        }
    }

    #[test]
    fn unexpected_text_matches_every_unicode_scalar() {
        for code in 0..=0x10ffff {
            if let Some(character) = char::from_u32(code) {
                let mut bytes = [0; 4];
                let text = character.encode_utf8(&mut bytes);
                assert_eq!(
                    unexpected_character_message(text),
                    format!("unexpected character `{text}`")
                );
            }
        }
        for text in ["", "!", "`", "\0", "\r\n", "é🦀", "a`b", "\\"] {
            assert_eq!(
                unexpected_character_message(text),
                format!("unexpected character `{text}`")
            );
        }
    }
}
