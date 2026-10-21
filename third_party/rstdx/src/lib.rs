//! # RSTDX
//!
//! Pure Rust implementation of TongDaXin (TDX) stock data protocol.

#![allow(unexpected_cfgs)]
#![allow(unsafe_op_in_unsafe_fn)]

pub mod calendar;
pub mod client;
pub mod codec;
pub mod commands;
pub mod discovery;
pub mod error;
pub mod protocol;
pub mod query;

// Python bindings (only when python feature is enabled)
#[cfg(feature = "python")]
pub mod python;

pub use client::{ConnectionPool, TdxClient};
pub use error::{Error, Result};
