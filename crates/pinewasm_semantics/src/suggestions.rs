//! "Did you mean" lookups, with a bound on the work one analysis spends on
//! them.
//!
//! A suggestion compares an unknown name with every name it could have been:
//! every built-in name for an unknown identifier or function, every method of
//! a type for an unknown method, every parameter of a callee for an unknown
//! named argument. A script can hold as many unknown names as it has room
//! for, and some of those candidate sets grow with the script as well, so
//! unbounded the searching is the product of the two: far more work than the
//! notes are worth, and most of the time a script full of unknown names takes
//! to analyze. [`Suggestions`] counts the names looked at and starts no
//! search once the budget is spent. A diagnostic reported after that has no
//! note; it is otherwise the same diagnostic.

use crate::registry::{closest, BuiltinRegistry};
use std::collections::HashMap;

/// Names one analysis may look at while searching for suggestions. A search
/// that starts with budget left runs to its end, so the total can pass this
/// by one search.
///
/// It pays for over a thousand searches of the whole built-in table, or many
/// thousands of a short list such as a callee's parameters: more distinct
/// unknown names than a script someone is working on contains.
pub(crate) const SUGGESTION_BUDGET: usize = 1 << 20;

/// The suggestion searches of one analysis.
pub(crate) struct Suggestions {
    /// Names that may still be looked at.
    left: usize,
    /// What a search of the built-in names answered, by the name searched
    /// for. One misspelled name used many times is the common case; every use
    /// gets the note the first one paid for.
    builtin: HashMap<String, Option<&'static str>>,
}

impl Suggestions {
    pub(crate) fn new() -> Self {
        Self::with_budget(SUGGESTION_BUDGET)
    }

    pub(crate) fn with_budget(budget: usize) -> Self {
        Suggestions {
            left: budget,
            builtin: HashMap::new(),
        }
    }

    /// The built-in name closest to `name`, if one is close.
    pub(crate) fn builtin(
        &mut self,
        registry: &BuiltinRegistry,
        name: &str,
    ) -> Option<&'static str> {
        if let Some(answer) = self.builtin.get(name) {
            return *answer;
        }
        if self.left == 0 {
            return None;
        }
        self.spend(registry.suggestion_candidates());
        let answer = registry.suggest(name);
        self.builtin.insert(name.to_string(), answer);
        answer
    }

    /// The candidate closest to `name`, if one is close.
    ///
    /// `scan` yields every name looked at to find the candidates, as `None`
    /// when the name turned out not to be one, so that looking is charged
    /// like comparing. It is not called once the budget is spent.
    pub(crate) fn among<'a, I>(&mut self, name: &str, scan: impl FnOnce() -> I) -> Option<&'a str>
    where
        I: Iterator<Item = Option<&'a str>>,
    {
        if self.left == 0 {
            return None;
        }
        let mut looked_at = 0usize;
        let answer = closest(name, scan().inspect(|_| looked_at += 1).flatten());
        self.spend(looked_at);
        answer
    }

    /// Charge one search. Even a search of nothing costs one, so that no
    /// sequence of searches is free.
    fn spend(&mut self, looked_at: usize) {
        self.left = self.left.saturating_sub(looked_at.max(1));
    }

    /// Names that may still be looked at.
    #[cfg(test)]
    pub(crate) fn left(&self) -> usize {
        self.left
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> &'static BuiltinRegistry {
        BuiltinRegistry::standard_ref()
    }

    #[test]
    fn a_built_in_search_is_paid_for_once_per_name() {
        let table = registry().suggestion_candidates();
        let mut suggestions = Suggestions::new();
        assert_eq!(suggestions.builtin(registry(), "clos"), Some("close"));
        assert_eq!(suggestions.left(), SUGGESTION_BUDGET - table);
        // The same name again, however often, is answered from the first
        // search, whether that search found a name or not.
        for _ in 0..1_000 {
            assert_eq!(suggestions.builtin(registry(), "clos"), Some("close"));
        }
        assert_eq!(suggestions.left(), SUGGESTION_BUDGET - table);
        assert_eq!(suggestions.builtin(registry(), "no_such_name_at_all"), None);
        assert_eq!(suggestions.builtin(registry(), "no_such_name_at_all"), None);
        assert_eq!(suggestions.left(), SUGGESTION_BUDGET - 2 * table);
    }

    #[test]
    fn searching_stops_when_the_budget_is_spent() {
        let table = registry().suggestion_candidates();
        // Room for two searches of the built-in names and part of a third.
        let mut suggestions = Suggestions::with_budget(2 * table + 1);
        assert_eq!(suggestions.builtin(registry(), "clos"), Some("close"));
        assert_eq!(suggestions.builtin(registry(), "opem"), Some("open"));
        // A search that starts inside the budget runs to its end.
        assert_eq!(suggestions.builtin(registry(), "hihg"), Some("high"));
        assert_eq!(suggestions.left(), 0);
        // After that nothing is searched, in the built-in names or a list.
        assert_eq!(suggestions.builtin(registry(), "volumee"), None);
        let mut scanned = false;
        let answer = suggestions.among("lenght", || {
            scanned = true;
            ["length"].into_iter().map(Some)
        });
        assert_eq!(answer, None);
        assert!(!scanned);
        // Names answered while there was budget keep their answer.
        assert_eq!(suggestions.builtin(registry(), "clos"), Some("close"));
    }

    #[test]
    fn a_list_search_is_charged_for_every_name_looked_at() {
        let mut suggestions = Suggestions::with_budget(100);
        // Five names looked at, two of them candidates.
        let scan = || [None, Some("length"), None, Some("offset"), None].into_iter();
        assert_eq!(suggestions.among("lenght", scan), Some("length"));
        assert_eq!(suggestions.left(), 95);
        // A search that looks at nothing still costs one.
        assert_eq!(suggestions.among("lenght", std::iter::empty), None);
        assert_eq!(suggestions.left(), 94);
    }
}
