//! cmsq-core: CMspark Quant P0 core (ingest → extract → sandbox backtest → evaluate → promote).
pub mod audit;
pub mod data;
pub mod replay;
pub mod extract;
pub mod gate;
pub mod home;
pub mod ingest;
pub mod market;
pub mod models;
pub mod orchestrator;
pub mod sandbox;
pub mod scorer;
pub mod probe;
pub mod error;
pub mod state;
pub mod store;
pub mod ctp;
pub mod futures;
pub mod tradestats;
pub mod qifi;
pub mod calendars;

pub use home::Home;
pub use models::*;
pub use orchestrator::Orchestrator;
pub use store::Store;
