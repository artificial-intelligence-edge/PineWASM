//! One analysis reports a bounded list of diagnostics, and nothing but that
//! list depends on the bound.
//!
//! The analyzer keeps the first `MAX_ERRORS` errors and the first
//! `MAX_WARNINGS` warnings it reports and ends the list with an `E0002` or a
//! `W0007` when it left some out. Several of its rules ask whether an inner
//! check already reported something, so that one mistake is not reported
//! twice. Those questions are about what was *reported*: with a full list the
//! answer must be what it would be with room for everything, or the analysis
//! itself would change with the number of mistakes that came before.

use crate::{
    analyze, analyze_into, validate_function_parameter_declarations, BuiltinRegistry,
    ImportedLibraries,
};
use pinewasm_hir::dump_hir;
use pinewasm_parse::parse;
use pinewasm_span::{
    codes, has_errors, too_many_errors, too_many_warnings, Diagnostic, Diagnostics, Severity,
    MAX_ERRORS, MAX_WARNINGS,
};
use pinewasm_syntax::ast::Item;

const HEADER: &str = "//@version=6\nindicator(\"limits\")\n";

/// One error for each repetition: an unknown identifier.
const ERROR: &str = "nope\n";

/// One warning for each repetition: an expression whose result is discarded.
const WARNING: &str = "close\n";

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

/// The analysis of `source`: its HIR as text, and its diagnostics.
fn check(source: &str) -> (String, Vec<Diagnostic>) {
    let parsed = parse(source);
    assert!(
        !has_errors(&parsed.diagnostics),
        "test source must parse cleanly: {:?}",
        parsed.diagnostics
    );
    let result = analyze(&parsed.program);
    (dump_hir(&result.hir), result.diagnostics)
}

/// [`check`], reporting into a list with `limits`.
fn check_with(source: &str, limits: (usize, usize)) -> Option<(String, Vec<Diagnostic>)> {
    let parsed = parse(source);
    if has_errors(&parsed.diagnostics) {
        return None;
    }
    let result = analyze_into(
        &parsed.program,
        BuiltinRegistry::standard_ref(),
        ImportedLibraries::default(),
        Diagnostics::with_limits(limits.0, limits.1),
    );
    Some((dump_hir(&result.hir), result.diagnostics))
}

/// What a list with `limits` holds after `reported`.
fn cut(limits: (usize, usize), reported: &[Diagnostic]) -> Vec<Diagnostic> {
    let mut list = Diagnostics::with_limits(limits.0, limits.1);
    list.extend(reported.iter().cloned());
    list.finish()
}

/// Limits small enough that an ordinary broken script runs out of room, so
/// the analyzer's own decisions are made with diagnostics left out.
const SHORT: [(usize, usize); 7] = [(0, 0), (1, 0), (0, 1), (1, 1), (2, 1), (1, 2), (3, 3)];

/// Analyzing into a short list gives the HIR a list with room for everything
/// gives, and the diagnostics that list is cut to. Returns whether the source
/// was analyzed (it parsed) and how many diagnostics it reports.
fn assert_only_the_list_depends_on_its_limits(source: &str) -> Option<usize> {
    let (hir, whole) = check_with(source, (usize::MAX, usize::MAX))?;
    for limits in SHORT {
        let (short_hir, short) = check_with(source, limits)?;
        assert_eq!(short_hir, hir, "{limits:?}: {source}");
        assert_eq!(short, cut(limits, &whole), "{limits:?}: {source}");
    }
    Some(whole.len())
}

#[test]
fn the_analyzer_reports_the_first_errors_and_then_says_it_stopped() {
    let (_, at_limit) = check(&format!("{HEADER}{}", ERROR.repeat(MAX_ERRORS)));
    assert_eq!(at_limit.len(), MAX_ERRORS);
    assert_eq!(count(&at_limit, Severity::Error), MAX_ERRORS);
    assert!(!has_code(&at_limit, codes::E0002));

    for over in [MAX_ERRORS + 1, 3 * MAX_ERRORS] {
        let (_, diagnostics) = check(&format!("{HEADER}{}", ERROR.repeat(over)));
        assert_eq!(diagnostics[..MAX_ERRORS], at_limit);
        assert_eq!(diagnostics[MAX_ERRORS..], [too_many_errors(MAX_ERRORS)]);
    }
}

