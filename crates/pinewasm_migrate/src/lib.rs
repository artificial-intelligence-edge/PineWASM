//! Source-to-source migration of Pine Script® v5 into Pine Script® v6 source.
//!
//! PineWASM compiles Pine v6 only. A v5 script is not a slightly different
//! v6 script: v6 changed what some unchanged-looking code *means* (`and`
//! and `or` stop evaluating early, `5 / 2` is `2.5`, `bool` lost its `na`
//! state, `timeframe.period` spells daily as `"1D"`), removed parameters
//! (`when`, `transp`), and tightened types. This crate rewrites a v5 script
//! into v6 source that compiles, following the rules described below.
//!
//! The rewrite is a sequence of phases (call normalization and strategy
//! semantics, types and `na`, evaluation order, history references,
//! built-in values, requests, then the version directive). Each phase
//! parses the current text, builds a v5-faithful type model, and splices
//! minimal text edits into it, repeating until it has nothing left to
//! change. Splicing instead of reprinting keeps
//! every comment and all untouched formatting.
//!
//! Some v5 behavior has no v6 spelling at all (a three-state `bool`, a
//! `series` plot offset). Those sites are reported as [`Finding`]s with
//! [`RuleClass::Diagnostic`] instead of being rewritten silently.
//!
//! ```
//! use pinewasm_migrate::{migrate, MigrationOptions, Status};
//!
//! let v5 = "//@version=5\nindicator(\"Demo\")\nplot(close, color = color.red, transp = 50)\n";
//! let result = migrate(v5, &MigrationOptions::default());
//! assert_eq!(result.status, Status::Migrated);
//! assert!(result.output.starts_with("//@version=6"));
//! assert!(result.output.contains("color.new(#FF5252, 50)"));
//! ```
#![forbid(unsafe_code)]

mod builtins;
mod model;
mod rules;
mod text;

use serde::Serialize;

/// How far the migration should go when v6 offers both a literal
/// translation and an idiomatic one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Profile {
    /// Preserve v5 semantics wherever v6 can express them, even when
    /// the v6 spelling is unusual (for example, hex literals for color
    /// constants, or a helper that reproduces v5's `timeframe.period`).
    #[default]
    Exact,
    /// Prefer the conventional v6 spelling where the difference is cosmetic
    /// or where the v5 code almost certainly meant the v6 behavior
    /// (`x == na` becomes `na(x)`, recolored constants stay as written).
    Idiomatic,
}

/// Options for [`migrate`].
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationOptions {
    pub profile: Profile,
    /// Also apply optional modernizations that change no behavior
    /// (`array<float>` for `float[]`, `array.new<float>` for
    /// `array.new_float`).
    pub modernize: bool,
    /// Guard indices so that a negative index from `array.indexof` raises
    /// an error instead of reading from the end of the array.
    pub strict_indices: bool,
}

/// What happened to the script as a whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Status {
    /// The script was v5 and has been rewritten (possibly with findings
    /// that need review).
    Migrated,
    /// The script already declares version 6; nothing was changed.
    AlreadyV6,
    /// The script declares a version this tool does not migrate (v4 and
    /// earlier must first be brought to v5; a missing directive means v1).
    UnsupportedVersion,
    /// The source does not parse, so no rewrite was attempted; or its v6
    /// text would be more than the parser reads (its source size cap or
    /// token budget), so the rewrite was abandoned whole. Either way the
    /// script comes back unchanged.
    ParseError,
}

/// The kind of guarantee a rule gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RuleClass {
    /// A local rewrite of one expression or statement.
    Exact,
    /// Exact, but needs new temporaries or restructured statements.
    ExactWithHoisting,
    /// Compiles, but differs from the v5 code in some edge cases.
    Approximate,
    /// No v6 spelling reproduces the v5 behavior; review by hand.
    Diagnostic,
}

/// How urgently a finding needs attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Severity {
    /// A rewrite that was applied and needs no review.
    Info,
    /// Applied, but results may differ from v5 in edge cases, or a
    /// construct needs review.
    Warning,
    /// v6 cannot reproduce the v5 behavior; the output may compute
    /// different values here.
    Error,
}

/// One rule application or one site needing review, located in the
/// *original* source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    /// The rule identifier, for example `C1`, `D5` or `D-4`.
    pub rule: &'static str,
    pub class: RuleClass,
    pub severity: Severity,
    pub message: String,
    /// UTF-8 byte range in the original source.
    pub start: u32,
    pub end: u32,
    /// Whether the migrated output already contains the rewrite.
    pub applied: bool,
}

/// A single replacement of original-source bytes `[start, end)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextEdit {
    pub start: u32,
    pub end: u32,
    pub new_text: String,
}

/// The result of [`migrate`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Migration {
    pub status: Status,
    /// The version the source declared, if any.
    pub source_version: Option<u32>,
    /// The migrated source. Equal to the input unless `status` is
    /// [`Status::Migrated`].
    pub output: String,
    /// One edit turning the input into `output` (empty when unchanged).
    pub edits: Vec<TextEdit>,
    pub findings: Vec<Finding>,
    /// Why nothing was migrated when `status` is [`Status::ParseError`]: the
    /// parser's messages, or the one message saying the v6 text would be too
    /// large to parse.
    pub parse_errors: Vec<String>,
}

impl Migration {
    /// Whether any finding says v6 cannot reproduce the v5 behavior.
    pub fn needs_review(&self) -> bool {
        self.findings
            .iter()
            .any(|f| f.severity >= Severity::Warning)
    }
}

/// Rewrite a Pine v5 script as Pine v6.
pub fn migrate(source: &str, options: &MigrationOptions) -> Migration {
    rules::run(source, options, None)
}

/// Apply only the rewrites whose site overlaps `[start, end)` of the
/// original source, for editor quick fixes that fix one issue at a time.
/// The version directive is left alone until the script is fully migrated,
/// so it keeps declaring v5 in the meantime.
pub fn migrate_range(source: &str, options: &MigrationOptions, start: u32, end: u32) -> Migration {
    rules::run(source, options, Some((start, end)))
}
