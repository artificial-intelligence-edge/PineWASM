use crate::{lex, lex_with_source_id, parse, parse_with_source_id, MAX_SOURCE_LEN, MAX_TOKENS};
use pinewasm_span::{has_errors, SourceId, Span};
use pinewasm_syntax::{dump_program, TokenKind};
use pretty_assertions::assert_eq;

const CORE_EXAMPLE: &str = "//@version=6\nindicator(\"Example\")\nx = close - close[1]\nplot(x)\n";

fn token_kinds(source: &str) -> Vec<TokenKind> {
    lex(source).tokens.into_iter().map(|t| t.kind).collect()
}

// -- lexer -------------------------------------------------------------------

#[test]
fn lex_operators_and_literals() {
    use TokenKind::*;
    assert_eq!(
        token_kinds("x = 1 + 2.5 * (a - b) / c"),
        vec![
            Ident, Eq, Number, Plus, Number, Star, LParen, Ident, Minus, Ident, RParen, Slash,
            Ident, Newline, Eof
        ]
    );
    assert_eq!(
        token_kinds("a >= b == c != d <= e < f > g"),
        vec![
            Ident, Ge, Ident, EqEq, Ident, BangEq, Ident, Le, Ident, Lt, Ident, Gt, Ident, Newline,
            Eof
        ]
    );
}

#[test]
fn lex_spans_are_exact() {
    let result = lex("x = close");
    let spans: Vec<(u32, u32)> = result
        .tokens
        .iter()
        .map(|t| (t.span.start, t.span.end))
        .collect();
    // x, =, close, newline(synthetic), eof
    assert_eq!(spans, vec![(0, 1), (2, 3), (4, 9), (9, 9), (9, 9)]);
    assert!(result.diagnostics.is_empty());
}

#[test]
fn lex_percent_operator() {
    use TokenKind::*;
    assert_eq!(
        token_kinds("x = a % 2 % b"),
        vec![Ident, Eq, Ident, Percent, Number, Percent, Ident, Newline, Eof]
    );
}

#[test]
fn lex_keywords_versus_identifiers() {
    use TokenKind::*;
    assert_eq!(
        token_kinds("if else true false and or not iffy"),
        vec![KwIf, KwElse, KwTrue, KwFalse, KwAnd, KwOr, KwNot, Ident, Newline, Eof]
    );
}

#[test]
fn lex_indentation_blocks() {
    use TokenKind::*;
    let source = "if a\n    b = 1\nelse\n    c = 2\n";
    assert_eq!(
        token_kinds(source),
        vec![
            KwIf, Ident, Newline, Indent, Ident, Eq, Number, Newline, Dedent, KwElse, Newline,
            Indent, Ident, Eq, Number, Newline, Dedent, Eof
        ]
    );
}

#[test]
fn lex_comments_and_blank_lines_are_skipped() {
    use TokenKind::*;
    let source = "// a comment\n\nx = 1 // trailing\n// another\n";
    assert_eq!(token_kinds(source), vec![Ident, Eq, Number, Newline, Eof]);
}

#[test]
fn lex_version_directive_token() {
    use TokenKind::*;
    assert_eq!(
        token_kinds("//@version=6\nx = 1\n"),
        vec![VersionDirective, Newline, Ident, Eq, Number, Newline, Eof]
    );
}

#[test]
fn lex_unterminated_string_diagnostic() {
    let result = lex("s = \"oops\nx = 1\n");
    assert!(has_errors(&result.diagnostics));
    assert_eq!(result.diagnostics[0].code.as_deref(), Some("E0102"));
}

#[test]
fn lex_string_does_not_continue_onto_a_statement_line() {
    // Outside parentheses a block-width indent (or none) starts a statement,
    // so the string still ends at the line break.
    for source in ["s = \"oops\n    x = 1\n", "s = \"oops\r\nx = 1\n"] {
        let result = lex(source);
        assert_eq!(
            result.diagnostics[0].code.as_deref(),
            Some("E0102"),
            "{source:?}"
        );
    }
    // A blank line ends a wrapped string even inside parentheses.
    let result = lex("f(\"oops\n\n  more\")\n");
    assert_eq!(result.diagnostics[0].code.as_deref(), Some("E0102"));
}

#[test]
fn lex_wrapped_string_is_one_token() {
    use TokenKind::*;
    let source = "s = str.format(\"a: {0}\n         \\nb: {1}\", x, y)\n";
    let result = lex(source);
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    assert_eq!(
        token_kinds(source),
        vec![
            Ident, Eq, Ident, Dot, Ident, LParen, Str, Comma, Ident, Comma, Ident, RParen, Newline,
            Eof
        ]
    );
}

#[test]
fn lex_unexpected_character_diagnostic() {
    let result = lex("x = 1 § 2");
    assert!(has_errors(&result.diagnostics));
    // The lexer keeps going after the bad character.
    assert!(result.tokens.iter().any(|t| t.kind == TokenKind::Error));
}

#[test]
fn lex_color_literal_six_and_eight_digits() {
    use TokenKind::*;
    assert_eq!(
        token_kinds("c = #FF00AA"),
        vec![Ident, Eq, Color, Newline, Eof]
    );
    assert_eq!(
        token_kinds("c = #12ab34CD"),
        vec![Ident, Eq, Color, Newline, Eof]
    );
}

#[test]
fn lex_color_literal_wrong_digit_count_is_e0229() {
    // 3, 5, and 7 digits are all invalid color literals.
    for src in ["#FFF", "#FF00A", "#FF00AABB0"] {
        let result = lex(&format!("c = {src}"));
        assert!(has_errors(&result.diagnostics), "expected error for {src}");
        assert_eq!(
            result.diagnostics[0].code.as_deref(),
            Some("E0229"),
            "for {src}"
        );
    }
}

#[test]
fn parse_color_literal_decodes_rgb_and_transparency() {
    // #RRGGBB is opaque (t = 0).
    let dump = dump_program(&parse("c = #FF0000\n").program);
    assert!(dump.contains("Color #FF0000 @"), "{dump}");
    // #RRGGBBAA: alpha FF ⇒ opaque; alpha 00 ⇒ t = 100.
    let opaque = dump_program(&parse("c = #00FF00FF\n").program);
    assert!(opaque.contains("Color #00FF00 @"), "{opaque}");
    let transparent = dump_program(&parse("c = #0000FF00\n").program);
    assert!(
        transparent.contains("Color #0000FF t=100 @"),
        "{transparent}"
    );
}

// -- parser ------------------------------------------------------------------

#[test]
fn parse_core_example_snapshot() {
    let result = parse(CORE_EXAMPLE);
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    insta::assert_snapshot!(dump_program(&result.program));
}

#[test]
fn parse_if_else_snapshot() {
    let source = "//@version=6\nindicator(\"Branch\")\nif close > open\n    plot(close)\nelse\n    plot(open)\n";
    let result = parse(source);
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    insta::assert_snapshot!(dump_program(&result.program));
}

#[test]
fn parse_precedence_snapshot() {
    let source = "y = 1 + 2 * 3 - -4 / (5 - 6)\nb = not close > open and true or false\n";
    let result = parse(source);
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    insta::assert_snapshot!(dump_program(&result.program));
}

#[test]
fn parse_modulo_precedence_snapshot() {
    // `%` binds like `*` and `/`: a + b % c is a + (b % c), and the three
    // multiplicative operators associate left to right.
    let source = "y = a + b % c\nz = a % b * c / d\n";
    let result = parse(source);
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    insta::assert_snapshot!(dump_program(&result.program));
}

#[test]
fn int_literal_out_of_range_is_a_parse_error() {
    // One past i64::MAX.
    let result = parse("x = 9223372036854775808\n");
    assert!(has_errors(&result.diagnostics));
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0110")
                && d.message.contains("integer literal out of range")),
        "diagnostics: {:?}",
        result.diagnostics
    );

    // i64::MAX itself is fine, and float-form literals of any magnitude are
    // not subject to the int range check.
    let result = parse("x = 9223372036854775807\ny = 92233720368547758080.5\n");
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
}

#[test]
fn parse_history_of_history() {
    let result = parse("x = close[1][2]\n");
    assert!(result.diagnostics.is_empty());
    insta::assert_snapshot!(dump_program(&result.program));
}

#[test]
fn parse_invalid_syntax_recovers_with_diagnostics() {
    // Broken first line; second line must still parse.
    let result = parse("x = close -\nplot(close)\n");
    assert!(has_errors(&result.diagnostics));
    assert_eq!(
        result.program.items.len(),
        1,
        "second statement should survive recovery"
    );
}

#[test]
fn parse_missing_paren_diagnostic() {
    let result = parse("plot(close\n");
    assert!(has_errors(&result.diagnostics));
}

#[test]
fn parse_trailing_garbage_diagnostic() {
    let result = parse("x = 1 2\n");
    assert!(has_errors(&result.diagnostics));
}

#[test]
fn parse_version_directive_value() {
    let result = parse("//@version=6\nx = 1\n");
    assert_eq!(result.program.version.map(|v| v.version), Some(6));
    let result = parse("x = 1\n");
    assert_eq!(result.program.version, None);
}

#[test]
fn parse_deep_nesting_is_rejected_not_crashed() {
    let source = format!("x = {}1{}\n", "(".repeat(5000), ")".repeat(5000));
    let result = parse(&source);
    assert!(has_errors(&result.diagnostics));
}

#[test]
fn parse_deep_block_nesting_is_rejected_not_crashed() {
    // Deeply nested `if` BLOCKS (each indented one level deeper) recurse
    // through `parse_block`. Without `MAX_BLOCK_DEPTH` this overflows the
    // native stack; it must be rejected with a diagnostic instead.
    let mut source = String::from("x = 0\n");
    for i in 0..400 {
        source.push_str(&"    ".repeat(i));
        source.push_str("if x == 0\n");
    }
    source.push_str(&"    ".repeat(400));
    source.push_str("x := 1\n");
    let result = parse(&source);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.message.contains("nested too deeply")),
        "expected a depth-limit diagnostic, got: {:?}",
        result.diagnostics
    );
}

#[test]
fn parse_deep_else_if_chain_is_rejected_not_crashed() {
    // A long `if / else if / else if / ...` CHAIN is flat (all at column 0),
    // so it recurses through `parse_if_expr`'s else-branch, NOT `parse_block`.
    // It must still be bounded or it overflows the stack here and in later
    // tree-walking passes such as semantic analysis.
    let mut source = String::from("x = 0\nif x == 0\n    y = 1\n");
    for i in 1..3000 {
        source.push_str(&format!("else if x == {i}\n    y = 1\n"));
    }
    let result = parse(&source);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.message.contains("nested too deeply")),
        "expected a depth-limit diagnostic, got {} diagnostics",
        result.diagnostics.len()
    );
}

