//! High-level query API implementation.

use super::{BarData, QuoteData};
use crate::calendar::{Frequency, Market, TradingCalendar, parse_date_flexible, NormalizeDirection};
use crate::client::TdxClient;
use crate::client::ManagedTdxClient;
use crate::commands::{FinanceInfo, Kline, MinuteTimeData, SecurityItem, SecurityQuote, TransactionData};
use crate::error::{Error, Result};
use chrono::NaiveDate;
use std::time::Duration;
use tracing::{debug, info, trace, warn};

/// Trait abstracting a data source for market queries.
///
/// This allows [`QueryApi`] to work with any client implementation
/// that can fetch K-line and quote data — not just [`TdxClient`].
pub trait DataSource: Send {
    /// Fetch K-line (candlestick) data.
    fn get_security_bars(
        &mut self,
        category: u16,
        market: u8,
        code: &str,
        start: u16,
        count: u16,
    ) -> impl std::future::Future<Output = Result<Vec<Kline>>> + Send;

    /// Fetch real-time quotes for multiple stocks.
    fn get_security_quotes(
        &mut self,
        stocks: &[(u8, &str)],
    ) -> impl std::future::Future<Output = Result<Vec<SecurityQuote>>> + Send;

    /// Fetch security list.
    fn get_security_list(
        &mut self,
        market: u8,
        start: u16,
    ) -> impl std::future::Future<Output = Result<Vec<SecurityItem>>> + Send;

    /// Fetch finance info.
    fn get_finance_info(
        &mut self,
        market: u8,
        code: &str,
    ) -> impl std::future::Future<Output = Result<FinanceInfo>> + Send;

    /// Fetch tick-by-tick transaction data.
    fn get_transaction_data(
        &mut self,
        market: u8,
        code: &str,
        start: u16,
        count: u16,
    ) -> impl std::future::Future<Output = Result<Vec<TransactionData>>> + Send;

    /// Fetch minute time-sharing data.
    fn get_minute_time_data(
        &mut self,
        market: u8,
        code: &str,
    ) -> impl std::future::Future<Output = Result<Vec<MinuteTimeData>>> + Send;
}

impl DataSource for TdxClient {
    async fn get_security_bars(
        &mut self,
        category: u16,
        market: u8,
        code: &str,
        start: u16,
        count: u16,
    ) -> Result<Vec<Kline>> {
        TdxClient::get_security_bars(self, category, market, code, start, count).await
    }

    async fn get_security_quotes(&mut self, stocks: &[(u8, &str)]) -> Result<Vec<SecurityQuote>> {
        TdxClient::get_security_quotes(self, stocks).await
    }

    async fn get_security_list(&mut self, market: u8, start: u16) -> Result<Vec<SecurityItem>> {
        TdxClient::get_security_list(self, market, start).await
    }

    async fn get_finance_info(&mut self, market: u8, code: &str) -> Result<FinanceInfo> {
        TdxClient::get_finance_info(self, market, code).await
    }

    async fn get_transaction_data(
        &mut self,
        market: u8,
        code: &str,
        start: u16,
        count: u16,
    ) -> Result<Vec<TransactionData>> {
        TdxClient::get_transaction_data(self, market, code, start, count).await
    }

    async fn get_minute_time_data(
        &mut self,
        market: u8,
        code: &str,
    ) -> Result<Vec<MinuteTimeData>> {
        TdxClient::get_minute_time_data(self, market, code).await
    }
}

impl DataSource for ManagedTdxClient {
    async fn get_security_bars(
        &mut self,
        category: u16,
        market: u8,
        code: &str,
        start: u16,
        count: u16,
    ) -> Result<Vec<Kline>> {
        ManagedTdxClient::get_security_bars(self, category, market, code, start, count).await
    }

    async fn get_security_quotes(&mut self, stocks: &[(u8, &str)]) -> Result<Vec<SecurityQuote>> {
        ManagedTdxClient::get_security_quotes(self, stocks).await
    }

    async fn get_security_list(&mut self, market: u8, start: u16) -> Result<Vec<SecurityItem>> {
        ManagedTdxClient::get_security_list(self, market, start).await
    }

