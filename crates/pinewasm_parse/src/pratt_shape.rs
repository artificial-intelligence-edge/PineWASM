//! A pure Pratt "shape" parser over token kinds.
//!
//! It mirrors the parser's expression core (`parse_ternary` ->
//! `parse_expr_bp` -> `parse_unary` -> `parse_primary`) on the operator
//! fragment: ternary `?:`, the binary operators and prefix operators of
//! [`crate::precedence`], parentheses, and atoms (`Ident`, `Number`, `true`,
//! `false`). Instead of an AST it emits the post-order (reverse Polish)
//! sequence of node kinds, which determines the tree shape uniquely. It
//! reads only token kinds: no source text, spans or diagnostics.
//!
//! It is not on the production path. It exists as an extraction target for
//! the Charon/Aeneas extraction (`aeneas` feature) and is tied to the real parser
//! by the differential tests below, which compare its output with the tree
//! [`crate::parse`] builds for the same tokens. It is written without loops,
//! `?` or iterator adapters (the binary loop is a tail-recursive function) to
//! stay inside the subset Aeneas accepts.

// Explicit `match ... None => return None` instead of `?`: `?` lowers to the
// `Try`/`FromResidual` trait machinery, which the extraction would have to
// model; a plain match translates directly.
#![allow(clippy::question_mark)]

use crate::precedence::{infix_binding_power, prefix_binding_power};
use pinewasm_syntax::{BinaryOp, TokenKind, UnaryOp};

/// Nesting bound; equal to the parser's `MAX_EXPR_DEPTH` and counted the
/// same way (one level per ternary and per binding-power recursion).
pub const SHAPE_MAX_DEPTH: u32 = 200;

/// One node of an expression tree, without its children (post-order).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeNode {
    /// A leaf: identifier, number, `true` or `false`.
    Atom,
    /// A parenthesized expression (`Expr::Group`), one child.
    Group,
    /// A prefix operator, one child.
    Unary(UnaryOp),
    /// A binary operator, two children (lhs then rhs).
    Binary(BinaryOp),
    /// `cond ? then : else`, three children.
    Ternary,
}

/// The token at `pos`, or `Eof` past the end of `toks`.
fn kind_at(toks: &[TokenKind], pos: usize) -> TokenKind {
    if pos < toks.len() {
        toks[pos]
    } else {
        TokenKind::Eof
    }
}

/// Parse all of `toks` as one expression, appending its post-order shape to
/// `out`. Returns `false` (with `out` holding a partial shape) when `toks` is
/// not exactly one expression of the fragment, or nests deeper than
/// [`SHAPE_MAX_DEPTH`].
pub fn pratt_shape(toks: &[TokenKind], out: &mut Vec<ShapeNode>) -> bool {
    // Not named `end`: Aeneas escapes it as `«end»` in its output and then
    // misindents the following line (it counts the guillemets as two columns each).
    match shape_ternary(toks, 0, 0, out) {
        Some(stop) => stop == toks.len(),
        None => false,
    }
}

/// `parse_ternary`: the depth guard, then one level deeper.
fn shape_ternary(
    toks: &[TokenKind],
    pos: usize,
    depth: u32,
    out: &mut Vec<ShapeNode>,
) -> Option<usize> {
    if depth >= SHAPE_MAX_DEPTH {
        return None;
    }
    shape_ternary_inner(toks, pos, depth + 1, out)
}

/// `parse_ternary_inner`: `cond [? then : else]`, right-associative.
fn shape_ternary_inner(
    toks: &[TokenKind],
    pos: usize,
    depth: u32,
    out: &mut Vec<ShapeNode>,
) -> Option<usize> {
    let after_cond = match shape_expr_bp(toks, pos, depth, 0, out) {
        Some(p) => p,
        None => return None,
    };
    if kind_at(toks, after_cond) != TokenKind::Question {
        return Some(after_cond);
    }
    let after_then = match shape_ternary(toks, after_cond + 1, depth, out) {
        Some(p) => p,
        None => return None,
    };
    if kind_at(toks, after_then) != TokenKind::Colon {
        return None;
    }
    let after_else = match shape_ternary(toks, after_then + 1, depth, out) {
        Some(p) => p,
        None => return None,
    };
    out.push(ShapeNode::Ternary);
    Some(after_else)
}

