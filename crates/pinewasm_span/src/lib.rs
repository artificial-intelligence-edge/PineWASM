//! Source positions, source maps, and structured diagnostics.
//!
//! This is the foundation crate of the PineWASM compiler. Every layer of
//! the pipeline (tokens, AST, HIR, MIR, CSIL) carries [`Span`]s so diagnostics
//! and runtime errors can point back at the original source text.
//!
//! This crate must stay free of browser, wasm, and compiler-pipeline
//! dependencies.
#![forbid(unsafe_code)]

pub mod codes;
mod diagnostics;

pub use diagnostics::{
    too_many_errors, too_many_warnings, Diagnostics, DiagnosticsMark, MAX_ERRORS, MAX_WARNINGS,
};

use serde::{Deserialize, Serialize};

/// Identifies a source file/buffer. The main script is source zero; imported
/// libraries receive additional ids so diagnostics retain file provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SourceId(
    /// Zero-based index into the compilation's source-map table.
    pub u32,
);

impl SourceId {
    /// Main script source; imported libraries receive ids starting at one.
    pub const MAIN: SourceId = SourceId(0);
}

/// A half-open byte range `[start, end)` into a source buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Span {
    /// Source buffer containing this range.
    pub source: SourceId,
    /// Inclusive UTF-8 byte offset from the start of the source buffer.
    pub start: u32,
    /// Exclusive UTF-8 byte offset from the start of the source buffer.
    pub end: u32,
}

impl Span {
    /// Construct a source range.
    ///
    /// `start` and `end` are UTF-8 byte offsets and `start` must not exceed
    /// `end`; debug builds assert that ordering.
    pub fn new(source: SourceId, start: u32, end: u32) -> Self {
        // Expression position proves this macro cannot expand to an item.
        let () = debug_assert!(start <= end, "span start must not exceed end");
        Span { source, start, end }
    }

    /// A span for synthesized nodes with no real source location.
    pub fn dummy() -> Self {
        Span {
            source: SourceId::MAIN,
            start: 0,
            end: 0,
        }
    }

    /// Length of this range in UTF-8 bytes.
    pub fn len(&self) -> u32 {
        self.end - self.start
    }

    /// Whether the range contains no bytes.
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// The smallest span covering both `self` and `other`.
    /// Both spans must belong to the same source.
    // `debug_assert_eq!` would pull `Debug` formatting into the body, which
    // Aeneas cannot extract; see the comment on the assertion below.
    #[allow(unknown_lints)]
    #[allow(clippy::manual_assert_eq)]
    pub fn to(self, other: Span) -> Span {
        // Expression position proves this macro cannot expand to an item.
        // Plain `debug_assert!` on the raw ids (rather than `debug_assert_eq!`)
        // keeps `Debug` formatting out of the body so Aeneas can extract it.
        let () = debug_assert!(self.source.0 == other.source.0);
        Span {
            source: self.source,
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

/// 1-based line/column position, suitable for editors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineCol {
    /// 1-based line number.
    pub line: u32,
    /// 1-based column, counted in UTF-8 bytes from the line start.
    /// (Editor adapters convert to UTF-16 code units where required.)
    pub col: u32,
}

/// A registered source buffer with a precomputed line index.
#[derive(Debug, Clone)]
pub struct SourceFile {
    pub id: SourceId,
    pub name: String,
    pub text: String,
    /// Byte offsets of the first character of each line.
    line_starts: Vec<u32>,
}

impl SourceFile {
    pub fn new(id: SourceId, name: impl Into<String>, text: impl Into<String>) -> Self {
        let text = text.into();
        let line_starts = compute_line_starts(text.as_bytes());
        SourceFile {
            id,
            name: name.into(),
            text,
            line_starts,
        }
    }

    /// Convert a byte offset into a 1-based line/column.
    /// Offsets past the end of the buffer clamp to the final position.
    pub fn line_col(&self, offset: u32) -> LineCol {
        line_col_in(&self.line_starts, self.text.len() as u32, offset)
    }

    /// The text a span covers, if it is in range.
    pub fn snippet(&self, span: Span) -> Option<&str> {
        self.text.get(span.start as usize..span.end as usize)
    }
}

/// Byte offsets of the first byte of each line in `bytes`.
///
/// The result starts with `0` and has one further entry, `i + 1`, for each
/// `b'\n'` at index `i`, so it is strictly increasing.
///
/// Written as an index loop over a byte slice (no `String`, no iterator
/// adapters) so Aeneas can extract it.
pub(crate) fn compute_line_starts(bytes: &[u8]) -> Vec<u32> {
    let mut line_starts = vec![0u32];
    let mut i: usize = 0;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            line_starts.push((i + 1) as u32);
        }
        i += 1;
    }
    line_starts
}

