//! One parse reports a bounded list of diagnostics, and nothing but that list
//! depends on the bound.
//!
//! A source can be wrong in more places than its size suggests: every stray
//! character is an error of its own. The lexer and the parser therefore
//! report into one list that keeps the first `MAX_ERRORS` errors and the
//! first `MAX_WARNINGS` warnings and ends with an `E0002` or a `W0007` when
//! it left some out. The tokens and the tree must be what they would be with
//! room for every diagnostic, because the limit is on what is reported and
//! not on what is read.

use crate::parser::parse_into;
use crate::{lex, parse, MAX_SOURCE_LEN};
use pinewasm_span::{
    codes, has_errors, too_many_errors, too_many_warnings, Diagnostic, Diagnostics, Severity,
    SourceId, MAX_ERRORS, MAX_WARNINGS,
};

/// One error from the lexer for each repetition.
const LEXER_ERRORS: [&str; 4] = ["$", "$ ", "\"\n", "#12 "];

/// One error from the parser for each repetition, and none from the lexer.
const PARSER_ERROR: &str = "x 1\n";

/// One warning from the parser for each repetition after the first.
const PARSER_WARNING: &str = "//@version=6\n";

fn count(diagnostics: &[Diagnostic], severity: Severity) -> usize {
    diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == severity)
        .count()
}

fn has_code(diagnostics: &[Diagnostic], code: &str) -> bool {
    diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code.as_deref() == Some(code))
}

/// What a list with `limits` holds after `reported`.
fn cut(limits: (usize, usize), reported: &[Diagnostic]) -> Vec<Diagnostic> {
    let mut list = Diagnostics::with_limits(limits.0, limits.1);
    list.extend(reported.iter().cloned());
    list.finish()
}

/// Limits small enough that an ordinary broken script runs out of room, so
/// the parser's own decisions are made with diagnostics left out.
const SHORT: [(usize, usize); 6] = [(0, 0), (1, 0), (0, 1), (1, 1), (2, 3), (5, 5)];

/// Parsing into a short list gives the program a list with room for
/// everything gives, and the diagnostics that list is cut to.
fn assert_only_the_list_depends_on_its_limits(source: &str) {
    let whole = parse_into(
        source,
        SourceId::MAIN,
        Diagnostics::with_limits(usize::MAX, usize::MAX),
    );
    for limits in SHORT {
        let short = parse_into(
            source,
            SourceId::MAIN,
            Diagnostics::with_limits(limits.0, limits.1),
        );
        assert_eq!(short.program, whole.program, "{limits:?}: {source:?}");
        assert_eq!(
            short.once_keyword_spans, whole.once_keyword_spans,
            "{limits:?}: {source:?}"
        );
        assert_eq!(
            short.diagnostics,
            cut(limits, &whole.diagnostics),
            "{limits:?}: {source:?}"
        );
    }
}

#[test]
fn the_lexer_reports_the_first_errors_and_then_says_it_stopped() {
    for unit in LEXER_ERRORS {
        let at_limit = lex(&unit.repeat(MAX_ERRORS));
        assert_eq!(at_limit.diagnostics.len(), MAX_ERRORS, "{unit:?}");
        assert!(!has_code(&at_limit.diagnostics, codes::E0002), "{unit:?}");

        for over in [MAX_ERRORS + 1, MAX_ERRORS + 2, 4 * MAX_ERRORS] {
            let lexed = lex(&unit.repeat(over));
            assert_eq!(
                lexed.diagnostics[..MAX_ERRORS],
                at_limit.diagnostics,
                "{unit:?}"
            );
            assert_eq!(
                lexed.diagnostics[MAX_ERRORS..],
                [too_many_errors(MAX_ERRORS)],
                "{unit:?} x {over}"
            );
        }
    }
}

