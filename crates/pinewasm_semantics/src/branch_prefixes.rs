//! Pure assembly of branch-local prefixes and their result assignments.
//!
//! Kept generic so Aeneas can translate the exact production kernel without
//! translating AST analysis, diagnostics, or the complete HIR type graph.

/// The two guarded statement lists, in source evaluation order.
pub(crate) struct Branches<T> {
    pub(crate) then_branch: Vec<T>,
    pub(crate) else_branch: Vec<T>,
}

/// Append each result assignment to its own prefix. Nothing from either arm
/// is returned as an unconditional statement. The analyzer installs these
/// lists under one `HirStmt::If` and reads its fresh result slot at the join.
pub(crate) fn finish_branches<T>(
    mut then_branch: Vec<T>,
    then_result: T,
    mut else_branch: Vec<T>,
    else_result: T,
) -> Branches<T> {
    then_branch.push(then_result);
    else_branch.push(else_result);
    Branches {
        then_branch,
        else_branch,
    }
}
