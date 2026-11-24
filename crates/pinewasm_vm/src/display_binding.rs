//! Runtime display-set binding projection for host style metadata.

use pinewasm_hir::{display_set, InputActiveExpr};

/// Project one runtime `plot_display` token into the binding expression
/// hosts receive: the union of its member names, never the token itself.
///
/// The type checker admits only canonical tokens as `plot_display` values,
/// so a non-token string is a compiler invariant break. Debug and test builds
/// fail loudly; release builds send `na`, which hosts already report as an
/// invalid display binding.
pub(crate) fn runtime_display_binding(token: &str) -> InputActiveExpr {
    let mask = display_set::mask(token);
    assert_canonical_display_token(mask.is_some(), token);
    mask.and_then(display_set::binding_expr)
        .unwrap_or(InputActiveExpr::Na)
}

#[inline(always)]
// Expression position is required by the explicit facade inventory.
#[allow(clippy::semicolon_if_nothing_returned)]
fn assert_canonical_display_token(canonical: bool, token: &str) {
    debug_assert!(canonical, "non-canonical display token {token:?}")
}

#[cfg(test)]
mod tests {
    use super::runtime_display_binding;
    use pinewasm_hir::{display_set, InputActiveExpr};

    #[test]
    fn canonical_tokens_project_to_member_union() {
        for mask in 0..=display_set::ALL_MASK {
            let token = display_set::token(mask).expect("every mask has a canonical token");
            assert_eq!(
                runtime_display_binding(&token),
                display_set::binding_expr(mask).expect("every mask has a binding"),
                "token {token:?}"
            );
            assert_ne!(runtime_display_binding(&token), InputActiveExpr::Na);
        }
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "non-canonical display token")]
    fn non_canonical_token_fails_loudly_in_debug_builds() {
        let _ = runtime_display_binding("display.bogus");
    }

    #[test]
    #[cfg(not(debug_assertions))]
    fn non_canonical_token_binds_na_in_release_builds() {
        assert_eq!(
            runtime_display_binding("display.bogus"),
            InputActiveExpr::Na
        );
    }
}
