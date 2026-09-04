//! A small blocking HTTP client for the command line.

use std::time::Duration;

use serde_json::Value;
use thiserror::Error;
use ureq::Agent;

/// A failure talking to the kernel.
#[derive(Debug, Error)]
pub enum ClientError {
    /// The kernel answered with an error body.
    #[error("{code}: {message}")]
    Api {
        /// HTTP status.
        status: u16,
        /// Stable code.
        code: String,
        /// Human sentence.
        message: String,
        /// Details.
        details: Value,
    },
    /// The request never got a proper answer.
    #[error("transport error: {0}")]
    Transport(String),
}

impl ClientError {
    /// The error in the wire shape, for `--json` output.
    pub fn to_json(&self) -> Value {
        match self {
            ClientError::Api {
                status,
                code,
                message,
                details,
            } => {
                serde_json::json!({"error": {"code": code, "message": message, "details": details, "status": status}})
            }
            ClientError::Transport(m) => {
                serde_json::json!({"error": {"code": "transport", "message": m, "details": {}}})
            }
        }
    }
}

/// The client.
#[derive(Debug, Clone)]
pub struct Client {
    agent: Agent,
    base: String,
    token: Option<String>,
}

impl Client {
    /// A client for a base URL such as `http://127.0.0.1:7401`.
    pub fn new(base: &str, token: Option<String>) -> Client {
        let config = Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(60)))
            .build();
        Client {
            agent: Agent::new_with_config(config),
            base: base.trim_end_matches('/').to_string(),
            token,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// `GET` a JSON resource.
    pub fn get(&self, path: &str) -> Result<Value, ClientError> {
        let mut request = self.agent.get(self.url(path));
        if let Some(token) = &self.token {
            request = request.header("Authorization", &format!("Bearer {token}"));
        }
        let response = request
            .call()
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        Self::finish(response)
    }

    /// `GET` a text resource such as the metrics.
    pub fn get_text(&self, path: &str) -> Result<String, ClientError> {
        let mut request = self.agent.get(self.url(path));
        if let Some(token) = &self.token {
            request = request.header("Authorization", &format!("Bearer {token}"));
        }
        let mut response = request
            .call()
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        if (200..300).contains(&status) {
            Ok(text)
        } else {
            Err(Self::api_error(status, &text))
        }
    }

    /// `POST` a JSON body.
    pub fn post(&self, path: &str, body: &Value) -> Result<Value, ClientError> {
        let mut request = self.agent.post(self.url(path));
        if let Some(token) = &self.token {
            request = request.header("Authorization", &format!("Bearer {token}"));
        }
        let response = request
            .send_json(body)
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        Self::finish(response)
    }

    fn finish(mut response: ureq::http::Response<ureq::Body>) -> Result<Value, ClientError> {
        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        if (200..300).contains(&status) {
            if text.trim().is_empty() {
                return Ok(Value::Null);
            }
            serde_json::from_str(&text)
                .map_err(|e| ClientError::Transport(format!("invalid JSON from server: {e}")))
        } else {
            Err(Self::api_error(status, &text))
        }
    }

    fn api_error(status: u16, text: &str) -> ClientError {
        let parsed: Value = serde_json::from_str(text).unwrap_or(Value::Null);
        let error = parsed.get("error").cloned().unwrap_or(Value::Null);
        ClientError::Api {
            status,
            code: error
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("http_error")
                .to_string(),
            message: error
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("HTTP {status}: {text}")),
            details: error.get("details").cloned().unwrap_or(Value::Null),
        }
    }
}
