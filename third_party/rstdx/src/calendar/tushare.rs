//! Tushare HTTP API client for trading calendar data.

use crate::error::{Error, Result};
use chrono::{Datelike, NaiveDate};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};
use tracing::{debug, info};

const TUSHARE_API_URL: &str = "http://api.tushare.pro";

/// Tushare API request payload.
#[derive(Debug, Serialize)]
struct TushareRequest<'a> {
    api_name: &'a str,
    token: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fields: Option<&'a str>,
}

/// Tushare API response wrapper.
#[derive(Debug, Deserialize)]
struct TushareResponse {
    code: i32,
    msg: Option<String>,
    #[serde(default)]
    data: Option<TushareData>,
}

/// Inner data section of Tushare response.
#[derive(Debug, Deserialize)]
struct TushareData {
    #[allow(dead_code)]
    fields: Vec<String>,
    items: Vec<Vec<serde_json::Value>>,
}

/// A single trading-calendar entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeCalEntry {
    pub exchange: String,
    pub cal_date: String,
    pub is_open: bool,
}

/// Tushare trading-calendar client.
#[derive(Debug, Clone)]
pub struct TushareClient {
    client: Client,
    token: String,
}

impl TushareClient {
    /// Create a new client reading `TUSHARE_TOKEN` from the environment.
    pub fn new() -> Result<Self> {
        let token = env::var("TUSHARE_TOKEN").map_err(|_| {
            Error::InvalidResponse("TUSHARE_TOKEN environment variable not set".into())
        })?;
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| Error::Connection(format!("Failed to build HTTP client: {e}")))?;
        Ok(Self { client, token })
    }

    /// Fetch trade-calendar entries from Tushare.
    ///
    /// # Arguments
    /// * `exchange` - Exchange code, e.g. "SSE" for Shanghai, "SZSE" for Shenzhen
    /// * `start_date` - Start date in YYYYMMDD format
    /// * `end_date` - End date in YYYYMMDD format
    pub async fn fetch_trade_calendar(
        &self,
        exchange: &str,
        start_date: &str,
        end_date: &str,
    ) -> Result<Vec<TradeCalEntry>> {
        let params = serde_json::json!({
            "exchange": exchange,
            "start_date": start_date,
            "end_date": end_date,
        });

        let req = TushareRequest {
            api_name: "trade_cal",
            token: &self.token,
            params: Some(params),
            fields: Some("exchange,cal_date,is_open"),
        };

        debug!(
            exchange,
            start_date, end_date, "Fetching trade calendar from Tushare"
        );

        let resp = self
            .client
            .post(TUSHARE_API_URL)
            .json(&req)
            .send()
            .await
            .map_err(|e| Error::Connection(format!("HTTP request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(Error::InvalidResponse(format!("HTTP error: {status}")));
        }

        let body: TushareResponse = resp
            .json()
            .await
            .map_err(|e| Error::InvalidResponse(format!("Failed to parse JSON: {e}")))?;

        if body.code != 0 {
            return Err(Error::InvalidResponse(
                body.msg
                    .unwrap_or_else(|| format!("Tushare API error code: {}", body.code)),
            ));
        }

        let data = body
            .data
            .ok_or_else(|| Error::InvalidResponse("No data in response".into()))?;

        let mut entries = Vec::with_capacity(data.items.len());
        for row in data.items {
            if row.len() >= 3 {
                let exchange = row[0].as_str().unwrap_or("").to_string();
                let cal_date = row[1].as_str().unwrap_or("").to_string();
                let is_open = match &row[2] {
                    serde_json::Value::String(s) => s == "1",
                    serde_json::Value::Number(n) => n.as_i64() == Some(1),
                    _ => false,
                };
                entries.push(TradeCalEntry {
                    exchange,
                    cal_date,
                    is_open,
                });
            }
        }

        info!("Fetched {} trade calendar entries from Tushare", entries.len());
        Ok(entries)
    }
}

/// On-disk cache for Tushare trade-calendar data.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TradeCalCache {
    pub fetched_at: Option<SystemTime>,
    /// Year when the cache was fetched (e.g. 2025).
    pub fetched_year: Option<u32>,
    /// Maximum year covered by the cached entries.
    pub max_year: Option<u32>,
    pub entries: Vec<TradeCalEntry>,
}

impl TradeCalCache {
    /// Load cache from disk, returning an empty cache if the file does not exist.
    pub fn load(path: &PathBuf) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = fs::read_to_string(path).map_err(Error::Io)?;
        let cache: TradeCalCache = serde_json::from_str(&content).map_err(Error::Json)?;
        debug!(
            entries = cache.entries.len(),
            "Loaded Tushare trade-calendar cache"
        );
        Ok(cache)
    }

    /// Persist cache to disk.
    pub fn save(&self, path: &PathBuf) -> Result<()> {
        let content = serde_json::to_string_pretty(self).map_err(Error::Json)?;
        fs::write(path, content).map_err(Error::Io)?;
        Ok(())
    }

    /// Returns `true` if the cache should be refreshed.
    ///
    /// A cache is considered fresh when it was fetched in the current calendar year
    /// and covers the current year. Historical years never expire.
    pub fn is_expired(&self) -> bool {
        let current_year = chrono::Utc::now().year() as u32;

        let fetched_year = self.fetched_year.unwrap_or(0);
        let covers_current = self.max_year.is_some_and(|max| max >= current_year);

        // Expired if it doesn't cover the current year, or wasn't fetched this year.
        !covers_current || fetched_year != current_year
    }

    /// Compute `max_year` from the cached entries.
    pub fn compute_max_year(&mut self) {
        self.max_year = self
            .entries
            .iter()
            .filter_map(|e| e.cal_date.get(..4)?.parse().ok())
            .max();
    }

    /// Convert entries to a [`HashSet<NaiveDate>`] of trading days.
    pub fn to_trading_dates(&self) -> HashSet<NaiveDate> {
        let mut set = HashSet::new();
        for entry in &self.entries {
            if entry.is_open
                && let Ok(date) = NaiveDate::parse_from_str(&entry.cal_date, "%Y%m%d")
            {
                set.insert(date);
            }
        }
        set
    }
}
