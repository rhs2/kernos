//! Server configuration: `kernos.json`, overridden by `KERNOS_*` variables,
//! overridden by command-line flags.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use kernos_core::KernelConfig;

/// Every setting of the configuration table in 02-KERNEL-API, plus the lease
/// TTL clamp bounds.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Listen address.
    pub listen: String,
    /// Data directory.
    pub data_dir: PathBuf,
    /// Bearer token; unset means open on loopback.
    pub token: Option<String>,
    /// Default lease TTL.
    pub lease_ttl_default: u64,
    /// Lower clamp of a requested lease TTL.
    pub lease_ttl_min: u64,
    /// Upper clamp of a requested lease TTL.
    pub lease_ttl_max: u64,
    /// Lease sweeper interval.
    pub lease_sweep_interval_ms: u64,
    /// Approval SLA sweeper interval.
    pub approval_sweep_interval_ms: u64,
    /// Soft budget ratio.
    pub budget_soft_ratio: f64,
    /// Attempts before quarantine, non-deterministic failures.
    pub max_attempts_nondeterministic: u32,
    /// Attempts before quarantine, deterministic failures.
    pub max_attempts_deterministic: u32,
    /// `text` or `json`.
    pub log_format: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            listen: "127.0.0.1:7401".into(),
            data_dir: PathBuf::from("./kernos-data"),
            token: None,
            lease_ttl_default: 30,
            lease_ttl_min: 1,
            lease_ttl_max: 300,
            lease_sweep_interval_ms: 1000,
            approval_sweep_interval_ms: 5000,
            budget_soft_ratio: 0.8,
            max_attempts_nondeterministic: 5,
            max_attempts_deterministic: 3,
            log_format: "text".into(),
        }
    }
}

/// A configuration failure: unreadable file or an unparsable value.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The config file could not be read.
    #[error("cannot read config {path}: {source}")]
    Read {
        /// The path.
        path: PathBuf,
        /// The cause.
        source: std::io::Error,
    },
    /// The config file is not valid JSON of the expected shape.
    #[error("config {path} does not parse: {source}")]
    Parse {
        /// The path.
        path: PathBuf,
        /// The cause.
        source: serde_json::Error,
    },
    /// An environment variable holds a value of the wrong type.
    #[error("environment variable {name} has an invalid value {value:?}")]
    Env {
        /// The variable.
        name: String,
        /// Its value.
        value: String,
    },
}

impl Config {
    /// Loads the file (when given), then applies environment overrides.
    pub fn load(path: Option<&Path>) -> Result<Config, ConfigError> {
        let mut config = match path {
            Some(path) => {
                let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                })?;
                serde_json::from_str(&text).map_err(|source| ConfigError::Parse {
                    path: path.to_path_buf(),
                    source,
                })?
            }
            None => Config::default(),
        };
        config.apply_env()?;
        Ok(config)
    }

    /// Applies `KERNOS_*` overrides.
    pub fn apply_env(&mut self) -> Result<(), ConfigError> {
        fn var(name: &str) -> Option<String> {
            std::env::var(name).ok().filter(|v| !v.is_empty())
        }
        fn parse<T: std::str::FromStr>(name: &str) -> Result<Option<T>, ConfigError> {
            match var(name) {
                None => Ok(None),
                Some(value) => value.parse().map(Some).map_err(|_| ConfigError::Env {
                    name: name.into(),
                    value,
                }),
            }
        }
        if let Some(v) = var("KERNOS_LISTEN") {
            self.listen = v;
        }
        if let Some(v) = var("KERNOS_DATA") {
            self.data_dir = PathBuf::from(v);
        }
        if let Some(v) = var("KERNOS_TOKEN") {
            self.token = Some(v);
        }
        if let Some(v) = parse("KERNOS_LEASE_TTL")? {
            self.lease_ttl_default = v;
        }
        if let Some(v) = parse("KERNOS_LEASE_TTL_MIN")? {
            self.lease_ttl_min = v;
        }
        if let Some(v) = parse("KERNOS_LEASE_TTL_MAX")? {
            self.lease_ttl_max = v;
        }
        if let Some(v) = parse("KERNOS_SWEEP_MS")? {
            self.lease_sweep_interval_ms = v;
        }
        if let Some(v) = parse("KERNOS_APPROVAL_SWEEP_MS")? {
            self.approval_sweep_interval_ms = v;
        }
        if let Some(v) = parse("KERNOS_BUDGET_SOFT_RATIO")? {
            self.budget_soft_ratio = v;
        }
        if let Some(v) = parse("KERNOS_MAX_ATTEMPTS")? {
            self.max_attempts_nondeterministic = v;
        }
        if let Some(v) = parse("KERNOS_MAX_DET_ATTEMPTS")? {
            self.max_attempts_deterministic = v;
        }
        if let Some(v) = var("KERNOS_LOG") {
            self.log_format = v;
        }
        Ok(())
    }

    /// The kernel's share of the configuration.
    pub fn kernel_config(&self) -> KernelConfig {
        KernelConfig {
            lease_ttl_default: self.lease_ttl_default,
            lease_ttl_min: self.lease_ttl_min,
            lease_ttl_max: self.lease_ttl_max,
            budget_soft_ratio: self.budget_soft_ratio,
            max_attempts_nondeterministic: self.max_attempts_nondeterministic,
            max_attempts_deterministic: self.max_attempts_deterministic,
            ..KernelConfig::default()
        }
    }
}
