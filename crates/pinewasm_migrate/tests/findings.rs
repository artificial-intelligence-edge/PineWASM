//! Sites the migration cannot convert exactly are reported, not silently
//! changed. Each test pins which finding a hand-written v5 input
//! produces, and whether the site was rewritten.

use pinewasm_migrate::{
    migrate, migrate_range, MigrationOptions, Profile, RuleClass, Severity, Status,
};

fn findings(input: &str) -> Vec<(&'static str, Severity, bool)> {
    let result = migrate(input, &MigrationOptions::default());
    assert_eq!(result.status, Status::Migrated, "{:?}", result.parse_errors);
    result
        .findings
        .iter()
        .map(|f| (f.rule, f.severity, f.applied))
        .collect()
}

fn has(input: &str, rule: &str, severity: Severity) {
    let found = findings(input);
    assert!(
        found.iter().any(|(r, s, _)| *r == rule && *s == severity),
        "expected {rule} at {severity:?}, got {found:?}"
    );
}

const H: &str = "//@version=5\nindicator(\"t\")\n";

#[test]
fn series_plot_offset_needs_review() {
    has(
        &format!("{H}o = bar_index % 3\nplot(close, offset = o)\n"),
        "D-2",
        Severity::Error,
    );
}

#[test]
fn constant_plot_offset_needs_nothing() {
    let found = findings(&format!("{H}plot(close, offset = 2)\n"));
    assert!(!found.iter().any(|(r, ..)| *r == "D-2"), "{found:?}");
}

#[test]
fn reassigned_length_that_cannot_be_folded_needs_review() {
    has(
        // A reassigned `var` is `simple`, so `n` is accepted, but its first-bar
        // value depends on an input and cannot be computed.
        &format!("{H}var n = input.int(3)\nn += 1\nplot(ta.sma(close, n))\n"),
        "D-3",
        Severity::Error,
    );
}

#[test]
fn input_derived_global_reassignment_does_not_need_length_review() {
    let found = findings(&format!(
        "{H}nInput = input.int(3)\nn = 1\nn := nInput\nplot(ta.ema(close, n))\n"
    ));
    assert!(!found.iter().any(|(rule, ..)| *rule == "D-3"), "{found:?}");
}

#[test]
fn nested_request_that_needs_dynamic_requests_is_reported() {
    let input = format!(
        "{H}f() => request.security(syminfo.tickerid, \"D\", close)\nv = request.security(syminfo.tickerid, \"W\", request.security(syminfo.tickerid, \"D\", close))\nif close > open\n    x = f()\nplot(v)\n"
    );
    has(&input, "D-4", Severity::Error);
}

#[test]
fn negative_trailing_points_need_review() {
    has(
        "//@version=5\nstrategy(\"t\")\nstrategy.exit(\"X\", \"L\", trail_points = -5, trail_offset = 2)\n",
        "D-7",
        Severity::Warning,
    );
}

#[test]
fn bars_back_on_another_timeframe_needs_review() {
    has(
        &format!("{H}plot(time(\"W\", 2))\n"),
        "D-8",
        Severity::Warning,
    );
}

#[test]
fn index_from_a_search_is_reported_and_guarded_on_request() {
    let input = format!("{H}a = array.from(1.0, 2.0)\nplot(array.get(a, array.indexof(a, 3.0)))\n");
    has(&input, "D-11", Severity::Warning);
    let strict = migrate(
        &input,
        &MigrationOptions {
            strict_indices: true,
            ..MigrationOptions::default()
        },
    );
    assert!(
        strict
            .output
            .contains("__v6_checkedIndex(array.indexof(a, 3.0))"),
        "{}",
        strict.output
    );
    assert!(strict.output.contains("runtime.error"), "{}", strict.output);
}

#[test]
fn negative_literal_index_is_reported_and_guarded_on_request() {
    let input = format!("{H}a = array.from(1.0, 2.0)\nplot(array.get(a, -1))\n");
    let default = migrate(&input, &MigrationOptions::default());
    assert!(default
        .findings
        .iter()
        .any(|f| f.rule == "D-11" && f.severity == Severity::Warning));
    assert!(
        default.output.contains("array.get(a, -1)"),
        "{}",
        default.output
    );

    let strict = migrate(
        &input,
        &MigrationOptions {
            strict_indices: true,
            ..MigrationOptions::default()
        },
    );
    assert!(
        strict
            .output
            .contains("array.get(a, __v6_checkedIndex(-1))"),
        "{}",
        strict.output
    );
    assert!(strict.output.contains("runtime.error"), "{}", strict.output);
}

