//! The kernel error type, shaped for the HTTP error contract of 00-OVERVIEW.

use serde_json::{json, Value};
use thiserror::Error;

/// Every failure a kernel operation can report. `Api` variants carry the HTTP
/// status and the stable error code that the HTTP layer returns verbatim; the
/// others are infrastructure failures that surface as `500 internal`.
#[derive(Debug, Error)]
pub enum KernelError {
    /// A failure with a stable code and a matching HTTP status.
    #[error("{message}")]
    Api {
        /// HTTP status code.
        status: u16,
        /// Stable snake_case code.
        code: String,
        /// Human sentence.
        message: String,
        /// Structured details, an object.
        details: Value,
    },
    /// A SQLite failure.
    #[error("storage error: {0}")]
    Storage(#[from] rusqlite::Error),
    /// A JSON failure inside the kernel.
    #[error("serialisation error: {0}")]
    Json(#[from] serde_json::Error),
    /// A filesystem failure.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

impl KernelError {
    /// Builds an API error with empty details.
    pub fn api(status: u16, code: &str, message: impl Into<String>) -> Self {
        KernelError::Api {
            status,
            code: code.to_string(),
            message: message.into(),
            details: json!({}),
        }
    }

    /// Adds structured details to an API error; a no-op on other variants.
    pub fn with_details(self, details: Value) -> Self {
        match self {
            KernelError::Api {
                status,
                code,
                message,
                ..
            } => KernelError::Api {
                status,
                code,
                message,
                details,
            },
            other => other,
        }
    }

    /// `400`.
    pub fn bad_request(code: &str, message: impl Into<String>) -> Self {
        Self::api(400, code, message)
    }

    /// `403`.
    pub fn forbidden(code: &str, message: impl Into<String>) -> Self {
        Self::api(403, code, message)
    }

    /// `404`.
    pub fn not_found(code: &str, message: impl Into<String>) -> Self {
        Self::api(404, code, message)
    }

    /// `409`.
    pub fn conflict(code: &str, message: impl Into<String>) -> Self {
        Self::api(409, code, message)
    }

    /// `410`.
    pub fn gone(code: &str, message: impl Into<String>) -> Self {
        Self::api(410, code, message)
    }

    /// `422`.
    pub fn unprocessable(code: &str, message: impl Into<String>) -> Self {
        Self::api(422, code, message)
    }

    /// The HTTP status for this error.
    pub fn status(&self) -> u16 {
        match self {
            KernelError::Api { status, .. } => *status,
            _ => 500,
        }
    }

    /// The stable code for this error.
    pub fn code(&self) -> &str {
        match self {
            KernelError::Api { code, .. } => code,
            _ => "internal",
        }
    }

    /// The details object for this error.
    pub fn details(&self) -> Value {
        match self {
            KernelError::Api { details, .. } => details.clone(),
            _ => json!({}),
        }
    }

    /// The whole error in the wire shape `{"error": {code, message, details}}`.
    pub fn to_json(&self) -> Value {
        json!({"error": {"code": self.code(), "message": self.to_string(), "details": self.details()}})
    }
}

/// Convenience alias used throughout the kernel.
pub type KernelResult<T> = Result<T, KernelError>;