#[test]
fn warnings_have_their_own_room_and_leaving_them_out_changes_no_program() {
    let (_, at_limit) = check(&format!("{HEADER}{}", WARNING.repeat(MAX_WARNINGS)));
    assert_eq!(at_limit.len(), MAX_WARNINGS);
    assert_eq!(count(&at_limit, Severity::Warning), MAX_WARNINGS);
    assert!(!has_code(&at_limit, codes::W0007));

    let source = format!("{HEADER}{}", WARNING.repeat(MAX_WARNINGS + 300));
    let (hir, diagnostics) = check(&source);
    assert_eq!(diagnostics[..MAX_WARNINGS], at_limit);
    assert_eq!(
        diagnostics[MAX_WARNINGS..],
        [too_many_warnings(MAX_WARNINGS)]
    );
    assert!(!has_errors(&diagnostics));
    // The program is the one a list with room for every warning goes with.
    let (whole_hir, whole) = check_with(&source, (usize::MAX, usize::MAX)).expect("parses");
    assert_eq!(whole.len(), MAX_WARNINGS + 300);
    assert_eq!(hir, whole_hir);

    // A flood of warnings does not use up the room for errors: the one
    // error after them is reported, and the analysis fails as it should.
    let (_, diagnostics) = check(&format!(
        "{HEADER}{}{ERROR}",
        WARNING.repeat(3 * MAX_WARNINGS)
    ));
    assert_eq!(count(&diagnostics, Severity::Error), 1);
    assert!(has_code(&diagnostics, codes::E0201));
    assert_eq!(diagnostics.len(), MAX_WARNINGS + 2);
    assert_eq!(diagnostics.last(), Some(&too_many_warnings(MAX_WARNINGS)));

    // And a flood of errors does not use up the room for warnings.
    let (_, diagnostics) = check(&format!(
        "{HEADER}{}{}",
        ERROR.repeat(2 * MAX_ERRORS),
        WARNING.repeat(4)
    ));
    assert_eq!(count(&diagnostics, Severity::Warning), 4);
    assert_eq!(diagnostics.len(), MAX_ERRORS + 4 + 1);
    assert_eq!(diagnostics.last(), Some(&too_many_errors(MAX_ERRORS)));

    // Both run out: the mark for errors comes first.
    let (_, diagnostics) = check(&format!(
        "{HEADER}{}{}",
        ERROR.repeat(MAX_ERRORS + 1),
        WARNING.repeat(MAX_WARNINGS + 1)
    ));
    assert_eq!(diagnostics.len(), MAX_ERRORS + MAX_WARNINGS + 2);
    assert_eq!(
        diagnostics[MAX_ERRORS + MAX_WARNINGS..],
        [too_many_errors(MAX_ERRORS), too_many_warnings(MAX_WARNINGS)]
    );
}

#[test]
fn a_parameter_list_reports_a_bounded_list_of_its_own() {
    // One definition with more invalid parameters than a list holds. The
    // check that reads the parameters keeps a list of its own, and that list
    // is bounded like every other.
    let over = MAX_ERRORS + 25;
    let parameters: Vec<String> = (0..over).map(|n| format!("input int p{n}")).collect();
    let source = format!("{HEADER}f({}) => 1\n", parameters.join(", "));
    let parsed = parse(&source);
    assert!(!has_errors(&parsed.diagnostics), "{:?}", parsed.diagnostics);
    let def = parsed
        .program
        .items
        .iter()
        .find_map(|item| match item {
            Item::FuncDef(def) => Some(def),
            _ => None,
        })
        .expect("the function definition");
    assert_eq!(def.params.len(), over);

    let own = validate_function_parameter_declarations(def);
    assert_eq!(own.len(), MAX_ERRORS + 1);
    assert_eq!(own.last(), Some(&too_many_errors(MAX_ERRORS)));
    assert!(own[..MAX_ERRORS]
        .iter()
        .all(|diagnostic| diagnostic.code.as_deref() == Some(codes::E0224)));

    // Reported into the analysis, it is still one list with one mark.
    let diagnostics = analyze(&parsed.program).diagnostics;
    assert_eq!(count(&diagnostics, Severity::Error), MAX_ERRORS + 1);
    assert_eq!(diagnostics[..MAX_ERRORS], own[..MAX_ERRORS]);
    assert_eq!(diagnostics.last(), Some(&too_many_errors(MAX_ERRORS)));
}

