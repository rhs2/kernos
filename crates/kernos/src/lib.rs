//! The `kernos` binary as a library: configuration, the HTTP server and the
//! command line, so the integration tests can start the server in-process.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod cli;
pub mod client;
pub mod config;
pub mod server;