#[test]
fn reordered_named_array_index_is_reported_and_guarded() {
    let input = format!("{H}a = array.from(10.0, 20.0)\nplot(array.get(index = -1, id = a))\n");
    let default = migrate(&input, &MigrationOptions::default());
    assert!(default
        .findings
        .iter()
        .any(|f| f.rule == "D-11" && f.severity == Severity::Warning));
    assert!(default.output.contains("array.get(index = -1, id = a)"));

    let strict = migrate(
        &input,
        &MigrationOptions {
            strict_indices: true,
            ..MigrationOptions::default()
        },
    );
    assert!(
        strict
            .output
            .contains("array.get(index = __v6_checkedIndex(-1), id = a)"),
        "{}",
        strict.output
    );
    assert!(strict.output.contains("runtime.error"));
}

#[test]
fn na_strategy_direction_needs_review() {
    has(
        "//@version=5\nstrategy(\"t\")\nd = close > open ? strategy.long : na\nstrategy.entry(\"L\", d)\n",
        "D-13",
        Severity::Error,
    );
}

#[test]
fn boolean_history_compared_with_false_needs_review() {
    has(
        &format!("{H}b = close > open\nx = b[1] == false\nplotshape(x)\n"),
        "D-1",
        Severity::Warning,
    );
}

#[test]
fn na_of_a_boolean_is_approximated() {
    let result = migrate(
        &format!("{H}b = close > open\nplotshape(na(b))\n"),
        &MigrationOptions::default(),
    );
    assert!(
        result.output.contains("plotshape(false)"),
        "{}",
        result.output
    );
    let f = result
        .findings
        .iter()
        .find(|f| f.rule == "D-1")
        .expect("D-1");
    assert_eq!(f.class, RuleClass::Approximate);
}

#[test]
fn transp_without_a_documented_default_color_needs_review() {
    let result = migrate(
        &format!("{H}plotarrow(close - open, transp = 50)\n"),
        &MigrationOptions::default(),
    );
    assert!(!result.output.contains("transp"), "{}", result.output);
    assert!(result
        .findings
        .iter()
        .any(|f| f.rule == "G4" && f.severity == Severity::Error));
}

#[test]
fn transp_on_known_conditional_colors_is_exact() {
    let result = migrate(
        &format!(
            "{H}c = close > open ? color.green : color.red\nplot(close, color = c, transp = 50)\n"
        ),
        &MigrationOptions::default(),
    );
    assert!(result
        .findings
        .iter()
        .any(|f| f.rule == "G4" && f.class == RuleClass::Exact && f.severity == Severity::Info));
    assert!(!result
        .findings
        .iter()
        .any(|f| f.rule == "G4" && f.severity == Severity::Warning));
}

#[test]
fn transp_on_an_unresolved_color_expression_still_needs_review() {
    has(
        &format!("{H}c = color.from_gradient(close, low, high, color.green, color.red)\nplot(close, color = c, transp = 50)\n"),
        "G4",
        Severity::Warning,
    );
}

#[test]
fn when_on_a_call_that_is_not_a_statement_needs_review() {
    let input = "//@version=5\nstrategy(\"t\")\nf() => strategy.entry(\"L\", strategy.long, when = close > open) \nx = f()\n";
    let result = migrate(input, &MigrationOptions::default());
    // The call is the body of a one-line function: it becomes a block.
    assert!(
        result.output.contains("    if close > open\n"),
        "{}",
        result.output
    );
}

#[test]
fn idiomatic_profile_prefers_conventional_spellings() {
    let options = MigrationOptions {
        profile: Profile::Idiomatic,
        ..MigrationOptions::default()
    };
    let result = migrate(
        &format!("{H}x = close == na\nplot(close, color = color.red)\nplot(6[1])\nlabel.new(bar_index, high, timeframe.period)\n"),
        &options,
    );
    assert!(result.output.contains("x = na(close)"), "{}", result.output);
    assert!(
        result.output.contains("color = color.red"),
        "{}",
        result.output
    );
    assert!(result.output.contains("plot(6)"), "{}", result.output);
    assert!(
        result.findings.iter().any(|f| f.rule == "D-5"),
        "{:?}",
        result.findings
    );
}

