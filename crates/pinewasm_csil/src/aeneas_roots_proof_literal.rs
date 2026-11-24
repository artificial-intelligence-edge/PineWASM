//! Free-function extraction root for the `ProofLiteral` conversion.
//!
//! This module is compiled only with the `aeneas` feature and is not part of
//! production builds. The function delegates directly to the existing
//! `TryFrom` implementation so Charon can include its body in a call graph.

use crate::{CsilInstr, ProofLiteral, ProofLiteralError};

pub fn try_from_proof_literal(instr: &CsilInstr) -> Result<ProofLiteral, ProofLiteralError> {
    <ProofLiteral as TryFrom<&CsilInstr>>::try_from(instr)
}
