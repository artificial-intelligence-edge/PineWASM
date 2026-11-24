//! The expression tree the parser builds is never taller than
//! `MAX_EXPR_DEPTH`, however the source spells it.
//!
//! Nested parentheses were always bounded by the recursion-depth guard. A
//! left-associative operator chain and a postfix chain are built by loops
//! that wrap the tree so far without recursing, so they need their own
//! accounting; without it `a + a + ...` (500 terms) overflowed the
//! stack of the passes that walk the tree after the parser.

use crate::parse;
use crate::parser::MAX_EXPR_DEPTH;
use pinewasm_span::{has_errors, Diagnostic};
use pinewasm_syntax::{Expr, Item, Stmt};
use proptest::prelude::*;

/// Well past the 500 terms that overflow the stack without the height cap.
const LONG: usize = 10_000;

/// The value of `x = <expr>`, or the diagnostics when it does not parse.
fn value_of(expr: &str) -> Result<Expr, Vec<Diagnostic>> {
    let mut result = parse(&format!("x = {expr}\n"));
    if has_errors(&result.diagnostics) {
        return Err(result.diagnostics);
    }
    assert_eq!(result.program.items.len(), 1, "{expr}");
    match result.program.items.remove(0) {
        Item::Stmt(Stmt::Assign { value, .. }) => Ok(value),
        other => panic!("expected an assignment, got {other:?}"),
    }
}

/// The height of `expr` in nodes (a leaf is 1). It uses an explicit stack so
/// that a tree the parser failed to bound fails the assertion that follows
/// instead of overflowing this test. Statement bodies of `if`, `switch` and
/// loop expressions are not descended into: block nesting has its own cap.
fn height(expr: &Expr) -> usize {
    let mut tallest = 0;
    let mut pending = vec![(expr, 1usize)];
    while let Some((node, depth)) = pending.pop() {
        tallest = tallest.max(depth);
        let below = depth + 1;
        match node {
            Expr::Unary { operand, .. } => pending.push((operand, below)),
            Expr::Binary { lhs, rhs, .. } => {
                pending.push((lhs, below));
                pending.push((rhs, below));
            }
            Expr::Ternary {
                cond,
                then_e,
                else_e,
                ..
            } => {
                pending.push((cond, below));
                pending.push((then_e, below));
                pending.push((else_e, below));
            }
            Expr::Call { args, .. } => {
                pending.extend(args.iter().map(|arg| (&arg.value, below)));
            }
            Expr::MethodCall { receiver, args, .. } => {
                pending.push((receiver, below));
                pending.extend(args.iter().map(|arg| (&arg.value, below)));
            }
            Expr::FieldAccess { receiver, .. } => pending.push((receiver, below)),
            Expr::History { target, offset, .. } => {
                pending.push((target, below));
                pending.push((offset, below));
            }
            Expr::Group { inner, .. } => pending.push((inner, below)),
            Expr::Tuple { elements, .. } => {
                pending.extend(elements.iter().map(|element| (element, below)));
            }
            _ => {}
        }
    }
    tallest
}

fn assert_too_deep(expr: &str, what: &str) {
    let diagnostics = match value_of(expr) {
        Ok(value) => panic!("{what}: accepted a tree {} nodes tall", height(&value)),
        Err(diagnostics) => diagnostics,
    };
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("expression is nested too deeply")),
        "{what}: expected the depth diagnostic, got {diagnostics:?}"
    );
}

fn assert_bounded(expr: &str, what: &str) -> usize {
    let value = value_of(expr).unwrap_or_else(|d| panic!("{what}: {d:?}"));
    let tall = height(&value);
    assert!(
        tall <= MAX_EXPR_DEPTH as usize,
        "{what}: tree is {tall} nodes tall"
    );
    tall
}

fn chain(term: &str, operator: &str, terms: usize) -> String {
    vec![term; terms].join(operator)
}

#[test]
fn long_chain_of_each_binary_operator_is_refused() {
    for operator in [
        "+", "-", "*", "/", "%", "==", "!=", "<", "<=", ">", ">=", "and", "or",
    ] {
        let source = chain("a", &format!(" {operator} "), LONG);
        assert_too_deep(&source, operator);
    }
}

#[test]
fn long_mixed_operator_chains_are_refused() {
    // Tighter operators nest inside looser ones, so the tree still grows by
    // one level per term of the loosest operator.
    let arithmetic = chain("a * a - a / a % a", " + ", LONG);
    assert_too_deep(&arithmetic, "mixed arithmetic");
    let logic = chain("a > a and not a or a == a", " or ", LONG);
    assert_too_deep(&logic, "mixed logic");
    let strings = chain("\"a\" + str.tostring(a)", " + ", LONG);
    assert_too_deep(&strings, "string concatenation");
}

#[test]
fn long_postfix_chains_are_refused() {
    assert_too_deep(&format!("a{}", ".b()".repeat(LONG)), "method chain");
    assert_too_deep(&format!("f(){}", ".b".repeat(LONG)), "field chain");
    assert_too_deep(&format!("a{}", "[1]".repeat(LONG)), "history chain");
    assert_too_deep(
        &format!("a{}", ".b(a)[1].c".repeat(LONG)),
        "mixed postfix chain",
    );
}

