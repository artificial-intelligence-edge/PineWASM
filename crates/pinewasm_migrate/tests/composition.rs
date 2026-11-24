//! Cross-rule contracts: options, untouched text, temporary hygiene and
//! multi-feature scripts. These sources are hand-written for PineWASM.
use pinewasm_migrate::{migrate, MigrationOptions, Profile, Status};

fn checked(source: &str, options: &MigrationOptions) -> String {
    let r = migrate(source, options);
    assert_eq!(r.status, Status::Migrated, "{:?}", r.parse_errors);
    assert!(
        !r.findings.iter().any(|f| f.rule == "internal"),
        "{:?}",
        r.findings
    );
    assert!(
        !pinewasm_parse::parse(&r.output)
            .diagnostics
            .iter()
            .any(|d| d.severity == pinewasm_span::Severity::Error),
        "{}",
        r.output
    );
    let mut replay = source.to_owned();
    for e in r.edits.iter().rev() {
        replay.replace_range(e.start as usize..e.end as usize, &e.new_text);
    }
    assert_eq!(replay, r.output);
    let again = migrate(&r.output, options);
    assert_eq!(again.status, Status::AlreadyV6);
    assert_eq!(again.output, r.output);
    assert!(again.edits.is_empty());
    assert_eq!(
        serde_json::to_value(&r).unwrap(),
        serde_json::to_value(migrate(source, options)).unwrap()
    );
    r.output
}

#[test]
fn option_matrix_composes_without_changing_division_policy() {
    let source = "//@version=5\nindicator(\"options\")\nfloat[] xs = array.new_float(2, close)\ni = array.indexof(xs, open)\nx = array.get(xs, i)\nplot(x + 7 / 3, color = color.red)\n";
    for profile in [Profile::Exact, Profile::Idiomatic] {
        for modernize in [false, true] {
            for strict_indices in [false, true] {
                let output = checked(
                    source,
                    &MigrationOptions {
                        profile,
                        modernize,
                        strict_indices,
                    },
                );
                assert!(output.contains("int(7 / 3)"), "{output}");
                assert_eq!(output.contains("#FF5252"), profile == Profile::Exact);
                assert_eq!(output.contains("array<float>"), modernize);
                assert_eq!(output.contains("runtime.error"), strict_indices);
            }
        }
    }
}

#[test]
fn comments_strings_unicode_and_crlf_are_not_rewritten() {
    let source = "//@version=5\r\nindicator(\"é ∑ color.red 5 / 2\")\r\n// untouched: color.red, transp=50, when=true, 5 / 2 — 雪\r\nx = 5 / 2\r\nplot(x)\r\n";
    let expected = source
        .replacen("version=5", "version=6", 1)
        .replace("x = 5 / 2", "x = int(5 / 2)");
    assert_eq!(checked(source, &MigrationOptions::default()), expected);
}

#[test]
fn hoisted_temporaries_do_not_capture_user_names() {
    let source = "//@version=5\nindicator(\"names\")\n__v6_1 = 100\n__v6_2 = 200\nb = close > 0 and ta.sma(close, 3) > 0\nplot(__v6_1 + __v6_2 + (b ? 1 : 0))\n";
    let output = checked(source, &MigrationOptions::default());
    assert!(output.contains("__v6_1 = 100\n__v6_2 = 200\n"));
    assert!(output.contains("__v6_3 = ta.sma(close, 3) > 0"), "{output}");
    assert!(output.contains("b = close > 0 and __v6_3"));
}

#[test]
fn complete_strategy_keeps_explicit_settings_and_order_argument_evaluation() {
    let source = "//@version=5\nstrategy(\"Crossover risk\", overlay = true, margin_long = 25)\nfastLength = input.int(3)\nslowLength = input.int(8)\nfast = ta.sma(close, fastLength)\nslow = ta.sma(close, slowLength)\nenter = ta.crossover(fast, slow)\nleave = ta.crossunder(fast, slow)\nstrategy.entry(\"L\", strategy.long, qty = ta.sma(volume, 3), when = enter)\nstrategy.close(\"L\", when = leave)\nstrategy.exit(\"risk\", \"L\", profit = 12, limit = high)\nplot(fast, color = color.red, transp = 20)\nplot(slow, color = color.teal)\n";
    let expected = "//@version=6\nstrategy(\"Crossover risk\", overlay = true, margin_long = 25, margin_short = 0)\nfastLength = input.int(3)\nslowLength = input.int(8)\nfast = ta.sma(close, fastLength)\nslow = ta.sma(close, slowLength)\nenter = ta.crossover(fast, slow)\nleave = ta.crossunder(fast, slow)\n__v6_1 = ta.sma(volume, 3)\nif enter\n    strategy.entry(\"L\", strategy.long, qty = __v6_1)\nif leave\n    strategy.close(\"L\")\nstrategy.exit(\"risk\", \"L\", profit = na(high) ? 12 : na, limit = high)\nplot(fast, color = color.new(#FF5252, 20))\nplot(slow, color = #00897B)\n";
    assert_eq!(checked(source, &MigrationOptions::default()), expected);
    let result = pinewasm_compiler::compile(expected, pinewasm_compiler::CompileOptions::default());
    assert!(
        !result
            .diagnostics
            .iter()
            .any(|d| d.severity == pinewasm_span::Severity::Error),
        "{:?}",
        result.diagnostics
    );
}

#[test]
fn search_aliases_are_reported_and_guarded_without_following_shadowed_names() {
    let source = "//@version=5\nindicator(\"aliases\")\na = array.from(1.0, 2.0)\ni = array.indexof(a, 8.0)\nj = (i)\nf() =>\n    i = 0\n    array.get(a, i)\nplot(array.get(a, j) + f())\n";
    let report = migrate(source, &MigrationOptions::default());
    assert_eq!(
        report.findings.iter().filter(|f| f.rule == "D-11").count(),
        1
    );
    let output = checked(
        source,
        &MigrationOptions {
            strict_indices: true,
            ..Default::default()
        },
    );
    assert!(
        output.contains("array.get(a, __v6_checkedIndex(j))"),
        "{output}"
    );
    assert!(output.contains("    array.get(a, i)\n"), "{output}");
}
