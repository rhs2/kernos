//! Error types for the policy crate.

use thiserror::Error;

/// A syntax or semantic error found while parsing policy text. Carries the
/// 1-based line and column so an editor can point at the offending token; the
/// control plane forwards these as `details.line` and `details.column`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("line {line}, column {column}: {message}")]
pub struct ParseError {
    /// 1-based line of the offending token.
    pub line: u32,
    /// 1-based column of the offending token.
    pub column: u32,
    /// Human sentence describing what was expected.
    pub message: String,
}

impl ParseError {
    /// Builds an error at a position. Exists so the lexer and parser share one
    /// constructor and never disagree on the numbering convention.
    pub fn new(line: u32, column: u32, message: impl Into<String>) -> Self {
        ParseError {
            line,
            column,
            message: message.into(),
        }
    }
}