/// Sources that reach every rule which asks what an inner check reported:
/// a declaration argument that is not a constant of the right type (the
/// rule adds its own error only when the inner evaluation reported none), an
/// input default, and an unknown `matrix.new` parameter name that has a
/// suggested replacement (the rule amends an error already in the list).
const RULES_THAT_ASK_WHAT_WAS_REPORTED: [&str; 36] = [
    "library(1)\n",
    "library(close)\n",
    "library(nope)\n",
    "indicator(1)\n",
    "indicator(close)\n",
    "indicator(nope)\n",
    "indicator(\"x\", scale = 1)\n",
    "indicator(\"x\", scale = close)\n",
    "indicator(\"x\", scale = nope)\n",
    "indicator(\"x\", scale = \"bad\")\n",
    "indicator(\"x\", max_bars_back = \"a\")\n",
    "indicator(\"x\", max_bars_back = close)\n",
    "indicator(\"x\", max_bars_back = nope)\n",
    "strategy(1)\n",
    "strategy(\"x\", currency = 1)\n",
    "strategy(\"x\", currency = close)\n",
    "strategy(\"x\", currency = nope)\n",
    "strategy(\"x\", pyramiding = \"a\")\n",
    "strategy(\"x\", pyramiding = close)\n",
    "strategy(\"x\", pyramiding = nope)\n",
    "strategy(\"x\", initial_capital = \"a\")\n",
    "strategy(\"x\", initial_capital = close)\n",
    "strategy(\"x\", initial_capital = nope)\n",
    "strategy(\"x\", process_orders_on_close = 1)\n",
    "strategy(\"x\", process_orders_on_close = close)\n",
    "strategy(\"x\", process_orders_on_close = nope)\n",
    "indicator(\"x\")\na = input.int(nope)\n",
    "indicator(\"x\")\na = input.int(close)\n",
    "indicator(\"x\")\na = input.int(\"a\")\n",
    "indicator(\"x\")\na = input.float(nope)\n",
    "indicator(\"x\")\na = input.bool(1)\n",
    "indicator(\"x\")\na = input.string(nope)\n",
    "indicator(\"x\")\na = input.int(1, minval = nope)\n",
    "indicator(\"x\")\nm = matrix.new<int>(2, 2, initial = 0)\n",
    "indicator(\"x\")\nm = matrix.new<float>(2, 2, initial = nope)\n",
    "indicator(\"x\")\nm = matrix.new<int>(2, 2, initial_value = 0)\n",
];

#[test]
fn only_the_list_depends_on_its_limits() {
    // Each rule alone, then after an earlier error or warning has used up
    // the room, then followed by more of each.
    let mut reporting = 0;
    for rule in RULES_THAT_ASK_WHAT_WAS_REPORTED {
        let (declaration, rest) = rule.split_once('\n').expect("a declaration line");
        for source in [
            format!("//@version=6\n{rule}"),
            format!("//@version=6\n{declaration}\n{ERROR}{WARNING}{rest}"),
            format!("//@version=6\n{declaration}\n{rest}{WARNING}{ERROR}{rest}"),
            format!("//@version=6\n{declaration}\n{WARNING}{WARNING}{rest}{ERROR}{ERROR}"),
        ] {
            let reported = assert_only_the_list_depends_on_its_limits(&source)
                .unwrap_or_else(|| panic!("must parse: {source}"));
            reporting += usize::from(reported > 0);
        }
    }
    // Nearly every one of these is a mistake; the comparison is only worth
    // something while they report.
    assert!(reporting >= 4 * (RULES_THAT_ASK_WHAT_WAS_REPORTED.len() - 1));
}