#[test]
fn parse_legitimate_shallow_nesting_is_accepted() {
    // The block-depth cap must not fire on ordinary code: ~50 levels of
    // nesting (far below the cap, far above any real script) parses clean.
    let mut source = String::from("x = 0\n");
    for i in 0..50 {
        source.push_str(&"    ".repeat(i));
        source.push_str("if x == 0\n");
    }
    source.push_str(&"    ".repeat(50));
    source.push_str("x := 1\n");
    let result = parse(&source);
    assert!(
        !result
            .diagnostics
            .iter()
            .any(|d| d.message.contains("nested too deeply")),
        "shallow nesting wrongly rejected: {:?}",
        result.diagnostics
    );
}

#[test]
fn parse_many_unclosed_parens_completes_quickly() {
    // Each `f(` opens a call whose parameter-list lookahead must bail at the
    // newline; before the fix it rescanned the rest of the file on every
    // statement (O(n^2)). This completes near-instantly when linear.
    let source = "f(\n".repeat(30_000);
    let result = parse(&source);
    assert!(has_errors(&result.diagnostics));
}

#[test]
fn parse_string_escapes() {
    let result = parse("indicator(\"a\\\"b\\nc\")\n");
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    let pinewasm_syntax::Item::Indicator(decl) = &result.program.items[0] else {
        panic!("expected indicator decl");
    };
    let pinewasm_syntax::Expr::Str { value, .. } = &decl.args[0].value else {
        panic!("expected string arg");
    };
    assert_eq!(value, "a\"b\nc");
}

#[test]
fn parse_unsupported_string_escapes_as_literal_characters() {
    let result = parse("message = \"session\\sHigh and \\SLOW\"\n");
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    let pinewasm_syntax::Item::Stmt(pinewasm_syntax::Stmt::Assign { value, .. }) =
        &result.program.items[0]
    else {
        panic!("expected string assignment");
    };
    let pinewasm_syntax::Expr::Str { value, .. } = value else {
        panic!("expected string expression");
    };
    assert_eq!(value, "session\\sHigh and \\SLOW");
}

#[test]
fn parse_line_wrapped_string_literals() {
    let result = parse(concat!(
        "indicator(\"first line\n",
        "    continued inside parentheses\")\n",
        "message = 'first line\r\n",
        "  continued outside parentheses'\n",
    ));
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    let pinewasm_syntax::Item::Indicator(decl) = &result.program.items[0] else {
        panic!("expected indicator decl");
    };
    let pinewasm_syntax::Expr::Str { value, .. } = &decl.args[0].value else {
        panic!("expected indicator title string");
    };
    assert_eq!(value, "first line continued inside parentheses");
    let pinewasm_syntax::Item::Stmt(pinewasm_syntax::Stmt::Assign {
        value: pinewasm_syntax::Expr::Str { value, .. },
        ..
    }) = &result.program.items[1]
    else {
        panic!("expected wrapped string assignment");
    };
    assert_eq!(value, "first line continued outside parentheses");
}

#[test]
fn parse_strategy_declaration() {
    let result = parse("strategy(\"S\", initial_capital=5000)\nplot(close)\n");
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    let pinewasm_syntax::Item::Strategy(decl) = &result.program.items[0] else {
        panic!("expected strategy decl");
    };
    assert_eq!(decl.args.len(), 2);
    let pinewasm_syntax::Expr::Str { value, .. } = &decl.args[0].value else {
        panic!("expected string title arg");
    };
    assert_eq!(value, "S");
    assert_eq!(
        decl.args[1].name.as_ref().map(|i| i.name.as_str()),
        Some("initial_capital")
    );
}

#[test]
fn parse_else_if_chain() {
    let source = "if a > 1\n    plot(a)\nelse if a > 0\n    plot(b)\nelse\n    plot(c)\n";
    let result = parse(source);
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    insta::assert_snapshot!(dump_program(&result.program));
}

// -- var declarations and := reassignment --------------------------------------

#[test]
fn lex_colon_eq_and_var_keyword() {
    use TokenKind::*;
    assert_eq!(
        token_kinds("var n = 0\nn := n + 1\n"),
        vec![KwVar, Ident, Eq, Number, Newline, Ident, ColonEq, Ident, Plus, Number, Newline, Eof]
    );
    // `var` is a keyword; identifiers merely starting with it are not.
    assert_eq!(
        token_kinds("variable = 1"),
        vec![Ident, Eq, Number, Newline, Eof]
    );
}

#[test]
fn lex_lone_colon_is_a_colon_token() {
    // `:` is a real token now (the ternary branch separator); the lexer
    // emits it without a diagnostic and the *parser* rejects strays.
    use TokenKind::*;
    let result = lex("x : 1\n");
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    assert_eq!(
        result.tokens.iter().map(|t| t.kind).collect::<Vec<_>>(),
        vec![Ident, Colon, Number, Newline, Eof]
    );
}

#[test]
fn stray_colon_is_a_parse_error() {
    // Statement position: `x : 1` parses `x`, then trips on the `:`.
    let result = parse("x : 1\nplot(close)\n");
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0110")
                && d.message.contains("expected end of line, found `:`")),
        "diagnostics: {:?}",
        result.diagnostics
    );
    // The broken line still yields a best-effort `x` statement, and the
    // following plot survives recovery.
    assert_eq!(result.program.items.len(), 2);

    // Expression position: a lone `:` gets the targeted ternary hint.
    let result = parse("x = : 1\n");
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0110")
                && d.message.contains("expected an expression, found `:`")),
        "diagnostics: {:?}",
        result.diagnostics
    );
}

#[test]
fn parse_var_and_reassign_snapshot() {
    let source = "//@version=6\nindicator(\"Var\")\nvar n = 0\nn := n + 1\nplot(n)\n";
    let result = parse(source);
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    insta::assert_snapshot!(dump_program(&result.program));
}

#[test]
fn parse_var_with_colon_eq_is_a_parse_error() {
    // `var` declares with `=`; `var x := 1` is rejected at parse time, and
    // the following statement still parses.
    let result = parse("var x := 1\nplot(close)\n");
    assert!(has_errors(&result.diagnostics));
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0110")
                && d.message.contains("`var` declares with `=`")),
        "diagnostics: {:?}",
        result.diagnostics
    );
    assert_eq!(
        result.program.items.len(),
        1,
        "plot should survive recovery"
    );
}

#[test]
fn parse_reassign_of_unknown_name_is_not_a_parse_error() {
    // `x := 1` with no prior `x` parses fine; whether the name exists is a
    // semantic question (E0217).
    let result = parse("x := 1\n");
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    assert!(matches!(
        &result.program.items[0],
        pinewasm_syntax::Item::Stmt(pinewasm_syntax::Stmt::Reassign { .. })
    ));
}

#[test]
fn parse_var_without_name_or_initializer_is_an_error() {
    assert!(has_errors(&parse("var = 1\n").diagnostics));
    assert!(has_errors(&parse("var x\n").diagnostics));
    assert!(has_errors(&parse("var\n").diagnostics));
}

// -- the ternary conditional operator --------------------------------------------

#[test]
fn lex_question_and_colon_tokens() {
    use TokenKind::*;
    // Whitespace around `?`/`:` is free-form: spaced and unspaced ternaries
    // lex identically, and `:=` still lexes as one token.
    assert_eq!(
        token_kinds("x = a ? b : c"),
        vec![Ident, Eq, Ident, Question, Ident, Colon, Ident, Newline, Eof]
    );
    assert_eq!(
        token_kinds("x = a?b:c"),
        vec![Ident, Eq, Ident, Question, Ident, Colon, Ident, Newline, Eof]
    );
    assert_eq!(
        token_kinds("x := a ? b : c"),
        vec![Ident, ColonEq, Ident, Question, Ident, Colon, Ident, Newline, Eof]
    );
}

#[test]
fn parse_ternary_snapshot() {
    let result = parse("x = close > open ? close : open\nplot(x)\n");
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    insta::assert_snapshot!(dump_program(&result.program));
}

#[test]
fn parse_ternary_is_right_associative() {
    // `a ? b : c ? d : e` is `a ? b : (c ? d : e)`.
    let result = parse("x = a ? b : c ? d : e\n");
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    insta::assert_snapshot!(dump_program(&result.program));
}

#[test]
fn parse_ternary_binds_looser_than_or() {
    // `a or b ? c : d and e` is `(a or b) ? c : (d and e)`.
    let result = parse("x = a or b ? c : d and e\n");
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    let dump = dump_program(&result.program);
    let ternary_pos = dump.find("Ternary").expect("ternary in dump");
    let or_pos = dump.find("Binary `or`").expect("or in dump");
    let and_pos = dump.find("Binary `and`").expect("and in dump");
    assert!(
        ternary_pos < or_pos && ternary_pos < and_pos,
        "ternary must be the outermost node:\n{dump}"
    );
}

#[test]
fn parse_ternary_in_the_middle_branch() {
    // The then-branch may itself be a ternary; the `:` separators pair up
    // innermost-first: `a ? b ? c : d : e` is `a ? (b ? c : d) : e`.
    let result = parse("x = a ? b ? c : d : e\n");
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    insta::assert_snapshot!(dump_program(&result.program));
}

#[test]
fn parse_ternary_usable_anywhere_an_expression_is() {
    // Call arguments, history targets, parentheses, if conditions.
    for source in [
        "plot(a ? b : c)\n",
        "x = (a ? close : open)[1]\n",
        "x = (a ? b : c)\n",
        "if a ? b : c\n    plot(close)\n",
    ] {
        let result = parse(source);
        assert!(
            result.diagnostics.is_empty(),
            "{source:?} diagnostics: {:?}",
            result.diagnostics
        );
    }
}

#[test]
fn parse_question_without_colon_is_an_error() {
    let result = parse("x = a ? b\nplot(close)\n");
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0110") && d.message.contains("expected `:`")),
        "diagnostics: {:?}",
        result.diagnostics
    );
    // Recovery: the next statement still parses.
    assert_eq!(result.program.items.len(), 1);
}

#[test]
fn parse_question_immediately_followed_by_colon_is_an_error() {
    // `x ?: y` — the then-branch is missing; the stray `:` gets the
    // targeted ternary hint.
    let result = parse("z = x ?: y\nplot(close)\n");
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0110")
                && d.message.contains("expected an expression, found `:`")),
        "diagnostics: {:?}",
        result.diagnostics
    );
    assert_eq!(result.program.items.len(), 1, "plot should survive");
}

#[test]
fn parse_bare_question_mark_is_an_error() {
    let result = parse("x = ?\n");
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0110")
                && d.message.contains("expected an expression, found `?`")),
        "diagnostics: {:?}",
        result.diagnostics
    );
}

