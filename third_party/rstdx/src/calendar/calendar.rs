//! Trading calendar implementation.

use crate::calendar::tushare::{TradeCalCache, TushareClient};
use crate::error::{Error, Result};
use chrono::{Datelike, Duration, NaiveDate, Weekday};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use tracing::{debug, info, warn};

/// Trading calendar for managing trading dates.
#[derive(Debug, Clone)]
pub struct TradingCalendar {
    trading_dates: HashSet<NaiveDate>,
    holidays: HashSet<NaiveDate>,
    cache_dir: PathBuf,
}

impl TradingCalendar {
    /// Create a new trading calendar with default cache directory.
    ///
    /// # Example
    /// ```
    /// let calendar = rstdx::calendar::TradingCalendar::new().unwrap();
    /// ```
    pub fn new() -> Result<Self> {
        let cache_dir = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".rstdx");
        Self::with_cache_dir(cache_dir)
    }

    /// Create a new trading calendar with specified cache directory.
    pub fn with_cache_dir(cache_dir: PathBuf) -> Result<Self> {
        let mut calendar = Self {
            trading_dates: HashSet::new(),
            holidays: HashSet::new(),
            cache_dir,
        };

        // Create cache directory if it doesn't exist
        fs::create_dir_all(&calendar.cache_dir)?;

        // Load cached data
        calendar.load_cache()?;

        Ok(calendar)
    }

    /// Load cached trading dates and holidays.
    fn load_cache(&mut self) -> Result<()> {
        let tushare_cache_path = self.cache_dir.join("tushare_trade_cal.json");

        // Prefer Tushare cache if it exists and is fresh
        if tushare_cache_path.exists() {
            match TradeCalCache::load(&tushare_cache_path) {
                Ok(cache) => {
                    if !cache.is_expired() {
                        self.trading_dates = cache.to_trading_dates();
                        info!(
                            "Loaded {} trading dates from Tushare cache",
                            self.trading_dates.len()
                        );
                        // Load holidays for reference, but do not use them to filter
                        let _ = self.load_holidays_from_file();
                        return Ok(());
                    }
                    info!("Tushare trade calendar cache expired, falling back to default logic");
                }
                Err(e) => {
                    warn!(
                        "Failed to load Tushare cache ({}), falling back to default logic",
                        e
                    );
                }
            }
        }

        let trade_dates_file = self.cache_dir.join("trade_dates.txt");
        let holidays_file = self.cache_dir.join("holidays.txt");

        // Load trading dates
        if trade_dates_file.exists() {
            let file = File::open(&trade_dates_file)?;
            for line in BufReader::new(file).lines() {
                let date_str = line?;
                if let Ok(date) = NaiveDate::parse_from_str(&date_str, "%Y-%m-%d") {
                    self.trading_dates.insert(date);
                }
            }
            debug!(
                "Loaded {} trading dates from cache",
                self.trading_dates.len()
            );
        } else {
            // Initialize with default trading dates (weekdays excluding holidays)
            self.initialize_default_dates()?;
        }

        // Load holidays
        if holidays_file.exists() {
            let file = File::open(&holidays_file)?;
            for line in BufReader::new(file).lines() {
                let date_str = line?;
                if let Ok(date) = NaiveDate::parse_from_str(&date_str, "%Y-%m-%d") {
                    self.holidays.insert(date);
                }
            }
            debug!("Loaded {} holidays from cache", self.holidays.len());
        } else {
            // Initialize with Chinese market holidays
            self.initialize_default_holidays()?;
        }

        Ok(())
    }

    /// Load holidays from cache file only (no fallback to default).
    fn load_holidays_from_file(&mut self) -> Result<()> {
        let holidays_file = self.cache_dir.join("holidays.txt");
        if holidays_file.exists() {
            let file = File::open(&holidays_file)?;
            for line in BufReader::new(file).lines() {
                let date_str = line?;
                if let Ok(date) = NaiveDate::parse_from_str(&date_str, "%Y-%m-%d") {
                    self.holidays.insert(date);
                }
            }
            debug!("Loaded {} holidays from cache", self.holidays.len());
        }
        Ok(())
    }

    /// Refresh trading calendar from Tushare HTTP API.
    ///
    /// This requires the `TUSHARE_TOKEN` environment variable to be set.
    /// On success, the data is persisted to the local cache and immediately
    /// used by this calendar instance.
    ///
    /// When `force` is `false`, the refresh is skipped if a fresh cache for the
    /// current year already exists. Set `force` to `true` to always re-fetch.
    ///
    /// # Example
    /// ```no_run
    /// # tokio_test::block_on(async {
    /// let mut calendar = rstdx::calendar::TradingCalendar::new().unwrap();
    /// calendar.refresh_from_tushare(false).await.unwrap();
    /// # });
    /// ```
    pub async fn refresh_from_tushare(&mut self, force: bool) -> Result<()> {
        if !force {
            let cache_path = self.cache_dir.join("tushare_trade_cal.json");
            if cache_path.exists() {
                match TradeCalCache::load(&cache_path) {
                    Ok(cache) => {
                        if !cache.is_expired() {
                            info!("Tushare cache is still fresh, skipping refresh");
                            return Ok(());
                        }
                    }
                    Err(e) => {
                        warn!("Failed to load existing cache ({}), will refresh", e);
                    }
                }
            }
        }

        let client = TushareClient::new()?;
        let entries = client
            .fetch_trade_calendar("SSE", "20000101", "20301231")
            .await?;

        let current_year = chrono::Utc::now().year() as u32;
        let mut cache = TradeCalCache {
            fetched_at: Some(std::time::SystemTime::now()),
            fetched_year: Some(current_year),
            max_year: None,
            entries,
        };
        cache.compute_max_year();

        let cache_path = self.cache_dir.join("tushare_trade_cal.json");
        cache.save(&cache_path)?;
        self.trading_dates = cache.to_trading_dates();

        info!(
            "Trading calendar refreshed from Tushare, {} dates available",
            self.trading_dates.len()
        );
        Ok(())
    }

    /// Returns `true` if the Tushare cache is missing or has expired.
    pub fn is_tushare_cache_stale(&self) -> bool {
        let cache_path = self.cache_dir.join("tushare_trade_cal.json");
        match TradeCalCache::load(&cache_path) {
            Ok(cache) => cache.is_expired(),
            Err(_) => true,
        }
    }

    /// Initialize default trading dates (all weekdays).
    fn initialize_default_dates(&mut self) -> Result<()> {
        info!("Initializing default trading dates");
        let start = NaiveDate::from_ymd_opt(2000, 1, 1).unwrap();
        let end = NaiveDate::from_ymd_opt(2030, 12, 31).unwrap();

        let mut current = start;
        while current <= end {
            if self.is_weekday(current) {
                self.trading_dates.insert(current);
            }
            current += Duration::days(1);
        }

        // Remove holidays
        for &holiday in &self.holidays {
            self.trading_dates.remove(&holiday);
        }

        self.save_cache()?;
        Ok(())
    }

    /// Initialize default Chinese market holidays.
    fn initialize_default_holidays(&mut self) -> Result<()> {
        info!("Initializing default holidays");
        // Common Chinese market holidays (simplified list)
        // In production, this should be fetched from a reliable source

        let holidays_2024 = vec![
            "2024-01-01", // New Year's Day
            "2024-02-10",
            "2024-02-11",
            "2024-02-12",
            "2024-02-13",
            "2024-02-14", // Spring Festival
            "2024-02-09",
            "2024-02-16",
            "2024-02-17", // Spring Festival adjusted
            "2024-04-04",
            "2024-04-05",
            "2024-04-06", // Qingming Festival
            "2024-05-01",
            "2024-05-02",
            "2024-05-03",
            "2024-05-04",
            "2024-05-05", // Labor Day
            "2024-06-10", // Dragon Boat Festival
            "2024-09-15",
            "2024-09-16",
            "2024-09-17", // Mid-Autumn Festival
            "2024-10-01",
            "2024-10-02",
            "2024-10-03",
            "2024-10-04",
            "2024-10-05",
            "2024-10-06",
            "2024-10-07", // National Day
        ];

        let holidays_2025 = vec![
            "2025-01-01", // New Year's Day
            "2025-01-28",
            "2025-01-29",
            "2025-01-30",
            "2025-01-31", // Spring Festival
            "2025-02-01",
            "2025-02-02",
            "2025-02-03",
            "2025-02-04",
            "2025-04-04",
            "2025-04-05",
            "2025-04-06", // Qingming Festival
            "2025-05-01",
            "2025-05-02",
            "2025-05-05", // Labor Day
            "2025-05-31",
            "2025-06-02", // Dragon Boat Festival
            "2025-10-01",
            "2025-10-02",
            "2025-10-03",
            "2025-10-06",
            "2025-10-07",
            "2025-10-08", // National Day
        ];

        for date_str in holidays_2024.iter().chain(holidays_2025.iter()) {
            if let Ok(date) = NaiveDate::parse_from_str(date_str, "%Y-%m-%d") {
                self.holidays.insert(date);
            }
        }

        // Remove holidays from trading dates
        for &holiday in &self.holidays {
            self.trading_dates.remove(&holiday);
        }

        self.save_cache()?;
        Ok(())
    }

    /// Save trading dates and holidays to cache.
    fn save_cache(&self) -> Result<()> {
        let trade_dates_file = self.cache_dir.join("trade_dates.txt");
        let holidays_file = self.cache_dir.join("holidays.txt");

        // Save trading dates
        let mut trade_file = File::create(&trade_dates_file)?;
        for date in self.trading_dates.iter().take(10000) {
            writeln!(trade_file, "{}", date.format("%Y-%m-%d"))?;
        }

        // Save holidays
        let mut holiday_file = File::create(&holidays_file)?;
        for date in &self.holidays {
            writeln!(holiday_file, "{}", date.format("%Y-%m-%d"))?;
        }

        debug!("Saved trading calendar to cache");
        Ok(())
    }

    /// Check if a date is a weekday (Monday-Friday).
    fn is_weekday(&self, date: NaiveDate) -> bool {
        matches!(
            date.weekday(),
            Weekday::Mon | Weekday::Tue | Weekday::Wed | Weekday::Thu | Weekday::Fri
        )
    }

    /// Check if a date is a trading day.
    pub fn is_trading_day(&self, date: NaiveDate) -> bool {
        self.trading_dates.contains(&date)
    }

    /// Get the number of trading days between two dates.
    pub fn get_trade_gap(&self, start: NaiveDate, end: NaiveDate) -> u32 {
        self.trading_dates
            .iter()
            .filter(|&&d| d >= start && d <= end)
            .count() as u32
    }

    /// Get the number of trading days between two dates (string format).
    pub fn get_trade_gap_str(&self, start: &str, end: &str) -> Result<u32> {
        let start_date = NaiveDate::parse_from_str(start, "%Y-%m-%d")
            .map_err(|e| Error::InvalidResponse(format!("Invalid start date: {}", e)))?;
        let end_date = NaiveDate::parse_from_str(end, "%Y-%m-%d")
            .map_err(|e| Error::InvalidResponse(format!("Invalid end date: {}", e)))?;
        Ok(self.get_trade_gap(start_date, end_date))
    }

    /// Get trading dates in a range.
    pub fn get_trade_dates_in_range(&self, start: NaiveDate, end: NaiveDate) -> Vec<NaiveDate> {
        let mut dates: Vec<_> = self
            .trading_dates
            .iter()
            .filter(|&&d| d >= start && d <= end)
            .copied()
            .collect();
        dates.sort();
        dates
    }

    /// Get the previous trading day.
    pub fn previous_trading_day(&self, date: NaiveDate) -> Option<NaiveDate> {
        self.get_pre_trade_date(date, 1, false)
    }

    /// Get the next trading day.
    pub fn next_trading_day(&self, date: NaiveDate) -> Option<NaiveDate> {
        self.get_next_trade_date(date, 1, false)
    }

    /// Get the n-th trading day before the given date.
    ///
    /// # Arguments
    /// * `date` — The reference date
    /// * `n` — How many trading days to go back (must be >= 1)
    /// * `include_input` — If `true` and `date` itself is a trading day, count it as the 1st day
    ///
    /// # Example
    /// ```
    /// let cal = rstdx::calendar::TradingCalendar::new().unwrap();
    /// let date = chrono::NaiveDate::from_ymd_opt(2024, 1, 8).unwrap(); // Monday
    /// // include_input=false: previous trading day is Friday 2024-01-05
    /// assert_eq!(cal.get_pre_trade_date(date, 1, false).unwrap(),
    ///            chrono::NaiveDate::from_ymd_opt(2024, 1, 5).unwrap());
    /// ```
    pub fn get_pre_trade_date(&self, date: NaiveDate, n: usize, include_input: bool) -> Option<NaiveDate> {
        if n == 0 {
            return None;
        }

        let mut current = if include_input && self.is_trading_day(date) {
            date
        } else {
            date - Duration::days(1)
        };

        let mut count = 0;
        while current > NaiveDate::from_ymd_opt(2000, 1, 1).unwrap() {
            if self.is_trading_day(current) {
                count += 1;
                if count >= n {
                    return Some(current);
                }
            }
            current -= Duration::days(1);
        }
        None
    }

    /// Get the n-th trading day after the given date.
    ///
    /// # Arguments
    /// * `date` — The reference date
    /// * `n` — How many trading days to go forward (must be >= 1)
    /// * `include_input` — If `true` and `date` itself is a trading day, count it as the 1st day
    pub fn get_next_trade_date(&self, date: NaiveDate, n: usize, include_input: bool) -> Option<NaiveDate> {
        if n == 0 {
            return None;
        }

        let mut current = if include_input && self.is_trading_day(date) {
            date
        } else {
            date + Duration::days(1)
        };

        let mut count = 0;
        while current < NaiveDate::from_ymd_opt(2035, 1, 1).unwrap() {
            if self.is_trading_day(current) {
                count += 1;
                if count >= n {
                    return Some(current);
                }
            }
            current += Duration::days(1);
        }
        None
    }

    /// Normalize a non-trading date to the nearest trading date.
    ///
    /// If `date` is already a trading day, returns it directly.
    pub fn normalize_to_trade_date(
        &self,
        date: NaiveDate,
        direction: super::NormalizeDirection,
    ) -> Option<NaiveDate> {
        if self.is_trading_day(date) {
            return Some(date);
        }

        match direction {
            super::NormalizeDirection::Previous => self.previous_trading_day(date),
            super::NormalizeDirection::Next => self.next_trading_day(date),
            super::NormalizeDirection::Nearest => {
                let prev = self.previous_trading_day(date);
                let next = self.next_trading_day(date);
                match (prev, next) {
                    (Some(p), Some(n)) => {
                        let prev_dist = (date - p).num_days();
                        let next_dist = (n - date).num_days();
                        if prev_dist <= next_dist {
                            Some(p)
                        } else {
                            Some(n)
                        }
                    }
                    (Some(p), None) => Some(p),
                    (None, Some(n)) => Some(n),
                    (None, None) => None,
                }
            }
        }
    }

    /// Get trading dates in a range as formatted strings.
    pub fn get_trade_dates(&self, start: NaiveDate, end: NaiveDate) -> Vec<String> {
        self.get_trade_dates_in_range(start, end)
            .into_iter()
            .map(|d| d.format("%Y-%m-%d").to_string())
            .collect()
    }

    /// Add a trading date.
    pub fn add_trading_date(&mut self, date: NaiveDate) -> Result<()> {
        self.trading_dates.insert(date);
        self.holidays.remove(&date);
        self.save_cache()?;
        Ok(())
    }

    /// Add a holiday.
    pub fn add_holiday(&mut self, date: NaiveDate) -> Result<()> {
        self.holidays.insert(date);
        self.trading_dates.remove(&date);
        self.save_cache()?;
        Ok(())
    }

    /// Get all trading dates as a sorted vector.
    pub fn all_trading_dates(&self) -> Vec<NaiveDate> {
        let mut dates: Vec<_> = self.trading_dates.iter().copied().collect();
        dates.sort();
        dates
    }
}

