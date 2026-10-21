//! Error types for RSTDX.

use std::io;

use thiserror::Error;

/// Result type for RSTDX operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors that can occur in RSTDX operations.
#[derive(Debug, Error)]
pub enum Error {
    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// Connection error.
    #[error("connection error: {0}")]
    Connection(String),

    /// Protocol error.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// Encoding error.
    #[error("encoding error: {0}")]
    Encoding(String),

    /// Invalid response.
    #[error("invalid response: {0}")]
    InvalidResponse(String),

    /// Compression error.
    #[error("compression error: {0}")]
    Compression(String),

    /// Database error.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    /// No available server.
    #[error("no available server in pool")]
    NoAvailableServer,

    /// HTTP request error.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// JSON parse error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// All servers exhausted.
    #[error("all servers exhausted, tried {0} servers")]
    AllServersExhausted(usize),

    /// Pool closed.
    #[error("connection pool is closed")]
    PoolClosed,
}
