//! Lossless typed projection for the first forward branch-entry coalescing case.
//!
//! This deliberately admits only the seven-to-five instruction profile used by
//! the Lean checker. Exact serialized fragments bind the projection to an
//! ordinary CSIL artifact; they are not a compiler-correctness certificate.

use crate::proof_scalar_view::{bound_fragments, empty_function_frame, hex_bytes};
use crate::{BuiltinSeries, CsilInstr, CsilProgram, ProofScalarViewError};
use pinewasm_span::Span;
use serde::Serialize;

pub const BRANCH_VIEW_FORMAT: &str = "pw-csil-branch-view-v1";
pub const BOUND_BRANCH_VIEW_FORMAT: &str = "pw-csil-bound-branch-view-v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "tag", rename_all = "snake_case")]
pub enum ProofBranchInstr {
    ReadClose,
    Jump { target: u32 },
    PushFiniteF64 { bits: String },
    StoreCurrent { slot: u32 },
    ReadCurrent { slot: u32 },
    Plot { slot: u32 },
    Return,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProofBranchFunction {
    pub name: String,
    pub max_stack: u32,
    pub code: Vec<ProofBranchInstr>,
    pub source_map: Vec<Span>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProofBranchView {
    pub format: &'static str,
    pub frame_hex: String,
    pub function: ProofBranchFunction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProofBoundBranchView {
    pub format: &'static str,
    pub branch_view: ProofBranchView,
    pub prefix_hex: String,
    pub function_hex: String,
    pub suffix_hex: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ProofBranchViewError {
    #[error("branch proof profile requires exactly one main function")]
    FunctionShape,
    #[error("source-map length differs from instruction count")]
    SourceMapLength,
    #[error("opcode outside branch proof profile at instruction {0}")]
    UnsupportedOpcode(usize),
    #[error("non-finite literal outside branch proof profile at instruction {0}")]
    NonFiniteLiteral(usize),
    #[error("source span is reversed at instruction {0}")]
    ReversedSpan(usize),
    #[error("failed to serialize CSIL program: {0}")]
    Serialize(serde_json::Error),
    #[error("failed to bind ordinary CSIL artifact: {0}")]
    Fragment(ProofScalarViewError),
}

impl From<serde_json::Error> for ProofBranchViewError {
    fn from(error: serde_json::Error) -> Self {
        Self::Serialize(error)
    }
}

impl From<ProofScalarViewError> for ProofBranchViewError {
    fn from(error: ProofScalarViewError) -> Self {
        Self::Fragment(error)
    }
}

impl ProofBranchView {
    pub fn from_program(program: &CsilProgram) -> Result<Self, ProofBranchViewError> {
        let [function] = program.functions.as_slice() else {
            return Err(ProofBranchViewError::FunctionShape);
        };
        if function.name != "main" {
            return Err(ProofBranchViewError::FunctionShape);
        }
        if function.code.len() != function.source_map.len() {
            return Err(ProofBranchViewError::SourceMapLength);
        }
        let code = function
            .code
            .iter()
            .enumerate()
            .map(|(index, instr)| match instr {
                CsilInstr::ReadBuiltin {
                    source: BuiltinSeries::Close,
                    offset: 0,
                } => Ok(ProofBranchInstr::ReadClose),
                CsilInstr::Jump { target } => Ok(ProofBranchInstr::Jump { target: *target }),
                CsilInstr::PushF64(value) if value.get().is_finite() => {
                    Ok(ProofBranchInstr::PushFiniteF64 {
                        bits: format!("{:016x}", value.to_bits()),
                    })
                }
                CsilInstr::PushF64(_) => Err(ProofBranchViewError::NonFiniteLiteral(index)),
                CsilInstr::StoreUserSlotCurrent { slot } => {
                    Ok(ProofBranchInstr::StoreCurrent { slot: *slot })
                }
                CsilInstr::ReadUserSlot { slot, offset: 0 } => {
                    Ok(ProofBranchInstr::ReadCurrent { slot: *slot })
                }
                CsilInstr::PlotF64 { plot_slot } => Ok(ProofBranchInstr::Plot { slot: *plot_slot }),
                CsilInstr::Return => Ok(ProofBranchInstr::Return),
                _ => Err(ProofBranchViewError::UnsupportedOpcode(index)),
            })
            .collect::<Result<Vec<_>, _>>()?;
        for (index, span) in function.source_map.iter().enumerate() {
            if span.start > span.end {
                return Err(ProofBranchViewError::ReversedSpan(index));
            }
        }
        Ok(Self {
            format: BRANCH_VIEW_FORMAT,
            frame_hex: hex_bytes(&serde_json::to_vec(&empty_function_frame(program))?),
            function: ProofBranchFunction {
                name: function.name.clone(),
                max_stack: function.max_stack,
                code,
                source_map: function.source_map.clone(),
            },
        })
    }
}

impl ProofBoundBranchView {
    pub fn from_program(program: &CsilProgram) -> Result<Self, ProofBranchViewError> {
        let branch_view = ProofBranchView::from_program(program)?;
        let (prefix, function_bytes, suffix) = bound_fragments(program)?;
        Ok(Self {
            format: BOUND_BRANCH_VIEW_FORMAT,
            branch_view,
            prefix_hex: hex_bytes(&prefix),
            function_hex: hex_bytes(&function_bytes),
            suffix_hex: hex_bytes(&suffix),
        })
    }
}