#[test]
fn modernize_rewrites_legacy_spellings() {
    let options = MigrationOptions {
        modernize: true,
        ..MigrationOptions::default()
    };
    let result = migrate(
        &format!("{H}float[] a = array.new_float(2)\nb = bool(na)\nplot(array.size(a))\n"),
        &options,
    );
    assert!(
        result
            .output
            .contains("array<float> a = array.new<float>(2)"),
        "{}",
        result.output
    );
}

#[test]
fn a_range_applies_only_the_rewrites_it_covers() {
    let input = format!("{H}x = 5 / 2\ny = color.red\nplot(x)\n");
    let start = input.find("5 / 2").unwrap() as u32;
    let result = migrate_range(&input, &MigrationOptions::default(), start, start + 5);
    assert!(result.output.contains("int(5 / 2)"), "{}", result.output);
    assert!(result.output.contains("color.red"), "{}", result.output);
    assert!(
        result.output.starts_with("//@version=5"),
        "{}",
        result.output
    );
}

#[test]
fn extreme_ranges_leave_unselected_rewrites_and_diagnostics_unchanged() {
    let options = MigrationOptions::default();
    for (input, rule) in [
        (format!("{H}x = 5 / 2\nplot(x)\n"), "D5"),
        (
            format!("{H}o = bar_index % 3\nplot(close, offset = o)\n"),
            "D-2",
        ),
    ] {
        assert!(migrate(&input, &options)
            .findings
            .iter()
            .any(|finding| finding.rule == rule));
        for (start, end) in [
            (u32::MAX, u32::MAX),
            (u32::MAX, 0),
            (u32::MAX - 1, u32::MAX),
            (u32::MAX - 1, 0),
        ] {
            let result = migrate_range(&input, &options, start, end);
            assert_eq!(result.status, Status::Migrated);
            assert_eq!(result.output, input);
            assert!(result.edits.is_empty());
            assert!(result.findings.is_empty());
            assert!(result.parse_errors.is_empty());
        }
    }
}

#[test]
fn maximum_range_end_preserves_selected_rewrites_and_diagnostics() {
    let input = format!("{H}x = 5 / 2\no = bar_index % 3\nplot(x, offset = o)\n");
    let options = MigrationOptions::default();
    let bounded = migrate_range(&input, &options, 0, input.len() as u32);
    let unbounded = migrate_range(&input, &options, 0, u32::MAX);
    assert_eq!(unbounded.output, bounded.output);
    assert_eq!(unbounded.edits, bounded.edits);
    assert_eq!(unbounded.findings, bounded.findings);
    assert!(unbounded.output.contains("int(5 / 2)"));
    assert!(unbounded
        .findings
        .iter()
        .any(|finding| finding.rule == "D-2"));
}

#[test]
fn empty_range_selects_the_expression_at_its_cursor() {
    let input = format!("{H}x = 5 / 2\nplot(x)\n");
    let start = input.find("5 / 2").unwrap() as u32;
    for cursor in [start, start + 2] {
        let result = migrate_range(&input, &MigrationOptions::default(), cursor, cursor);
        assert!(result.output.contains("int(5 / 2)"));
    }
    let end = start + 5;
    let result = migrate_range(&input, &MigrationOptions::default(), end, end);
    assert_eq!(result.output, input);
    assert!(result.edits.is_empty());
    assert!(result.findings.is_empty());
}

#[test]
fn statuses_for_inputs_that_are_not_migrated() {
    let options = MigrationOptions::default();
    assert_eq!(
        migrate("//@version=6\nindicator(\"t\")\nplot(close)\n", &options).status,
        Status::AlreadyV6
    );
    assert_eq!(
        migrate("//@version=4\nstudy(\"t\")\nplot(close)\n", &options).status,
        Status::UnsupportedVersion
    );
    assert_eq!(
        migrate("indicator(\"t\")\nplot(close)\n", &options).status,
        Status::UnsupportedVersion
    );
    let broken = migrate("//@version=5\nindicator(\"t\"\n", &options);
    assert_eq!(broken.status, Status::ParseError);
    assert!(!broken.parse_errors.is_empty());
}