#[test]
fn parse_deep_ternary_nesting_is_rejected_not_crashed() {
    // A right-leaning ternary chain nests one level per `?`; the depth cap
    // must reject it with a diagnostic instead of overflowing the stack.
    let source = format!("x = {}1\n", "a ? 1 : ".repeat(5000));
    let result = parse(&source);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.message.contains("nested too deeply")),
        "expected a depth diagnostic"
    );
}

// -- the `na` keyword -----------------------------------------------------------

#[test]
fn lex_na_is_a_keyword_not_an_identifier() {
    use TokenKind::*;
    assert_eq!(
        token_kinds("x = na nap"),
        vec![Ident, Eq, KwNa, Ident, Newline, Eof]
    );
}

#[test]
fn parse_bare_na_is_the_literal() {
    let result = parse("plot(close + na)\n");
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    let dump = dump_program(&result.program);
    assert!(dump.contains("Na @ 13..15"), "dump:\n{dump}");
}

#[test]
fn parse_na_followed_by_paren_is_a_call() {
    let result = parse("x = na(close)\n");
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    let pinewasm_syntax::Item::Stmt(pinewasm_syntax::Stmt::Assign { value, .. }) =
        &result.program.items[0]
    else {
        panic!("expected assignment");
    };
    let pinewasm_syntax::Expr::Call { callee, args, .. } = value else {
        panic!("expected call, got {value:?}");
    };
    assert_eq!(callee.name, "na");
    assert_eq!(args.len(), 1);
}

#[test]
fn parse_assignment_to_na_is_a_targeted_error() {
    for source in ["na = 1\n", "na := 1\n"] {
        let result = parse(source);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("cannot assign to `na`")),
            "{source:?} diagnostics: {:?}",
            result.diagnostics
        );
    }
}

// -- type annotations ----------------------------------------------------------

#[test]
fn lex_type_annotation_keywords() {
    use TokenKind::*;
    assert_eq!(
        token_kinds("int float bool string varip"),
        vec![KwInt, KwFloat, KwBool, KwString, KwVarip, Newline, Eof]
    );
}

#[test]
fn parse_annotated_declaration_records_type() {
    for (source, marker) in [
        ("int n = 1\n", "Assign `n` : int"),
        ("float f = 1.5\n", "Assign `f` : float"),
        ("bool b = true\n", "Assign `b` : bool"),
        ("string s = \"a\"\n", "Assign `s` : string"),
        ("var float g = na\n", "VarAssign `g` : float"),
    ] {
        let result = parse(source);
        assert!(
            !has_errors(&result.diagnostics),
            "{source:?} diagnostics: {:?}",
            result.diagnostics
        );
        let dump = dump_program(&result.program);
        assert!(dump.contains(marker), "expected {marker:?} in:\n{dump}");
    }
}

#[test]
fn parse_varip_declares_a_persistent_slot() {
    // `varip` parses to a `VaripAssign` declaration.
    let result = parse("varip x = 1\n");
    assert!(
        !has_errors(&result.diagnostics),
        "diagnostics: {:?}",
        result.diagnostics
    );
    let dump = dump_program(&result.program);
    assert!(
        dump.contains("VaripAssign `x`"),
        "expected a VaripAssign in:\n{dump}"
    );
}

#[test]
fn parse_varip_annotated_and_reassign_rules() {
    // `varip int x = 1` is a valid annotated declaration.
    let annotated = parse("varip int x = 1\n");
    assert!(
        !has_errors(&annotated.diagnostics),
        "diagnostics: {:?}",
        annotated.diagnostics
    );
    assert!(dump_program(&annotated.program).contains("VaripAssign `x`"));

    // `varip x := 1` is a parse error, like `var x := 1`.
    let reassign = parse("varip x := 1\n");
    assert!(has_errors(&reassign.diagnostics));
    assert!(
        reassign.diagnostics[0]
            .message
            .contains("`varip` declares with `=`, not `:=`"),
        "diagnostics: {:?}",
        reassign.diagnostics
    );
}

#[test]
fn parse_annotated_declaration_with_colon_eq_is_an_error() {
    // Annotations always declare with `=`; `:=` here is a targeted parse error.
    let result = parse("int n := 1\n");
    assert!(has_errors(&result.diagnostics));
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.message.contains("uses `=`, not `:=`")),
        "diagnostics: {:?}",
        result.diagnostics
    );
}

#[test]
fn parse_annotation_missing_name_recovers() {
    // A broken annotated header (`int[] = 1`) reports one error and does not
    // cascade into phantom diagnostics on the following line.
    let result = parse("int[] = 1\nplot(close)\n");
    assert!(has_errors(&result.diagnostics));
    // Exactly one diagnostic: the missing identifier after the type keyword.
    assert_eq!(
        result.diagnostics.len(),
        1,
        "diagnostics: {:?}",
        result.diagnostics
    );
}

// -- namespaced (dotted) identifiers -------------------------------------------

#[test]
fn lex_dot_token_and_number_fractions() {
    use TokenKind::*;
    // A dot between identifiers is its own token...
    assert_eq!(
        token_kinds("x = ta.sma(close, 14)"),
        vec![Ident, Eq, Ident, Dot, Ident, LParen, Ident, Comma, Number, RParen, Newline, Eof]
    );
    // ...but a fraction is still consumed by the number rule: `1.5` is one
    // Number token, and `x.5` is Ident Dot Number.
    assert_eq!(
        token_kinds("y = 1.5"),
        vec![Ident, Eq, Number, Newline, Eof]
    );
    assert_eq!(
        token_kinds("y = x.5"),
        vec![Ident, Eq, Ident, Dot, Number, Newline, Eof]
    );
    // `1.` is Pine's trailing-dot float (equal to `1.0`) — one Number token.
    assert_eq!(token_kinds("y = 1."), vec![Ident, Eq, Number, Newline, Eof]);
    // The dot stays separate when an identifier follows, so a member/method
    // name after a number literal is not swallowed into it.
    assert_eq!(
        token_kinds("y = 1.x"),
        vec![Ident, Eq, Number, Dot, Ident, Newline, Eof]
    );
}

#[test]
fn parse_dotted_name_folds_into_one_identifier() {
    let source = "x = ta.sma(close, 14)\n";
    let result = parse(source);
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    let pinewasm_syntax::Item::Stmt(pinewasm_syntax::Stmt::Assign { value, .. }) =
        &result.program.items[0]
    else {
        panic!("expected assignment");
    };
    let pinewasm_syntax::Expr::Call { callee, .. } = value else {
        panic!("expected call, got {value:?}");
    };
    assert_eq!(callee.name, "ta.sma");
    // The identifier's span covers the whole dotted path.
    assert_eq!(
        &source[callee.span.start as usize..callee.span.end as usize],
        "ta.sma"
    );
}

#[test]
fn parse_deeply_dotted_name_folds_all_segments() {
    let result = parse("x = a.b.c\n");
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    let pinewasm_syntax::Item::Stmt(pinewasm_syntax::Stmt::Assign { value, .. }) =
        &result.program.items[0]
    else {
        panic!("expected assignment");
    };
    let pinewasm_syntax::Expr::Ident(ident) = value else {
        panic!("expected identifier, got {value:?}");
    };
    assert_eq!(ident.name, "a.b.c");
}

#[test]
fn parse_syminfo_type_allows_keyword_namespace_segment() {
    let source = "x = syminfo.type\n";
    let result = parse(source);
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    let pinewasm_syntax::Item::Stmt(pinewasm_syntax::Stmt::Assign { value, .. }) =
        &result.program.items[0]
    else {
        panic!("expected assignment");
    };
    let pinewasm_syntax::Expr::Ident(ident) = value else {
        panic!("expected identifier, got {value:?}");
    };
    assert_eq!(ident.name, "syminfo.type");
    assert_eq!(
        &source[ident.span.start as usize..ident.span.end as usize],
        "syminfo.type"
    );
}

#[test]
fn parse_assignment_to_namespaced_name_is_a_targeted_error() {
    // A dotted **declaration** target (`=`) is always a parse error: declaration
    // targets must be plain identifiers.
    let source = "ta.sma = 1\nplot(close)\n";
    let result = parse(source);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0110")
                && d.message.contains("cannot assign to a namespaced name")),
        "diagnostics for {source:?}: {:?}",
        result.diagnostics
    );
    // Recovery: the following statement still parses.
    assert_eq!(result.program.items.len(), 1, "source: {source:?}");
}

#[test]
fn parse_dotted_reassign_target_is_not_a_parse_error() {
    // A dotted `:=` target parses cleanly (it is a field-write reassignment,
    // `obj.field := value`); whether the head is an object variable is a
    // semantic question resolved later (E0217 for a namespaced builtin).
    let result = parse("ta.sma := 1\nplot(close)\n");
    assert!(
        !has_errors(&result.diagnostics),
        "dotted `:=` should parse cleanly: {:?}",
        result.diagnostics
    );
    let dump = dump_program(&result.program);
    assert!(dump.contains("Reassign `ta.sma`"), "{dump}");
}

#[test]
fn parse_dot_followed_by_non_identifier_is_an_error() {
    // `x.5` — the dot is not claimed by a number, and `5` is not an
    // identifier.
    let result = parse("y = x.5\n");
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0110")
                && d.message.contains("expected an identifier after `.`")),
        "diagnostics: {:?}",
        result.diagnostics
    );

    // A leading dot that does not follow an operand starts a float.
    let result = parse("y = .5\n");
    assert!(!has_errors(&result.diagnostics), "{:?}", result.diagnostics);
}

// -- robustness --------------------------------------------------------------

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(256))]

    /// The lexer and parser must never panic, whatever bytes come in.
    #[test]
    fn parse_never_panics(source in "\\PC*") {
        let _ = parse(&source);
    }

    /// Structured-looking input must not panic either.
    #[test]
    fn parse_never_panics_structured(source in "(if |else|plot|close|open|[a-z]{1,3}|[0-9]{1,3}|\\[|\\]|\\(|\\)|=|==|-|\\+|\\*|/| |\\n|    |\"[a-z]*\"?){0,40}") {
        let _ = parse(&source);
    }

    /// Nor must declaration-shaped input: types, type arguments, parameter
    /// lists and the keywords that introduce them, in any order.
    #[test]
    fn parse_never_panics_on_declarations(source in "(type |enum |method |export |var |varip |series |simple |array<|matrix<|map<|<|>|,|\\[\\]|int|float|bool|string|color|[A-Za-z]{1,3}|[0-9]|\\.|=|:=|=>|\\(|\\)| |\\n|    |\\t){0,60}") {
        let _ = parse(&source);
    }
}

// -- input guards --------------------------------------------------------------

/// The one diagnostic an unread or unparsed source comes back with: the
/// input-guard code and the message, with nothing in the buffer to point at.
#[track_caller]
fn assert_refused(diagnostics: &[pinewasm_span::Diagnostic], message: &str) {
    assert_eq!(
        diagnostics,
        [pinewasm_span::Diagnostic::error("E0001", message)]
    );
}

