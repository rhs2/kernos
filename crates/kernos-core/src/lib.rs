//! The Kernos kernel as a library.
//!
//! Everything durable in Kernos lives here: the hash-chained event log, the
//! pure [`fold`](fold::fold), the SQLite store, the scheduler with leases and
//! retries, budgets, remits, bundles, approvals, compensation and replay. The
//! `kernos` binary is a thin HTTP and CLI layer over [`kernel::Kernel`].

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod bundle;
pub mod canonical;
pub mod clock;
pub mod error;
pub mod events;
pub mod fold;
pub mod ids;
pub mod kernel;
pub mod keys;
pub mod metrics;
pub mod remit;
pub mod replay;
pub mod schema;
pub mod store;
pub mod template;
pub mod time;

pub use error::{KernelError, KernelResult};
pub use kernel::{Kernel, KernelConfig};