#[test]
fn a_type_nested_past_the_parser_cap_is_a_parse_error() {
    // The migrator parses before it does anything else, so a type the parser
    // refuses is reported like any other source that does not parse, one
    // level past the cap or far past it.
    let options = MigrationOptions::default();
    let field = |depth: usize| {
        format!(
            "{H}type Deep\n    {}int{} held\n    int count\nplot(close)\n",
            "array<".repeat(depth),
            ">".repeat(depth)
        )
    };
    for depth in [201, 60_000] {
        let source = field(depth);
        let refused = migrate(&source, &options);
        assert_eq!(refused.status, Status::ParseError, "{depth} levels");
        assert_eq!(refused.source_version, Some(5));
        assert_eq!(refused.parse_errors.len(), 1, "{:?}", refused.parse_errors);
        assert!(
            refused.parse_errors[0].ends_with("type is nested too deeply"),
            "{}",
            refused.parse_errors[0]
        );
        assert_eq!(refused.output, source);
        assert!(refused.edits.is_empty());
    }

    // The deepest type the parser admits migrates, and the result compiles.
    let migrated = migrate(&field(200), &options);
    assert_eq!(
        migrated.status,
        Status::Migrated,
        "{:?}",
        migrated.parse_errors
    );
    let compiled = pinewasm_compiler::compile(
        &migrated.output,
        pinewasm_compiler::CompileOptions::default(),
    );
    assert!(compiled.success, "{:?}", compiled.diagnostics);
}

#[test]
fn a_source_with_more_mistakes_than_are_reported_is_a_short_parse_error() {
    // The parser reports its first 1,000 errors and then says it left the
    // rest out, so the migrator's list is no longer than that either. The
    // last line is about the list, not about a line of the script.
    let mut broken = String::from(H);
    let first_broken_line = H.matches('\n').count() + 1;
    broken.push_str(&"x 1\n".repeat(20_000));
    let refused = migrate(&broken, &MigrationOptions::default());
    assert_eq!(refused.status, Status::ParseError);
    assert_eq!(refused.output, broken);
    assert_eq!(refused.parse_errors.len(), 1001);
    for (n, error) in refused.parse_errors[..1000].iter().enumerate() {
        assert_eq!(
            *error,
            format!(
                "line {}: expected end of line, found number",
                first_broken_line + n
            )
        );
    }
    assert_eq!(
        refused.parse_errors[1000],
        "too many errors: only the first 1000 are reported"
    );
}

#[test]
fn a_source_the_parser_does_not_read_is_a_parse_error() {
    let options = MigrationOptions::default();
    // One byte over the source size cap.
    let mut oversized = String::from(H);
    oversized.push_str(&" ".repeat(pinewasm_parse::MAX_SOURCE_LEN + 1 - H.len()));
    let refused = migrate(&oversized, &options);
    assert_eq!(refused.status, Status::ParseError);
    assert_eq!(
        refused.parse_errors,
        [format!(
            "source is {} bytes; the maximum supported size is {}",
            pinewasm_parse::MAX_SOURCE_LEN + 1,
            pinewasm_parse::MAX_SOURCE_LEN
        )]
    );
    assert_eq!(refused.output, oversized);

    // Inside the size cap, past the token budget.
    let pair = "a\n\ta\n";
    let mut dense = String::from(H);
    dense.push_str(&pair.repeat((pinewasm_parse::MAX_SOURCE_LEN - H.len()) / pair.len()));
    assert!(dense.len() <= pinewasm_parse::MAX_SOURCE_LEN);
    let refused = migrate(&dense, &options);
    assert_eq!(refused.status, Status::ParseError);
    assert_eq!(refused.parse_errors.len(), 1, "{:?}", refused.parse_errors);
    assert!(
        refused.parse_errors[0].contains("tokens; the maximum supported count"),
        "{}",
        refused.parse_errors[0]
    );
    assert_eq!(refused.output, dense);
}

/// A v5 script that is `room` bytes under the parser's source size cap (to
/// within one padding line): `body` followed by comment lines.
fn script_under_the_cap(body: &str, room: usize) -> String {
    let comment = "// padding\n";
    let mut source = format!("{H}{body}");
    let target = pinewasm_parse::MAX_SOURCE_LEN - room;
    source.push_str(&comment.repeat((target - source.len()) / comment.len()));
    assert!(source.len() <= target && target - source.len() < comment.len());
    source
}