/// Convert a byte offset into a 1-based line/column against a line-start
/// table produced by [`compute_line_starts`] for a buffer of `text_len`
/// bytes. Offsets past `text_len` clamp to `text_len`.
///
/// The search finds the number of line starts `<= offset` (the partition
/// point) with a hand-written binary search; on a strictly increasing table
/// that begins with `0` this selects the same line as
/// `<[u32]>::binary_search` did (`Ok(i) => i`, `Err(i) => i - 1`), in the same
/// `O(log n)` time. Written without closures or `binary_search` so Aeneas can
/// extract it.
pub(crate) fn line_col_in(line_starts: &[u32], text_len: u32, offset: u32) -> LineCol {
    let offset = offset.min(text_len);
    let mut lo: usize = 0;
    let mut hi: usize = line_starts.len();
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if line_starts[mid] <= offset {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let line_idx = lo - 1;
    LineCol {
        line: (line_idx + 1) as u32,
        col: offset - line_starts[line_idx] + 1,
    }
}

/// Maps `SourceId`s to main and imported source files.
#[derive(Debug, Clone, Default)]
pub struct SourceMap {
    files: Vec<SourceFile>,
}

impl SourceMap {
    pub fn new() -> Self {
        SourceMap::default()
    }

    pub fn add(&mut self, name: impl Into<String>, text: impl Into<String>) -> SourceId {
        let id = SourceId(self.files.len() as u32);
        self.files.push(SourceFile::new(id, name, text));
        id
    }

    pub fn get(&self, id: SourceId) -> Option<&SourceFile> {
        self.files.get(id.0 as usize)
    }

    pub fn line_col(&self, source: SourceId, offset: u32) -> Option<LineCol> {
        self.get(source).map(|f| f.line_col(offset))
    }
}

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

/// Severity level attached to a compiler [`Diagnostic`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Compilation cannot produce an executable program.
    Error,
    /// Compilation may continue, but the caller should surface the condition.
    Warning,
}

/// A message attached to a specific source range.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Label {
    /// Source range to underline or otherwise associate with the message.
    pub span: Span,
    /// Contextual explanation attached to this source range.
    pub message: String,
}

/// A structured diagnostic. Never a bare string: the browser renders these
/// as editor markers, so severity, code, and precise ranges are mandatory
/// data, not formatting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Whether compilation must stop or may continue with a warning.
    pub severity: Severity,
    /// Stable machine-readable code, e.g. `E0001`. `None` only for
    /// internal-error diagnostics.
    pub code: Option<String>,
    /// Primary human-readable explanation of the condition.
    pub message: String,
    /// The first label is the primary location.
    pub labels: Vec<Label>,
    /// Supplemental explanations or guidance not tied to a source range.
    pub notes: Vec<String>,
}