/// Rules whose inner check analyzes a function body. A body is analyzed at
/// its call, so its errors are reported after the rule's mark, and an error
/// in it can leave the call's result a constant of the right type. Then
/// only the question "was an error reported?" keeps the rule from folding
/// that result into the program, and it must get the same answer when the
/// error was left out of a full list.
const ERRORS_INSIDE_A_CALLED_FUNCTION: [&str; 4] = [
    "f() =>\n    a = nope\n    \"t\"\nindicator(title = f())\nplot(close)\n",
    "f() =>\n    a = nope\n    \"t\"\nindicator(\"x\", shorttitle = f())\nplot(close)\n",
    "g() =>\n    a = nope\n    2\nstrategy(\"x\", pyramiding = g())\nplot(close)\n",
    "g() =>\n    a = nope\n    2.5\nstrategy(\"x\", initial_capital = g())\nplot(close)\n",
];

#[test]
fn an_error_inside_a_called_function_is_seen_however_full_the_list_is() {
    for body in ERRORS_INSIDE_A_CALLED_FUNCTION {
        let source = format!("//@version=6\n{body}");
        let reported = assert_only_the_list_depends_on_its_limits(&source)
            .unwrap_or_else(|| panic!("must parse: {source}"));
        let (_, diagnostics) = check(&source);
        assert!(reported > 0, "{source}");
        assert!(has_code(&diagnostics, codes::E0201), "{source}");
        // An earlier error fills the list first: the one in the body is left
        // out, and the program is still the one a whole list goes with.
        let crowded = source.replacen("f() =>", &format!("{ERROR}f() =>"), 1);
        let crowded = crowded.replacen("g() =>", &format!("{ERROR}g() =>"), 1);
        assert_only_the_list_depends_on_its_limits(&crowded)
            .unwrap_or_else(|| panic!("must parse: {crowded}"));
    }
}

#[test]
fn the_renamed_matrix_parameter_note_reaches_an_error_that_was_kept() {
    let source = "//@version=6\nindicator(\"x\")\nm = matrix.new<int>(2, 2, initial = 0)\n";
    let (_, diagnostics) = check(source);
    let renamed = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code.as_deref() == Some(codes::E0228))
        .expect("the unknown parameter");
    assert_eq!(renamed.notes, ["did you mean `initial_value`?"]);

    // The rule amends what its own call reported and nothing before it: a
    // second call does not add the note to the first call's error again.
    let twice = format!("{source}n = matrix.new<int>(2, 2, initial = 0)\n");
    let (_, diagnostics) = check(&twice);
    let renamed: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code.as_deref() == Some(codes::E0228))
        .collect();
    assert_eq!(renamed.len(), 2);
    for diagnostic in renamed {
        assert_eq!(diagnostic.notes, ["did you mean `initial_value`?"]);
    }

    // With no room for the error there is nothing to amend, and the rule
    // must find that out without reaching past the end of the list.
    let (_, diagnostics) = check_with(source, (0, 0)).expect("parses");
    assert_eq!(diagnostics, [too_many_errors(0)]);

    // An earlier error took the only room: the rule's error is left out and
    // the earlier one is not amended in its place.
    let earlier = "//@version=6\nindicator(\"x\")\nnope\n";
    let (_, alone) = check(earlier);
    assert_eq!(alone.len(), 1);
    assert_eq!(alone[0].code.as_deref(), Some(codes::E0201));
    let source = format!("{earlier}m = matrix.new<int>(2, 2, initial = 0)\n");
    let (_, diagnostics) = check_with(&source, (1, 0)).expect("parses");
    assert_eq!(diagnostics, [alone[0].clone(), too_many_errors(1)]);
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
    let (mut analyzed, mut reporting) = (0, 0);
    for file in files {
        let Ok(source) = std::fs::read_to_string(&file) else {
            continue;
        };
        if let Some(reported) = assert_only_the_list_depends_on_its_limits(&source) {
            analyzed += 1;
            reporting += usize::from(reported > 0);
        }
    }
    assert!(analyzed > 50, "{analyzed} fixtures analyzed");
    assert!(reporting > 10, "{reporting} fixtures report diagnostics");
}
