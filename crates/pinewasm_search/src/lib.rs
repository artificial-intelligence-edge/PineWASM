//! Find-in-files over bounded in-memory sources, powered by ripgrep's
//! `grep-regex` matcher. It follows ripgrep's regex dialect, smart-case,
//! word-boundary and multiline semantics.

#![forbid(unsafe_code)]

use grep_matcher::Matcher;
use grep_regex::RegexMatcherBuilder;
use serde::{Deserialize, Serialize};

/// Maximum number of in-memory files accepted by one search request.
pub const MAX_SEARCH_FILES: usize = 128;
/// Maximum UTF-8 content size accepted for any one search file (1 MiB).
pub const MAX_SEARCH_FILE_CONTENT_BYTES: usize = 1024 * 1024;
/// Maximum aggregate UTF-8 content size accepted across a request (8 MiB).
pub const MAX_SEARCH_TOTAL_CONTENT_BYTES: usize = 8 * 1024 * 1024;
/// Maximum UTF-8 byte length accepted for any one workspace-relative path.
pub const MAX_SEARCH_PATH_BYTES: usize = 256;
/// Maximum UTF-8 byte length accepted for a literal or regex query (4 KiB).
pub const MAX_SEARCH_QUERY_BYTES: usize = 4 * 1024;
/// Maximum regex hits returned by one request.
pub const MAX_SEARCH_RESULTS: usize = 20_000;
/// Maximum line-local ranges projected into one response.
///
/// A single multiline regex hit can touch many source lines, so this separate
/// cap prevents a small hit limit from amplifying into an unbounded response.
pub const MAX_SEARCH_RESULT_FRAGMENTS: usize = 20_000;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchOptions {
    pub case_sensitive: bool,
    /// VS Code's smart case: sensitive iff the query carries an uppercase
    /// char. Explicit `case_sensitive` wins when set.
    #[serde(default)]
    pub smart_case: bool,
    pub whole_word: bool,
    pub is_regex: bool,
    /// Regex-hit cap across all files (VS Code stops at 20000 by default).
    pub max_results: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchFile {
    pub path: String,
    pub content: String,
}

/// One source line containing one or more line-local match fragments.
///
/// A regex hit spanning multiple lines is split into one range on every
/// touched line. [`SearchResults::total`] still counts that regex hit once,
/// rather than counting its projected line fragments.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchMatch {
    /// Zero-based source line containing these ranges.
    pub line: u64,
    /// The full line text, trailing newline stripped.
    pub line_text: String,
    /// UTF-16 column ranges of every match fragment within `line_text`.
    pub ranges: Vec<(u32, u32)>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileMatches {
    pub path: String,
    pub matches: Vec<SearchMatch>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResults {
    pub files: Vec<FileMatches>,
    /// Whether either the regex-hit cap or line-fragment cap stopped search.
    pub hit_limit: bool,
    /// Total regex hits found (capped at `max_results`).
    pub total: usize,
}

/// Build the matcher exactly the way rg's CLI layer would for our flags.
fn build_matcher(query: &str, options: &SearchOptions) -> Result<grep_regex::RegexMatcher, String> {
    let mut builder = RegexMatcherBuilder::new();
    if options.case_sensitive {
        builder.case_insensitive(false);
    } else if options.smart_case && options.is_regex {
        // Let ripgrep's parsed-AST analysis distinguish uppercase literals
        // from uppercase regex syntax such as `\S`.
        builder.case_smart(true);
    } else {
        // In literal mode every character is data, so an uppercase character
        // in the unescaped query intentionally enables case sensitivity.
        let case_sensitive = options.smart_case && query.chars().any(char::is_uppercase);
        builder.case_insensitive(!case_sensitive);
    }
    builder
        .word(options.whole_word)
        // rg's --crlf: `$` tolerates a trailing `\r`.
        .crlf(true)
        // Multiline search must permit expressions that consume terminators.
        // Calling this after `crlf` keeps CRLF-aware anchors enabled.
        .line_terminator(None)
        .multi_line(true);
    let pattern = if options.is_regex {
        query.to_string()
    } else {
        regex_escape(query)
    };
    builder.build(&pattern).map_err(|error| error.to_string())
}

/// Literal-mode escape (rg -F): neutralize every regex metacharacter.
fn regex_escape(literal: &str) -> String {
    let mut out = String::with_capacity(literal.len() * 2);
    for ch in literal.chars() {
        if "\\.+*?()|[]{}^$#&-~".contains(ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Monotonic byte-to-UTF-16 projector for one source file.
///
/// Matcher hits are non-overlapping and arrive in source order. Retaining the
/// last projected byte therefore lets us decode each matched line segment at
/// most once. The previous implementation rescanned the complete line prefix
/// for every range endpoint, making 20,000 valid hits on one maximum-size line
/// quadratic in practice.
#[derive(Default)]
struct Utf16Cursor {
    line_start: usize,
    byte: usize,
    column: u32,
    initialized: bool,
    #[cfg(test)]
    scanned_bytes: usize,
}

impl Utf16Cursor {
    fn column(&mut self, line: SourceLine<'_>, byte: usize) -> u32 {
        let mut end = byte.min(line.text.len());
        while end > 0 && !line.text.is_char_boundary(end) {
            end -= 1;
        }

        let target = line.start + end;
        if !self.initialized || self.line_start != line.start || target < self.byte {
            self.line_start = line.start;
            self.byte = line.start;
            self.column = 0;
            self.initialized = true;
        }

        let local_start = self.byte - line.start;
        self.column += line.text[local_start..end].encode_utf16().count() as u32;
        #[cfg(test)]
        {
            self.scanned_bytes += end - local_start;
        }
        self.byte = target;
        self.column
    }
}

/// Return the first UTF-8 boundary after `byte`, or `None` at end of input.
///
/// Empty regex matches need an explicit advance before the next `find_at`.
/// Advancing by a raw byte can land inside a multibyte code point and either
/// stop the scan early or make no progress for a subsequent empty match.
fn next_utf8_boundary(text: &str, byte: usize) -> Option<usize> {
    if byte >= text.len() {
        return None;
    }

    ((byte + 1)..=text.len()).find(|next| text.is_char_boundary(*next))
}

fn validate_search_request(
    files: &[SearchFile],
    query: &str,
    options: &SearchOptions,
) -> Result<(), String> {
    if files.len() > MAX_SEARCH_FILES {
        return Err(format!("search accepts at most {MAX_SEARCH_FILES} files"));
    }
    if query.len() > MAX_SEARCH_QUERY_BYTES {
        return Err(format!(
            "search query exceeds {MAX_SEARCH_QUERY_BYTES} bytes"
        ));
    }
    if options.max_results > MAX_SEARCH_RESULTS {
        return Err(format!(
            "search maxResults must be between 0 and {MAX_SEARCH_RESULTS}"
        ));
    }

    let mut aggregate_content_bytes = 0usize;
    for file in files {
        if file.path.len() > MAX_SEARCH_PATH_BYTES {
            return Err(format!("search path exceeds {MAX_SEARCH_PATH_BYTES} bytes"));
        }
        if file.content.len() > MAX_SEARCH_FILE_CONTENT_BYTES {
            return Err(format!(
                "search file content exceeds {MAX_SEARCH_FILE_CONTENT_BYTES} bytes"
            ));
        }
        aggregate_content_bytes = aggregate_content_bytes
            .checked_add(file.content.len())
            .ok_or_else(|| {
                format!("search aggregate content exceeds {MAX_SEARCH_TOTAL_CONTENT_BYTES} bytes")
            })?;
        if aggregate_content_bytes > MAX_SEARCH_TOTAL_CONTENT_BYTES {
            return Err(format!(
                "search aggregate content exceeds {MAX_SEARCH_TOTAL_CONTENT_BYTES} bytes"
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct SourceLine<'a> {
    number: u64,
    start: usize,
    content_end: usize,
    text: &'a str,
}

/// Compact, lazily built line index for one already bounded source.
///
/// The 1 MiB file cap makes every byte offset fit in `u32`. Storing only
/// terminator-inclusive line ends keeps a newline-heavy maximum-size file near
/// 4 MiB of index storage instead of allocating a full struct per empty line.
struct LineIndex<'a> {
    content: &'a str,
    ends: Vec<u32>,
}

impl<'a> LineIndex<'a> {
    fn new(content: &'a str) -> Self {
        debug_assert!(content.len() <= MAX_SEARCH_FILE_CONTENT_BYTES);
        let mut ends = Vec::new();
        for (index, byte) in content.bytes().enumerate() {
            if byte == b'\n' {
                ends.push((index + 1) as u32);
            }
        }
        if ends.last().copied().map(|end| end as usize) != Some(content.len()) {
            ends.push(content.len() as u32);
        }
        Self { content, ends }
    }

    fn line(&self, index: usize) -> SourceLine<'a> {
        let start = index
            .checked_sub(1)
            .map_or(0, |previous| self.ends[previous] as usize);
        let end = self.ends[index] as usize;
        let bytes = self.content.as_bytes();
        let content_end = if end > start && bytes[end - 1] == b'\n' {
            let newline = end - 1;
            if newline > start && bytes[newline - 1] == b'\r' {
                newline - 1
            } else {
                newline
            }
        } else {
            end
        };
        SourceLine {
            number: index as u64,
            start,
            content_end,
            text: &self.content[start..content_end],
        }
    }

    fn line_containing_byte(&self, byte: usize) -> Option<usize> {
        let index = self.ends.partition_point(|end| *end as usize <= byte);
        if index < self.ends.len() {
            Some(index)
        } else if self.ends.last().copied().map(|end| end as usize) == Some(byte)
            && self.line(self.ends.len() - 1).content_end == byte
        {
            Some(self.ends.len() - 1)
        } else {
            None
        }
    }
}

fn empty_match_is_inside_crlf(content: &str, byte: usize) -> bool {
    byte > 0
        && content.as_bytes().get(byte - 1) == Some(&b'\r')
        && content.as_bytes().get(byte) == Some(&b'\n')
}

fn push_line_range(matches: &mut Vec<SearchMatch>, line: SourceLine<'_>, range: (u32, u32)) {
    // The matcher emits non-overlapping hits in source order. Aggregating only
    // the last line is therefore sufficient to clone each touched line's text
    // at most once.
    if let Some(previous) = matches.last_mut() {
        if previous.line == line.number {
            previous.ranges.push(range);
            return;
        }
    }
    matches.push(SearchMatch {
        line: line.number,
        line_text: line.text.to_string(),
        ranges: vec![range],
    });
}

/// Project one exact regex hit into the public per-line result model.
fn push_match_fragments(
    lines: &LineIndex<'_>,
    start: usize,
    end: usize,
    matches: &mut Vec<SearchMatch>,
    projected_fragments: &mut usize,
    utf16: &mut Utf16Cursor,
) -> bool {
    let Some(first_line) = lines.line_containing_byte(start) else {
        return true;
    };

    if start == end {
        if *projected_fragments >= MAX_SEARCH_RESULT_FRAGMENTS {
            return false;
        }
        let line = lines.line(first_line);
        let local = start.saturating_sub(line.start).min(line.text.len());
        let column = utf16.column(line, local);
        push_line_range(matches, line, (column, column));
        *projected_fragments += 1;
        return true;
    }

    let Some(last_line) = lines.line_containing_byte(end - 1) else {
        return true;
    };
    for index in first_line..=last_line {
        if *projected_fragments >= MAX_SEARCH_RESULT_FRAGMENTS {
            return false;
        }
        let line = lines.line(index);
        let local_start = if index == first_line {
            start.saturating_sub(line.start).min(line.text.len())
        } else {
            0
        };
        let local_end = if index == last_line {
            end.saturating_sub(line.start).min(line.text.len())
        } else {
            line.text.len()
        };
        push_line_range(
            matches,
            line,
            (
                utf16.column(line, local_start),
                utf16.column(line, local_end),
            ),
        );
        *projected_fragments += 1;
    }
    true
}

struct SearchState<'a> {
    matches: &'a mut Vec<SearchMatch>,
    total: &'a mut usize,
    projected_fragments: &'a mut usize,
    max_results: usize,
    hit_limit: &'a mut bool,
    #[cfg(test)]
    projected_utf8_bytes: Option<&'a mut usize>,
}

/// Enumerate exact non-overlapping hits directly over one complete source.
///
/// Keeping one full-buffer cursor avoids the repeated line/block discovery
/// that can make absolute zero-width anchors quadratic on large files. Every
/// empty hit advances to the next UTF-8 boundary, and both result caps can
/// stop the scan immediately.
fn search_file<'content>(
    matcher: &grep_regex::RegexMatcher,
    content: &'content str,
    line_index: &mut Option<LineIndex<'content>>,
    state: &mut SearchState<'_>,
) -> Result<(), String> {
    let buffer = content.as_bytes();
    let mut start = 0usize;
    let mut last_match_end = None;
    let mut utf16 = Utf16Cursor::default();

    while let Some(found) = matcher
        .find_at(buffer, start)
        .map_err(|error| error.to_string())?
    {
        // A trailing terminator does not invent a final empty source line.
        if found.is_empty() && found.start() == buffer.len() && buffer.last() == Some(&b'\n') {
            break;
        }
        if found.is_empty() && empty_match_is_inside_crlf(content, found.start()) {
            let Some(next) = next_utf8_boundary(content, found.end()) else {
                break;
            };
            start = next;
            continue;
        }
        // Match grep_matcher's non-overlapping iterator semantics: suppress an
        // empty hit immediately adjacent to the end of the preceding accepted
        // hit, while retaining independent empty hits at later boundaries.
        if found.is_empty() && last_match_end == Some(found.end()) {
            let Some(next) = next_utf8_boundary(content, found.end()) else {
                break;
            };
            start = next;
            continue;
        }
        if *state.total >= state.max_results {
            *state.hit_limit = true;
            break;
        }

        let fully_projected = push_match_fragments(
            line_index.get_or_insert_with(|| LineIndex::new(content)),
            found.start(),
            found.end(),
            state.matches,
            state.projected_fragments,
            &mut utf16,
        );
        *state.total += 1;
        last_match_end = Some(found.end());
        if !fully_projected
            || *state.total >= state.max_results
            || *state.projected_fragments >= MAX_SEARCH_RESULT_FRAGMENTS
        {
            *state.hit_limit = true;
            break;
        }

        if found.is_empty() {
            let Some(next) = next_utf8_boundary(content, found.end()) else {
                break;
            };
            start = next;
        } else {
            start = found.end();
        }
    }
    #[cfg(test)]
    if let Some(projected_utf8_bytes) = state.projected_utf8_bytes.as_deref_mut() {
        *projected_utf8_bytes += utf16.scanned_bytes;
    }
    Ok(())
}

/// Search every file, ripgrep-style, and group matches per file.
pub fn search_files(
    files: &[SearchFile],
    query: &str,
    options: &SearchOptions,
) -> Result<SearchResults, String> {
    validate_search_request(files, query, options)?;
    let matcher = build_matcher(query, options)?;
    let mut out = Vec::new();
    let mut total = 0usize;
    let mut projected_fragments = 0usize;
    let mut hit_limit = false;

    for file in files {
        if hit_limit {
            break;
        }
        let mut matches: Vec<SearchMatch> = Vec::new();
        if file.content.is_empty() {
            continue;
        }
        let mut line_index = None;
        search_file(
            &matcher,
            &file.content,
            &mut line_index,
            &mut SearchState {
                matches: &mut matches,
                total: &mut total,
                projected_fragments: &mut projected_fragments,
                max_results: options.max_results,
                hit_limit: &mut hit_limit,
                #[cfg(test)]
                projected_utf8_bytes: None,
            },
        )?;
        if !matches.is_empty() {
            out.push(FileMatches {
                path: file.path.clone(),
                matches,
            });
        }
    }

    Ok(SearchResults {
        files: out,
        hit_limit,
        total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> SearchOptions {
        SearchOptions {
            case_sensitive: false,
            smart_case: false,
            whole_word: false,
            is_regex: false,
            max_results: 1000,
        }
    }

    fn file(path: &str, content: &str) -> SearchFile {
        SearchFile {
            path: path.to_string(),
            content: content.to_string(),
        }
    }

    #[test]
    fn finds_literals_case_insensitively() {
        let files = [file("a.pine", "plot(Close)\nPLOT(open)\n")];
        let results = search_files(&files, "plot", &options()).unwrap();
        assert_eq!(results.total, 2);
        assert_eq!(results.files[0].matches[0].ranges, vec![(0, 4)]);
    }

    #[test]
    fn smart_case_matches_rg() {
        let files = [file("a.pine", "plot(x)\nPlot(x)\n")];
        let mut opts = options();
        opts.smart_case = true;
        assert_eq!(search_files(&files, "plot", &opts).unwrap().total, 2);
        assert_eq!(search_files(&files, "Plot", &opts).unwrap().total, 1);

        // Regex-looking text remains literal data, including its uppercase S.
        let escaped = [file("literal.pine", "foo\\S\nFOO\\S\n")];
        assert_eq!(search_files(&escaped, r"foo\S", &opts).unwrap().total, 1);
    }

    #[test]
    fn regex_smart_case_distinguishes_escape_syntax_from_literals() {
        let files = [file("a.pine", "FOO!\nFoo!\nfoo!\n")];
        let mut opts = options();
        opts.smart_case = true;
        opts.is_regex = true;

        // `S` names the non-whitespace class; it is not an uppercase literal.
        assert_eq!(search_files(&files, r"foo\S", &opts).unwrap().total, 3);
        // `F` is an uppercase literal and therefore enables case sensitivity.
        assert_eq!(search_files(&files, r"Foo\S", &opts).unwrap().total, 1);

        opts.case_sensitive = true;
        assert_eq!(search_files(&files, r"foo\S", &opts).unwrap().total, 1);
    }

    #[test]
    fn whole_word_and_regex() {
        let files = [file("a.pine", "up = 1\nsetup = 2\nup2\n")];
        let mut opts = options();
        opts.whole_word = true;
        assert_eq!(search_files(&files, "up", &opts).unwrap().total, 1);
        let mut regex = options();
        regex.is_regex = true;
        assert_eq!(search_files(&files, r"up\d", &regex).unwrap().total, 1);
    }

    #[test]
    fn caps_at_max_results() {
        let files = [file("a.pine", "x x x x x\n".repeat(100).as_str())];
        let mut opts = options();
        opts.max_results = 42;
        let results = search_files(&files, "x", &opts).unwrap();
        assert!(results.hit_limit);
        assert_eq!(results.total, 42);
    }

    #[test]
    fn multiline_regex_hits_are_split_into_utf16_line_fragments() {
        let files = [file(
            "unicode.pine",
            "xx😀foo\nmiddle\nbaré tail\nxx😀foo\nmiddle\nbaré tail",
        )];
        let mut opts = options();
        opts.is_regex = true;

        let results = search_files(&files, "😀foo\nmiddle\nbaré", &opts).unwrap();
        assert_eq!(results.total, 2);
        assert_eq!(results.files[0].matches.len(), 6);
        assert_eq!(results.files[0].matches[0].line, 0);
        assert_eq!(results.files[0].matches[0].line_text, "xx😀foo");
        assert_eq!(results.files[0].matches[0].ranges, vec![(2, 7)]);
        assert_eq!(results.files[0].matches[1].line_text, "middle");
        assert_eq!(results.files[0].matches[1].ranges, vec![(0, 6)]);
        assert_eq!(results.files[0].matches[2].line_text, "baré tail");
        assert_eq!(results.files[0].matches[2].ranges, vec![(0, 4)]);
        assert_eq!(results.files[0].matches[3].line, 3);
        assert_eq!(results.files[0].matches[3].ranges, vec![(2, 7)]);
        assert_eq!(results.files[0].matches[4].ranges, vec![(0, 6)]);
        assert_eq!(results.files[0].matches[5].ranges, vec![(0, 4)]);
    }

    #[test]
    fn multiline_hits_count_once_toward_the_result_limit() {
        let files = [file("a.pine", "foo\nbar\nfoo\nbar")];
        let mut opts = options();
        opts.is_regex = true;
        opts.max_results = 1;

        let results = search_files(&files, "foo\nbar", &opts).unwrap();
        assert!(results.hit_limit);
        assert_eq!(results.total, 1);
        assert_eq!(results.files[0].matches.len(), 2);
        assert_eq!(results.files[0].matches[0].ranges, vec![(0, 3)]);
        assert_eq!(results.files[0].matches[1].ranges, vec![(0, 3)]);
    }

    #[test]
    fn caps_multiline_line_fragment_projection() {
        let files = [file(
            "many-lines.pine",
            &"\n".repeat(MAX_SEARCH_RESULT_FRAGMENTS + 1),
        )];
        let mut opts = options();
        opts.is_regex = true;

        let results = search_files(&files, "(?s:.*)", &opts).unwrap();
        assert!(results.hit_limit);
        assert_eq!(results.total, 1);
        assert_eq!(results.files[0].matches.len(), MAX_SEARCH_RESULT_FRAGMENTS);
        assert!(results.files[0]
            .matches
            .iter()
            .all(|matched| matched.ranges == [(0, 0)]));
    }

    #[test]
    fn multiline_crlf_fragments_exclude_line_terminators() {
        let files = [file("a.pine", "before foo\r\nbar after\r\n")];
        let mut opts = options();
        opts.is_regex = true;

        let results = search_files(&files, "foo\\r?\\nbar", &opts).unwrap();
        assert_eq!(results.total, 1);
        assert_eq!(results.files[0].matches.len(), 2);
        assert_eq!(results.files[0].matches[0].line_text, "before foo");
        assert_eq!(results.files[0].matches[0].ranges, vec![(7, 10)]);
        assert_eq!(results.files[0].matches[1].line_text, "bar after");
        assert_eq!(results.files[0].matches[1].ranges, vec![(0, 3)]);
    }

    #[test]
    fn multiline_blocks_use_whole_file_offsets() {
        let prefix = "z".repeat(16 * 1024);
        let files = [file(
            "offsets.pine",
            &format!("{prefix}\nxx foo\nbar yy\nunmatched\nxx foo\nbar yy"),
        )];
        let mut opts = options();
        opts.is_regex = true;

        let results = search_files(&files, "foo\nbar", &opts).unwrap();
        assert_eq!(results.total, 2);
        assert_eq!(results.files[0].matches.len(), 4);
        assert_eq!(results.files[0].matches[0].line, 1);
        assert_eq!(results.files[0].matches[0].ranges, vec![(3, 6)]);
        assert_eq!(results.files[0].matches[1].line, 2);
        assert_eq!(results.files[0].matches[1].ranges, vec![(0, 3)]);
        assert_eq!(results.files[0].matches[2].line, 4);
        assert_eq!(results.files[0].matches[2].ranges, vec![(3, 6)]);
        assert_eq!(results.files[0].matches[3].line, 5);
        assert_eq!(results.files[0].matches[3].ranges, vec![(0, 3)]);
    }

    #[test]
    fn regex_line_anchors_are_preserved_as_zero_width_ranges() {
        let files = [file("a.pine", "alpha\nβeta\n")];
        let mut opts = options();
        opts.is_regex = true;

        let starts = search_files(&files, "^", &opts).unwrap();
        assert_eq!(starts.total, 2);
        assert_eq!(starts.files[0].matches[0].ranges, vec![(0, 0)]);
        assert_eq!(starts.files[0].matches[1].ranges, vec![(0, 0)]);

        let ends = search_files(&files, "$", &opts).unwrap();
        assert_eq!(ends.total, 2);
        assert_eq!(ends.files[0].matches[0].ranges, vec![(5, 5)]);
        assert_eq!(ends.files[0].matches[1].ranges, vec![(4, 4)]);
    }

    #[test]
    fn absolute_start_anchor_scales_to_the_maximum_file_size() {
        let files = [file(
            "maximum.pine",
            &"x".repeat(MAX_SEARCH_FILE_CONTENT_BYTES),
        )];
        let mut opts = options();
        opts.is_regex = true;

        let results = search_files(&files, r"\A", &opts).unwrap();
        assert!(!results.hit_limit);
        assert_eq!(results.total, 1);
        assert_eq!(results.files[0].matches.len(), 1);
        assert_eq!(results.files[0].matches[0].ranges, vec![(0, 0)]);
    }

    #[test]
    fn maximum_single_line_match_projection_decodes_each_byte_at_most_once() {
        let mut bytes = vec![b'a'; MAX_SEARCH_FILE_CONTENT_BYTES];
        let stride = MAX_SEARCH_FILE_CONTENT_BYTES / MAX_SEARCH_RESULTS;
        for index in 0..MAX_SEARCH_RESULTS {
            bytes[(index + 1) * stride - 1] = b'x';
        }
        let content = String::from_utf8(bytes).unwrap();
        let mut opts = options();
        opts.max_results = MAX_SEARCH_RESULTS;
        let matcher = build_matcher("x", &opts).unwrap();
        let mut line_index = None;
        let mut matches = Vec::new();
        let mut total = 0;
        let mut projected_fragments = 0;
        let mut hit_limit = false;
        let mut projected_utf8_bytes = 0;

        search_file(
            &matcher,
            &content,
            &mut line_index,
            &mut SearchState {
                matches: &mut matches,
                total: &mut total,
                projected_fragments: &mut projected_fragments,
                max_results: opts.max_results,
                hit_limit: &mut hit_limit,
                projected_utf8_bytes: Some(&mut projected_utf8_bytes),
            },
        )
        .unwrap();

        assert_eq!(total, MAX_SEARCH_RESULTS);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].ranges.len(), MAX_SEARCH_RESULTS);
        assert!(projected_utf8_bytes <= content.len());
    }

    #[test]
    fn newline_heavy_no_match_does_not_build_a_line_index() {
        let content = "\n".repeat(MAX_SEARCH_FILE_CONTENT_BYTES);
        let opts = options();
        let matcher = build_matcher("needle", &opts).unwrap();
        let mut line_index = None;
        let mut matches = Vec::new();
        let mut total = 0;
        let mut projected_fragments = 0;
        let mut hit_limit = false;

        search_file(
            &matcher,
            &content,
            &mut line_index,
            &mut SearchState {
                matches: &mut matches,
                total: &mut total,
                projected_fragments: &mut projected_fragments,
                max_results: opts.max_results,
                hit_limit: &mut hit_limit,
                projected_utf8_bytes: None,
            },
        )
        .unwrap();

        assert!(line_index.is_none());
        assert!(matches.is_empty());
        assert_eq!(total, 0);
        assert!(!hit_limit);
    }

    #[test]
    fn zero_width_matches_advance_at_utf8_boundaries() {
        let files = [file("unicode.pine", "éx")];
        let mut opts = options();
        opts.is_regex = true;

        let results = search_files(&files, "", &opts).unwrap();
        assert_eq!(results.total, 3);
        assert_eq!(
            results.files[0].matches[0].ranges,
            vec![(0, 0), (1, 1), (2, 2)]
        );
    }

    #[test]
    fn empty_alternative_adjacent_to_a_nonempty_hit_is_suppressed() {
        let files = [file("a.pine", "ab")];
        let mut opts = options();
        opts.is_regex = true;

        let results = search_files(&files, "a|", &opts).unwrap();
        assert_eq!(results.total, 2);
        assert_eq!(results.files[0].matches[0].ranges, vec![(0, 1), (2, 2)]);
    }

    #[test]
    fn empty_repetition_at_the_end_of_its_nonempty_hit_is_suppressed() {
        let files = [file("a.pine", "a")];
        let mut opts = options();
        opts.is_regex = true;

        let results = search_files(&files, "a*", &opts).unwrap();
        assert_eq!(results.total, 1);
        assert_eq!(results.files[0].matches[0].ranges, vec![(0, 1)]);
    }

    #[test]
    fn zero_width_matches_do_not_split_crlf_terminators() {
        let files = [file("crlf.pine", "é\r\nx")];
        let mut opts = options();
        opts.is_regex = true;

        let results = search_files(&files, "", &opts).unwrap();
        assert_eq!(results.total, 4);
        assert_eq!(results.files[0].matches[0].ranges, vec![(0, 0), (1, 1)]);
        assert_eq!(results.files[0].matches[1].ranges, vec![(0, 0), (1, 1)]);
    }

    #[test]
    fn zero_width_matches_count_toward_the_result_limit() {
        let files = [file("a.pine", "é\nx\n")];
        let mut opts = options();
        opts.is_regex = true;
        opts.max_results = 3;

        let results = search_files(&files, "^|$", &opts).unwrap();
        assert!(results.hit_limit);
        assert_eq!(results.total, 3);
        assert_eq!(results.files[0].matches[0].ranges, vec![(0, 0), (1, 1)]);
        assert_eq!(results.files[0].matches[1].ranges, vec![(0, 0)]);
    }

    #[test]
    fn zero_max_results_is_valid_and_stops_at_the_first_hit() {
        let files = [file("a.pine", "x")];
        let mut opts = options();
        opts.max_results = 0;

        let results = search_files(&files, "x", &opts).unwrap();
        assert!(results.hit_limit);
        assert_eq!(results.total, 0);
        assert!(results.files.is_empty());
    }

    #[test]
    fn accepts_search_request_limits_inclusively() {
        let exact_path = "p".repeat(MAX_SEARCH_PATH_BYTES);
        let exact_content = "x".repeat(MAX_SEARCH_FILE_CONTENT_BYTES);
        let mut files = Vec::with_capacity(MAX_SEARCH_FILES);
        for index in 0..MAX_SEARCH_FILES {
            files.push(file(
                &exact_path,
                if index < MAX_SEARCH_TOTAL_CONTENT_BYTES / MAX_SEARCH_FILE_CONTENT_BYTES {
                    &exact_content
                } else {
                    ""
                },
            ));
        }
        let exact_query = "q".repeat(MAX_SEARCH_QUERY_BYTES);
        let mut opts = options();
        opts.max_results = MAX_SEARCH_RESULTS;

        assert!(validate_search_request(&files, &exact_query, &opts).is_ok());
    }

    #[test]
    fn rejects_each_search_request_limit_before_scanning() {
        let opts = options();

        let too_many_files = (0..=MAX_SEARCH_FILES)
            .map(|index| file(&format!("{index}.pine"), ""))
            .collect::<Vec<_>>();
        assert_eq!(
            validate_search_request(&too_many_files, "x", &opts).unwrap_err(),
            "search accepts at most 128 files"
        );

        let path_over = "p".repeat(MAX_SEARCH_PATH_BYTES + 1);
        assert_eq!(
            validate_search_request(&[file(&path_over, "")], "x", &opts).unwrap_err(),
            "search path exceeds 256 bytes"
        );

        let content_over = "x".repeat(MAX_SEARCH_FILE_CONTENT_BYTES + 1);
        assert_eq!(
            validate_search_request(&[file("a.pine", &content_over)], "x", &opts).unwrap_err(),
            "search file content exceeds 1048576 bytes"
        );

        let exact_content = "x".repeat(MAX_SEARCH_FILE_CONTENT_BYTES);
        let mut aggregate_over = (0..8)
            .map(|index| file(&format!("{index}.pine"), &exact_content))
            .collect::<Vec<_>>();
        aggregate_over.push(file("over.pine", "x"));
        assert_eq!(
            validate_search_request(&aggregate_over, "x", &opts).unwrap_err(),
            "search aggregate content exceeds 8388608 bytes"
        );

        let query_over = "q".repeat(MAX_SEARCH_QUERY_BYTES + 1);
        assert_eq!(
            validate_search_request(&[], &query_over, &opts).unwrap_err(),
            "search query exceeds 4096 bytes"
        );

        let mut results_over = options();
        results_over.max_results = MAX_SEARCH_RESULTS + 1;
        assert_eq!(
            validate_search_request(&[], "x", &results_over).unwrap_err(),
            "search maxResults must be between 0 and 20000"
        );
    }
}