#[test]
fn every_lexer_diagnostic_is_an_error() {
    // The lexer's list is cut by its length alone, which is the number of
    // errors only while it reports nothing else.
    let mut source = String::new();
    for unit in LEXER_ERRORS {
        source.push_str(unit);
        source.push('\n');
    }
    // A dedent to a width no enclosing block has, then an unterminated
    // multiline string.
    source.push_str("a\n        b\n                c\n            d\n\"\"\"open");
    let lexed = lex(&source);
    assert_eq!(lexed.diagnostics.len(), LEXER_ERRORS.len() + 2);
    assert_eq!(
        count(&lexed.diagnostics, Severity::Error),
        lexed.diagnostics.len()
    );
    for code in [codes::E0100, codes::E0101, codes::E0102, codes::E0229] {
        assert!(has_code(&lexed.diagnostics, code), "{code}");
    }
}

#[test]
fn an_unterminated_multiline_string_is_left_out_like_any_other_error() {
    // The second lexer used by the comparison tests has no multiline strings,
    // so this path is checked here: one error short of the limit it is kept,
    // at the limit it is left out and the list says so.
    let multiline = "unterminated multiline string literal";
    for (before, kept) in [(MAX_ERRORS - 1, true), (MAX_ERRORS, false)] {
        let mut source = "$ ".repeat(before);
        source.push_str("\"\"\"open");
        let lexed = lex(&source);
        let last = lexed.diagnostics.last().expect("a diagnostic");
        if kept {
            assert_eq!(lexed.diagnostics.len(), MAX_ERRORS);
            assert_eq!(last.code.as_deref(), Some(codes::E0102));
            assert_eq!(last.message, multiline);
        } else {
            assert_eq!(lexed.diagnostics.len(), MAX_ERRORS + 1);
            assert_eq!(*last, too_many_errors(MAX_ERRORS));
            assert!(lexed
                .diagnostics
                .iter()
                .all(|diagnostic| diagnostic.message != multiline));
        }
    }
}

#[test]
fn the_lexer_reads_the_whole_source_whatever_it_reports() {
    // The limit is on the list, not on the tokens: a source one error past
    // the limit lexes to the tokens of the source at the limit and those of
    // the last repetition.
    for unit in LEXER_ERRORS {
        let per_unit = lex(unit).tokens.len() - lex("").tokens.len();
        let at_limit = lex(&unit.repeat(MAX_ERRORS)).tokens.len();
        let over = lex(&unit.repeat(MAX_ERRORS + 500)).tokens.len();
        assert!(per_unit > 0, "{unit:?}");
        assert!(over > at_limit, "{unit:?}");
        // Tokens grow with the source in equal steps past the limit as below
        // it.
        let below = lex(&unit.repeat(MAX_ERRORS - 500)).tokens.len();
        assert_eq!(over - at_limit, at_limit - below, "{unit:?}");
    }
}

#[test]
fn a_megabyte_of_errors_is_still_one_short_list() {
    // Under the token budget, so every byte is lexed and parsed.
    let stray = "$ ".repeat(MAX_SOURCE_LEN / 2);
    let lexed = lex(&stray);
    assert_eq!(lexed.tokens.len(), MAX_SOURCE_LEN / 2 + 2);
    assert_eq!(lexed.diagnostics.len(), MAX_ERRORS + 1);

    let parsed = parse(&stray);
    assert_eq!(parsed.diagnostics.len(), MAX_ERRORS + 1);
    assert_eq!(
        parsed.diagnostics.last(),
        Some(&too_many_errors(MAX_ERRORS))
    );

    let lines = PARSER_ERROR.repeat(MAX_SOURCE_LEN / PARSER_ERROR.len());
    let parsed = parse(&lines);
    assert_eq!(parsed.diagnostics.len(), MAX_ERRORS + 1);
    assert_eq!(
        parsed.diagnostics.last(),
        Some(&too_many_errors(MAX_ERRORS))
    );
    // Every line was parsed all the same.
    assert_eq!(
        parsed.program.items.len(),
        MAX_SOURCE_LEN / PARSER_ERROR.len()
    );
}