    async fn get_finance_info(&mut self, market: u8, code: &str) -> Result<FinanceInfo> {
        ManagedTdxClient::get_finance_info(self, market, code).await
    }

    async fn get_transaction_data(
        &mut self,
        market: u8,
        code: &str,
        start: u16,
        count: u16,
    ) -> Result<Vec<TransactionData>> {
        ManagedTdxClient::get_transaction_data(self, market, code, start, count).await
    }

    async fn get_minute_time_data(
        &mut self,
        market: u8,
        code: &str,
    ) -> Result<Vec<MinuteTimeData>> {
        ManagedTdxClient::get_minute_time_data(self, market, code).await
    }
}

/// Retry configuration for transient failures.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_retries: usize,
    pub initial_delay: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 2,
            initial_delay: Duration::from_millis(200),
        }
    }
}

fn is_transient(err: &Error) -> bool {
    matches!(err, Error::Io(_) | Error::Connection(_))
}

/// Options for querying data.
#[derive(Debug, Clone)]
pub struct QueryOptions {
    pub adjust: String, // 'none', 'qfq', 'hfq'
    pub limit: usize,
}

impl Default for QueryOptions {
    fn default() -> Self {
        Self {
            adjust: "none".to_string(),
            limit: 5000,
        }
    }
}

/// High-level query API with date range support.
pub struct QueryApi<D: DataSource = TdxClient> {
    client: D,
    calendar: TradingCalendar,
    market_override: Option<u8>,
    retry_config: RetryConfig,
}

impl QueryApi {
    /// Create a new query API from a TDX client.
    pub fn new(client: TdxClient) -> Result<Self> {
        Ok(Self {
            client,
            calendar: TradingCalendar::new()?,
            market_override: None,
            retry_config: RetryConfig::default(),
        })
    }

    /// Create a new query API with a custom trading calendar.
    pub fn with_calendar(client: TdxClient, calendar: TradingCalendar) -> Self {
        Self {
            client,
            calendar,
            market_override: None,
            retry_config: RetryConfig::default(),
        }
    }

    /// Get the client reference for direct access.
    pub fn client(&mut self) -> &mut TdxClient {
        &mut self.client
    }
}

impl<D: DataSource> QueryApi<D> {
    /// Create a new query API from any data source.
    pub fn with_source(client: D) -> Result<Self> {
        Ok(Self {
            client,
            calendar: TradingCalendar::new()?,
            market_override: None,
            retry_config: RetryConfig::default(),
        })
    }

    /// Create a new query API with a custom calendar and any data source.
    pub fn with_source_and_calendar(client: D, calendar: TradingCalendar) -> Self {
        Self {
            client,
            calendar,
            market_override: None,
            retry_config: RetryConfig::default(),
        }
    }

    /// Override market auto-detection.
    ///
    /// Use this when querying Shanghai indices whose codes start with "0"
    /// (e.g., 000001 上证指数, 000300 沪深300), since auto-detection cannot
    /// distinguish them from Shenzhen stocks in the same code range.
    pub fn set_market_override(&mut self, market: u8) {
        self.market_override = Some(market);
    }

    /// Set the retry configuration for transient failures.
    pub fn set_retry_config(&mut self, config: RetryConfig) {
        self.retry_config = config;
    }

    /// Get market code from stock code, respecting any override.
    fn get_market(&self, code: &str) -> u8 {
        if let Some(m) = self.market_override {
            return m;
        }
        Market::from_code(code).to_tdx_code()
    }

    /// Parse date string to NaiveDate (supports "YYYY-MM-DD" and "YYYYMMDD").
    fn parse_date(&self, date_str: &str) -> Result<NaiveDate> {
        parse_date_flexible(date_str)
    }

