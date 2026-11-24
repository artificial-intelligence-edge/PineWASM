//! Native PineWASM language tooling for Pine Script® v6 source.
//!
//! The crate deliberately keeps protocol transports thin. [`analysis`] owns
//! the shared compiler-backed language intelligence, and [`lsp`] adapts it to
//! the Language Server Protocol.
#![forbid(unsafe_code)]

pub mod analysis;
#[cfg(feature = "server")]
pub mod lsp;
pub mod outline;
pub mod query;
pub mod text;
#[cfg(feature = "server")]
pub mod workspace;

use std::error::Error;

pub type ToolResult<T> = Result<T, Box<dyn Error + Send + Sync>>;
