//! Pine's nominal `plot_display` domain.
//!
//! `display.*` values are sets of destinations, and Pine's `+` and `-` on them
//! are set union and set difference. They are not strings: a string such as
//! `"display.pane"` is not a valid `display` argument. Values still travel on
//! our string lane, but only as the canonical tokens below, so a host never
//! has to decode a concatenation such as `"display.panedisplay.price_scale"`.
//!
//! When an operand is only known at run time, the compiler computes the
//! union or difference of the canonical tokens in the engine (see
//! `analyze_display_set_op` in the semantics crate), and a host binding for
//! an input-chosen operand applies the operation under each choice
//! ([`combine_bindings`]).
//!
//! A set with a Pine name uses that name (`display.none`, `display.pane`,
//! `display.price_scale`, `display.status_line`, `display.data_window`,
//! `display.all`). Any other set joins its members with `+` in bit order, for
//! example `display.pane+display.price_scale`. These are the canonical wire
//! strings this crate uses.

use crate::{InputActiveBinaryOp, InputActiveExpr, InputValue};

pub const PLOT_DISPLAY_TYPE: &str = "plot_display";
pub const DISPLAY_NONE: &str = "display.none";
pub const DISPLAY_ALL: &str = "display.all";
/// Every destination; `display.all`.
pub const ALL_MASK: u8 = 0b1_1111;
/// The screener destination bit (the `display.pine_screener` token).
pub const PINE_SCREENER_MASK: u8 = 16;

/// The five destinations, in bit order.
const MEMBERS: [(u8, &str); 5] = [
    (1, "display.pane"),
    (2, "display.price_scale"),
    (4, "display.status_line"),
    (8, "display.data_window"),
    (PINE_SCREENER_MASK, "display.pine_screener"),
];

fn member_mask(token: &str) -> Option<u8> {
    MEMBERS
        .iter()
        .find(|(_, name)| *name == token)
        .map(|(bit, _)| *bit)
}

/// The destination set a canonical token names. Only the seven Pine names and
/// the canonical `+`-joined spelling parse; any other string is `None`.
pub fn mask(token: &str) -> Option<u8> {
    match token {
        DISPLAY_NONE => return Some(0),
        DISPLAY_ALL => return Some(ALL_MASK),
        _ => {}
    }
    if let Some(bit) = member_mask(token) {
        return Some(bit);
    }
    let mut mask = 0u8;
    let mut parts = 0;
    for part in token.split('+') {
        let bit = member_mask(part)?;
        // Strictly increasing bit order keeps one spelling per set.
        if bit <= mask {
            return None;
        }
        mask |= bit;
        parts += 1;
    }
    (parts >= 2 && mask != ALL_MASK).then_some(mask)
}

/// The canonical token for a destination set.
pub fn token(mask: u8) -> Option<String> {
    match mask {
        0 => Some(DISPLAY_NONE.to_string()),
        ALL_MASK => Some(DISPLAY_ALL.to_string()),
        1..=30 => Some(
            MEMBERS
                .iter()
                .filter(|(bit, _)| mask & bit != 0)
                .map(|(_, name)| *name)
                .collect::<Vec<_>>()
                .join("+"),
        ),
        _ => None,
    }
}

/// The host binding for a destination set: a Pine name, or the union of its
/// member names. The host applies `+` between display names as a set union,
/// so this shape needs no new decoding.
pub fn binding_expr(mask: u8) -> Option<InputActiveExpr> {
    let name = |name: &str| InputActiveExpr::Value {
        value: InputValue::String(name.to_string()),
    };
    match mask {
        0 => Some(name(DISPLAY_NONE)),
        ALL_MASK => Some(name(DISPLAY_ALL)),
        1..=30 => MEMBERS
            .iter()
            .filter(|(bit, _)| mask & bit != 0)
            .map(|(_, member)| name(member))
            .reduce(|lhs, rhs| InputActiveExpr::Binary {
                op: InputActiveBinaryOp::Add,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            }),
        _ => None,
    }
}

/// The destination set a binding from [`binding_expr`] names: one Pine
/// display name, or a `+` union of member names. Anything else, including a
/// joined wire token, `na` or an input read, is `None`.
pub fn binding_mask(expr: &InputActiveExpr) -> Option<u8> {
    match expr {
        InputActiveExpr::Value {
            value: InputValue::String(name),
        } if !name.contains('+') => mask(name),
        InputActiveExpr::Binary {
            op: InputActiveBinaryOp::Add,
            lhs,
            rhs,
        } => Some(binding_mask(lhs)? | binding_mask(rhs)?),
        _ => None,
    }
}