    /// Get K-line data for a stock within a date range.
    ///
    /// # Arguments
    /// * `code` - Stock code (e.g., "600000", "000001")
    /// * `start_date` - Start date in "YYYY-MM-DD" format
    /// * `end_date` - End date in "YYYY-MM-DD" format
    /// * `frequency` - Data frequency ("1min", "5min", "15min", "30min", "60min", "day", "week", "month")
    ///
    /// # Example
    /// ```no_run
    /// # use rstdx::query::QueryApi;
    /// # async fn example(api: &mut QueryApi) -> Result<(), Box<dyn std::error::Error>> {
    /// let bars = api.get_bars("600000", "2024-01-01", "2024-01-31", "day").await?;
    /// # Ok(())
    /// # }
    /// ```
    async fn fetch_bars_with_retry(
        &mut self,
        category: u16,
        market: u8,
        code: &str,
        start: u16,
        count: u16,
    ) -> Result<Vec<Kline>> {
        let mut last_err = None;
        for attempt in 0..=self.retry_config.max_retries {
            match self.client.get_security_bars(category, market, code, start, count).await {
                Ok(bars) => return Ok(bars),
                Err(e) if is_transient(&e) && attempt < self.retry_config.max_retries => {
                    let delay = self.retry_config.initial_delay * (1 << attempt) as u32;
                    warn!("Transient error (attempt {}): {}, retrying in {:?}", attempt + 1, e, delay);
                    tokio::time::sleep(delay).await;
                    last_err = Some(e);
                }
                Err(e) => return Err(e),
            }
        }
        Err(last_err.unwrap_or(Error::Connection("all retry attempts failed".into())))
    }

    pub async fn get_bars(
        &mut self,
        code: &str,
        start_date: &str,
        end_date: &str,
        frequency: &str,
    ) -> Result<Vec<BarData>> {
        let market = self.get_market(code);
        let freq = Frequency::from_str(frequency);
        let freq_code = freq.to_tdx_code();

        // Normalize non-trading boundaries:
        // start_date → next trading day, end_date → previous trading day
        let mut start = self.parse_date(start_date)?;
        if !self.calendar.is_trading_day(start)
            && let Some(normalized) = self.calendar.normalize_to_trade_date(start, NormalizeDirection::Next)
        {
            info!(
                "start_date {} is not a trading day, normalized to {}",
                start, normalized
            );
            start = normalized;
        }

        let today = chrono::Utc::now().date_naive();
        let mut end = self.parse_date(end_date)?.min(today);
        if !self.calendar.is_trading_day(end)
            && let Some(normalized) = self.calendar.normalize_to_trade_date(end, NormalizeDirection::Previous)
        {
            info!(
                "end_date {} is not a trading day, normalized to {}",
                end, normalized
            );
            end = normalized;
        }

        debug!(
            "Querying bars for {} from {} to {} (freq: {})",
            code, start, end, frequency
        );

        // TDX API returns data from newest to oldest (offset 0 = newest)
        // To get historical data, we need to fetch from start_date to today
        let days_to_fetch = if start > today {
            return Ok(Vec::new());
        } else {
            self.calendar.get_trade_gap(start, today)
        };

        let multiplier = freq.get_multiplier();
        let total_bars = days_to_fetch * multiplier;

        trace!(
            "Need to fetch approximately {} bars ({} trading days * {} multiplier)",
            total_bars, days_to_fetch, multiplier
        );

        // Fetch data in chunks (max 800 per request)
        let mut all_bars: Vec<Kline> = Vec::new();
        let total_bars_u32 = (total_bars as u32).min(800 * 65535);
        let chunks = (total_bars_u32 + 799) / 800;

        for i in 0..chunks {
            let offset = (i * 800) as u16;
            let remaining = total_bars_u32.saturating_sub(i * 800);
            let count = remaining.min(800) as u16;

            trace!("Fetching chunk {}: offset={}, count={}", i, offset, count);

            let bars = self
                .fetch_bars_with_retry(freq_code, market, code, offset, count)
                .await?;

            let bars_len = bars.len();
            all_bars.extend(bars);

            // Stop if we got less than requested (reached beginning of data)
            if bars_len < count as usize {
                break;
            }
        }

        // Convert to BarData
        let bar_data: Vec<BarData> = all_bars.into_iter().map(BarData::from).collect();

        // Filter by date range and reverse to get chronological order
        let mut filtered: Vec<BarData> = bar_data
            .into_iter()
            .filter(|bar| {
                NaiveDate::parse_from_str(&bar.date, "%Y-%m-%d")
                    .map(|d| d >= start && d <= end)
                    .unwrap_or(false)
            })
            .collect();

        // Reverse to get chronological order (oldest first)
        filtered.reverse();

        debug!("Retrieved {} bars in date range", filtered.len());

        Ok(filtered)
    }