#[test]
fn a_migration_that_outgrows_the_parser_is_refused_whole() {
    // The v6 text of a script is a little longer than its v5 text. For a
    // script just under the source size cap that is more than the parser
    // reads, and the migrator returns a whole migration or none: it leaves
    // the script alone and says why.
    let cap = pinewasm_parse::MAX_SOURCE_LEN;
    let one_site = "label.new(bar_index, high, timeframe.period)\n";
    let many_sites = one_site.repeat(200);
    for (what, body, room) in [
        // One rewrite fits in the room left; the helper definition it needs,
        // inserted after the last phase, does not.
        ("helper definition", one_site, 100),
        // Two hundred rewrites of a few bytes each do not fit by themselves.
        ("rewrites", many_sites.as_str(), 1_000),
    ] {
        let source = script_under_the_cap(body, room);
        let refused = migrate(&source, &MigrationOptions::default());
        assert_eq!(refused.status, Status::ParseError, "{what}");
        assert_eq!(refused.source_version, Some(5), "{what}");
        assert_eq!(refused.output, source, "{what}");
        assert!(refused.edits.is_empty(), "{what}");
        assert!(refused.findings.is_empty(), "{what}");
        assert_eq!(
            refused.parse_errors.len(),
            1,
            "{what}: {:?}",
            refused.parse_errors
        );
        let reason = &refused.parse_errors[0];
        assert!(
            reason.starts_with("the migrated script would be too large to parse (source is ")
                && reason.ends_with(&format!(" bytes; the maximum supported size is {cap})")),
            "{what}: {reason}"
        );

        // With room for its v6 text the same script migrates completely, and
        // the result is one the parser reads.
        let source = script_under_the_cap(body, 64 * 1024);
        let migrated = migrate(&source, &MigrationOptions::default());
        assert_eq!(migrated.status, Status::Migrated, "{what}");
        assert!(
            !migrated.findings.iter().any(|f| f.rule == "internal"),
            "{what}"
        );
        assert!(migrated.output.starts_with("//@version=6\n"), "{what}");
        assert!(
            migrated.output.contains("\n__v6_timeframePeriodV5() =>\n"),
            "{what}"
        );
        assert!(!migrated.output.contains("timeframe.period)"), "{what}");
        assert!(migrated.output.len() > source.len() && migrated.output.len() <= cap);
        assert!(
            pinewasm_parse::parse(&migrated.output)
                .diagnostics
                .is_empty(),
            "{what}"
        );
    }
}

#[test]
fn findings_point_into_the_original_source() {
    let input = format!("{H}// padding\nx = 5 / 2\n");
    let result = migrate(&input, &MigrationOptions::default());
    let d5 = result.findings.iter().find(|f| f.rule == "D5").unwrap();
    assert_eq!(&input[d5.start as usize..d5.end as usize], "5 / 2");
}

#[test]
fn boolean_tuple_na_requires_manual_conversion() {
    for conditional in [
        "if close > open\n    [na, 3]\nelse\n    [true, 5]",
        "if close > open\n    [true, 5]\nelse\n    [na, 3]",
        "switch\n    close > open => [na, 3]\n    => [true, 5]",
    ] {
        let source = format!("{H}[a, b] = {conditional}\nplot(a ? 1 : 0)\nplot(b)\n");
        let result = migrate(&source, &MigrationOptions::default());
        assert_eq!(
            result.output,
            source.replacen("//@version=5", "//@version=6", 1)
        );
        let finding = result.findings.iter().find(|f| f.rule == "D-1").unwrap();
        assert_eq!(finding.severity, Severity::Error);
        assert_eq!(finding.class, RuleClass::Diagnostic);
        assert!(!finding.applied);
        let compiled = pinewasm_compiler::compile(
            &result.output,
            pinewasm_compiler::CompileOptions::default(),
        );
        assert!(compiled
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0207")));
    }
}

#[test]
fn numeric_tuple_na_and_nonmissing_boolean_tuples_need_no_manual_conversion() {
    for branches in [
        "[na, 3]\nelse\n    [close, 5]",
        "[true, 3]\nelse\n    [false, 5]",
        "[na, true]\nelse\n    [close, false]",
    ] {
        let source = format!("{H}[a, b] = if close > open\n    {branches}\n");
        assert!(!findings(&source).iter().any(|(r, ..)| *r == "D-1"));
    }
}