/// The most sets a combined input-bound display binding may choose between.
/// Beyond it the engine resolves the value after the run instead.
pub const MAX_BOUND_DISPLAY_CHOICES: usize = 64;

fn choices(expr: &InputActiveExpr) -> usize {
    match expr {
        InputActiveExpr::Ternary {
            when_true,
            when_false,
            ..
        } => choices(when_true).saturating_add(choices(when_false)),
        _ => 1,
    }
}

/// The host binding for `lhs + rhs` (`union`) or `lhs - rhs` on two display
/// bindings that may choose a set by ternaries over input values.
///
/// The operation is applied to every pair of sets the operands can choose,
/// under the same ternaries, so the result has the shapes a host already
/// reads for display: ternaries whose leaves are one Pine name or a `+` union
/// of member names, as [`binding_expr`] gives. A host never has to evaluate a
/// set difference. `None` when an operand is not such a binding or the result
/// would choose between more than [`MAX_BOUND_DISPLAY_CHOICES`] sets.
pub fn combine_bindings(
    union: bool,
    lhs: &InputActiveExpr,
    rhs: &InputActiveExpr,
) -> Option<InputActiveExpr> {
    if choices(lhs).saturating_mul(choices(rhs)) > MAX_BOUND_DISPLAY_CHOICES {
        return None;
    }
    combine(union, lhs, rhs, &mut Vec::new())
}

/// A condition already decided on the path to the current choice.
type Decided<'a> = Vec<(&'a InputActiveExpr, bool)>;