#[test]
fn the_parser_reports_the_first_errors_and_then_says_it_stopped() {
    let at_limit = parse(&PARSER_ERROR.repeat(MAX_ERRORS));
    assert_eq!(at_limit.diagnostics.len(), MAX_ERRORS);
    assert_eq!(count(&at_limit.diagnostics, Severity::Error), MAX_ERRORS);
    assert!(!has_code(&at_limit.diagnostics, codes::E0002));

    for over in [MAX_ERRORS + 1, 3 * MAX_ERRORS] {
        let parsed = parse(&PARSER_ERROR.repeat(over));
        assert_eq!(parsed.diagnostics[..MAX_ERRORS], at_limit.diagnostics);
        assert_eq!(
            parsed.diagnostics[MAX_ERRORS..],
            [too_many_errors(MAX_ERRORS)]
        );
        assert_eq!(parsed.program.items.len(), over);
    }
}

#[test]
fn warnings_have_their_own_room_and_leaving_them_out_is_not_an_error() {
    // The first directive is the version; each later one is a `W0005`.
    let at_limit = parse(&PARSER_WARNING.repeat(MAX_WARNINGS + 1));
    assert_eq!(at_limit.diagnostics.len(), MAX_WARNINGS);
    assert!(!has_code(&at_limit.diagnostics, codes::W0007));

    let over = parse(&PARSER_WARNING.repeat(MAX_WARNINGS + 2));
    assert_eq!(over.diagnostics[..MAX_WARNINGS], at_limit.diagnostics);
    assert_eq!(
        over.diagnostics[MAX_WARNINGS..],
        [too_many_warnings(MAX_WARNINGS)]
    );
    assert!(!has_errors(&over.diagnostics));
    assert_eq!(
        over.program.version.map(|directive| directive.version),
        Some(6)
    );

    // A flood of warnings does not use up the room for errors: the one
    // error after them is reported.
    let mut source = PARSER_WARNING.repeat(3 * MAX_WARNINGS);
    source.push_str(PARSER_ERROR);
    let parsed = parse(&source);
    assert_eq!(count(&parsed.diagnostics, Severity::Error), 1);
    assert_eq!(
        parsed.diagnostics.last(),
        Some(&too_many_warnings(MAX_WARNINGS))
    );
    assert_eq!(parsed.diagnostics.len(), MAX_WARNINGS + 2);

    // And a flood of errors does not use up the room for warnings.
    let mut source = PARSER_WARNING.repeat(3);
    source.push_str(&PARSER_ERROR.repeat(2 * MAX_ERRORS));
    source.push_str(PARSER_WARNING);
    let parsed = parse(&source);
    assert_eq!(count(&parsed.diagnostics, Severity::Warning), 3);
    assert_eq!(
        parsed.diagnostics.last(),
        Some(&too_many_errors(MAX_ERRORS))
    );
    assert_eq!(parsed.diagnostics.len(), MAX_ERRORS + 3 + 1);
}

#[test]
fn the_lexer_and_the_parser_report_into_one_list() {
    // The lexer fills the list on its own: the parser's errors are left out
    // and the list says so once, not once for each of them.
    let mut source = "$\n".repeat(MAX_ERRORS + 5);
    source.push_str(&PARSER_ERROR.repeat(10));
    let lexed = lex(&source);
    let parsed = parse(&source);
    assert_eq!(parsed.diagnostics, lexed.diagnostics);
    assert_eq!(
        parsed.diagnostics[MAX_ERRORS..],
        [too_many_errors(MAX_ERRORS)]
    );

    // The lexer leaves room: its errors come first and the parser's fill
    // what is left.
    let from_lexer = 400;
    let mut source = "#12\n".repeat(from_lexer);
    source.push_str(&PARSER_ERROR.repeat(MAX_ERRORS));
    let lexed = lex(&source);
    assert_eq!(lexed.diagnostics.len(), from_lexer);
    let parsed = parse(&source);
    assert_eq!(parsed.diagnostics[..from_lexer], lexed.diagnostics);
    assert_eq!(count(&parsed.diagnostics, Severity::Error), MAX_ERRORS + 1);
    assert_eq!(
        parsed.diagnostics[MAX_ERRORS..],
        [too_many_errors(MAX_ERRORS)]
    );
    assert!(parsed.diagnostics[from_lexer..MAX_ERRORS]
        .iter()
        .all(|diagnostic| diagnostic.code.as_deref() == Some(codes::E0110)));

    // Exactly full between them: nothing was left out, so nothing says so.
    let mut source = "#12\n".repeat(from_lexer);
    source.push_str(&PARSER_ERROR.repeat(MAX_ERRORS - from_lexer));
    let parsed = parse(&source);
    assert_eq!(parsed.diagnostics.len(), MAX_ERRORS);
    assert!(!has_code(&parsed.diagnostics, codes::E0002));
}