    /// Get daily K-line data.
    pub async fn get_daily_bars(
        &mut self,
        code: &str,
        start_date: &str,
        end_date: &str,
    ) -> Result<Vec<BarData>> {
        self.get_bars(code, start_date, end_date, "day").await
    }

    /// Get minute K-line data.
    pub async fn get_minute_bars(
        &mut self,
        code: &str,
        start_date: &str,
        end_date: &str,
        frequency: &str,
    ) -> Result<Vec<BarData>> {
        self.get_bars(code, start_date, end_date, frequency).await
    }

    /// Get real-time quotes for multiple stocks.
    ///
    /// # Arguments
    /// * `codes` - List of stock codes
    ///
    /// # Example
    /// ```no_run
    /// # use rstdx::query::QueryApi;
    /// # async fn example(api: &mut QueryApi) -> Result<(), Box<dyn std::error::Error>> {
    /// let quotes = api.get_quotes(&["600000", "000001"]).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_quotes(&mut self, codes: &[&str]) -> Result<Vec<QuoteData>> {
        debug!("Fetching quotes for {} stocks", codes.len());

        let stocks: Vec<(u8, &str)> = codes
            .iter()
            .map(|code| (self.get_market(code), *code))
            .collect();

        let quotes = self.client.get_security_quotes(&stocks).await?;

        let result: Vec<QuoteData> = quotes
            .iter()
            .zip(codes.iter())
            .map(|(quote, code)| QuoteData::from_quote_with_code(quote, code))
            .collect();

