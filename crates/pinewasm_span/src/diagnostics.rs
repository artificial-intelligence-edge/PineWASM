//! The bounded list of diagnostics a stage of the compiler reports.
//!
//! A source can be wrong in more places than anyone will read: a megabyte of
//! stray characters is a million lexer errors, and holding a [`Diagnostic`]
//! for each costs memory in proportion. Every stage therefore records its
//! diagnostics in a [`Diagnostics`], which keeps the first [`MAX_ERRORS`]
//! errors and the first [`MAX_WARNINGS`] warnings and only counts the rest.
//! [`Diagnostics::finish`] ends a list that left some out with an `E0002` or
//! a `W0007`, so the list itself says that it is not the whole story.
//!
//! The bound is on how many diagnostics a list holds, not on how large one
//! of them is.
//!
//! Errors and warnings are bounded separately: warnings never crowd out an
//! error, and a list that left an error out still holds errors, so cutting a
//! list short never turns a failed compile into a successful one.
//!
//! A stage must decide nothing from what was kept. It asks what was
//! *reported*, through a [`DiagnosticsMark`], and gets the same answer whether
//! or not the diagnostics in question were kept.

use crate::{codes, Diagnostic, Severity};

/// The most errors one list of diagnostics holds.
pub const MAX_ERRORS: usize = 1000;

/// The most warnings one list of diagnostics holds.
pub const MAX_WARNINGS: usize = 1000;

// The lexer reports through `too_many_errors`, and it keeps its diagnostic
// text visible to the extracted value model, so this text is built the way
// the lexer builds its own numbers: no formatting machinery, and the same
// loops. A `u64` has at most twenty decimal digits; each remainder is in
// 0..=9.
fn append_decimal(text: &mut String, value: u64) {
    let mut digits = [0_u8; 20];
    let mut index = 20;
    let mut rest = value;
    while rest >= 10 {
        index -= 1;
        digits[index] = b'0' + (rest % 10) as u8;
        rest /= 10;
    }
    index -= 1;
    digits[index] = b'0' + rest as u8;
    while index < 20 {
        text.push(char::from(digits[index]));
        index += 1;
    }
}

fn only_the_first(what: &str, kept: usize) -> String {
    let mut text = String::new();
    text.push_str("too many ");
    text.push_str(what);
    text.push_str(": only the first ");
    // Exact on every target with `usize::BITS <= 64`.
    append_decimal(&mut text, kept as u64);
    text.push_str(" are reported");
    text
}

/// The `E0002` that ends a list of diagnostics which left errors out. `kept`
/// is how many errors the list holds. It carries no label: it is about the
/// list, not about a place in the source.
pub fn too_many_errors(kept: usize) -> Diagnostic {
    Diagnostic::error(codes::E0002, only_the_first("errors", kept))
        .with_note("fix the errors reported and compile again to see any that remain")
}

/// The `W0007` that ends a list of diagnostics which left warnings out.
/// `kept` is how many warnings the list holds. It carries no label.
pub fn too_many_warnings(kept: usize) -> Diagnostic {
    Diagnostic::warning(codes::W0007, only_the_first("warnings", kept))
        .with_note("resolve the warnings reported and compile again to see any that remain")
}

fn has_code(diagnostic: &Diagnostic, code: &str) -> bool {
    diagnostic.code.as_deref() == Some(code)
}

/// A point in the life of a [`Diagnostics`], from [`Diagnostics::mark`].
///
/// It answers what was reported after that point, kept or not, and is where
/// [`Diagnostics::rollback`] returns to. A mark belongs to the list it was
/// taken from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiagnosticsMark {
    kept: usize,
    kept_errors: usize,
    kept_warnings: usize,
    errors: u64,
    warnings: u64,
}