#[test]
fn an_error_the_parser_takes_back_leaves_no_trace_in_a_full_list() {
    // Each of these opens like a declared type, fails part way through it
    // with an error, and is then read as the comparison it is: the parser
    // takes the error back. With no room in the list that error was only
    // counted, and taking it back must uncount it. Otherwise the list would
    // end by saying that errors were left out of a source that has none.
    let statements = [
        "array < 2 ? 1 : 0",
        "map < matrix ? 1 : 0",
        "array < array < 3",
    ];
    for statement in statements {
        let source = format!("{statement}\n").repeat(20);
        assert_eq!(parse(&source).diagnostics, [], "{statement}");
        for limits in SHORT {
            let parsed = parse_into(
                &source,
                SourceId::MAIN,
                Diagnostics::with_limits(limits.0, limits.1),
            );
            assert_eq!(parsed.diagnostics, [], "{limits:?}: {statement}");
        }
    }

    // After real errors have filled the list, taking one back changes
    // neither what is kept nor the one mark at the end.
    for statement in statements {
        let mut source = PARSER_ERROR.repeat(3);
        source.push_str(&format!("{statement}\n").repeat(5));
        source.push_str(PARSER_ERROR);
        assert_only_the_list_depends_on_its_limits(&source);
        let parsed = parse_into(&source, SourceId::MAIN, Diagnostics::with_limits(2, 2));
        assert_eq!(parsed.diagnostics.len(), 3);
        assert_eq!(parsed.diagnostics.last(), Some(&too_many_errors(2)));
    }
}

#[test]
fn only_the_list_depends_on_its_limits() {
    // Written sources that fail in every way the lexer and the parser
    // report, alone and together.
    let pieces = [
        "$",
        "\"open",
        "#12",
        "x 1",
        "//@version=6",
        "//@version=5",
        "array < 2 ? 1 : 0",
        "array<array<int>> a = na",
        "f(a,) => 1",
        "if x\n    y = 1\n  z = 2",
        "a = (1 +",
        "type T\n    int",
        "x := := 1",
        "for i = 0 to\n    y",
        "plot(close)",
    ];
    for first in pieces {
        assert_only_the_list_depends_on_its_limits(first);
        for second in pieces {
            for third in ["", "$\nx 1\n//@version=6\n"] {
                assert_only_the_list_depends_on_its_limits(&format!(
                    "{first}\n{second}\n{third}{first}\n"
                ));
            }
        }
    }
}

#[test]
fn only_the_list_depends_on_its_limits_for_every_fixture() {
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
    assert!(files.len() > 50, "{} fixtures", files.len());
    let mut with_diagnostics = 0;
    for file in files {
        let Ok(source) = std::fs::read_to_string(&file) else {
            continue;
        };
        with_diagnostics += usize::from(!parse(&source).diagnostics.is_empty());
        assert_only_the_list_depends_on_its_limits(&source);
    }
    // The comparison is only worth something if some fixtures report.
    assert!(with_diagnostics > 0);
}