        Ok(result)
    }

    /// Get real-time quote for a single stock.
    pub async fn get_quote(&mut self, code: &str) -> Result<QuoteData> {
        let mut quotes = self.get_quotes(&[code]).await?;
        quotes
            .pop()
            .ok_or_else(|| Error::InvalidResponse("No quote data returned".to_string()))
    }

    /// Get latest bars (most recent N bars).
    pub async fn get_latest_bars(
        &mut self,
        code: &str,
        frequency: &str,
        count: u16,
    ) -> Result<Vec<BarData>> {
        let market = self.get_market(code);
        let freq = Frequency::from_str(frequency);
        let freq_code = freq.to_tdx_code();

        let bars = self
            .client
            .get_security_bars(freq_code, market, code, 0, count)
            .await?;

        Ok(bars.into_iter().map(BarData::from).collect())
    }

    /// Check if a date is a trading day.
    pub fn is_trading_day(&self, date: &str) -> Result<bool> {
        let date = self.parse_date(date)?;
        Ok(self.calendar.is_trading_day(date))
    }

    /// Get the previous trading day.
    pub fn previous_trading_day(&self, date: &str) -> Result<String> {
        let date = self.parse_date(date)?;
        self.calendar
            .get_pre_trade_date(date, 1, false)
            .map(|d: NaiveDate| d.format("%Y-%m-%d").to_string())
            .ok_or_else(|| Error::InvalidResponse("No previous trading day found".to_string()))
    }

    /// Get the next trading day.
    pub fn next_trading_day(&self, date: &str) -> Result<String> {
        let date = self.parse_date(date)?;
        self.calendar
            .get_next_trade_date(date, 1, false)
            .map(|d: NaiveDate| d.format("%Y-%m-%d").to_string())
            .ok_or_else(|| Error::InvalidResponse("No next trading day found".to_string()))
    }

    /// Get the calendar reference for direct access.
    pub fn calendar(&self) -> &TradingCalendar {
        &self.calendar
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_query_options_default() {
        let opts = QueryOptions::default();
        assert_eq!(opts.adjust, "none");
        assert_eq!(opts.limit, 5000);
    }

    #[test]
    fn test_bardata_from_kline() {
        let kline = crate::commands::Kline {
            year: 2024,
            month: 4,
            day: 24,
            hour: 15,
            minute: 0,
            open: 10.0,
            high: 11.0,
            low: 9.5,
            close: 10.5,
            volume: 1000000.0,
            amount: 10000000.0,
        };
        let bar = BarData::from(kline);
        assert_eq!(bar.date, "2024-04-24");
        assert_eq!(bar.datetime, "2024-04-24 15:00");
        assert_eq!(bar.open, 10.0);
        assert_eq!(bar.close, 10.5);
    }

    #[test]
    fn test_quotedata_from_quote() {
        let quote = crate::commands::SecurityQuote {
            market: 1,
            code: "600000".to_string(),
            price: 11.0,
            last_close: 10.0,
            open: 10.0,
            high: 11.5,
            low: 9.8,
            volume: 1000000.0,
            amount: 11000000.0,
            bid1: 10.99,
            ask1: 11.01,
            bid_vol1: 100.0,
            ask_vol1: 200.0,
            bid2: 0.0,
            ask2: 0.0,
            bid_vol2: 0.0,
            ask_vol2: 0.0,
            bid3: 0.0,
            ask3: 0.0,
            bid_vol3: 0.0,
            ask_vol3: 0.0,
            bid4: 0.0,
            ask4: 0.0,
            bid_vol4: 0.0,
            ask_vol4: 0.0,
            bid5: 0.0,
            ask5: 0.0,
            bid_vol5: 0.0,
            ask_vol5: 0.0,
        };
        let qd = QuoteData::from_quote_with_code(&quote, "600000");
        assert_eq!(qd.code, "600000");
        assert_eq!(qd.change_pct, 10.0);
    }

    #[test]
    fn test_quotedata_change_pct_zero() {
        let quote = crate::commands::SecurityQuote {
            market: 0,
            code: "000001".to_string(),
            price: 10.0,
            last_close: 0.0,
            open: 0.0,
            high: 0.0,
            low: 0.0,
            volume: 0.0,
            amount: 0.0,
            bid1: 0.0,
            ask1: 0.0,
            bid_vol1: 0.0,
            ask_vol1: 0.0,
            bid2: 0.0,
            ask2: 0.0,
            bid_vol2: 0.0,
            ask_vol2: 0.0,
            bid3: 0.0,
            ask3: 0.0,
            bid_vol3: 0.0,
            ask_vol3: 0.0,
            bid4: 0.0,
            ask4: 0.0,
            bid_vol4: 0.0,
            ask_vol4: 0.0,
            bid5: 0.0,
            ask5: 0.0,
            bid_vol5: 0.0,
            ask_vol5: 0.0,
        };
        let qd = QuoteData::from_quote_with_code(&quote, "000001");
        assert_eq!(qd.change_pct, 0.0);
    }

    #[test]
    fn test_date_filtering_with_naive_date() {
        let bars = vec![
            BarData {
                datetime: "2024-01-15 15:00".into(),
                date: "2024-01-15".into(),
                open: 10.0, high: 10.5, low: 9.8, close: 10.2,
                volume: 1000.0, amount: 10000.0,
            },
            BarData {
                datetime: "2024-02-01 15:00".into(),
                date: "2024-02-01".into(),
                open: 10.2, high: 11.0, low: 10.0, close: 10.8,
                volume: 2000.0, amount: 20000.0,
            },
            BarData {
                datetime: "2024-03-10 15:00".into(),
                date: "2024-03-10".into(),
                open: 10.8, high: 12.0, low: 10.5, close: 11.5,
                volume: 3000.0, amount: 30000.0,
            },
        ];

        let start = NaiveDate::from_ymd_opt(2024, 1, 20).unwrap();
        let end = NaiveDate::from_ymd_opt(2024, 3, 1).unwrap();

        let filtered: Vec<BarData> = bars
            .into_iter()
            .filter(|bar| {
                NaiveDate::parse_from_str(&bar.date, "%Y-%m-%d")
                    .map(|d| d >= start && d <= end)
                    .unwrap_or(false)
            })
            .collect();

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].date, "2024-02-01");
    }
}
