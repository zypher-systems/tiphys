//! The core of Tiphys.
//!
//! Everything that is not a screen lives here: config, the key store, sessions
//! and the agent. The terminal app and, later, the daemon are built on it.

#![forbid(unsafe_code)]

pub mod actionlog;
pub mod agent;
pub mod approval;
pub mod cancel;
pub mod client;
pub mod compact;
pub mod config;
pub mod doctor;
pub mod error;
pub mod files;
pub mod host;
pub mod jsonl;
pub mod keys;
pub mod llm;
pub mod lock;
pub mod log;
pub mod policy;
pub mod prompt;
pub mod proto;
pub mod report;
pub mod runner;
pub mod session;
pub mod settings;
pub mod spend;
pub mod start;
pub mod tools;
pub mod wire;
pub mod worker;

pub use error::{Error, Result};

/// The version of this build.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