/// The diagnostics one stage reports, in order, holding at most
/// [`MAX_ERRORS`] errors and [`MAX_WARNINGS`] warnings.
///
/// The errors kept are always the first ones reported, and likewise the
/// warnings: once one of a severity has been left out, every later one of
/// that severity is left out too.
#[derive(Debug, Clone)]
pub struct Diagnostics {
    kept: Vec<Diagnostic>,
    kept_errors: usize,
    kept_warnings: usize,
    /// Errors reported, kept or not. Above `kept_errors` once one was left
    /// out.
    errors: u64,
    /// Warnings reported, kept or not.
    warnings: u64,
    max_errors: usize,
    max_warnings: usize,
}

impl Default for Diagnostics {
    fn default() -> Self {
        Diagnostics::new()
    }
}

impl Diagnostics {
    /// An empty list with the limits every stage uses.
    pub fn new() -> Self {
        Diagnostics::with_limits(MAX_ERRORS, MAX_WARNINGS)
    }

    /// An empty list that keeps at most `max_errors` errors and
    /// `max_warnings` warnings. The compiler uses [`Diagnostics::new`]; other
    /// limits are for tests that compare a short list with a long one.
    pub fn with_limits(max_errors: usize, max_warnings: usize) -> Self {
        Diagnostics {
            kept: Vec::new(),
            kept_errors: 0,
            kept_warnings: 0,
            errors: 0,
            warnings: 0,
            max_errors,
            max_warnings,
        }
    }

    /// Report one diagnostic. It is kept while its severity has room and
    /// counted either way.
    ///
    /// An `E0002` or `W0007` is the mark another list left when it ran out of
    /// room. It is not kept as a diagnostic of its own: it records that
    /// diagnostics of its severity were left out, and
    /// [`Diagnostics::finish`] says so once, at the end. That makes a
    /// finished list safe to report into another one.
    pub fn push(&mut self, diagnostic: Diagnostic) {
        match diagnostic.severity {
            Severity::Error => {
                if self.errors == self.kept_errors as u64
                    && self.kept_errors < self.max_errors
                    && !has_code(&diagnostic, codes::E0002)
                {
                    self.kept.push(diagnostic);
                    self.kept_errors += 1;
                }
                self.errors += 1;
            }
            Severity::Warning => {
                if self.warnings == self.kept_warnings as u64
                    && self.kept_warnings < self.max_warnings
                    && !has_code(&diagnostic, codes::W0007)
                {
                    self.kept.push(diagnostic);
                    self.kept_warnings += 1;
                }
                self.warnings += 1;
            }
        }
    }

    /// Whether nothing has been reported.
    pub fn is_empty(&self) -> bool {
        self.errors == 0 && self.warnings == 0
    }

    /// The current point, to ask later what was reported after it or to
    /// [`Diagnostics::rollback`] to.
    pub fn mark(&self) -> DiagnosticsMark {
        DiagnosticsMark {
            kept: self.kept.len(),
            kept_errors: self.kept_errors,
            kept_warnings: self.kept_warnings,
            errors: self.errors,
            warnings: self.warnings,
        }
    }

    /// Whether any diagnostic was reported after `mark`, kept or not.
    pub fn reported_since(&self, mark: DiagnosticsMark) -> bool {
        let () = debug_assert!(self.has_reached(mark), "a mark withdrawn by a rollback");
        self.errors != mark.errors || self.warnings != mark.warnings
    }

    /// Whether an error was reported after `mark`, kept or not. A rule that
    /// skips its own error when an inner check already reported one asks
    /// this, not [`Self::reported_since`]: a warning is no reason to skip an
    /// error.
    pub fn errors_since(&self, mark: DiagnosticsMark) -> bool {
        let () = debug_assert!(self.has_reached(mark), "a mark withdrawn by a rollback");
        self.errors != mark.errors
    }