#[test]
fn a_source_at_the_size_cap_is_read_and_one_byte_more_is_not() {
    // The cap counts UTF-8 bytes. `é` is two of them, so this source is one
    // byte over while holding fewer characters than the cap.
    let line = "x = 1\n";
    let mut at_cap = line.repeat(MAX_SOURCE_LEN / line.len());
    at_cap.push_str(&" ".repeat(MAX_SOURCE_LEN - at_cap.len()));
    assert_eq!(at_cap.len(), MAX_SOURCE_LEN);
    let over = format!("{}//éé", &at_cap[..MAX_SOURCE_LEN - 5]);
    assert_eq!(over.len(), MAX_SOURCE_LEN + 1);
    assert!(over.chars().count() < MAX_SOURCE_LEN);

    let lexed = lex(&at_cap);
    assert!(lexed.diagnostics.is_empty());
    assert!(lexed.tokens.len() > MAX_SOURCE_LEN / 2);
    let parsed = parse(&at_cap);
    assert!(parsed.diagnostics.is_empty());
    assert_eq!(parsed.program.items.len(), MAX_SOURCE_LEN / line.len());

    let message = format!(
        "source is {} bytes; the maximum supported size is {MAX_SOURCE_LEN}",
        MAX_SOURCE_LEN + 1
    );
    let lexed = lex(&over);
    assert_eq!(
        lexed.tokens.iter().map(|t| t.kind).collect::<Vec<_>>(),
        vec![TokenKind::Eof]
    );
    assert_eq!(lexed.tokens[0].span, Span::new(SourceId::MAIN, 0, 0));
    assert_refused(&lexed.diagnostics, &message);

    let parsed = parse(&over);
    assert!(parsed.program.items.is_empty());
    assert_eq!(parsed.program.version, None);
    assert!(parsed.once_keyword_spans.is_empty());
    assert_refused(&parsed.diagnostics, &message);
}

#[test]
fn a_refused_source_keeps_its_own_source_id() {
    // A library is lexed and parsed under its own source id. What comes back
    // for a refused one still belongs to that buffer, not the main script.
    let library = SourceId(3);
    let over = " ".repeat(MAX_SOURCE_LEN + 7);
    let message = format!(
        "source is {} bytes; the maximum supported size is {MAX_SOURCE_LEN}",
        MAX_SOURCE_LEN + 7
    );
    let lexed = lex_with_source_id(&over, library);
    assert_eq!(lexed.tokens[0].span, Span::new(library, 0, 0));
    assert_refused(&lexed.diagnostics, &message);
    let parsed = parse_with_source_id(&over, library);
    assert_eq!(parsed.program.span, Span::new(library, 0, 0));
    assert_refused(&parsed.diagnostics, &message);

    let pair = "a\n\ta\n";
    let dense = pair.repeat(MAX_SOURCE_LEN / pair.len());
    let parsed = parse_with_source_id(&dense, library);
    assert_eq!(parsed.program.span, Span::new(library, 0, 0));
    assert_eq!(parsed.diagnostics.len(), 1);
}

#[test]
fn a_far_larger_source_is_refused_without_being_scanned() {
    // 64 MiB of text that would lex to tens of millions of tokens: refusing
    // it costs nothing that grows with its size.
    let huge = "x = 1\n".repeat((64 << 20) / 6);
    let lexed = lex(&huge);
    assert_eq!(lexed.tokens.len(), 1);
    assert_eq!(lexed.diagnostics.len(), 1);
    let parsed = parse(&huge);
    assert!(parsed.program.items.is_empty());
    assert_eq!(parsed.diagnostics.len(), 1);
}

#[test]
fn a_source_at_the_token_budget_is_parsed_and_one_token_more_is_not() {
    // A run of `(` is one token per byte, plus the `Newline` and `Eof` the
    // lexer adds, so the token count is set exactly. The parser gives up on
    // the run at its nesting cap without building anything for the rest,
    // which keeps this cheap; what matters is that it was asked to parse.
    let at_budget = "(".repeat(MAX_TOKENS - 2);
    assert!(at_budget.len() <= MAX_SOURCE_LEN);
    assert_eq!(lex(&at_budget).tokens.len(), MAX_TOKENS);
    let parsed = parse(&at_budget);
    assert!(parsed
        .diagnostics
        .iter()
        .all(|d| d.code.as_deref() == Some("E0110")));
    assert!(parsed
        .diagnostics
        .iter()
        .any(|d| d.message.contains("nested too deeply")));

    let over = "(".repeat(MAX_TOKENS - 1);
    assert!(over.len() <= MAX_SOURCE_LEN);
    assert_eq!(lex(&over).tokens.len(), MAX_TOKENS + 1);
    let parsed = parse(&over);
    assert!(parsed.program.items.is_empty());
    assert_refused(
        &parsed.diagnostics,
        &format!(
            "source has {} tokens; the maximum supported count is {MAX_TOKENS}",
            MAX_TOKENS + 1
        ),
    );
}

#[test]
fn block_structure_can_put_a_source_over_the_token_budget() {
    // `Indent`, `Dedent` and `Newline` are tokens no byte spells. Short
    // lines that alternate between two indentation levels lex to six tokens
    // per five bytes, so a source inside the size cap passes the budget, and
    // the budget, not the size cap, is what refuses it.
    let pair = "a\n\ta\n";
    let source = pair.repeat(MAX_SOURCE_LEN / pair.len());
    assert!(source.len() <= MAX_SOURCE_LEN);
    let lexed = lex(&source);
    assert!(lexed.diagnostics.is_empty());
    let tokens = lexed.tokens.len();
    assert!(tokens > MAX_TOKENS, "{tokens}");
    assert!(tokens > source.len());

    let parsed = parse(&source);
    assert!(parsed.program.items.is_empty());
    assert_refused(
        &parsed.diagnostics,
        &format!("source has {tokens} tokens; the maximum supported count is {MAX_TOKENS}"),
    );

    // Written code is nowhere near: this line is 33 tokens in 104 bytes.
    let written = "fastLength = input.int(12, \"Fast length\", minval = 1)\n\
                   plot(ta.ema(close, fastLength), color = color.blue)\n";
    let full = written.repeat(MAX_SOURCE_LEN / written.len());
    let lexed = lex(&full);
    assert!(lexed.diagnostics.is_empty());
    assert!(
        lexed.tokens.len() < MAX_TOKENS / 3,
        "{}",
        lexed.tokens.len()
    );
    assert!(parse(&full).diagnostics.is_empty());
}

// -- adversarial regression tests ---------------------------------------------

#[test]
fn trailing_version_comment_is_a_plain_comment() {
    // A //@version= comment after a statement must not become a directive
    // token (which would be a spurious parse error).
    let result = parse("//@version=6\nx = close //@version=6\nplot(x)\n");
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    assert_eq!(result.program.version.map(|v| v.version), Some(6));
}

#[test]
fn version_directive_can_follow_top_level_code() {
    for source in [
        "indicator(\"Late\")\n//@version=6\nplot(close)\n",
        "import author/library/1 as lib\n//@version=6\nindicator(\"Imports first\")\nplot(close)\n",
        "indicator(\"Last\")\nplot(close)\n//@version=6\n",
    ] {
        let result = parse(source);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert_eq!(result.program.version.map(|v| v.version), Some(6));
    }
}

#[test]
fn indented_version_directive_is_an_error() {
    let result = parse("    //@version=6\nplot(close)\n");
    assert!(has_errors(&result.diagnostics));
    // The rest of the file still parses.
    assert_eq!(result.program.items.len(), 1);
}

#[test]
fn broken_if_does_not_leave_phantom_indentation_errors() {
    // The `if` line fails to parse; its block must be consumed by recovery,
    // not reported as stray top-level indentation.
    let result = parse("if close >\n    plot(close)\nplot(open)\n");
    assert!(has_errors(&result.diagnostics));
    assert!(
        !result
            .diagnostics
            .iter()
            .any(|d| d.message.contains("unexpected indentation")),
        "phantom indentation error: {:?}",
        result.diagnostics
    );
    assert_eq!(
        result.program.items.len(),
        1,
        "trailing statement should survive"
    );
}

#[test]
fn one_bad_character_produces_one_diagnostic() {
    let result = parse("x = 1 § 2\n");
    let errors: Vec<_> = result.diagnostics.iter().filter(|d| d.is_error()).collect();
    assert_eq!(errors.len(), 1, "expected exactly one error: {errors:?}");
    assert_eq!(errors[0].code.as_deref(), Some("E0101"));
}

#[test]
fn deep_postfix_chain_is_bounded_not_overflowed() {
    // `a[1][1][1]...` builds a left-nested History tree in a loop; without a
    // bound it overflows the stack on drop. It must be rejected with a
    // structured diagnostic instead.
    let source = format!("z = a{}\n", "[1]".repeat(5000));
    let result = parse(&source);
    assert!(has_errors(&result.diagnostics));
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.message.contains("nested too deeply")),
        "expected a depth diagnostic: {:?}",
        result.diagnostics
    );
}

#[test]
fn deep_call_chain_is_bounded() {
    // Long call/subscript mixes are bounded the same way.
    let source = format!("z = f{}\n", "(a)".repeat(5000));
    let result = parse(&source);
    assert!(has_errors(&result.diagnostics));
}

#[test]
fn shallow_postfix_chains_still_parse() {
    // A handful of postfix ops must remain fine — the bound only trips deep
    // adversarial chains.
    let result = parse("z = close[1]\ny = a[1][2][3]\n");
    assert!(
        !has_errors(&result.diagnostics),
        "diagnostics: {:?}",
        result.diagnostics
    );
}

// -- loops -------------------------------------------------------------------

#[test]
fn lex_loop_keywords() {
    use TokenKind::*;
    assert_eq!(
        token_kinds("for i = 1 to n by 2"),
        vec![KwFor, Ident, Eq, Number, KwTo, Ident, KwBy, Number, Newline, Eof]
    );
    assert_eq!(
        token_kinds("while x\n    break\n    continue"),
        vec![KwWhile, Ident, Newline, Indent, KwBreak, Newline, KwContinue, Newline, Dedent, Eof]
    );
}

#[test]
fn parse_for_loop_snapshot() {
    let source = "for i = 1 to 10\n    s := s + i\n";
    let result = parse(source);
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    insta::assert_snapshot!(dump_program(&result.program));
}

#[test]
fn parse_for_loop_with_step_snapshot() {
    let source = "for i = 10 to 0 by -2\n    plot(i)\n";
    let result = parse(source);
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    insta::assert_snapshot!(dump_program(&result.program));
}