impl Diagnostic {
    /// Construct an error-severity diagnostic without labels or notes.
    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Diagnostic {
            severity: Severity::Error,
            code: Some(code.into()),
            message: message.into(),
            labels: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// Construct a warning-severity diagnostic without labels or notes.
    pub fn warning(code: impl Into<String>, message: impl Into<String>) -> Self {
        Diagnostic {
            severity: Severity::Warning,
            code: Some(code.into()),
            message: message.into(),
            labels: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// Append a source label, preserving insertion order, and return the
    /// updated diagnostic.
    pub fn with_label(mut self, span: Span, message: impl Into<String>) -> Self {
        self.labels.push(Label {
            span,
            message: message.into(),
        });
        self
    }

    /// Append a supplemental note and return the updated diagnostic.
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }

    /// Source range of the first label, when one exists.
    pub fn primary_span(&self) -> Option<Span> {
        self.labels.first().map(|l| l.span)
    }

    /// Whether this diagnostic has [`Severity::Error`].
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

/// True if any diagnostic in the slice is an error.
pub fn has_errors(diagnostics: &[Diagnostic]) -> bool {
    diagnostics.iter().any(Diagnostic::is_error)
}

/// Free-function extraction roots for the Aeneas extraction pipeline. The
/// extractor cannot name inherent methods as roots, so each root simply
/// calls the method it stands for. Compiled only with `--features aeneas`;
/// production builds never enable it.
#[cfg(feature = "aeneas")]
pub mod aeneas_roots {
    use super::{LineCol, SourceFile, SourceId, SourceMap, Span};

    /// Root for [`Span::len`].
    pub fn span_len(s: &Span) -> u32 {
        s.len()
    }

    /// Root for [`Span::to`].
    pub fn span_to(a: Span, b: Span) -> Span {
        a.to(b)
    }

    /// Root for `compute_line_starts`. (Roots must not reuse the
    /// callee's name: Aeneas resolves the unqualified call inside the root's
    /// own namespace, which would make the root call itself.)
    pub fn root_compute_line_starts(bytes: &[u8]) -> Vec<u32> {
        super::compute_line_starts(bytes)
    }

    /// Root for `line_col_in`.
    pub fn root_line_col_in(line_starts: &[u32], text_len: u32, offset: u32) -> LineCol {
        super::line_col_in(line_starts, text_len, offset)
    }

    /// Root for [`SourceFile::line_col`].
    pub fn source_file_line_col(file: &SourceFile, offset: u32) -> LineCol {
        file.line_col(offset)
    }

    /// Root for [`SourceFile::new`] at the instantiation `String`/`String`
    /// (the one [`SourceMap::add`] and the driver reach after `into()`).
    pub fn source_file_new(id: SourceId, name: String, text: String) -> SourceFile {
        SourceFile::new(id, name, text)
    }

    /// Root for [`SourceMap::add`] at the instantiation `String`/`String`.
    pub fn source_map_add(map: &mut SourceMap, name: String, text: String) -> SourceId {
        map.add(name, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn line_col_conversion() {
        let f = SourceFile::new(SourceId::MAIN, "test.pine", "ab\ncd\n\nef");
        assert_eq!(f.line_col(0), LineCol { line: 1, col: 1 });
        assert_eq!(f.line_col(1), LineCol { line: 1, col: 2 });
        assert_eq!(f.line_col(2), LineCol { line: 1, col: 3 }); // the '\n'
        assert_eq!(f.line_col(3), LineCol { line: 2, col: 1 });
        assert_eq!(f.line_col(6), LineCol { line: 3, col: 1 }); // empty line
        assert_eq!(f.line_col(7), LineCol { line: 4, col: 1 });
        assert_eq!(f.line_col(9), LineCol { line: 4, col: 3 }); // end of file
        assert_eq!(f.line_col(999), LineCol { line: 4, col: 3 }); // clamped
    }

    #[test]
    fn span_join_and_snippet() {
        let f = SourceFile::new(SourceId::MAIN, "t", "hello world");
        let a = Span::new(SourceId::MAIN, 0, 5);
        let b = Span::new(SourceId::MAIN, 6, 11);
        assert_eq!(f.snippet(a), Some("hello"));
        assert_eq!(f.snippet(b), Some("world"));
        let joined = a.to(b);
        assert_eq!(f.snippet(joined), Some("hello world"));
        assert_eq!(joined.len(), 11);
    }

    #[test]
    fn diagnostic_builder() {
        let d = Diagnostic::error("E0001", "unknown identifier `foo`")
            .with_label(Span::new(SourceId::MAIN, 4, 7), "not found in this scope")
            .with_note("built-in series are: open, high, low, close, volume, bar_index");
        assert!(d.is_error());
        assert_eq!(d.code.as_deref(), Some("E0001"));
        assert_eq!(d.primary_span(), Some(Span::new(SourceId::MAIN, 4, 7)));
        assert!(has_errors(&[d]));
    }

    #[test]
    fn source_map_multi_file() {
        let mut sm = SourceMap::new();
        let a = sm.add("a.pine", "x = 1\n");
        let b = sm.add("b.pine", "y = 2\n");
        assert_eq!(a, SourceId(0));
        assert_eq!(b, SourceId(1));
        assert_eq!(sm.get(b).unwrap().name, "b.pine");
        assert_eq!(sm.line_col(a, 4), Some(LineCol { line: 1, col: 5 }));
    }

    // -----------------------------------------------------------------------
    // Differential tests for the Aeneas-extractable helpers.
    // The `reference_*` functions are plain, unoptimized implementations that
    // the tests compare against the production helpers.
    // -----------------------------------------------------------------------

    fn reference_to(a: Span, other: Span) -> Span {
        let () = debug_assert_eq!(a.source, other.source);
        Span {
            source: a.source,
            start: a.start.min(other.start),
            end: a.end.max(other.end),
        }
    }

    fn reference_line_starts(text: &str) -> Vec<u32> {
        let mut line_starts = vec![0u32];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push((i + 1) as u32);
            }
        }
        line_starts
    }

    fn reference_line_col(line_starts: &[u32], text_len: u32, offset: u32) -> LineCol {
        let offset = offset.min(text_len);
        let line_idx = match line_starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i - 1,
        };
        LineCol {
            line: (line_idx + 1) as u32,
            col: offset - line_starts[line_idx] + 1,
        }
    }

    /// Deterministic xorshift so the sweep is reproducible without a
    /// dependency.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn random_text(rng: &mut Rng, max_len: u64) -> String {
        const PIECES: [&str; 6] = ["a", "\n", "\n", "\r\n", "\u{e9}", "\u{1f600}"];
        let n = rng.below(max_len + 1);
        let mut s = String::new();
        for _ in 0..n {
            s.push_str(PIECES[rng.below(PIECES.len() as u64) as usize]);
        }
        s
    }

    fn assert_line_col_matches(text: &str) {
        let f = SourceFile::new(SourceId::MAIN, "t", text);
        let expected_starts = reference_line_starts(text);
        assert_eq!(f.line_starts, expected_starts, "line starts for {text:?}");
        let len = text.len() as u32;
        let mut offsets: Vec<u32> = (0..=len + 3).collect();
        offsets.extend([u32::MAX - 1, u32::MAX, len.saturating_add(1000)]);
        for off in offsets {
            assert_eq!(
                f.line_col(off),
                reference_line_col(&expected_starts, len, off),
                "line_col({off}) for {text:?}"
            );
        }
    }

    #[test]
    fn refactor_line_col_edge_cases() {
        for text in [
            "",
            "\n",
            "\n\n",
            "\n\n\n",
            "a",
            "a\n",
            "\na",
            "ab\ncd\n\nef",
            "\r\n\r\n",
            "\u{e9}\n\u{1f600}",
            "no newline at all",
            "trailing\n\n",
        ] {
            assert_line_col_matches(text);
        }
    }

    #[test]
    fn refactor_line_col_deterministic_sweep() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for _ in 0..2000 {
            let text = random_text(&mut rng, 40);
            assert_line_col_matches(&text);
        }
        // A few long buffers exercise deeper binary-search trees.
        for _ in 0..20 {
            let text = random_text(&mut rng, 5000);
            assert_line_col_matches(&text);
        }
    }