#[test]
fn long_right_nested_chains_are_refused() {
    assert_too_deep(&format!("{}a", "a ? a : ".repeat(LONG)), "ternary chain");
    assert_too_deep(&format!("{}a", "- ".repeat(LONG)), "prefix chain");
    assert_too_deep(&format!("{}a", "not ".repeat(LONG)), "not chain");
}

#[test]
fn a_chain_is_accepted_up_to_the_height_cap_and_no_further() {
    // `x = <chain>` starts its operands two recursion levels down (ternary,
    // then binding power), so the tallest chain it can hold is 198 terms.
    let longest = MAX_EXPR_DEPTH as usize - 2;
    for (term, link) in [("a", " + "), ("a", " and "), ("\"s\"", " + ")] {
        let tall = assert_bounded(&chain(term, link, longest), link);
        assert_eq!(tall, longest, "{link}");
        assert_too_deep(&chain(term, link, longest + 1), link);
    }
    // The first `.b()` folds into the dotted call `a.b()`, a leaf.
    let tall = assert_bounded(&format!("a{}", ".b()".repeat(longest)), "methods");
    assert_eq!(tall, longest);
    assert_too_deep(&format!("a{}", ".b()".repeat(longest + 1)), "methods");
}

#[test]
fn chains_inside_chains_add_up() {
    // Each of these stays under the cap on its own axis, but without the
    // height check they would be accepted and give trees thousands of nodes tall.
    let mut indexed = String::from("a");
    for _ in 0..50 {
        indexed = format!("({indexed}{})", "[1]".repeat(90));
    }
    assert_too_deep(&indexed, "history chains inside groups");

    let mut summed = String::from("a");
    for _ in 0..50 {
        summed = format!("({summed}{})", " + a".repeat(90));
    }
    assert_too_deep(&summed, "sums inside groups");

    // A chain whose last operand is itself tall: the taller side counts.
    let nested = format!(
        "{} + {}a{}",
        chain("a", " + ", 120),
        "(".repeat(60),
        ")".repeat(60)
    );
    assert_bounded(&nested, "short chain, deep operand");
    let argument = format!("f({}){}", chain("a", " + ", 150), ".b()".repeat(100));
    assert_too_deep(&argument, "method chain off a call with a tall argument");
}

#[test]
fn wide_expressions_are_not_refused() {
    // The cap is on height. Width (many arguments, many elements, many short
    // operands) costs no stack in the passes that follow.
    let arguments = format!("f({})", chain("a + a * a", ", ", LONG));
    assert!(assert_bounded(&arguments, "argument list") < 10);
    let named = format!("f({})", chain("p = a.b()", ", ", LONG));
    assert!(assert_bounded(&named, "named argument list") < 10);
    let elements = format!("[{}]", chain("a[1]", ", ", LONG));
    assert!(assert_bounded(&elements, "tuple") < 10);
    let grouped = chain(&format!("f({})", chain("a", " + ", 100)), " + ", 90);
    assert_bounded(&grouped, "sum of calls");
}

/// Expression source with long chains at every level, so that many cases
/// land on both sides of the cap.
fn chained_source() -> impl Strategy<Value = String> {
    let leaf = prop::sample::select(vec!["a", "1", "true", "f()", "\"s\""]).prop_map(String::from);
    leaf.prop_recursive(5, 96, 4, |inner| {
        let link = prop::sample::select(vec![" + ", " * ", " and ", " or ", " == ", " - "]);
        let suffix = prop::sample::select(vec![".b()", "[1]", ".c", ".d(a)", ".e(p = 1)"]);
        prop_oneof![
            (inner.clone(), link, inner.clone(), 1usize..150).prop_map(
                |(first, link, rest, count)| format!(
                    "{first}{}",
                    format!("{link}{rest}").repeat(count)
                )
            ),
            (inner.clone(), suffix, 1usize..150).prop_map(|(receiver, suffix, count)| format!(
                "{receiver}{}",
                suffix.repeat(count)
            )),
            (inner.clone(), 1usize..40).prop_map(|(e, count)| format!(
                "{}{e}{}",
                "(".repeat(count),
                ")".repeat(count)
            )),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("f({a}, q = {b})")),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("{a}.m({b})")),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("{a}[{b}]")),
            (inner.clone(), inner.clone(), inner.clone())
                .prop_map(|(c, t, e)| format!("{c} ? {t} : {e}")),
            (inner.clone(), 1usize..60).prop_map(|(e, count)| format!("{}{e}", "- ".repeat(count))),
            inner.prop_map(|e| format!("[{e}, {e}]")),
        ]
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Whatever parses is at most `MAX_EXPR_DEPTH` nodes tall.
    #[test]
    fn accepted_trees_never_exceed_the_cap(source in chained_source()) {
        if let Ok(value) = value_of(&source) {
            let tall = height(&value);
            prop_assert!(tall <= MAX_EXPR_DEPTH as usize, "{} nodes tall: {}", tall, source);
        }
    }
}