#[test]
fn parse_while_with_break_continue_snapshot() {
    let source = "while x > 0\n    if x == 5\n        break\n    continue\n";
    let result = parse(source);
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    insta::assert_snapshot!(dump_program(&result.program));
}

#[test]
fn parse_broken_for_header_recovers() {
    // Missing `to`: the header is broken; the parser reports and resyncs,
    // and the following statement still parses.
    let source = "for i = 1 10\n    plot(i)\nplot(close)\n";
    let result = parse(source);
    assert!(has_errors(&result.diagnostics));
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0110")),
        "diagnostics: {:?}",
        result.diagnostics
    );
    // The trailing `plot(close)` recovered into a statement.
    assert!(dump_program(&result.program).contains("Call `plot`"));
}

#[test]
fn parse_for_counter_with_reassign_is_a_parse_error() {
    // The counter is declared with `=`, not `:=`.
    let result = parse("for i := 1 to 10\n    plot(i)\n");
    assert!(has_errors(&result.diagnostics));
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0110") && d.message.contains("declared with `=`")),
        "diagnostics: {:?}",
        result.diagnostics
    );
}

#[test]
fn parse_stray_break_still_produces_a_statement() {
    // A `break` outside a loop parses fine (it is a semantic error, E0223);
    // parser-level recovery of a stray keyword line.
    let result = parse("break\nplot(close)\n");
    assert!(
        !has_errors(&result.diagnostics),
        "diagnostics: {:?}",
        result.diagnostics
    );
    let dump = dump_program(&result.program);
    assert!(dump.contains("Break"));
    assert!(dump.contains("Call `plot`"));
}

// -- if / switch expressions -------------------------------------------------

#[test]
fn lex_fat_arrow_and_switch() {
    use TokenKind::*;
    // `=>` is one token; `=` and `==` are unaffected.
    assert_eq!(
        token_kinds("a = b == c => d"),
        vec![Ident, Eq, Ident, EqEq, Ident, FatArrow, Ident, Newline, Eof]
    );
    assert_eq!(token_kinds("switch")[0], KwSwitch);
}

#[test]
fn parse_if_expression_snapshot() {
    let source = "p = if close > open\n    close\nelse\n    open\n";
    let result = parse(source);
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    insta::assert_snapshot!(dump_program(&result.program));
}

#[test]
fn parse_switch_expression_snapshot() {
    let source = "p = switch bar_index\n    0 => 1.0\n    1 => 2.0\n    => 3.0\n";
    let result = parse(source);
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    insta::assert_snapshot!(dump_program(&result.program));
}

#[test]
fn parse_subjectless_switch_snapshot() {
    let source = "p = switch\n    close > open => 1.0\n    => 0.0\n";
    let result = parse(source);
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    insta::assert_snapshot!(dump_program(&result.program));
}

#[test]
fn parse_switch_statement_with_block_arm_snapshot() {
    let source = "switch bar_index\n    0 =>\n        x = 1\n        plot(x)\n    => plot(close)\n";
    let result = parse(source);
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    insta::assert_snapshot!(dump_program(&result.program));
}

#[test]
fn parse_if_expression_followed_by_statement_recovers() {
    // A block-form if-expression as an RHS ends the logical line at its
    // dedent; the following statement must still parse.
    let source = "p = if close > open\n    close\nelse\n    open\nplot(p)\n";
    let result = parse(source);
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    assert_eq!(
        result.program.items.len(),
        2,
        "the plot after an if-expression must survive: {:?}",
        result.program.items
    );
}

#[test]
fn block_expression_ends_before_a_statement_that_opens_with_an_operator_token() {
    // The next statement starts with a token that could continue an
    // expression (`(` would read as a call, `-` as a subtraction, `[` as a
    // history reference). After a block-form `if`/`switch` expression has
    // ended with its dedent, none of them may attach to it.
    for next in ["(a or b)", "-x", "[p, q] = f()"] {
        for block in [
            "t = if a\n    1\nelse\n    2\n",
            "t = switch\n    a => 1\n    => 2\n",
        ] {
            let source = format!("{block}{next}\n");
            let result = parse(&source);
            assert!(
                result.diagnostics.is_empty(),
                "{source:?}: {:?}",
                result.diagnostics
            );
            assert_eq!(
                result.program.items.len(),
                2,
                "{source:?}: {:?}",
                result.program.items
            );
        }
    }
}

#[test]
fn parse_switch_arm_missing_arrow_recovers() {
    // A missing `=>` is a parse error; later arms/statements still parse.
    let source = "p = switch bar_index\n    0 1.0\n    => 2.0\nplot(p)\n";
    let result = parse(source);
    assert!(has_errors(&result.diagnostics));
}

// -- user-defined functions -------------------------------------------------

#[test]
fn parse_single_line_function_definition() {
    let source = "indicator(\"x\")\nadd(a, b) => a + b\nplot(add(close, 1))\n";
    let result = parse(source);
    assert!(
        !has_errors(&result.diagnostics),
        "diagnostics: {:?}",
        result.diagnostics
    );
    use pinewasm_syntax::Item;
    let def = result
        .program
        .items
        .iter()
        .find_map(|it| match it {
            Item::FuncDef(d) => Some(d),
            _ => None,
        })
        .expect("a function definition item");
    assert_eq!(def.name.name, "add");
    assert_eq!(def.params.len(), 2);
    assert_eq!(def.params[0].name.name, "a");
    assert_eq!(def.params[1].name.name, "b");
    assert!(def.params.iter().all(|p| p.default.is_none()));
    // A single-line body is one expression statement (the return value).
    assert_eq!(def.body.len(), 1);
}

#[test]
fn parse_multiline_function_definition_with_locals() {
    let source =
        "indicator(\"x\")\ndelta(src) =>\n    prev = src\n    src - prev[1]\nplot(delta(close))\n";
    let result = parse(source);
    assert!(
        !has_errors(&result.diagnostics),
        "diagnostics: {:?}",
        result.diagnostics
    );
    use pinewasm_syntax::Item;
    let def = result
        .program
        .items
        .iter()
        .find_map(|it| match it {
            Item::FuncDef(d) => Some(d),
            _ => None,
        })
        .expect("a function definition item");
    // Two body statements: the local `prev` and the trailing return value.
    assert_eq!(def.body.len(), 2);
}

#[test]
fn parse_function_default_parameter() {
    let source = "indicator(\"x\")\nscale(x, factor = 2) => x * factor\nplot(scale(close))\n";
    let result = parse(source);
    assert!(
        !has_errors(&result.diagnostics),
        "diagnostics: {:?}",
        result.diagnostics
    );
    use pinewasm_syntax::Item;
    let def = result
        .program
        .items
        .iter()
        .find_map(|it| match it {
            Item::FuncDef(d) => Some(d),
            _ => None,
        })
        .expect("a function definition item");
    assert!(def.params[0].default.is_none());
    assert!(def.params[1].default.is_some());
}

#[test]
fn a_call_is_not_mistaken_for_a_definition() {
    // `f(x)` with no `=>` is a plain call statement, not a definition.
    let source = "indicator(\"x\")\nplot(close)\nf(close)\n";
    let result = parse(source);
    use pinewasm_syntax::Item;
    assert!(
        !result
            .program
            .items
            .iter()
            .any(|it| matches!(it, Item::FuncDef(_))),
        "a bare call must not parse as a function definition"
    );
}

// -- tuples ------------------------------------------------------------------

#[test]
fn parse_destructuring_declaration() {
    let result = parse("indicator(\"T\")\n[a, b, c] = ta.macd(close, 12, 26, 9)\n");
    assert!(
        !has_errors(&result.diagnostics),
        "diagnostics: {:?}",
        result.diagnostics
    );
    let dump = dump_program(&result.program);
    assert!(dump.contains("Destructure [a, b, c]"), "{dump}");
    // The RHS is an ordinary call, not a tuple pattern.
    assert!(dump.contains("Call `ta.macd`"), "{dump}");
}

#[test]
fn parse_tuple_expression_as_function_body_tail() {
    // A `[hi, lo]` at a statement start with no `=` after `]` is a tuple
    // expression statement (a function-body return value), not a destructure.
    let result = parse("indicator(\"T\")\nf(x) =>\n    hi = x + 1.0\n    [hi, x]\n");
    assert!(
        !has_errors(&result.diagnostics),
        "diagnostics: {:?}",
        result.diagnostics
    );
    let dump = dump_program(&result.program);
    assert!(dump.contains("Tuple @"), "{dump}");
    // It is NOT parsed as a destructure (no `=`).
    assert!(!dump.contains("Destructure"), "{dump}");
}

#[test]
fn parse_single_line_tuple_function() {
    let result = parse("indicator(\"T\")\npair(x) => [x + 1.0, x - 1.0]\n");
    assert!(
        !has_errors(&result.diagnostics),
        "diagnostics: {:?}",
        result.diagnostics
    );
    assert!(dump_program(&result.program).contains("Tuple @"));
}

#[test]
fn parse_destructure_with_colon_eq_is_error() {
    let result = parse("indicator(\"T\")\n[a, b] := ta.bb(close, 5, 1.0)\n");
    assert!(has_errors(&result.diagnostics));
    assert_eq!(result.diagnostics[0].code.as_deref(), Some("E0110"));
}

#[test]
fn parse_destructure_with_dotted_target_is_error() {
    let result = parse("indicator(\"T\")\n[a, ta.b] = ta.bb(close, 5, 1.0)\n");
    assert!(has_errors(&result.diagnostics));
    assert_eq!(result.diagnostics[0].code.as_deref(), Some("E0110"));
}

#[test]
fn history_offset_bracket_is_not_a_tuple() {
    // `close[1]` after a primary is a history postfix, never a tuple.
    let result = parse("indicator(\"T\")\nx = close[1]\nplot(x)\n");
    assert!(
        !has_errors(&result.diagnostics),
        "diagnostics: {:?}",
        result.diagnostics
    );
    let dump = dump_program(&result.program);
    assert!(dump.contains("History @"), "{dump}");
    assert!(!dump.contains("Tuple"), "{dump}");
}

// -- named arguments ---------------------------------------------------------

#[test]
fn parse_named_argument_in_call() {
    let source = "indicator(\"T\")\nx = ta.sma(close, length = 14)\nplot(x)\n";
    let result = parse(source);
    assert!(
        !has_errors(&result.diagnostics),
        "diagnostics: {:?}",
        result.diagnostics
    );
    let pinewasm_syntax::Item::Stmt(pinewasm_syntax::Stmt::Assign { value, .. }) =
        &result.program.items[1]
    else {
        panic!("expected assignment");
    };
    let pinewasm_syntax::Expr::Call { args, .. } = value else {
        panic!("expected call, got {value:?}");
    };
    assert_eq!(args.len(), 2);
    // First argument is positional.
    assert!(args[0].name.is_none());
    // Second argument is `length = 14`.
    let name = args[1].name.as_ref().expect("second arg is named");
    assert_eq!(name.name, "length");
    assert!(
        matches!(args[1].value, pinewasm_syntax::Expr::Int { value: 14, .. }),
        "value: {:?}",
        args[1].value
    );
}

