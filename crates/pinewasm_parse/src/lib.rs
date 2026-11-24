//! Lexer and parser for PineWASM.
//!
//! Entry points:
//! - [`lex`] / [`lex_with_source_id`] — source text to a span-carrying token
//!   stream (the latter attributes spans to a caller-chosen
//!   [`pinewasm_span::SourceId`]).
//! - [`parse`] / [`parse_with_source_id`] — source text to an AST plus
//!   diagnostics (likewise source-id-aware, for multi-file/library compilation).
//!
//! Both are designed to accept arbitrary input: malformed source produces
//! diagnostics and a best-effort result. A panic is a defect, not part of the
//! public error contract.
//!
//! Both also bound what they allocate for it. A source over
//! [`MAX_SOURCE_LEN`] bytes is not read, and one that lexes to more than
//! [`MAX_TOKENS`] tokens is not parsed; either is one `E0001` diagnostic and
//! an empty result. Three further caps bound how deeply the tree nests:
//! expressions, statement blocks and declared types each stop at 200 levels
//! with an `E0110` diagnostic.
//!
//! The diagnostics are bounded too. Either entry point reports at most
//! [`pinewasm_span::MAX_ERRORS`] errors and [`pinewasm_span::MAX_WARNINGS`]
//! warnings, the first of each, and ends the list with an `E0002` or a
//! `W0007` when it left some out. The tokens and the tree do not depend on
//! what was left out.
#![forbid(unsafe_code)]

#[cfg(feature = "aeneas")]
pub mod aeneas_roots_parse_prec;
mod integer_literal;
mod lexer;
mod parser;
#[cfg(any(test, feature = "aeneas"))]
mod pratt_shape;
mod precedence;

pub use lexer::{lex, lex_with_source_id, LexResult, MAX_SOURCE_LEN};
pub use parser::{parse, parse_with_source_id, ParseResult, MAX_TOKENS};

#[cfg(test)]
mod chain_depth_tests;
#[cfg(test)]
mod diagnostic_limit_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod type_depth_tests;
