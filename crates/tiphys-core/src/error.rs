//! The one error type of the core crate.
//!
//! Variants carry a message that is ready to show. A caller decides what to do
//! from the variant, never by reading the text.

/// What can go wrong in the core.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A config or settings file says something Tiphys cannot use.
    #[error("config: {0}")]
    Config(String),
    /// A file could not be read or written.
    #[error("io: {0}")]
    Io(String),
    /// The model provider refused, failed or sent something unusable. The
    /// message is in words the owner can act on.
    #[error("{0}")]
    Provider(String),
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