#[test]
fn parse_all_named_arguments() {
    let source = "indicator(title = \"My Script\")\nplot(close)\n";
    let result = parse(source);
    assert!(
        !has_errors(&result.diagnostics),
        "diagnostics: {:?}",
        result.diagnostics
    );
    let pinewasm_syntax::Item::Indicator(decl) = &result.program.items[0] else {
        panic!("expected indicator decl");
    };
    assert_eq!(decl.args.len(), 1);
    assert_eq!(
        decl.args[0].name.as_ref().map(|n| n.name.as_str()),
        Some("title")
    );
}

#[test]
fn parse_keyword_spelling_as_named_argument() {
    let source =
        "indicator(\"T\")\nx = request.currency_rate(from = \"EUR\", to = \"USD\")\nplot(x)\n";
    let result = parse(source);
    assert!(
        !has_errors(&result.diagnostics),
        "diagnostics: {:?}",
        result.diagnostics
    );
    let pinewasm_syntax::Item::Stmt(pinewasm_syntax::Stmt::Assign { value, .. }) =
        &result.program.items[1]
    else {
        panic!("expected assignment");
    };
    let pinewasm_syntax::Expr::Call { args, .. } = value else {
        panic!("expected call, got {value:?}");
    };
    assert_eq!(
        args[1].name.as_ref().map(|name| name.name.as_str()),
        Some("to")
    );
}

#[test]
fn parse_positional_after_named_is_error() {
    // A positional argument may not follow a named one.
    let result = parse("indicator(\"T\")\nx = ta.sma(source = close, 14)\nplot(x)\n");
    assert!(has_errors(&result.diagnostics), "expected a parse error");
    assert!(
        result.diagnostics.iter().any(|d| d
            .message
            .contains("positional argument cannot follow a named argument")),
        "diagnostics: {:?}",
        result.diagnostics
    );
}

#[test]
fn parse_equality_is_not_a_named_argument() {
    // `a == b` uses the distinct `==` token and must not be mistaken for a
    // named argument (`a = ...`).
    let source = "indicator(\"T\")\nx = na(close == open)\nplot(x)\n";
    let result = parse(source);
    assert!(
        !has_errors(&result.diagnostics),
        "diagnostics: {:?}",
        result.diagnostics
    );
    let pinewasm_syntax::Item::Stmt(pinewasm_syntax::Stmt::Assign { value, .. }) =
        &result.program.items[1]
    else {
        panic!("expected assignment");
    };
    let pinewasm_syntax::Expr::Call { args, .. } = value else {
        panic!("expected call");
    };
    assert_eq!(args.len(), 1);
    assert!(args[0].name.is_none(), "`==` must stay positional");
    assert!(matches!(
        args[0].value,
        pinewasm_syntax::Expr::Binary { .. }
    ));
}

#[test]
fn parse_named_argument_dump() {
    let source = "indicator(\"T\")\nx = ta.sma(close, length = 14)\nplot(x)\n";
    let result = parse(source);
    let dump = dump_program(&result.program);
    assert!(dump.contains("NamedArg `length`"), "{dump}");
}

// -- for…in, method-call, generic constructors -------------------------------

#[test]
fn lex_in_keyword() {
    use TokenKind::*;
    assert_eq!(
        token_kinds("for x in data"),
        vec![KwFor, Ident, KwIn, Ident, Newline, Eof]
    );
}

#[test]
fn parse_for_in_element_snapshot() {
    let source = "for x in data\n    plot(x)\n";
    let result = parse(source);
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    insta::assert_snapshot!(dump_program(&result.program));
}

#[test]
fn parse_for_in_index_element_snapshot() {
    let source = "for [i, x] in data\n    plot(x)\n";
    let result = parse(source);
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    insta::assert_snapshot!(dump_program(&result.program));
}

#[test]
fn parse_loop_result_expression() {
    let source =
        "values = array.from(1, 2)\nresult = for value in values\n    value * 2\nplot(result)\n";
    let result = parse(source);
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    let dump = dump_program(&result.program);
    assert!(dump.contains("LoopExpr"), "{dump}");
    assert!(dump.contains("ForIn"), "{dump}");
    assert!(dump.contains("Binary `*`"), "{dump}");
}

#[test]
fn parse_broken_for_in_header_recovers() {
    // `for [i in data` — the tuple pattern is missing its comma/close; the
    // parser reports and resyncs, and the following statement still parses.
    let source = "for [i in data\n    plot(i)\nplot(close)\n";
    let result = parse(source);
    assert!(has_errors(&result.diagnostics));
    assert!(dump_program(&result.program).contains("Call `plot`"));
}

#[test]
fn parse_method_call_folds_receiver_dotted_name() {
    // The parser folds `a.push` into one dotted callee; the semantic layer
    // splits it back into a method call. Here we only check the parse shape.
    let source = "a = array.new_float(0)\na.push(1.0)\n";
    let result = parse(source);
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    let dump = dump_program(&result.program);
    assert!(dump.contains("Call `a.push`"), "{dump}");
}

#[test]
fn parse_generic_constructor_rewrites_to_typed_name() {
    let source = "a = array.new<float>(0)\n";
    let result = parse(source);
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    let dump = dump_program(&result.program);
    assert!(dump.contains("Call `array.new_float`"), "{dump}");
}

#[test]
fn parse_generic_constructor_map_two_type_args() {
    let source = "m = map.new<string, float>()\n";
    let result = parse(source);
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    let dump = dump_program(&result.program);
    assert!(dump.contains("Call `map.new_string_float`"), "{dump}");
}

#[test]
fn parse_generic_constructor_matrix_int() {
    let source = "mat = matrix.new<int>(2, 2)\n";
    let result = parse(source);
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    let dump = dump_program(&result.program);
    assert!(dump.contains("Call `matrix.new_int`"), "{dump}");
}

#[test]
fn parse_generic_type_arg_identifier_is_object_element() {
    // `array.new<banana>()` — an identifier type argument is a (possible)
    // user-defined-type element. The parser accepts it and carries the
    // type argument structurally on the call node; whether `banana` is a real
    // UDT is validated in semantics (E0234 there), not at parse time. So the
    // parse is clean.
    let result = parse("a = array.new<banana>()\n");
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    let dump = dump_program(&result.program);
    assert!(dump.contains("Call `array.new`"), "{dump}");
    assert!(dump.contains("TypeArg `banana`"), "{dump}");
}

#[test]
fn parse_generic_type_arg_non_ident_is_e0234() {
    // `array.new<123>()` — a numeric type argument is neither a scalar type nor
    // an identifier, so it is a parse-level E0234.
    let result = parse("a = array.new<123>()\n");
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0234")),
        "diagnostics: {:?}",
        result.diagnostics
    );
}

#[test]
fn parse_wrong_generic_arity_is_e0234() {
    // `map.new<string>()` — map takes two type arguments.
    let result = parse("m = map.new<string>()\n");
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0234")),
        "diagnostics: {:?}",
        result.diagnostics
    );
}

#[test]
fn comparison_operators_still_parse_outside_generic_position() {
    use TokenKind::*;
    // `<` / `>` remain comparison operators everywhere but the registered
    // generic-constructor position. A plain identifier `.new` that is not a
    // generic-constructor base keeps `<` as a comparison.
    assert_eq!(
        token_kinds("z = a < b"),
        vec![Ident, Eq, Ident, Lt, Ident, Newline, Eof]
    );
    let result = parse("z = a < b and b > c\n");
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    let dump = dump_program(&result.program);
    assert!(dump.contains("Binary `<`"), "{dump}");
    assert!(dump.contains("Binary `>`"), "{dump}");
}

#[test]
fn non_generic_dotted_name_keeps_lt_as_comparison() {
    // `foo.new` is not a registered generic-constructor base, so `<` after it
    // is a comparison, not a type-argument list.
    let result = parse("z = foo.new < 3\n");
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    let dump = dump_program(&result.program);
    assert!(dump.contains("Binary `<`"), "{dump}");
}

#[test]
fn malformed_generics_and_for_in_never_panic() {
    // Adversarial headers must report structured diagnostics and never panic.
    for src in [
        "a = array.new<>()\n",
        "a = array.new<float, int>()\n",
        "a = array.new<float\n",
        "m = map.new<string,>()\n",
        "for in data\n\tplot(x)\n",
        "for [] in data\n\tplot(x)\n",
        "for [a,] in data\n\tplot(x)\n",
        "for [a b] in data\n\tplot(x)\n",
    ] {
        let result = parse(src);
        assert!(
            has_errors(&result.diagnostics),
            "expected an error for {src:?}"
        );
    }
}

#[test]
fn generic_like_comparison_chain_is_not_a_constructor() {
    // `a < b > c` where the head is a plain identifier stays a comparison
    // chain, never a generic constructor.
    let result = parse("z = a < b\n");
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
}

// -- enums -------------------------------------------------------------------

#[test]
fn lex_enum_keyword() {
    use TokenKind::*;
    assert_eq!(
        token_kinds("enum Dir\n    up\n    down\n"),
        vec![KwEnum, Ident, Newline, Indent, Ident, Newline, Ident, Newline, Dedent, Eof]
    );
}

#[test]
fn parse_enum_decl_snapshot() {
    let src = "//@version=6\nindicator(\"E\")\nenum Signal\n    buy = \"Buy signal\"\n    sell = \"Sell signal\"\n    neutral\n";
    let result = parse(src);
    assert!(
        !has_errors(&result.diagnostics),
        "unexpected errors: {:?}",
        result.diagnostics
    );
    insta::assert_snapshot!(dump_program(&result.program));
}

#[test]
fn parse_empty_enum_is_error() {
    // An `enum` with no members is a parse error (the Dedent closes an empty
    // block).
    let src = "enum E\n    \nx = 1\n";
    let result = parse(src);
    assert!(has_errors(&result.diagnostics));
}

#[test]
fn parse_enum_non_string_title_is_error() {
    let src = "enum E\n    a = 3\n";
    let result = parse(src);
    assert!(has_errors(&result.diagnostics));
}

#[test]
fn parse_type_decl_snapshot() {
    let src = "//@version=6\nindicator(\"U\")\n\
               type Point\n    float x\n    int y = 5\n    string s\n    Point next\n    array<float> xs\n";
    let result = parse(src);
    assert!(
        !has_errors(&result.diagnostics),
        "unexpected errors: {:?}",
        result.diagnostics
    );
    insta::assert_snapshot!(dump_program(&result.program));
}

