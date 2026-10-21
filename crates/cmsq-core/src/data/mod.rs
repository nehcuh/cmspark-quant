//! Multi-source market data: tushare (primary, adj_factor), akshare and
//! baostock (python helper), rstdx/TDX (feature `tdx`), behind a [`router::Router`].
pub mod pyhelper;
pub mod ratelimit;
pub mod router;
pub mod tushare;

use crate::market::MarketDataSource;
use std::sync::Arc;

/// Build sources from a comma list (default `CMSQ_DATA_SOURCES` or
/// "tushare,akshare,baostock,tdx"). Unavailable ones (no token, no feature) are skipped with a note.
pub fn build_sources(
    list: Option<&str>,
    secrets_dir: &std::path::Path,
) -> (Vec<Arc<dyn MarketDataSource>>, Vec<String>) {
    let env = std::env::var("CMSQ_DATA_SOURCES").ok();
    let list = list
        .map(String::from)
        .or(env)
        .unwrap_or_else(|| "tushare,akshare,baostock,tdx".into());
    let mut out: Vec<Arc<dyn MarketDataSource>> = vec![];
    let mut notes = vec![];
    for name in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        match name {
            "tushare" => match tushare::Tushare::from_secrets(secrets_dir) {
                Ok(t) => out.push(Arc::new(t)),
                Err(e) => notes.push(format!("tushare skipped: {e}")),
            },
            "akshare" | "baostock" => {
                out.push(Arc::new(pyhelper::PyProvider::new(if name == "akshare" {
                    "akshare"
                } else {
                    "baostock"
                })))
            }
            "tdx" | "rstdx" => {
                #[cfg(feature = "tdx")]
                out.push(Arc::new(crate::market::RstdxSource {
                    server: std::env::var("CMSQ_TDX_SERVER").ok(),
                }));
                #[cfg(not(feature = "tdx"))]
                notes.push("tdx skipped: built without feature `tdx`".into());
            }
            other => notes.push(format!("unknown source {other}")),
        }
    }
    (out, notes)
}
