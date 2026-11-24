//! Token and AST definitions for the Pine Script® v6 source subset that PineWASM parses.
//!
//! This crate defines *what the syntax is*; the lexer and parser that
//! produce these values live in `pinewasm_parse`. Every token and AST
//! node carries a [`pinewasm_span::Span`].
#![forbid(unsafe_code)]

pub mod ast;
pub mod token;

pub use ast::*;
pub use token::*;
