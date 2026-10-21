//! Signals → paper orders. A PROMOTED strategy's stored target positions (fraction
//! of equity, as produced by the scorer) are applied once per trading day: the
//! latest signal dated BEFORE the quote's trading day (signal at close t acts on t+1),
//! sized at the first quote of the day, filled by the matcher on later quotes.
use crate::{Fill, Market, PaperSession, Quote};
use anyhow::Result;

pub struct SignalDriver {
    /// (date YYYY-MM-DD, target fraction), ascending
    pub signals: Vec<(String, f64)>,
    pub instrument: String,
    last_day: Option<String>,
    pub rebalances: Vec<(String, f64, i64)>,
}

impl SignalDriver {
    pub fn new(mut signals: Vec<(String, f64)>, instrument: &str) -> Self {
        signals.sort_by(|a, b| a.0.cmp(&b.0));
        Self {
            signals,
            instrument: instrument.into(),
            last_day: None,
            rebalances: vec![],
        }
    }
    pub fn target_for(&self, trading_day: &str) -> Option<f64> {
        self.signals
            .iter()
            .rev()
            .find(|(d, _)| d.as_str() < trading_day)
            .map(|x| x.1)
    }

    pub fn on_quote(
        &mut self,
        ps: &mut PaperSession,
        strategy: &cmsq_core::models::Strategy,
        q: &Quote,
    ) -> Result<Vec<Fill>> {
        if q.symbol != self.instrument {
            return Ok(vec![]);
        }
        let fills = ps.engine.on_quote(q);
        if self.last_day.as_deref() != Some(q.trading_day.as_str()) {
            self.last_day = Some(q.trading_day.clone());
            if let Some(t) = self.target_for(&q.trading_day) {
                let eq = ps.engine.equity();
                let lots = match ps.engine.market(&self.instrument) {
                    Some(Market::Futures { spec }) => {
                        (t * eq / (q.last * spec.multiplier)).trunc() as i64
                    }
                    Some(Market::AShare { .. }) => {
                        ((t.max(0.0) * eq / q.last) / 100.0).floor() as i64 * 100
                    }
                    None => 0,
                };
                self.rebalances.push((q.trading_day.clone(), t, lots));
                let _ = ps.engine.rebalance(strategy, &self.instrument, lots, &q.ts);
            }
        }
        ps.flush()?;
        Ok(fills)
    }
}