    #[test]
    fn refactor_line_col_in_matches_on_arbitrary_increasing_tables() {
        // Beyond tables produced by `SourceFile::new`: any strictly
        // increasing table that starts at zero, with arbitrary text_len.
        let mut rng = Rng(0x0123_4567_89ab_cdef);
        for _ in 0..2000 {
            let n = rng.below(30) as usize;
            let mut table = vec![0u32];
            for _ in 0..n {
                let last = *table.last().unwrap();
                table.push(last + 1 + rng.below(8) as u32);
            }
            let text_len = rng.below(u64::from(*table.last().unwrap()) + 20) as u32;
            for off in 0..=text_len + 2 {
                assert_eq!(
                    line_col_in(&table, text_len, off),
                    reference_line_col(&table, text_len, off)
                );
            }
        }
    }

    #[test]
    fn refactor_line_col_in_panics_where_reference_panics() {
        use std::panic::catch_unwind;
        // Empty table, and a table whose first start exceeds the offset:
        // both the plain reference helper and the production code compute
        // `0 - 1` and panic.
        let cases: [(&[u32], u32, u32); 3] = [(&[], 5, 3), (&[4, 9], 20, 2), (&[1], 0, 0)];
        for (table, text_len, off) in cases {
            let new = catch_unwind(|| line_col_in(table, text_len, off)).is_err();
            let old = catch_unwind(|| reference_line_col(table, text_len, off)).is_err();
            assert!(new && old, "both must panic for {table:?} {text_len} {off}");
        }
    }

