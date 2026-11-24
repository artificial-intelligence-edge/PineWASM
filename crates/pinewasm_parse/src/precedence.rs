//! Operator precedence and associativity of the Pratt expression core, as
//! pure functions over [`TokenKind`].
//!
//! The parser's binary loop ([`crate::parser`]'s `parse_expr_bp_inner`) and
//! its prefix-operator dispatch (`parse_unary`) consult these tables instead
//! of matching token kinds inline, so the precedence relation lives in one
//! place with no parser state, strings or spans. That also keeps it inside the
//! subset the Charon/Aeneas extraction accepts (see the `aeneas` feature).
//!
//! Binding powers, loosest to tightest:
//! `or`(1) < `and`(3) < `==`/`!=`(5) < `<`/`<=`/`>`/`>=`(7) < `+`/`-`(9)
//! < `*`/`/`/`%`(11) < prefix `-`/`+`(13). Every binary operator is
//! left-associative: its right binding power is its left one plus one. The
//! ternary `?:` sits below all of them and is handled by the parser directly
//! (right-associative).

use pinewasm_syntax::{BinaryOp, TokenKind, UnaryOp};

/// The binding power a prefix `-`/`+` parses its operand at: tighter than
/// every binary operator.
pub(crate) const PREFIX_ARITH_BP: u8 = 13;

/// The binding power prefix `not` parses its operand at: the level of
/// `==`/`!=`, so `not` binds looser than comparison.
pub(crate) const PREFIX_NOT_BP: u8 = 5;

/// The infix binary operator `kind` denotes, with its left and right binding
/// powers `(op, l_bp, r_bp)`, or `None` when `kind` is not a binary operator
/// (which ends the Pratt loop).
pub(crate) fn infix_binding_power(kind: TokenKind) -> Option<(BinaryOp, u8, u8)> {
    match kind {
        TokenKind::KwOr => Some((BinaryOp::Or, 1, 2)),
        TokenKind::KwAnd => Some((BinaryOp::And, 3, 4)),
        TokenKind::EqEq => Some((BinaryOp::Eq, 5, 6)),
        TokenKind::BangEq => Some((BinaryOp::Ne, 5, 6)),
        TokenKind::Lt => Some((BinaryOp::Lt, 7, 8)),
        TokenKind::Le => Some((BinaryOp::Le, 7, 8)),
        TokenKind::Gt => Some((BinaryOp::Gt, 7, 8)),
        TokenKind::Ge => Some((BinaryOp::Ge, 7, 8)),
        TokenKind::Plus => Some((BinaryOp::Add, 9, 10)),
        TokenKind::Minus => Some((BinaryOp::Sub, 9, 10)),
        TokenKind::Star => Some((BinaryOp::Mul, 11, 12)),
        TokenKind::Slash => Some((BinaryOp::Div, 11, 12)),
        TokenKind::Percent => Some((BinaryOp::Rem, 11, 12)),
        _ => None,
    }
}

