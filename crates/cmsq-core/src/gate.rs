//! Promotion gate (structural overfitting defence). Thresholds are fixed by config,
//! never by the strategy author.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateConfig {
    pub min_oos_bars: i64,
    pub base_min_oos_sharpe: f64,
    /// Sharpe hurdle grows by `penalty * ln(attempts)` (multiple-testing haircut).
    pub multiple_testing_penalty: f64,
    pub max_drawdown_floor: f64,
    pub min_exposure: f64,
}

impl Default for GateConfig {
    fn default() -> Self {
        Self { min_oos_bars: 120, base_min_oos_sharpe: 0.3, multiple_testing_penalty: 0.15, max_drawdown_floor: -0.35, min_exposure: 0.05 }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct GateOutcome {
    pub passed: bool,
    pub reasons: Vec<String>,
    pub checks: Value,
}

fn f(v: &Value, k: &str) -> Option<f64> {
    v.get(k).and_then(|x| x.as_f64())
}

pub fn evaluate(result: &Value, attempts: i64, cfg: &GateConfig) -> GateOutcome {
    let oos = &result["out_of_sample"];
    let hurdle = cfg.base_min_oos_sharpe + cfg.multiple_testing_penalty * (attempts.max(1) as f64).ln();
    let mut checks = vec![];
    let mut reasons = vec![];
    let mut check = |name: &str, ok: bool, detail: String| {
        checks.push(json!({"check": name, "ok": ok, "detail": detail}));
        if !ok {
            reasons.push(format!("{name}: {detail}"));
        }
    };
    let bars = oos["bars"].as_i64().unwrap_or(0);
    check("oos_sample_size", bars >= cfg.min_oos_bars, format!("{bars} OOS bars (min {})", cfg.min_oos_bars));
    let ret = f(oos, "total_return");
    check("oos_net_return_positive", ret.is_some_and(|r| r > 0.0), format!("OOS return after costs {:.2}%", ret.unwrap_or(f64::NAN) * 100.0));
    let sh = f(oos, "sharpe");
    check("oos_sharpe_hurdle", sh.is_some_and(|s| s >= hurdle),
        format!("OOS Sharpe {:.2} vs hurdle {:.2} (base {:.2} + {:.2}·ln({attempts} attempts))", sh.unwrap_or(f64::NAN), hurdle, cfg.base_min_oos_sharpe, cfg.multiple_testing_penalty));
    let halves: Vec<Option<f64>> = result["oos_halves"].as_array().map(|a| a.iter().map(|h| f(h, "total_return")).collect()).unwrap_or_default();
    let stable = halves.len() == 2 && halves.iter().all(|h| h.is_some_and(|r| r > 0.0));
    check("stability_oos_halves", stable, format!("OOS halves net returns {:?}", halves.iter().map(|h| h.map(|r| (r * 10000.0).round() / 100.0)).collect::<Vec<_>>()));
    let dd = f(oos, "max_drawdown");
    check("max_drawdown", dd.is_some_and(|d| d >= cfg.max_drawdown_floor), format!("OOS max DD {:.2}% (floor {:.0}%)", dd.unwrap_or(f64::NAN) * 100.0, cfg.max_drawdown_floor * 100.0));
    let ex = f(oos, "exposure");
    check("min_exposure", ex.is_some_and(|e| e >= cfg.min_exposure), format!("OOS exposure {:.1}%", ex.unwrap_or(0.0) * 100.0));
    let leak = result["lookahead_probe"]["detected"].as_bool().unwrap_or(true);
    check("no_lookahead", !leak, if leak { "look-ahead probe failed or missing".into() } else { "point-in-time probe clean".into() });
    GateOutcome { passed: reasons.is_empty(), reasons, checks: json!({"hurdle_sharpe": hurdle, "attempts": attempts, "checks": checks, "config": cfg}) }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn res(ret: f64, sharpe: f64, h1: f64, h2: f64) -> Value {
        json!({"out_of_sample": {"bars": 300, "total_return": ret, "sharpe": sharpe, "max_drawdown": -0.1, "exposure": 0.5},
               "oos_halves": [{"total_return": h1}, {"total_return": h2}], "lookahead_probe": {"detected": false}})
    }
    #[test]
    fn passes_good() {
        assert!(evaluate(&res(0.2, 1.2, 0.1, 0.08), 1, &GateConfig::default()).passed);
    }
    #[test]
    fn rejects_negative_after_costs_and_unstable() {
        let o = evaluate(&res(-0.01, 0.9, 0.1, -0.1), 1, &GateConfig::default());
        assert!(!o.passed);
        assert!(o.reasons.iter().any(|r| r.starts_with("oos_net_return_positive")));
        assert!(o.reasons.iter().any(|r| r.starts_with("stability")));
    }
    #[test]
    fn hurdle_rises_with_attempts() {
        let r = res(0.1, 0.5, 0.05, 0.04);
        assert!(evaluate(&r, 1, &GateConfig::default()).passed);
        assert!(!evaluate(&r, 20, &GateConfig::default()).passed, "0.3+0.15*ln20=0.75 > 0.5");
    }
    #[test]
    fn missing_probe_fails_closed() {
        let mut r = res(0.2, 1.2, 0.1, 0.08);
        r.as_object_mut().unwrap().remove("lookahead_probe");
        assert!(!evaluate(&r, 1, &GateConfig::default()).passed);
    }
}