    #[test]
    fn refactor_span_to_and_len_match_reference() {
        let mut rng = Rng(0xdead_beef_cafe_f00d);
        let edge = [0u32, 1, 2, u32::MAX - 1, u32::MAX];
        let mut pairs = Vec::new();
        for &a in &edge {
            for &b in &edge {
                pairs.push((a.min(b), a.max(b)));
            }
        }
        for _ in 0..500 {
            let a = rng.next() as u32;
            let b = rng.next() as u32;
            pairs.push((a.min(b), a.max(b)));
        }
        for &(s1, e1) in &pairs {
            for _ in 0..4 {
                let &(s2, e2) = &pairs[rng.below(pairs.len() as u64) as usize];
                let src = SourceId(rng.below(3) as u32);
                let x = Span::new(src, s1, e1);
                let y = Span::new(src, s2, e2);
                let joined = x.to(y);
                assert_eq!(joined, reference_to(x, y));
                assert_eq!(joined.len(), joined.end - joined.start);
                assert_eq!(x.len(), e1 - s1);
            }
        }
    }

    #[cfg(debug_assertions)]
    #[test]
    fn refactor_span_to_still_asserts_same_source_in_debug() {
        use std::panic::catch_unwind;
        let a = Span::new(SourceId(0), 0, 1);
        let b = Span::new(SourceId(1), 0, 1);
        assert!(catch_unwind(|| a.to(b)).is_err());
        assert!(catch_unwind(|| reference_to(a, b)).is_err());
    }

    /// Documents existing behavior: for a buffer
    /// of exactly `u32::MAX` bytes with no newline, the column at the clamped
    /// end offset is `u32::MAX + 1`, which overflows. This crate enables
    /// overflow checks in both debug and release, so both builds panic.
    /// The table is passed directly so the test does not allocate 4 GiB.
    #[test]
    fn aeneas_finding_col_overflows_at_u32_max_single_line() {
        use std::panic::catch_unwind;
        let got = catch_unwind(|| line_col_in(&[0], u32::MAX, u32::MAX));
        let reference = catch_unwind(|| reference_line_col(&[0], u32::MAX, u32::MAX));
        assert!(got.is_err());
        assert!(reference.is_err());
        // One byte shorter is fine: col = u32::MAX.
        assert_eq!(
            line_col_in(&[0], u32::MAX - 1, u32::MAX),
            LineCol {
                line: 1,
                col: u32::MAX
            }
        );
    }
}
