use pinewasm_parse::{lex, parse};
use pinewasm_syntax::{Item, Stmt, TokenKind};

#[test]
fn contextual_once_retains_optional_condition_and_body() {
    let source =
        "//@version=6\nindicator(\"Once parse\")\nonce\n    x=1\nonce close>open\n    y=2\n";
    let parsed = parse(source);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let statements: Vec<_> = parsed
        .program
        .items
        .iter()
        .filter_map(|item| {
            if let Item::Stmt(Stmt::Once(once)) = item {
                Some(once)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(statements.len(), 2);
    assert!(statements[0].cond.is_none());
    assert!(statements[1].cond.is_some());
    assert_eq!(statements[0].body.len(), 1);
    assert!(
        source[statements[0].span.start as usize..statements[0].span.end as usize]
            .starts_with("once\n")
    );
    assert!(lex("once")
        .tokens
        .iter()
        .any(|token| token.kind == TokenKind::Ident));
}

#[test]
fn once_requires_an_indented_effect_block_and_is_not_an_expression() {
    for source in [
        "once true\nplot(close)\n",
        "x=once true\n    close\n",
        "once true plot(close)\n",
    ] {
        assert!(!parse(source).diagnostics.is_empty(), "accepted {source}");
    }
}

#[test]
fn once_remains_an_identifier_in_v5_and_v6() {
    for version in [5, 6] {
        for body in [
            "once=1\nplot(once)",
            "f(int once) =>\n    once\nplot(f(1))",
            "once(int x) => x+1\nplot(once(1))",
            "type T\n    int once\nt=T.new(1)\nt.once+=1\nplot(t.once)",
        ] {
            let source = format!("//@version={version}\nindicator(\"Contextual once\")\n{body}\n");
            let parsed = parse(&source);
            assert!(
                parsed.diagnostics.is_empty(),
                "{source}: {:?}",
                parsed.diagnostics
            );
        }
    }
}

#[test]
fn once_block_gate_uses_the_existing_resolved_script_version() {
    for source in [
        "//@version=5\nonce true\n    x=1\n",
        "once true\n    x=1\n//@version=5\n",
        "//@version=5\n//@version=6\nonce true\n    x=1\n",
    ] {
        let parsed = parse(source);
        assert!(
            parsed
                .diagnostics
                .iter()
                .any(|d| d.code.as_deref() == Some("E0110")
                    && d.message.contains("require version 6")),
            "{source}: {:?}",
            parsed.diagnostics
        );
    }
    let parsed = parse("//@version=6\n//@version=5\nonce true\n    x=1\n");
    assert!(
        parsed.diagnostics.iter().all(|d| !d.is_error()),
        "{:?}",
        parsed.diagnostics
    );
}

#[test]
fn contextual_header_lookahead_preserves_assignments_and_nested_call_arguments() {
    let source = "//@version=6\nonce=if true\n    1\nelse\n    2\nonce+=1\nvalue=once+1, other=once\nf(bool a, bool b) => a and b\nonce f(a=true,b=true)\n    local=1\n";
    let parsed = parse(source);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    assert_eq!(
        parsed
            .program
            .items
            .iter()
            .filter(|item| matches!(item, Item::Stmt(Stmt::Once(_))))
            .count(),
        1
    );
}
