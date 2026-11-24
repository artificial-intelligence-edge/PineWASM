use pinewasm_lsp::{analysis::DocumentAnalysis, query::checked_rename};

fn assert_contextual_navigation(source: &str) {
    let analysis = DocumentAnalysis::new("once.pine", source, Vec::new());
    assert!(analysis.success(), "{:?}", analysis.diagnostics());
    let declaration = source.find("once=").unwrap() as u32;
    let header = source.find("once once").unwrap() as u32;
    let condition = header + 5;
    let symbol = analysis.local_symbol_at(declaration).unwrap();
    assert!(analysis.identifier_at(header).is_none());
    assert!(analysis.local_symbol_at(header).is_none());
    let expected = vec![symbol.span, analysis.identifier_at(condition).unwrap().1];
    assert_eq!(analysis.references("once"), expected);
    assert_eq!(analysis.references_to(symbol), expected);
    assert_eq!(
        checked_rename(&analysis, declaration, "ready").unwrap(),
        expected
    );
    assert!(checked_rename(&analysis, header, "ready").is_err());
}

#[test]
fn once_header_is_excluded_from_same_named_identifier_references_and_rename() {
    assert_contextual_navigation("//@version=6\nindicator(\"Contextual Once navigation\")\nonce=bar_index>=2\nvar int n=0\nonce once\n    n+=1\nplot(n)\n");
}

#[test]
fn once_header_inside_a_value_block_is_also_excluded() {
    assert_contextual_navigation("//@version=6\nindicator(\"Once value block navigation\")\nonce=bar_index>=2\nvar int n=0\nx=if true\n    once once\n        n+=1\n    n\nelse\n    0\nplot(x)\n");
}
