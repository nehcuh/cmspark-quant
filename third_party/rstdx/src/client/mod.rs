//! TCP client for TDX protocol.

pub use connection::{Connection, MockConnection, MockResponse, TdxClient, TdxConnection};
pub use heartbeat::{HeartbeatClient, HeartbeatConfig, HeartbeatManager};
pub use managed::ManagedTdxClient;
pub use pool::{ConnectionPool, PoolConfig, PoolStats, PooledClient};
pub use reconnect::{ReconnectConfig, ReconnectManager, ReconnectingClient};

mod connection;
mod heartbeat;
mod managed;
mod pool;
mod reconnect;

/// Default TDX server port.
pub const DEFAULT_PORT: u16 = 7709;

/// Default heartbeat interval in seconds.
pub const DEFAULT_HEARTBEAT_INTERVAL_SECS: u64 = 10;