/// The prefix unary operator `kind` denotes, with the binding power its
/// operand is parsed at, or `None` when `kind` does not start a prefix
/// operation.
///
/// Unary `+` is identity on a numeric operand and binds at the
/// same tight prefix level as unary `-`.
///
/// `not` binds looser than comparison: `not a > b` is `not (a > b)`.
pub(crate) fn prefix_binding_power(kind: TokenKind) -> Option<(UnaryOp, u8)> {
    match kind {
        TokenKind::Minus => Some((UnaryOp::Neg, PREFIX_ARITH_BP)),
        TokenKind::Plus => Some((UnaryOp::Pos, PREFIX_ARITH_BP)),
        TokenKind::KwNot => Some((UnaryOp::Not, PREFIX_NOT_BP)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{infix_binding_power, prefix_binding_power};
    use pinewasm_syntax::{BinaryOp, TokenKind, UnaryOp};

    /// Every `TokenKind` variant. The `exhaustive` match below fails to
    /// compile when a variant is added, forcing this list to be updated.
    const ALL_KINDS: &[TokenKind] = {
        use TokenKind::*;
        &[
            Ident,
            Number,
            Str,
            Color,
            KwIf,
            KwElse,
            KwTrue,
            KwFalse,
            KwAnd,
            KwOr,
            KwNot,
            KwVar,
            KwVarip,
            KwInt,
            KwFloat,
            KwBool,
            KwString,
            KwNa,
            KwFor,
            KwTo,
            KwBy,
            KwIn,
            KwWhile,
            KwBreak,
            KwContinue,
            KwSwitch,
            KwEnum,
            KwType,
            KwMethod,
            Plus,
            Minus,
            Star,
            Slash,
            Percent,
            PlusEq,
            MinusEq,
            StarEq,
            SlashEq,
            PercentEq,
            Lt,
            Le,
            Gt,
            Ge,
            EqEq,
            BangEq,
            Eq,
            ColonEq,
            FatArrow,
            Question,
            Colon,
            LParen,
            RParen,
            LBracket,
            RBracket,
            Comma,
            Dot,
            VersionDirective,
            AlertMessageDirective,
            Newline,
            Indent,
            Dedent,
            Eof,
            Error,
        ]
    };

    #[allow(clippy::match_same_arms)]
    fn exhaustive(kind: TokenKind) -> usize {
        use TokenKind::*;
        // Position in ALL_KINDS; a new variant makes this non-exhaustive.
        match kind {
            Ident => 0,
            Number => 1,
            Str => 2,
            Color => 3,
            KwIf => 4,
            KwElse => 5,
            KwTrue => 6,
            KwFalse => 7,
            KwAnd => 8,
            KwOr => 9,
            KwNot => 10,
            KwVar => 11,
            KwVarip => 12,
            KwInt => 13,
            KwFloat => 14,
            KwBool => 15,
            KwString => 16,
            KwNa => 17,
            KwFor => 18,
            KwTo => 19,
            KwBy => 20,
            KwIn => 21,
            KwWhile => 22,
            KwBreak => 23,
            KwContinue => 24,
            KwSwitch => 25,
            KwEnum => 26,
            KwType => 27,
            KwMethod => 28,
            Plus => 29,
            Minus => 30,
            Star => 31,
            Slash => 32,
            Percent => 33,
            PlusEq => 34,
            MinusEq => 35,
            StarEq => 36,
            SlashEq => 37,
            PercentEq => 38,
            Lt => 39,
            Le => 40,
            Gt => 41,
            Ge => 42,
            EqEq => 43,
            BangEq => 44,
            Eq => 45,
            ColonEq => 46,
            FatArrow => 47,
            Question => 48,
            Colon => 49,
            LParen => 50,
            RParen => 51,
            LBracket => 52,
            RBracket => 53,
            Comma => 54,
            Dot => 55,
            VersionDirective => 56,
            AlertMessageDirective => 57,
            Newline => 58,
            Indent => 59,
            Dedent => 60,
            Eof => 61,
            Error => 62,
        }
    }

    /// Expected values for `infix_binding_power`: the binary-operator table
    /// written as one plain match (the `break` arm is `None`).
    fn old_infix(kind: TokenKind) -> Option<(BinaryOp, u8, u8)> {
        Some(match kind {
            TokenKind::KwOr => (BinaryOp::Or, 1, 2),
            TokenKind::KwAnd => (BinaryOp::And, 3, 4),
            TokenKind::EqEq => (BinaryOp::Eq, 5, 6),
            TokenKind::BangEq => (BinaryOp::Ne, 5, 6),
            TokenKind::Lt => (BinaryOp::Lt, 7, 8),
            TokenKind::Le => (BinaryOp::Le, 7, 8),
            TokenKind::Gt => (BinaryOp::Gt, 7, 8),
            TokenKind::Ge => (BinaryOp::Ge, 7, 8),
            TokenKind::Plus => (BinaryOp::Add, 9, 10),
            TokenKind::Minus => (BinaryOp::Sub, 9, 10),
            TokenKind::Star => (BinaryOp::Mul, 11, 12),
            TokenKind::Slash => (BinaryOp::Div, 11, 12),
            TokenKind::Percent => (BinaryOp::Rem, 11, 12),
            _ => return None,
        })
    }

    /// Expected values for `prefix_binding_power`: the prefix dispatch written
    /// as one plain match, with `-`/`+` at 13, `not` at 5, and anything else
    /// falling through to postfix parsing.
    fn old_prefix(kind: TokenKind) -> Option<(UnaryOp, u8)> {
        match kind {
            TokenKind::Minus => Some((UnaryOp::Neg, 13)),
            TokenKind::Plus => Some((UnaryOp::Pos, 13)),
            TokenKind::KwNot => Some((UnaryOp::Not, 5)),
            _ => None,
        }
    }

    #[test]
    fn all_kinds_is_complete_and_ordered() {
        assert_eq!(ALL_KINDS.len(), 63);
        for (i, &k) in ALL_KINDS.iter().enumerate() {
            assert_eq!(exhaustive(k), i, "{k:?}");
        }
    }

    #[test]
    fn infix_table_matches_pre_refactor_inline_match() {
        for &k in ALL_KINDS {
            assert_eq!(infix_binding_power(k), old_infix(k), "{k:?}");
        }
    }

    #[test]
    fn prefix_table_matches_pre_refactor_dispatch() {
        for &k in ALL_KINDS {
            assert_eq!(prefix_binding_power(k), old_prefix(k), "{k:?}");
        }
    }

    #[test]
    fn binary_operators_are_left_associative_with_odd_left_power() {
        for &k in ALL_KINDS {
            if let Some((_, l, r)) = infix_binding_power(k) {
                assert_eq!(r, l + 1, "{k:?}");
                assert_eq!(l % 2, 1, "{k:?}");
                assert!(l < super::PREFIX_ARITH_BP, "{k:?}");
            }
        }
    }
}
