//! IP discovery module for detecting and managing TDX servers.

pub use detector::{DetectionConfig, IpDetector, ProbeResult};
pub use source::{HttpSource, ServerInfo, ServerSource, StaticListSource};
pub use store::{ServerRecord, ServerStats, ServerStore, StoreConfig};

pub mod detector;
pub mod source;
pub mod store;
