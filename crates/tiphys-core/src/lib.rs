//! The core of Tiphys.
//!
//! Everything that is not a screen lives here: config, the key store, sessions
//! and the agent. The terminal app and, later, the daemon are built on it.

#![forbid(unsafe_code)]

pub mod config;
pub mod error;
pub mod files;
pub mod jsonl;
pub mod keys;
pub mod llm;
pub mod log;
pub mod settings;
pub mod spend;

pub use error::{Error, Result};

/// The version of this build.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
