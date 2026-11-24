//! Charon/Aeneas extraction roots for the parse-prec lane (operator precedence
//! and associativity). Compiled only with the `aeneas` feature; production
//! builds never enable it.
//!
//! Charon cannot start from crate-private items reliably or from inherent
//! methods, so each root is a public free function delegating to the real
//! item. Root names differ from their callees to avoid name capture in the
//! generated Lean.

pub use crate::pratt_shape::{ShapeNode, SHAPE_MAX_DEPTH};
use pinewasm_syntax::{BinaryOp, TokenKind, UnaryOp};

/// The parser's binary-operator table: `(op, l_bp, r_bp)` or `None`.
#[must_use]
pub fn root_infix_binding_power(kind: TokenKind) -> Option<(BinaryOp, u8, u8)> {
    crate::precedence::infix_binding_power(kind)
}

/// The parser's prefix-operator table: `(op, operand_bp)` or `None`.
#[must_use]
pub fn root_prefix_binding_power(kind: TokenKind) -> Option<(UnaryOp, u8)> {
    crate::precedence::prefix_binding_power(kind)
}

/// The token-kind Pratt shape parser (post-order node kinds into `out`).
pub fn root_pratt_shape(toks: &[TokenKind], out: &mut Vec<ShapeNode>) -> bool {
    crate::pratt_shape::pratt_shape(toks, out)
}