/// `parse_expr_bp`: the depth guard, then one level deeper.
fn shape_expr_bp(
    toks: &[TokenKind],
    pos: usize,
    depth: u32,
    min_bp: u8,
    out: &mut Vec<ShapeNode>,
) -> Option<usize> {
    if depth >= SHAPE_MAX_DEPTH {
        return None;
    }
    let after_lhs = match shape_unary(toks, pos, depth + 1, out) {
        Some(p) => p,
        None => return None,
    };
    shape_infix(toks, after_lhs, depth + 1, min_bp, out)
}

/// One iteration of `parse_expr_bp_inner`'s binary loop, then the rest of
/// the loop by tail recursion.
fn shape_infix(
    toks: &[TokenKind],
    pos: usize,
    depth: u32,
    min_bp: u8,
    out: &mut Vec<ShapeNode>,
) -> Option<usize> {
    match infix_binding_power(kind_at(toks, pos)) {
        None => Some(pos),
        Some((op, l_bp, r_bp)) => {
            if l_bp < min_bp {
                return Some(pos);
            }
            let after_rhs = match shape_expr_bp(toks, pos + 1, depth, r_bp, out) {
                Some(p) => p,
                None => return None,
            };
            out.push(ShapeNode::Binary(op));
            shape_infix(toks, after_rhs, depth, min_bp, out)
        }
    }
}

/// `parse_unary`: a prefix operator, else a primary.
fn shape_unary(
    toks: &[TokenKind],
    pos: usize,
    depth: u32,
    out: &mut Vec<ShapeNode>,
) -> Option<usize> {
    match prefix_binding_power(kind_at(toks, pos)) {
        None => shape_primary(toks, pos, depth, out),
        Some((op, r_bp)) => {
            let after_operand = match shape_expr_bp(toks, pos + 1, depth, r_bp, out) {
                Some(p) => p,
                None => return None,
            };
            out.push(ShapeNode::Unary(op));
            Some(after_operand)
        }
    }
}

