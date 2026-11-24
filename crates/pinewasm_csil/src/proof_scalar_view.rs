//! A lossless, opt-in interchange for the first straight-line scalar proof.
//!
//! The unchanged frame is the complete typed program with its function vector
//! cleared, serialized directly to bytes and hex-encoded. Byte equality is a
//! conservative preservation check for every non-function field, including
//! floating-point spellings. This is not yet a canonical certificate or a
//! proof that the Lean model implements the Rust VM.

use crate::{BuiltinSeries, CsilInstr, CsilProgram};
use pinewasm_span::Span;
use serde::Serialize;

pub const SCALAR_VIEW_FORMAT: &str = "pw-csil-scalar-view-v1";
pub const BOUND_SCALAR_VIEW_FORMAT: &str = "pw-csil-bound-scalar-view-v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "tag", rename_all = "snake_case")]
pub enum ProofScalarInstr {
    ReadClose,
    StoreCurrent { slot: u32 },
    ReadCurrent { slot: u32 },
    Plot { slot: u32 },
    Return,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProofScalarFunction {
    pub name: String,
    pub max_stack: u32,
    pub code: Vec<ProofScalarInstr>,
    pub source_map: Vec<Span>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProofScalarView {
    pub format: &'static str,
    pub frame_hex: String,
    pub function: ProofScalarFunction,
}

/// Binds a typed scalar view to the exact bytes of one ordinary serialized
/// CSIL artifact. `prefix_hex + function_hex + suffix_hex` reconstructs the
/// full artifact; replacing `function_hex` with the bytes for `[]` reconstructs
/// the scalar view's unchanged frame. Lean must independently check both
/// equalities and decode the function before using the view's theorem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProofBoundScalarView {
    pub format: &'static str,
    pub scalar_view: ProofScalarView,
    pub prefix_hex: String,
    pub function_hex: String,
    pub suffix_hex: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ProofScalarViewError {
    #[error("the first scalar profile requires exactly one main function")]
    FunctionShape,
    #[error("source-map length differs from instruction count")]
    SourceMapLength,
    #[error("opcode outside the first scalar proof profile at instruction {0}")]
    UnsupportedOpcode(usize),
    #[error("source span is reversed at instruction {0}")]
    ReversedSpan(usize),
    #[error("failed to serialize typed CSIL program frame: {0}")]
    Serialize(serde_json::Error),
    #[error("serialized scalar frame does not have exactly one functions field marker")]
    FrameMarker,
    #[error("function insertion did not reconstruct the typed CSIL artifact")]
    ArtifactReconstruction,
}

impl From<serde_json::Error> for ProofScalarViewError {
    fn from(error: serde_json::Error) -> Self {
        Self::Serialize(error)
    }
}

pub(crate) fn hex_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut hex, byte| {
            use std::fmt::Write;
            write!(&mut hex, "{byte:02x}").expect("writing into String is infallible");
            hex
        })
}

pub(crate) fn empty_function_frame(program: &CsilProgram) -> CsilProgram {
    let mut frame = program.clone();
    frame.functions.clear();
    frame
}

/// Exact serializer fragments shared by admitted scalar and branch views.
/// A successful result has already been checked against ordinary CSIL JSON.
pub(crate) type BoundFragments = (Vec<u8>, Vec<u8>, Vec<u8>);

pub(crate) fn bound_fragments(
    program: &CsilProgram,
) -> Result<BoundFragments, ProofScalarViewError> {
    let frame_bytes = serde_json::to_vec(&empty_function_frame(program))?;
    let marker = b"\"functions\":[]";
    let mut hits = frame_bytes
        .windows(marker.len())
        .enumerate()
        .filter_map(|(index, candidate)| (candidate == marker).then_some(index));
    let Some(marker_index) = hits.next() else {
        return Err(ProofScalarViewError::FrameMarker);
    };
    if hits.next().is_some() {
        return Err(ProofScalarViewError::FrameMarker);
    }
    let prefix_end = marker_index + b"\"functions\":".len();
    let prefix = frame_bytes[..prefix_end].to_vec();
    let suffix = frame_bytes[prefix_end + 2..].to_vec();
    let function_bytes = serde_json::to_vec(&program.functions)?;
    let reconstructed = [&prefix[..], &function_bytes, &suffix].concat();
    if reconstructed != serde_json::to_vec(program)? {
        return Err(ProofScalarViewError::ArtifactReconstruction);
    }
    Ok((prefix, function_bytes, suffix))
}

impl ProofScalarView {
    /// Project a typed CSIL value without deserializing ordinary JSON numbers.
    /// The strict Lean consumer independently validates the exported shape.
    pub fn from_program(program: &CsilProgram) -> Result<Self, ProofScalarViewError> {
        let [function] = program.functions.as_slice() else {
            return Err(ProofScalarViewError::FunctionShape);
        };
        if function.name != "main" {
            return Err(ProofScalarViewError::FunctionShape);
        }
        if function.code.len() != function.source_map.len() {
            return Err(ProofScalarViewError::SourceMapLength);
        }
        let code = function
            .code
            .iter()
            .enumerate()
            .map(|(index, instr)| match instr {
                CsilInstr::ReadBuiltin {
                    source: BuiltinSeries::Close,
                    offset: 0,
                } => Ok(ProofScalarInstr::ReadClose),
                CsilInstr::StoreUserSlotCurrent { slot } => {
                    Ok(ProofScalarInstr::StoreCurrent { slot: *slot })
                }
                CsilInstr::ReadUserSlot { slot, offset: 0 } => {
                    Ok(ProofScalarInstr::ReadCurrent { slot: *slot })
                }
                CsilInstr::PlotF64 { plot_slot } => Ok(ProofScalarInstr::Plot { slot: *plot_slot }),
                CsilInstr::Return => Ok(ProofScalarInstr::Return),
                _ => Err(ProofScalarViewError::UnsupportedOpcode(index)),
            })
            .collect::<Result<Vec<_>, _>>()?;
        for (index, span) in function.source_map.iter().enumerate() {
            if span.start > span.end {
                return Err(ProofScalarViewError::ReversedSpan(index));
            }
        }
        let frame_bytes = serde_json::to_vec(&empty_function_frame(program))?;
        let frame_hex = hex_bytes(&frame_bytes);
        Ok(Self {
            format: SCALAR_VIEW_FORMAT,
            frame_hex,
            function: ProofScalarFunction {
                name: function.name.clone(),
                max_stack: function.max_stack,
                code,
                source_map: function.source_map.clone(),
            },
        })
    }
}

impl ProofBoundScalarView {
    /// Export fragments from the same typed value as ordinary CSIL JSON.
    /// The final byte equality checks protect against serializer-layout drift.
    pub fn from_program(program: &CsilProgram) -> Result<Self, ProofScalarViewError> {
        let scalar_view = ProofScalarView::from_program(program)?;
        let (prefix, function_bytes, suffix) = bound_fragments(program)?;
        Ok(Self {
            format: BOUND_SCALAR_VIEW_FORMAT,
            scalar_view,
            prefix_hex: hex_bytes(&prefix),
            function_hex: hex_bytes(&function_bytes),
            suffix_hex: hex_bytes(&suffix),
        })
    }
}