fn combine<'a>(
    union: bool,
    lhs: &'a InputActiveExpr,
    rhs: &'a InputActiveExpr,
    decided: &mut Decided<'a>,
) -> Option<InputActiveExpr> {
    let ((condition, when_true, when_false), other, on_left) = match (lhs, rhs) {
        (
            InputActiveExpr::Ternary {
                condition,
                when_true,
                when_false,
            },
            _,
        ) => ((condition, when_true, when_false), rhs, true),
        (
            _,
            InputActiveExpr::Ternary {
                condition,
                when_true,
                when_false,
            },
        ) => ((condition, when_true, when_false), lhs, false),
        _ => {
            let (left, right) = (binding_mask(lhs)?, binding_mask(rhs)?);
            return binding_expr(if union { left | right } else { left & !right });
        }
    };
    let pick = |branch: &'a InputActiveExpr, decided: &mut Decided<'a>| {
        if on_left {
            combine(union, branch, other, decided)
        } else {
            combine(union, other, branch, decided)
        }
    };
    // Both operands often choose by the same input (`X = sel ? ...`,
    // `Y = sel ? ...`, `X + Y`): a condition decided further out picks its
    // branch instead of testing the input again.
    if let Some((_, value)) = decided.iter().find(|(seen, _)| *seen == condition.as_ref()) {
        return pick(if *value { when_true } else { when_false }, decided);
    }
    let branch = |value: bool, decided: &mut Decided<'a>| {
        decided.push((condition.as_ref(), value));
        let result = pick(if value { when_true } else { when_false }, decided);
        decided.pop();
        result
    };
    let when_true = branch(true, decided)?;
    let when_false = branch(false, decided)?;
    Some(InputActiveExpr::Ternary {
        condition: condition.clone(),
        when_true: Box::new(when_true),
        when_false: Box::new(when_false),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_masks_read_back_every_binding() {
        for mask in 0..=ALL_MASK {
            assert_eq!(binding_mask(&binding_expr(mask).unwrap()), Some(mask));
        }
        let joined = InputActiveExpr::Value {
            value: InputValue::String("display.pane+display.price_scale".into()),
        };
        assert_eq!(binding_mask(&joined), None);
        assert_eq!(binding_mask(&InputActiveExpr::Na), None);
        assert_eq!(binding_mask(&InputActiveExpr::Input { input: 0 }), None);
    }

    #[test]
    fn combined_bindings_apply_the_operation_under_each_choice() {
        let choose = |when_true: u8, when_false: u8| InputActiveExpr::Ternary {
            condition: Box::new(InputActiveExpr::Input { input: 0 }),
            when_true: Box::new(binding_expr(when_true).unwrap()),
            when_false: Box::new(binding_expr(when_false).unwrap()),
        };
        // (input ? pane : none) + price_scale
        assert_eq!(
            combine_bindings(true, &choose(1, 0), &binding_expr(2).unwrap()),
            Some(choose(3, 2))
        );
        // all - (input ? pane : none)
        assert_eq!(
            combine_bindings(false, &binding_expr(15).unwrap(), &choose(1, 0)),
            Some(choose(14, 15))
        );
        // Both operands choose by the same input: it is tested once.
        assert_eq!(
            combine_bindings(true, &choose(1, 0), &choose(0, 4)),
            Some(choose(1, 4))
        );
        // By different inputs: the left choice is outermost.
        let other = InputActiveExpr::Ternary {
            condition: Box::new(InputActiveExpr::Input { input: 1 }),
            when_true: Box::new(binding_expr(0).unwrap()),
            when_false: Box::new(binding_expr(4).unwrap()),
        };
        let by_other = |when_true: u8, when_false: u8| InputActiveExpr::Ternary {
            condition: Box::new(InputActiveExpr::Input { input: 1 }),
            when_true: Box::new(binding_expr(when_true).unwrap()),
            when_false: Box::new(binding_expr(when_false).unwrap()),
        };
        assert_eq!(
            combine_bindings(true, &choose(1, 0), &other),
            Some(InputActiveExpr::Ternary {
                condition: Box::new(InputActiveExpr::Input { input: 0 }),
                when_true: Box::new(by_other(1, 5)),
                when_false: Box::new(by_other(0, 4)),
            })
        );
        assert_eq!(
            combine_bindings(true, &InputActiveExpr::Na, &binding_expr(2).unwrap()),
            None
        );
    }

    #[test]
    fn combined_bindings_are_bounded() {
        let mut wide = binding_expr(0).unwrap();
        for _ in 0..8 {
            wide = InputActiveExpr::Ternary {
                condition: Box::new(InputActiveExpr::Input { input: 0 }),
                when_true: Box::new(binding_expr(1).unwrap()),
                when_false: Box::new(wide),
            };
        }
        assert_eq!(choices(&wide), 9);
        assert!(combine_bindings(true, &wide, &binding_expr(2).unwrap()).is_some());
        assert_eq!(combine_bindings(true, &wide, &wide), None);
    }

    #[test]
    fn every_set_has_one_canonical_token_that_round_trips() {
        let mut seen = std::collections::HashSet::new();
        for mask in 0..=ALL_MASK {
            let token = token(mask).unwrap();
            assert_eq!(super::mask(&token), Some(mask), "{token}");
            assert!(seen.insert(token));
        }
        assert_eq!(
            token(3).as_deref(),
            Some("display.pane+display.price_scale")
        );
        assert_eq!(token(ALL_MASK).as_deref(), Some("display.all"));
        assert_eq!(token(16).as_deref(), Some("display.pine_screener"));
        assert_eq!(token(32), None);
    }

    #[test]
    fn concatenations_and_other_spellings_are_not_tokens() {
        for text in [
            "display.panedisplay.price_scale",
            "display.price_scale+display.pane",
            "display.pane+display.pane",
            "display.pane+display.price_scale+display.status_line+display.data_window+display.pine_screener",
            "display.all+display.pane",
            "display.pane+",
            "",
        ] {
            assert_eq!(mask(text), None, "{text}");
        }
    }

    #[test]
    fn bindings_name_each_member() {
        let value = |name: &str| InputActiveExpr::Value {
            value: InputValue::String(name.to_string()),
        };
        assert_eq!(binding_expr(0), Some(value("display.none")));
        assert_eq!(binding_expr(ALL_MASK), Some(value("display.all")));
        assert_eq!(binding_expr(4), Some(value("display.status_line")));
        assert_eq!(
            binding_expr(11),
            Some(InputActiveExpr::Binary {
                op: InputActiveBinaryOp::Add,
                lhs: Box::new(InputActiveExpr::Binary {
                    op: InputActiveBinaryOp::Add,
                    lhs: Box::new(value("display.pane")),
                    rhs: Box::new(value("display.price_scale")),
                }),
                rhs: Box::new(value("display.data_window")),
            })
        );
    }
}