    /// Whether the list has reported at least what it had at `mark`. A mark
    /// taken after the point a later rollback returned to fails this until
    /// the counts grow back, and asking about it is a mistake: the answer
    /// would compare against diagnostics that were withdrawn.
    fn has_reached(&self, mark: DiagnosticsMark) -> bool {
        mark.errors <= self.errors && mark.warnings <= self.warnings
    }

    /// The diagnostics reported after `mark` that were kept, to amend in
    /// place. One that was left out cannot be amended and does not need to
    /// be.
    pub fn kept_since_mut(&mut self, mark: DiagnosticsMark) -> &mut [Diagnostic] {
        self.kept.get_mut(mark.kept..).unwrap_or_default()
    }

    /// Withdraw everything reported after `mark`, kept or not, leaving the
    /// list as it was when the mark was taken.
    pub fn rollback(&mut self, mark: DiagnosticsMark) {
        self.kept.truncate(mark.kept);
        self.kept_errors = mark.kept_errors;
        self.kept_warnings = mark.kept_warnings;
        self.errors = mark.errors;
        self.warnings = mark.warnings;
    }

    /// The diagnostics kept, in the order they were reported, followed by an
    /// `E0002` if errors were left out and a `W0007` if warnings were.
    pub fn finish(self) -> Vec<Diagnostic> {
        let mut list = self.kept;
        if self.errors > self.kept_errors as u64 {
            list.push(too_many_errors(self.kept_errors));
        }
        if self.warnings > self.kept_warnings as u64 {
            list.push(too_many_warnings(self.kept_warnings));
        }
        list
    }
}

impl Extend<Diagnostic> for Diagnostics {
    fn extend<I: IntoIterator<Item = Diagnostic>>(&mut self, diagnostics: I) {
        for diagnostic in diagnostics {
            self.push(diagnostic);
        }
    }
}