#[test]
fn parse_empty_type_is_error() {
    // A `type` with no fields is a parse error.
    let src = "type P\n    \nx = 1\n";
    let result = parse(src);
    assert!(has_errors(&result.diagnostics));
}

#[test]
fn parse_type_field_write_is_reassign() {
    // `obj.field := value` parses as a reassignment with a dotted target (no
    // error); the resolver decides it is a field write.
    let src = "type P\n    float x\np = P.new()\np.x := 1.0\n";
    let result = parse(src);
    assert!(
        !has_errors(&result.diagnostics),
        "unexpected errors: {:?}",
        result.diagnostics
    );
    let dump = dump_program(&result.program);
    assert!(dump.contains("Reassign `p.x`"), "{dump}");
}

#[test]
fn parse_method_def_single_line() {
    // `method f(TypeName this, p) => expr` — a typed receiver first parameter.
    let result = parse("method f(Point this, dx) => this\n");
    assert!(
        !has_errors(&result.diagnostics),
        "unexpected errors: {:?}",
        result.diagnostics
    );
    let dump = dump_program(&result.program);
    assert!(dump.contains("Method `f`[Point](this, dx)"), "{dump}");
}

#[test]
fn parse_method_def_multi_line() {
    let result = parse("method move(Point this, dx, dy) =>\n    this.x := dx\n    this\n");
    assert!(
        !has_errors(&result.diagnostics),
        "unexpected errors: {:?}",
        result.diagnostics
    );
    let dump = dump_program(&result.program);
    assert!(
        dump.contains("Method `move`[Point](this, dx, dy)"),
        "{dump}"
    );
}

#[test]
fn parse_method_chaining_produces_method_call_nodes() {
    // `obj.a().b()` chains a `.b()` off the result of the `.a()` call.
    let result = parse("r = obj.a().b()\n");
    assert!(
        !has_errors(&result.diagnostics),
        "unexpected errors: {:?}",
        result.diagnostics
    );
    let dump = dump_program(&result.program);
    // The outer `.b()` is a MethodCall whose receiver is the `obj.a()` call.
    assert!(dump.contains("MethodCall `b`"), "{dump}");
    assert!(dump.contains("Call `obj.a`"), "{dump}");
}

#[test]
fn parse_field_access_after_call_produces_field_access_node() {
    let result = parse("value = array.get(points, 0).price\n");
    assert!(
        !has_errors(&result.diagnostics),
        "unexpected errors: {:?}",
        result.diagnostics
    );
    let dump = dump_program(&result.program);
    assert!(dump.contains("FieldAccess .price"), "{dump}");
    assert!(dump.contains("Call `array.get`"), "{dump}");
}

#[test]
fn parse_array_new_with_udt_type_arg() {
    let result = parse("a = array.new<Point>()\n");
    assert!(
        !has_errors(&result.diagnostics),
        "unexpected errors: {:?}",
        result.diagnostics
    );
    let dump = dump_program(&result.program);
    assert!(dump.contains("Call `array.new`"), "{dump}");
    assert!(dump.contains("TypeArg `Point`"), "{dump}");
}

#[test]
fn parse_map_new_with_udt_value_type_arg() {
    let result = parse("m = map.new<string, Point>()\n");
    assert!(
        !has_errors(&result.diagnostics),
        "unexpected errors: {:?}",
        result.diagnostics
    );
    let dump = dump_program(&result.program);
    assert!(dump.contains("Call `map.new`"), "{dump}");
    assert!(dump.contains("TypeArg `string`"), "{dump}");
    assert!(dump.contains("TypeArg `Point`"), "{dump}");
}

// -- libraries / import / export --------------------------------------------

#[test]
fn parse_library_declaration() {
    let result = parse("//@version=6\nlibrary(\"MathLib\")\nexport dbl(x) =>\n    x * 2\n");
    assert!(
        !has_errors(&result.diagnostics),
        "unexpected errors: {:?}",
        result.diagnostics
    );
    let dump = dump_program(&result.program);
    assert!(dump.contains("LibraryDecl"), "{dump}");
    assert!(dump.contains("export FuncDef `dbl`"), "{dump}");
}

#[test]
fn parse_import_with_alias() {
    let result =
        parse("//@version=6\nindicator(\"X\")\nimport me/MathLib/1 as math\nplot(math.dbl(21))\n");
    assert!(
        !has_errors(&result.diagnostics),
        "unexpected errors: {:?}",
        result.diagnostics
    );
    let dump = dump_program(&result.program);
    assert!(dump.contains("Import `me/MathLib/1` as `math`"), "{dump}");
}

#[test]
fn parse_import_with_hyphenated_publisher() {
    let result =
        parse("//@version=5\nindicator(\"X\")\nimport Example-Author/TextKit/6 as textLib\n");
    assert!(
        !has_errors(&result.diagnostics),
        "unexpected errors: {:?}",
        result.diagnostics
    );
    let dump = dump_program(&result.program);
    assert!(
        dump.contains("Import `Example-Author/TextKit/6` as `textLib`"),
        "{dump}"
    );
}

#[test]
fn parse_import_defaults_alias_to_library_name() {
    let result = parse("//@version=6\nindicator(\"X\")\nimport me/MathLib/1\n");
    assert!(
        !has_errors(&result.diagnostics),
        "unexpected errors: {:?}",
        result.diagnostics
    );
    let dump = dump_program(&result.program);
    assert!(
        dump.contains("Import `me/MathLib/1` as `MathLib`"),
        "{dump}"
    );
}

#[test]
fn export_is_a_contextual_identifier() {
    // A bare `export`/`import`/`library` used as an ordinary identifier still
    // parses — they are contextual, not reserved keywords (back-compat).
    for name in ["export", "import", "library"] {
        let src = format!("//@version=6\nindicator(\"X\")\n{name} = 1\nplot(close)\n");
        let result = parse(&src);
        assert!(
            !has_errors(&result.diagnostics),
            "`{name}` as an identifier unexpectedly failed: {:?}",
            result.diagnostics
        );
    }
}

// -- quoting, literals, line wrapping and parameter types --------------------

#[test]
fn single_quoted_strings_lex_like_double_quoted() {
    use TokenKind::*;
    // Pine accepts `'...'` and `"..."` interchangeably; the other quote is an
    // ordinary character inside the literal.
    assert_eq!(
        token_kinds("x = 'hello'\n"),
        vec![Ident, Eq, Str, Newline, Eof]
    );
    let result =
        parse("//@version=6\nindicator(\"X\")\ny = input.bool(true, 'Show')\nplot(close)\n");
    assert!(
        !has_errors(&result.diagnostics),
        "single-quoted arg should parse: {:?}",
        result.diagnostics
    );
}

#[test]
fn trailing_dot_and_exponent_float_literals() {
    use TokenKind::*;
    // `8.` is a valid float (equal to `8.0`); `10e10`/`2.5e-3` carry exponents.
    for src in ["x = 8.\n", "x = 10e10\n", "x = 2.5e-3\n", "x = 1E5\n"] {
        assert_eq!(
            token_kinds(src),
            vec![Ident, Eq, Number, Newline, Eof],
            "unexpected tokens for `{src}`"
        );
        let result = parse(src);
        assert!(
            !has_errors(&result.diagnostics),
            "`{src}` should parse: {:?}",
            result.diagnostics
        );
    }
    // An `e` not followed by an exponent stays an identifier boundary.
    assert_eq!(
        token_kinds("x = 1 exit\n"),
        vec![Ident, Eq, Number, Ident, Newline, Eof]
    );
}

#[test]
fn line_wrapping_continues_the_previous_statement() {
    // A continuation line indented by a non-block multiple joins the prior
    // logical line (the parser's line-wrapping rule), so a call split across
    // lines parses as one statement.
    let src = "//@version=6\nindicator(\"X\")\ny = plot(\n   close,\n   color=color.red)\n";
    let result = parse(src);
    assert!(
        !has_errors(&result.diagnostics),
        "wrapped call should parse as one statement: {:?}",
        result.diagnostics
    );
}

#[test]
fn parenthesized_line_wrapping_accepts_any_indentation() {
    // Inside parentheses, continuation lines may use any indentation. Cover a
    // block-width continuation, nested parentheses, a blank/comment line,
    // and a column-zero closing delimiter in one representative statement.
    let src = "//@version=6\r\nindicator(\"X\",\r\n    overlay = false,\r\n    max_bars_back = math.max(\r\n        10,\r\n        // Keep the comment attached to this argument.\r\n        20)\r\n)\r\nplot(close)\r\n";
    let result = parse(src);
    assert!(
        !has_errors(&result.diagnostics),
        "parenthesized wrapping should accept any indentation: {:?}",
        result.diagnostics
    );
}

#[test]
fn parenthesized_line_wrapping_preserves_enclosing_block_scope() {
    // The continuation is eight spaces (a block multiple) but belongs to the
    // open call, not a nested local block. The following four-space statement
    // must still be parsed as the function body's next statement.
    let src = "//@version=6\nindicator(\"X\")\nf(float x) =>\n    float y = math.max(\n        x,\n        0.0\n    )\n    y\nplot(f(close))\n";
    let result = parse(src);
    assert!(
        !has_errors(&result.diagnostics),
        "parenthesized wrapping must not alter the block stack: {:?}",
        result.diagnostics
    );
}

#[test]
fn four_space_continuation_outside_parentheses_remains_invalid() {
    let result =
        parse("//@version=6\nindicator(\"X\")\nfloat value = close +\n    open\nplot(value)\n");
    assert!(
        has_errors(&result.diagnostics),
        "a block-width continuation outside parentheses must remain invalid"
    );
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0110")),
        "expected E0110, got {:?}",
        result.diagnostics
    );
}

#[test]
fn block_if_expression_inside_parentheses_is_rejected() {
    // PineWASM rejects a multi-line `if` expression opened
    // inside a call's parentheses, reporting E0110. Its
    // parenthesized line wrapping swallows the block structure, so the body
    // joins the condition's logical line. Keep rejecting it rather than
    // restoring value blocks inside parentheses.
    let result =
        parse("//@version=6\nindicator(\"x\")\nplot(if close > open\n    1\nelse\n    0)\n");
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0110")),
        "expected E0110, got {:?}",
        result.diagnostics
    );
}