impl Default for TradingCalendar {
    fn default() -> Self {
        Self::new().unwrap_or_else(|_| Self {
            trading_dates: HashSet::new(),
            holidays: HashSet::new(),
            cache_dir: PathBuf::from("."),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Create a deterministic test calendar isolated from the user's home cache.
    fn test_calendar() -> TradingCalendar {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let cache_dir = std::env::temp_dir().join(format!("rstdx_test_calendar_{}", ts));
        fs::create_dir_all(&cache_dir).unwrap();
        let _ = fs::copy(manifest_dir.join("trade_dates.txt"), cache_dir.join("trade_dates.txt"));
        let _ = fs::copy(manifest_dir.join("holidays.txt"), cache_dir.join("holidays.txt"));
        TradingCalendar::with_cache_dir(cache_dir).unwrap()
    }

    #[test]
    fn test_is_trading_day() {
        let calendar = test_calendar();
        // Monday should be a trading day (unless it's a holiday)
        let date = NaiveDate::from_ymd_opt(2024, 1, 8).unwrap(); // 2024-01-08 is Monday
        assert!(calendar.is_trading_day(date));
    }

    #[test]
    fn test_get_trade_gap() {
        let calendar = test_calendar();
        let start = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
        let end = NaiveDate::from_ymd_opt(2024, 1, 31).unwrap();
        let gap = calendar.get_trade_gap(start, end);
        assert!(gap > 0);
    }

    #[test]
    fn test_previous_trading_day() {
        let calendar = test_calendar();
        let date = NaiveDate::from_ymd_opt(2024, 1, 8).unwrap(); // Monday
        let prev = calendar.previous_trading_day(date);
        assert!(prev.is_some());
        // Previous trading day should be Friday
        assert_eq!(prev.unwrap(), NaiveDate::from_ymd_opt(2024, 1, 5).unwrap());
    }

    #[test]
    fn test_next_trading_day() {
        let calendar = test_calendar();
        let date = NaiveDate::from_ymd_opt(2024, 1, 5).unwrap(); // Friday
        let next = calendar.next_trading_day(date);
        assert!(next.is_some());
        assert_eq!(next.unwrap(), NaiveDate::from_ymd_opt(2024, 1, 8).unwrap()); // Monday
    }

    #[test]
    fn test_get_trade_dates_in_range() {
        let calendar = test_calendar();
        let start = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
        let end = NaiveDate::from_ymd_opt(2024, 1, 7).unwrap();
        let dates = calendar.get_trade_dates_in_range(start, end);
        assert!(!dates.is_empty());
        // All returned dates should be within range
        for d in &dates {
            assert!(*d >= start && *d <= end);
        }
    }

    #[test]
    fn test_get_trade_gap_str() {
        let calendar = test_calendar();
        let gap = calendar
            .get_trade_gap_str("2024-01-01", "2024-01-31")
            .unwrap();
        assert!(gap > 0);
    }

    #[test]
    fn test_add_trading_date_and_holiday() {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let cache_dir = std::env::temp_dir().join(format!("rstdx_test_calendar_mut_{}", ts));
        fs::create_dir_all(&cache_dir).unwrap();
        let mut calendar = TradingCalendar::with_cache_dir(cache_dir).unwrap();
        let date = NaiveDate::from_ymd_opt(2025, 1, 1).unwrap();

        // Initially may or may not be trading day
        calendar.add_trading_date(date).unwrap();
        assert!(calendar.is_trading_day(date));

        calendar.add_holiday(date).unwrap();
        assert!(!calendar.is_trading_day(date));
    }
}