impl FromIterator<Diagnostic> for Diagnostics {
    fn from_iter<I: IntoIterator<Item = Diagnostic>>(diagnostics: I) -> Self {
        let mut list = Diagnostics::new();
        list.extend(diagnostics);
        list
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{has_errors, SourceId, Span};
    use pretty_assertions::assert_eq;

    fn error(n: usize) -> Diagnostic {
        Diagnostic::error("E0110", format!("error {n}"))
            .with_label(Span::new(SourceId::MAIN, n as u32, n as u32 + 1), "here")
    }

    fn warning(n: usize) -> Diagnostic {
        Diagnostic::warning("W0002", format!("warning {n}"))
    }

    fn finished(limits: (usize, usize), reported: &[Diagnostic]) -> Vec<Diagnostic> {
        let mut diagnostics = Diagnostics::with_limits(limits.0, limits.1);
        diagnostics.extend(reported.iter().cloned());
        diagnostics.finish()
    }

    /// What a list must hold, written without the list: the first errors and
    /// the first warnings in their original order, then the marks.
    fn expected(limits: (usize, usize), reported: &[Diagnostic]) -> Vec<Diagnostic> {
        let (mut errors, mut warnings) = (0, 0);
        let mut list = Vec::new();
        for diagnostic in reported {
            let (seen, limit) = match diagnostic.severity {
                Severity::Error => (&mut errors, limits.0),
                Severity::Warning => (&mut warnings, limits.1),
            };
            if *seen < limit {
                list.push(diagnostic.clone());
            }
            *seen += 1;
        }
        if errors > limits.0 {
            list.push(too_many_errors(limits.0));
        }
        if warnings > limits.1 {
            list.push(too_many_warnings(limits.1));
        }
        list
    }

    /// A fixed sequence of pseudo-random numbers, so a failure reproduces.
    struct Sequence(u64);

    impl Sequence {
        fn next(&mut self, below: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((self.0 >> 33) as usize) % below
        }
    }

    fn mixed(sequence: &mut Sequence, len: usize) -> Vec<Diagnostic> {
        (0..len)
            .map(|n| {
                if sequence.next(3) == 0 {
                    warning(n)
                } else {
                    error(n)
                }
            })
            .collect()
    }

    #[test]
    fn a_list_within_its_limits_is_reported_whole_and_in_order() {
        let reported = [error(0), warning(1), error(2), warning(3)];
        assert_eq!(finished((2, 2), &reported), reported);
        assert_eq!(finished((MAX_ERRORS, MAX_WARNINGS), &reported), reported);
        assert_eq!(Diagnostics::new().finish(), []);
        assert_eq!(Diagnostics::default().finish(), []);
        let collected: Diagnostics = reported.iter().cloned().collect();
        assert_eq!(collected.finish(), reported);
    }

    #[test]
    fn the_default_limits_are_the_documented_ones() {
        assert_eq!((MAX_ERRORS, MAX_WARNINGS), (1000, 1000));
        for (count, marked) in [(MAX_ERRORS, false), (MAX_ERRORS + 1, true)] {
            let errors: Vec<_> = (0..count).map(error).collect();
            let mut diagnostics = Diagnostics::new();
            diagnostics.extend(errors.iter().cloned());
            let list = diagnostics.finish();
            assert_eq!(list[..MAX_ERRORS], errors[..MAX_ERRORS]);
            assert_eq!(list.len(), MAX_ERRORS + usize::from(marked));
            assert_eq!(list.last() == Some(&too_many_errors(MAX_ERRORS)), marked);
        }
        for (count, marked) in [(MAX_WARNINGS, false), (MAX_WARNINGS + 1, true)] {
            let warnings: Vec<_> = (0..count).map(warning).collect();
            let mut diagnostics = Diagnostics::new();
            diagnostics.extend(warnings.iter().cloned());
            let list = diagnostics.finish();
            assert_eq!(list[..MAX_WARNINGS], warnings[..MAX_WARNINGS]);
            assert_eq!(list.len(), MAX_WARNINGS + usize::from(marked));
            assert_eq!(
                list.last() == Some(&too_many_warnings(MAX_WARNINGS)),
                marked
            );
        }
    }

    #[test]
    fn the_marks_say_what_was_left_out_and_point_at_nothing() {
        let errors = too_many_errors(1000);
        assert_eq!(errors.severity, Severity::Error);
        assert_eq!(errors.code.as_deref(), Some("E0002"));
        assert_eq!(
            errors.message,
            "too many errors: only the first 1000 are reported"
        );
        assert_eq!(errors.labels, []);
        assert_eq!(errors.notes.len(), 1);

        let warnings = too_many_warnings(1000);
        assert_eq!(warnings.severity, Severity::Warning);
        assert_eq!(warnings.code.as_deref(), Some("W0007"));
        assert_eq!(
            warnings.message,
            "too many warnings: only the first 1000 are reported"
        );
        assert_eq!(warnings.labels, []);
        assert_eq!(warnings.notes.len(), 1);

        for kept in [
            0,
            1,
            7,
            9,
            10,
            11,
            99,
            100,
            999,
            1000,
            1001,
            123_456_789,
            usize::MAX,
        ] {
            assert_eq!(
                too_many_errors(kept).message,
                format!("too many errors: only the first {kept} are reported")
            );
            assert_eq!(
                too_many_warnings(kept).message,
                format!("too many warnings: only the first {kept} are reported")
            );
        }
    }

    #[test]
    fn each_severity_has_its_own_room() {
        // A flood of warnings first: the error after it is still reported.
        let mut reported: Vec<_> = (0..50).map(warning).collect();
        reported.push(error(50));
        let list = finished((3, 3), &reported);
        assert_eq!(
            list,
            [
                warning(0),
                warning(1),
                warning(2),
                error(50),
                too_many_warnings(3)
            ]
        );
        assert!(has_errors(&list));

        // A flood of errors first: the warning after it is still reported.
        let mut reported: Vec<_> = (0..50).map(error).collect();
        reported.push(warning(50));
        assert_eq!(
            finished((3, 3), &reported),
            [
                error(0),
                error(1),
                error(2),
                warning(50),
                too_many_errors(3)
            ]
        );

        // Both: the error mark comes before the warning mark.
        let mut reported: Vec<_> = (0..10).map(error).collect();
        reported.extend((10..20).map(warning));
        assert_eq!(
            finished((1, 1), &reported),
            [
                error(0),
                warning(10),
                too_many_errors(1),
                too_many_warnings(1)
            ]
        );
    }

    #[test]
    fn a_cut_list_never_turns_failure_into_success() {
        let mut sequence = Sequence(7);
        for limits in [(0, 0), (0, 5), (1, 0), (1, 1), (2, 9), (5, 2)] {
            for len in 0..40 {
                let reported = mixed(&mut sequence, len);
                let list = finished(limits, &reported);
                assert_eq!(has_errors(&list), has_errors(&reported), "{limits:?}");
                // Only warnings left out: still no error.
                let warnings: Vec<_> = (0..len).map(warning).collect();
                assert!(!has_errors(&finished(limits, &warnings)));
            }
        }
    }

    #[test]
    fn a_list_holds_the_first_of_each_severity_and_never_more_than_its_limits() {
        let mut sequence = Sequence(1);
        for round in 0..400 {
            let limits = (sequence.next(6), sequence.next(6));
            let reported = mixed(&mut sequence, round % 37);
            let mut diagnostics = Diagnostics::with_limits(limits.0, limits.1);
            for diagnostic in &reported {
                diagnostics.push(diagnostic.clone());
                assert!(diagnostics.kept.len() <= limits.0 + limits.1);
            }
            assert_eq!(diagnostics.finish(), expected(limits, &reported));
        }
    }

    #[test]
    fn a_long_flood_holds_no_more_than_the_limits() {
        let mut diagnostics = Diagnostics::new();
        for n in 0..(3 * MAX_ERRORS + 17) {
            diagnostics.push(error(n));
            diagnostics.push(warning(n));
        }
        assert_eq!(diagnostics.kept.len(), MAX_ERRORS + MAX_WARNINGS);
        assert_eq!(diagnostics.finish().len(), MAX_ERRORS + MAX_WARNINGS + 2);
    }

    #[test]
    fn a_finished_list_reports_into_another_without_repeating_its_marks() {
        let mut sequence = Sequence(3);
        for round in 0..300 {
            let limits = (sequence.next(5), sequence.next(5));
            let first = mixed(&mut sequence, round % 23);
            let second = mixed(&mut sequence, round % 19);

            // Finishing twice changes nothing.
            let once = finished(limits, &first);
            assert_eq!(finished(limits, &once), once);

            // Two finished lists reported into a third are the list of
            // everything reported, cut once.
            let mut merged = Diagnostics::with_limits(limits.0, limits.1);
            merged.extend(finished(limits, &first));
            merged.extend(finished(limits, &second));
            let together: Vec<_> = first.iter().chain(&second).cloned().collect();
            assert_eq!(merged.finish(), expected(limits, &together));
        }
    }

    #[test]
    fn a_mark_from_a_shorter_list_stops_a_longer_one_at_the_same_place() {
        // The first list kept two errors and left some out. A list with more
        // room must not keep a later error after that gap: what it holds is
        // always the first errors reported, with nothing missing in between.
        let mut longer = Diagnostics::with_limits(10, 10);
        longer.extend(finished((2, 2), &[error(0), error(1), error(2)]));
        longer.push(error(3));
        longer.push(warning(4));
        assert_eq!(
            longer.finish(),
            [error(0), error(1), warning(4), too_many_errors(2)]
        );
    }

    #[test]
    fn a_mark_answers_for_diagnostics_that_were_not_kept() {
        let mut diagnostics = Diagnostics::with_limits(1, 1);
        diagnostics.push(error(0));
        diagnostics.push(warning(1));

        let mark = diagnostics.mark();
        assert!(!diagnostics.reported_since(mark));
        assert!(!diagnostics.errors_since(mark));
        assert!(!diagnostics.is_empty());
        assert!(Diagnostics::new().is_empty());
        // A list with no room at all still knows something was reported.
        let mut full = Diagnostics::with_limits(0, 0);
        full.push(warning(0));
        assert!(!full.is_empty());

        // Nothing below is kept: both severities are full.
        diagnostics.push(warning(2));
        assert!(diagnostics.reported_since(mark));
        assert!(!diagnostics.errors_since(mark));

        let after_warning = diagnostics.mark();
        diagnostics.push(error(3));
        assert!(diagnostics.reported_since(after_warning));
        assert!(diagnostics.errors_since(after_warning));
        assert!(diagnostics.errors_since(mark));
        assert_eq!(diagnostics.kept.len(), 2);
    }

    #[test]
    fn the_diagnostics_kept_since_a_mark_can_be_amended() {
        let mut diagnostics = Diagnostics::with_limits(2, 2);
        diagnostics.push(error(0));
        let mark = diagnostics.mark();
        assert_eq!(diagnostics.kept_since_mut(mark), []);
        diagnostics.push(warning(1));
        diagnostics.push(error(2));
        diagnostics.push(error(3)); // left out
        for diagnostic in diagnostics.kept_since_mut(mark) {
            diagnostic.notes.push("amended".to_owned());
        }
        assert_eq!(
            diagnostics.finish(),
            [
                error(0),
                warning(1).with_note("amended"),
                error(2).with_note("amended"),
                too_many_errors(2)
            ]
        );

        // A mark past the end of the list (the list was rolled back to an
        // earlier one) has nothing after it.
        let mut diagnostics = Diagnostics::with_limits(2, 2);
        let start = diagnostics.mark();
        diagnostics.push(error(0));
        let later = diagnostics.mark();
        diagnostics.rollback(start);
        assert_eq!(diagnostics.kept_since_mut(later), []);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "a mark withdrawn by a rollback")]
    fn asking_about_a_withdrawn_mark_is_caught_in_debug_builds() {
        let mut diagnostics = Diagnostics::new();
        let start = diagnostics.mark();
        diagnostics.push(error(0));
        let withdrawn = diagnostics.mark();
        diagnostics.rollback(start);
        let _ = diagnostics.reported_since(withdrawn);
    }