#[test]
fn typed_and_qualified_function_parameters_parse() {
    // Parameters may carry a type and/or a qualifier
    // (`series float`, `simple int`, `array<color>`, `matrix<float>`,
    // `map<string, float>`). Both are accepted; the
    // qualifier and type are both kept on the parameter.
    let src = "//@version=6\nlibrary(\"L\")\n\
        export f(series float _src, simple int n, array<color> shades, int[] xs, matrix<float> values, map<string, float> scores, chart.point[] anchors, shapes.Row[] rows, x) =>\n    _src\n";
    let result = parse(src);
    assert!(
        !has_errors(&result.diagnostics),
        "typed/qualified params should parse: {:?}",
        result.diagnostics
    );
    let pinewasm_syntax::Item::FuncDef(function) = &result.program.items[1] else {
        panic!("expected function definition");
    };
    use pinewasm_syntax::ParamQualifier;
    assert_eq!(function.params[0].qualifier, Some(ParamQualifier::Series));
    assert_eq!(function.params[1].qualifier, Some(ParamQualifier::Simple));
    assert_eq!(function.params[2].qualifier, None);
    assert_eq!(function.params[3].qualifier, None);
    assert_eq!(
        function.params[4].ty.as_ref().unwrap().describe(),
        "matrix<float>"
    );
    assert_eq!(
        function.params[5].ty.as_ref().unwrap().describe(),
        "map<string, float>"
    );
    assert_eq!(
        function.params[6].ty.as_ref().unwrap().describe(),
        "array<chart.point>"
    );
    assert_eq!(
        function.params[7].ty.as_ref().unwrap().describe(),
        "array<shapes.Row>"
    );
    assert_eq!(function.params[8].qualifier, None);
    // A parameter literally named after a qualifier still works.
    let result =
        parse("//@version=6\nindicator(\"X\")\nf(series) =>\n    series\nplot(f(close))\n");
    assert!(
        !has_errors(&result.diagnostics),
        "a param named `series` should still parse: {:?}",
        result.diagnostics
    );
}

#[test]
fn nested_array_and_bracket_sugar_field_types() {
    // `array<color>` (non-scalar element) and the `T[]` sugar in type fields.
    let src = "//@version=6\nindicator(\"X\")\ntype T\n    array<color> shades\n    float[] xs\nplot(close)\n";
    let result = parse(src);
    assert!(
        !has_errors(&result.diagnostics),
        "nested/sugar array field types should parse: {:?}",
        result.diagnostics
    );
}

#[test]
fn udt_typed_variable_annotation_parses() {
    // `TypeName var = TypeName.new(...)` — a user-type variable annotation
    // (beyond the drawing types), disambiguated from a call by the `IDENT =`.
    let src =
        "//@version=6\nindicator(\"X\")\ntype Pt\n    float v\nPt p = Pt.new(1.0)\nplot(p.v)\n";
    let result = parse(src);
    assert!(
        !has_errors(&result.diagnostics),
        "UDT-typed declaration should parse: {:?}",
        result.diagnostics
    );
    // A bare call to a same-named constructor is NOT a declaration.
    let result = parse("//@version=6\nindicator(\"X\")\ntype Pt\n    float v\nlabel.new(bar_index, high)\nplot(close)\n");
    assert!(
        !has_errors(&result.diagnostics),
        "a bare constructor call should still parse as a statement: {:?}",
        result.diagnostics
    );
}

#[test]
fn compound_assignment_desugars_to_reassignment() {
    // `x += e` desugars to `x := x + e` (and the other four operators).
    for (op, sym) in [
        ("+=", "+"),
        ("-=", "-"),
        ("*=", "*"),
        ("/=", "/"),
        ("%=", "%"),
    ] {
        let src = format!("//@version=6\nindicator(\"X\")\nvar x = 0.0\nx {op} close\nplot(x)\n");
        let result = parse(&src);
        assert!(
            !has_errors(&result.diagnostics),
            "`x {op} close` should parse: {:?}",
            result.diagnostics
        );
        let dump = dump_program(&result.program);
        assert!(
            dump.contains(&format!("Binary `{sym}`")),
            "`{op}` should desugar to a `{sym}` binary op:\n{dump}"
        );
    }
}

#[test]
fn contextual_type_words_are_names_without_losing_declarations() {
    let source = r#"//@version=6
indicator("Contextual names")
type = input.string("SMA")
bool = close > open
var int = 1
int += 1
f(type, bool) => type == "SMA" and bool
bool typed = f(type, bool)
plot(typed ? float(int) : na)
type Point
    float value
p = Point.new(close)
"#;
    let parsed = parse(source);
    assert!(!has_errors(&parsed.diagnostics), "{:?}", parsed.diagnostics);
}

// -- syntax that ordinary Pine scripts use and the parser accepts ------------

/// Parse `source`, assert it has no error diagnostics, and return the AST dump.
fn parse_ok(source: &str) -> String {
    let result = parse(source);
    assert!(
        !has_errors(&result.diagnostics),
        "should parse: {source:?}\n{:?}",
        result.diagnostics
    );
    dump_program(&result.program)
}

/// The top-level statements of `source`, one dump line each (the first line
/// of each item's dump), for order-sensitive shape checks.
fn item_heads(source: &str) -> Vec<String> {
    let result = parse(source);
    assert!(
        !has_errors(&result.diagnostics),
        "should parse: {source:?}\n{:?}",
        result.diagnostics
    );
    result
        .program
        .items
        .iter()
        .map(|item| {
            let program = pinewasm_syntax::Program {
                version: None,
                strategy_alert_message: None,
                items: vec![item.clone()],
                span: result.program.span,
            };
            // Line 0 is `Program`, line 1 `Version`, line 2 the item.
            dump_program(&program)
                .lines()
                .nth(2)
                .unwrap_or_default()
                .trim()
                .to_string()
        })
        .collect()
}

#[test]
fn comma_separates_statements_on_one_line() {
    let heads = item_heads("a = 1, b = 2\nx := a, y := b\nplot(a), plot(b)\n");
    assert_eq!(heads.len(), 6, "{heads:#?}");
    assert!(heads[0].starts_with("Assign `a`"), "{heads:#?}");
    assert!(heads[1].starts_with("Assign `b`"), "{heads:#?}");
    assert!(heads[2].starts_with("Reassign `x`"), "{heads:#?}");
    assert!(heads[3].starts_with("Reassign `y`"), "{heads:#?}");
    assert!(heads[4].starts_with("ExprStmt"), "{heads:#?}");
    assert!(heads[5].starts_with("ExprStmt"), "{heads:#?}");
}

#[test]
fn comma_joins_annotated_and_var_declarations() {
    let heads = item_heads("float p = 0.0, float q = 1.0\nvar int n = 0, var line l = na\n");
    assert_eq!(heads.len(), 4, "{heads:#?}");
}

#[test]
fn trailing_comma_ends_the_statement() {
    let heads = item_heads("a = 1,\nb = 2\nif a > 0\n    b := 3,\nplot(b)\n");
    assert_eq!(heads.len(), 4, "{heads:#?}");
}

#[test]
fn comma_separated_statements_in_blocks_and_inline_bodies() {
    let dump = parse_ok("if close > open\n    a = 1, b = 2\n");
    assert!(
        dump.contains("Assign `a`") && dump.contains("Assign `b`"),
        "{dump}"
    );
    // A single-line function body keeps every statement; the last is the value.
    let dump = parse_ok("f(x) => y = x * 2, y + 1\n");
    let y = dump.find("Assign `y`").expect(&dump);
    let add = dump.find("Binary `+`").expect(&dump);
    assert!(y < add, "{dump}");
    // A switch arm's inline body likewise.
    let dump = parse_ok("switch\n    close > open => x := 1, y := 2\n    => x := 3\n");
    assert!(dump.contains("Reassign `y`"), "{dump}");
    // Call statements in a loop nested in a multi-line function body.
    let dump = parse_ok(
        "f() =>\n    while array.size(a) > 20\n        array.shift(a), array.shift(b)\n    \
         while array.size(c) > 20\n        array.shift(c), array.shift(d)\n",
    );
    assert_eq!(dump.matches("While").count(), 2, "{dump}");
    assert_eq!(dump.matches("ExprStmt").count(), 4, "{dump}");
}

#[test]
fn comma_does_not_end_declaration_lines() {
    // Declarations and type fields still end at the line end.
    let result = parse("indicator(\"x\"), a = 1\n");
    assert!(has_errors(&result.diagnostics));
    let result = parse("type T\n    int a, int b\n");
    assert!(has_errors(&result.diagnostics));
}

#[test]
fn multiline_triple_quoted_strings_keep_lines() {
    let result = parse("s = \"\"\"\nHello\n    world!\n\"\"\"\nt = '''a \"b\" c'''\n");
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    let values: Vec<String> = result
        .program
        .items
        .iter()
        .filter_map(|item| match item {
            pinewasm_syntax::Item::Stmt(pinewasm_syntax::Stmt::Assign {
                value: pinewasm_syntax::Expr::Str { value, .. },
                ..
            }) => Some(value.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(values, vec!["\nHello\n    world!\n", "a \"b\" c"]);
}

#[test]
fn unterminated_triple_quoted_string_is_e0102() {
    let result = lex("s = \"\"\"never closed\nx = 1\n");
    assert_eq!(result.diagnostics[0].code.as_deref(), Some("E0102"));
}

#[test]
fn duplicate_version_directive_warns_w0005() {
    let result = parse("//@version=6\n//@version=5\nx = 1\n");
    assert!(!has_errors(&result.diagnostics), "{:?}", result.diagnostics);
    assert_eq!(result.diagnostics[0].code.as_deref(), Some("W0005"));
    assert_eq!(result.program.version.map(|v| v.version), Some(6));
}

#[test]
fn for_loop_counter_may_declare_int() {
    let dump = parse_ok("s = 0\nfor int i = 0 to 3\n    s += i\n");
    assert!(dump.contains("For `i`"), "{dump}");
}

#[test]
fn method_is_a_name_outside_method_definitions() {
    let dump = parse_ok(concat!(
        "method = input.string(\"a\")\n",
        "f(simple string method) => method == \"a\"\n",
        "method twice(int x) => x * 2\n",
    ));
    assert!(dump.contains("Assign `method`"), "{dump}");
    assert!(dump.contains("FuncDef `f`(method)"), "{dump}");
    assert!(dump.contains("Method `twice`[int](x)"), "{dump}");
}

#[test]
fn method_receiver_may_have_a_qualifier_and_default() {
    let dump = parse_ok("method m(simple string s) => s\nmethod n(series float x = 0) => x\n");
    assert!(dump.contains("Method `m`[string](s)"), "{dump}");
    assert!(dump.contains("Method `n`[float](x=…)"), "{dump}");
}

#[test]
fn dotted_reassign_may_target_a_type_field() {
    let dump = parse_ok("zone.type := 1\n");
    assert!(dump.contains("Reassign `zone.type`"), "{dump}");
}

#[test]
fn var_declaration_may_name_a_type_keyword() {
    let dump = parse_ok("var int = 1\nvarip string = \"a\"\nvar float x = 0.5\n");
    assert!(dump.contains("VarAssign `int`"), "{dump}");
    assert!(dump.contains("VaripAssign `string`"), "{dump}");
    assert!(dump.contains("VarAssign `x` : float"), "{dump}");
}