/// `parse_primary` on the fragment: an atom or `( expr )`.
fn shape_primary(
    toks: &[TokenKind],
    pos: usize,
    depth: u32,
    out: &mut Vec<ShapeNode>,
) -> Option<usize> {
    match kind_at(toks, pos) {
        TokenKind::Ident | TokenKind::Number | TokenKind::KwTrue | TokenKind::KwFalse => {
            out.push(ShapeNode::Atom);
            Some(pos + 1)
        }
        TokenKind::LParen => {
            let after_inner = match shape_ternary(toks, pos + 1, depth, out) {
                Some(p) => p,
                None => return None,
            };
            if kind_at(toks, after_inner) != TokenKind::RParen {
                return None;
            }
            out.push(ShapeNode::Group);
            Some(after_inner + 1)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{pratt_shape, ShapeNode, SHAPE_MAX_DEPTH};
    use crate::parse;
    use pinewasm_span::has_errors;
    use pinewasm_syntax::{BinaryOp, Expr, Item, Stmt, TokenKind, UnaryOp};
    use proptest::prelude::*;

    const _: () = assert!(SHAPE_MAX_DEPTH == crate::parser::MAX_EXPR_DEPTH);

    /// Source text for one token of the fragment's alphabet.
    fn text(kind: TokenKind) -> &'static str {
        match kind {
            TokenKind::Ident => "a",
            TokenKind::Number => "1",
            TokenKind::KwTrue => "true",
            TokenKind::KwFalse => "false",
            TokenKind::KwAnd => "and",
            TokenKind::KwOr => "or",
            TokenKind::KwNot => "not",
            TokenKind::Plus => "+",
            TokenKind::Minus => "-",
            TokenKind::Star => "*",
            TokenKind::Slash => "/",
            TokenKind::Percent => "%",
            TokenKind::Lt => "<",
            TokenKind::Le => "<=",
            TokenKind::Gt => ">",
            TokenKind::Ge => ">=",
            TokenKind::EqEq => "==",
            TokenKind::BangEq => "!=",
            TokenKind::Question => "?",
            TokenKind::Colon => ":",
            TokenKind::LParen => "(",
            TokenKind::RParen => ")",
            other => panic!("not in the test alphabet: {other:?}"),
        }
    }

    const ALPHABET: &[TokenKind] = &[
        TokenKind::Ident,
        TokenKind::Number,
        TokenKind::KwTrue,
        TokenKind::KwFalse,
        TokenKind::KwAnd,
        TokenKind::KwOr,
        TokenKind::KwNot,
        TokenKind::Plus,
        TokenKind::Minus,
        TokenKind::Star,
        TokenKind::Slash,
        TokenKind::Percent,
        TokenKind::Lt,
        TokenKind::Le,
        TokenKind::Gt,
        TokenKind::Ge,
        TokenKind::EqEq,
        TokenKind::BangEq,
        TokenKind::Question,
        TokenKind::Colon,
        TokenKind::LParen,
        TokenKind::RParen,
    ];

    /// Post-order shape of a real AST, or `None` if it leaves the fragment.
    fn expr_shape(e: &Expr, out: &mut Vec<ShapeNode>) -> bool {
        match e {
            Expr::Int { .. } | Expr::Float { .. } | Expr::Bool { .. } | Expr::Ident(_) => {
                out.push(ShapeNode::Atom);
                true
            }
            Expr::Group { inner, .. } => {
                let ok = expr_shape(inner, out);
                out.push(ShapeNode::Group);
                ok
            }
            Expr::Unary { op, operand, .. } => {
                let ok = expr_shape(operand, out);
                out.push(ShapeNode::Unary(*op));
                ok
            }
            Expr::Binary { op, lhs, rhs, .. } => {
                let ok = expr_shape(lhs, out) && expr_shape(rhs, out);
                out.push(ShapeNode::Binary(*op));
                ok
            }
            Expr::Ternary {
                cond,
                then_e,
                else_e,
                ..
            } => {
                let ok =
                    expr_shape(cond, out) && expr_shape(then_e, out) && expr_shape(else_e, out);
                out.push(ShapeNode::Ternary);
                ok
            }
            _ => false,
        }
    }

    /// What the real parser makes of `x = <toks>`: the RHS shape when the
    /// program parses cleanly to that single assignment and its RHS stays
    /// inside the fragment; `None` otherwise.
    fn real_shape(toks: &[TokenKind]) -> Option<Vec<ShapeNode>> {
        let rhs: Vec<&str> = toks.iter().map(|&k| text(k)).collect();
        let source = format!("x = {}\n", rhs.join(" "));
        let result = parse(&source);
        if has_errors(&result.diagnostics) || result.program.items.len() != 1 {
            return None;
        }
        let Item::Stmt(Stmt::Assign { value, .. }) = &result.program.items[0] else {
            return None;
        };
        let mut out = Vec::new();
        expr_shape(value, &mut out).then_some(out)
    }

    fn model_shape(toks: &[TokenKind]) -> Option<Vec<ShapeNode>> {
        let mut out = Vec::new();
        pratt_shape(toks, &mut out).then_some(out)
    }

    fn assert_agree(toks: &[TokenKind]) {
        assert_eq!(model_shape(toks), real_shape(toks), "tokens: {toks:?}");
    }

    #[test]
    fn precedence_and_associativity_examples() {
        use BinaryOp::*;
        use ShapeNode::{Atom, Binary, Group, Ternary, Unary};
        use TokenKind as K;
        // a - a - a  ==  (a - a) - a
        assert_eq!(
            model_shape(&[K::Ident, K::Minus, K::Ident, K::Minus, K::Ident]),
            Some(vec![Atom, Atom, Binary(Sub), Atom, Binary(Sub)])
        );
        // a + a * a  ==  a + (a * a)
        assert_eq!(
            model_shape(&[K::Ident, K::Plus, K::Ident, K::Star, K::Ident]),
            Some(vec![Atom, Atom, Atom, Binary(Mul), Binary(Add)])
        );
        // not a > a  ==  not (a > a)
        assert_eq!(
            model_shape(&[K::KwNot, K::Ident, K::Gt, K::Ident]),
            Some(vec![Atom, Atom, Binary(Gt), Unary(UnaryOp::Not)])
        );
        // -a * a  ==  (-a) * a
        assert_eq!(
            model_shape(&[K::Minus, K::Ident, K::Star, K::Ident]),
            Some(vec![Atom, Unary(UnaryOp::Neg), Atom, Binary(Mul)])
        );
        // a ? a : a ? a : a  ==  a ? a : (a ? a : a)
        assert_eq!(
            model_shape(&[
                K::Ident,
                K::Question,
                K::Ident,
                K::Colon,
                K::Ident,
                K::Question,
                K::Ident,
                K::Colon,
                K::Ident
            ]),
            Some(vec![Atom, Atom, Atom, Atom, Atom, Ternary, Ternary])
        );
        // (a or a) and a
        assert_eq!(
            model_shape(&[
                K::LParen,
                K::Ident,
                K::KwOr,
                K::Ident,
                K::RParen,
                K::KwAnd,
                K::Ident
            ]),
            Some(vec![Atom, Atom, Binary(Or), Group, Atom, Binary(And)])
        );
        for toks in [
            &[K::Ident, K::Minus, K::Ident, K::Minus, K::Ident][..],
            &[K::KwNot, K::Ident, K::Gt, K::Ident],
            &[K::Minus, K::Ident, K::Star, K::Ident],
            &[K::Ident, K::Question, K::Ident],
            &[K::Ident, K::Plus],
            &[K::LParen, K::Ident],
            &[],
        ] {
            assert_agree(toks);
        }
    }

    /// Pins the parse of a prefix `not` after a binary operator: prefix
    /// `not` parses its operand at binding power 5, so after a binary
    /// operator it captures every following operator whose left binding
    /// power is at least 5 (`==` `!=` comparisons `+` `-` `*` `/` `%`), but
    /// not `and`/`or`. Checked on both the shape model and the real parser.
    #[test]
    fn not_captures_tail_at_equality_level() {
        use BinaryOp::*;
        use ShapeNode::{Atom, Binary, Unary};
        use TokenKind as K;
        let cases: &[(&[TokenKind], Vec<ShapeNode>)] = &[
            // a * not b + c  ==  a * not (b + c)
            (
                &[K::Ident, K::Star, K::KwNot, K::Ident, K::Plus, K::Ident],
                vec![
                    Atom,
                    Atom,
                    Atom,
                    Binary(Add),
                    Unary(UnaryOp::Not),
                    Binary(Mul),
                ],
            ),
            // a == not b != c  ==  a == not (b != c)
            (
                &[K::Ident, K::EqEq, K::KwNot, K::Ident, K::BangEq, K::Ident],
                vec![
                    Atom,
                    Atom,
                    Atom,
                    Binary(Ne),
                    Unary(UnaryOp::Not),
                    Binary(Eq),
                ],
            ),
            // not a and b  ==  (not a) and b
            (
                &[K::KwNot, K::Ident, K::KwAnd, K::Ident],
                vec![Atom, Unary(UnaryOp::Not), Atom, Binary(And)],
            ),
        ];
        for (toks, shape) in cases {
            assert_eq!(model_shape(toks).as_ref(), Some(shape), "tokens: {toks:?}");
            assert_eq!(real_shape(toks).as_ref(), Some(shape), "tokens: {toks:?}");
        }
    }

    #[test]
    fn rejections() {
        use TokenKind as K;
        assert_eq!(model_shape(&[]), None);
        assert_eq!(model_shape(&[K::Ident, K::Question, K::Ident]), None);
        assert_eq!(model_shape(&[K::Ident, K::Ident]), None);
        assert_eq!(model_shape(&[K::LParen, K::Ident]), None);
        assert_eq!(model_shape(&[K::Comma]), None);
    }

    #[test]
    fn depth_limit_matches_parser() {
        use TokenKind as K;
        // Nested parentheses cost two levels each (ternary + binding power);
        // walk across the limit and compare accept/reject with the parser.
        for n in 90..=110 {
            let mut toks = vec![K::LParen; n];
            toks.push(K::Ident);
            toks.extend(std::iter::repeat_n(K::RParen, n));
            assert_agree(&toks);
        }
        // Prefix chains cost one level each.
        for n in 190..=205 {
            let mut toks = vec![K::Minus; n];
            toks.push(K::Ident);
            assert_agree(&toks);
        }
    }

    /// A well-formed expression of the fragment, as tokens.
    fn well_formed() -> impl Strategy<Value = Vec<TokenKind>> {
        let atom = prop::sample::select(vec![
            TokenKind::Ident,
            TokenKind::Number,
            TokenKind::KwTrue,
            TokenKind::KwFalse,
        ])
        .prop_map(|k| vec![k]);
        atom.prop_recursive(6, 64, 3, |inner| {
            let binop = prop::sample::select(vec![
                TokenKind::KwAnd,
                TokenKind::KwOr,
                TokenKind::Plus,
                TokenKind::Minus,
                TokenKind::Star,
                TokenKind::Slash,
                TokenKind::Percent,
                TokenKind::Lt,
                TokenKind::Le,
                TokenKind::Gt,
                TokenKind::Ge,
                TokenKind::EqEq,
                TokenKind::BangEq,
            ]);
            let prefix =
                prop::sample::select(vec![TokenKind::Minus, TokenKind::Plus, TokenKind::KwNot]);
            prop_oneof![
                (inner.clone(), binop, inner.clone()).prop_map(|(mut l, op, r)| {
                    l.push(op);
                    l.extend(r);
                    l
                }),
                (prefix, inner.clone()).prop_map(|(op, e)| {
                    let mut v = vec![op];
                    v.extend(e);
                    v
                }),
                inner.clone().prop_map(|e| {
                    let mut v = vec![TokenKind::LParen];
                    v.extend(e);
                    v.push(TokenKind::RParen);
                    v
                }),
                (inner.clone(), inner.clone(), inner).prop_map(|(c, t, e)| {
                    let mut v = c;
                    v.push(TokenKind::Question);
                    v.extend(t);
                    v.push(TokenKind::Colon);
                    v.extend(e);
                    v
                }),
            ]
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(1024))]

        /// Arbitrary token strings: the model accepts exactly when the parser
        /// builds an in-fragment tree, and the trees agree.
        #[test]
        fn differential_random_tokens(
            toks in prop::collection::vec(prop::sample::select(ALPHABET), 0..16)
        ) {
            prop_assert_eq!(model_shape(&toks), real_shape(&toks), "tokens: {:?}", toks);
        }

        /// Well-formed expressions of the fragment: full agreement (both
        /// accept, with the same shape, up to the shared depth limit).
        #[test]
        fn differential_well_formed(toks in well_formed()) {
            let model = model_shape(&toks);
            prop_assert!(model.is_some(), "tokens: {:?}", toks);
            prop_assert_eq!(model, real_shape(&toks), "tokens: {:?}", toks);
        }
    }
}