    #[test]
    fn rollback_withdraws_what_was_reported_after_the_mark_kept_or_not() {
        let mut sequence = Sequence(11);
        for round in 0..400 {
            let limits = (sequence.next(5), sequence.next(5));
            let before = mixed(&mut sequence, round % 13);
            let withdrawn = mixed(&mut sequence, round % 17);
            let after = mixed(&mut sequence, round % 11);

            let mut diagnostics = Diagnostics::with_limits(limits.0, limits.1);
            diagnostics.extend(before.iter().cloned());
            let mark = diagnostics.mark();
            diagnostics.extend(withdrawn.iter().cloned());
            diagnostics.rollback(mark);
            assert_eq!(diagnostics.mark(), mark);
            assert!(!diagnostics.reported_since(mark));
            diagnostics.extend(after.iter().cloned());

            let kept: Vec<_> = before.iter().chain(&after).cloned().collect();
            assert_eq!(diagnostics.finish(), expected(limits, &kept));
        }
    }

    #[test]
    fn a_diagnostic_without_a_code_is_kept_like_any_other() {
        let internal = Diagnostic {
            severity: Severity::Error,
            code: None,
            message: "internal".to_owned(),
            labels: Vec::new(),
            notes: Vec::new(),
        };
        assert_eq!(
            finished((1, 1), &[internal.clone(), internal.clone()]),
            [internal, too_many_errors(1)]
        );
    }
}
