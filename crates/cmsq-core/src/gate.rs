//! Promotion gate. Every input is produced by TRUSTED code: job status and runner
//! identity come from the orchestrator / sandbox backend, metrics from the Rust
//! scorer, the look-ahead verdict from the host-side probe comparison. Nothing
//! written by the strategy container is read here.
use crate::probe::ProbeReport;
use crate::scorer::{Dsr, Metrics};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const RUNNER_VERSION: &str = "2.2.2";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateConfig {
    pub min_bars: usize,
    pub min_sharpe: f64,
    /// Deflated Sharpe Ratio threshold (probability the true SR > 0 after
    /// deflating for the library-wide number of trials).
    pub min_dsr: f64,
    pub min_positive_folds: usize,
    pub max_drawdown_floor: f64,
    pub min_exposure: f64,
    pub require_hardened_backend: bool,
    pub require_adjusted_data: bool,
    /// DSR trial-count floor: community posts are pre-selected (the author's
    /// failed tries are invisible), so N is never taken below this.
    pub dsr_min_trials: usize,
    /// strategy Sharpe must exceed buy-and-hold Sharpe on the same window
    pub require_beat_buy_hold: bool,
    /// synthetic SAMPLE_* data may pass only when explicitly allowed
    /// (`CMSQ_ALLOW_SYNTHETIC=1`: demos/tests; recorded in every evaluation)
    #[serde(default)]
    pub allow_synthetic: bool,
    /// minimum capacity estimate in CNY (`CMSQ_GATE_MIN_CAPACITY`, default 1,000,000
    /// = the simulated capital): see [`crate::scorer::capacity`]
    #[serde(default)]
    pub min_capacity: f64,
    /// regime stability: a regime counts once it has this many bars
    #[serde(default = "default_regime_min_bars")]
    pub regime_min_bars: usize,
    /// regime stability: at most this many qualifying regimes may lose money
    #[serde(default = "default_regime_max_losing")]
    pub regime_max_losing: usize,
    /// regime stability: no qualifying regime may lose more than this
    #[serde(default = "default_regime_max_loss")]
    pub regime_max_loss: f64,
    /// pretrained-model embargo: every scored window must start at least this
    /// many trading days after the model's training cutoff
    #[serde(default = "default_model_gap_days")]
    pub model_gap_days: usize,
    /// `CMSQ_GATE_REQUIRE_KRONOS_BASELINE=1`: a missing Kronos baseline fails
    /// `beats_kronos_baseline` (default: missing baseline is reported, not blocking)
    #[serde(default)]
    pub require_kronos_baseline: bool,
}

fn default_model_gap_days() -> usize {
    crate::pretrained::DEFAULT_GAP_DAYS
}

/// Environment override for a gate threshold; unparsable values are ignored (default kept).
fn env_or<T: std::str::FromStr>(k: &str, d: T) -> T {
    std::env::var(k)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(d)
}
fn default_regime_max_loss() -> f64 {
    -0.10
}

fn default_regime_min_bars() -> usize {
    60
}
fn default_regime_max_losing() -> usize {
    1
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            min_bars: 250,
            min_sharpe: 0.5,
            min_dsr: 0.95,
            min_positive_folds: 3,
            max_drawdown_floor: -0.35,
            min_exposure: 0.05,
            require_hardened_backend: true,
            require_adjusted_data: true,
            dsr_min_trials: std::env::var("CMSQ_DSR_MIN_TRIALS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(20),
            require_beat_buy_hold: true,
            allow_synthetic: std::env::var("CMSQ_ALLOW_SYNTHETIC").is_ok_and(|v| v == "1"),
            min_capacity: std::env::var("CMSQ_GATE_MIN_CAPACITY")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(crate::scorer::INITIAL_CAPITAL),
            regime_min_bars: env_or("CMSQ_GATE_REGIME_MIN_BARS", default_regime_min_bars()),
            regime_max_losing: env_or("CMSQ_GATE_REGIME_MAX_LOSING", default_regime_max_losing()),
            regime_max_loss: env_or("CMSQ_GATE_REGIME_MAX_LOSS", default_regime_max_loss()),
            model_gap_days: env_or("CMSQ_GATE_MODEL_GAP_DAYS", default_model_gap_days()),
            require_kronos_baseline: std::env::var("CMSQ_GATE_REQUIRE_KRONOS_BASELINE")
                .is_ok_and(|v| v == "1"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct GateInput<'a> {
    pub job_status: &'a str,
    pub hardened: bool,
    /// from the backend (docker image inspect), e.g. "sha256:…|cmsq.runner.version=2.2.2"
    pub runner_identity: Option<&'a str>,
    pub dev: &'a Metrics,
    pub dev_cost_2x: &'a Metrics,
    pub folds: &'a [Metrics],
    pub dsr: &'a Dsr,
    pub probe: &'a ProbeReport,
    pub data_adjusted: bool,
    /// cross-source validation of the dataset (router::validation_status)
    pub data_validated: (bool, String),
    pub buy_hold: &'a Metrics,
    /// expected runner image id (recorded by scripts/build_runner.sh); None = not recorded
    pub expected_runner: Option<&'a str>,
    pub high_risks: &'a [String],
    /// dataset is a bundled synthetic sample
    pub synthetic: bool,
    /// capacity estimate (CNY); None = no trades to estimate from
    pub capacity: Option<f64>,
    /// futures: rolls held through without executable old and new legs (issue #10)
    pub roll_unpriced: usize,
    /// strategy return per market regime ([`crate::scorer::regime_slices`])
    pub regimes: &'a [crate::scorer::RegimeSlice],
    /// pretrained models detected by the host-side scan + scored window starts
    pub model_use: &'a crate::pretrained::ModelUse,
    /// host-scored dev metrics of the Kronos zero-shot baseline on the SAME
    /// dataset snapshot (None = baseline not run for this snapshot)
    pub kronos_baseline: Option<&'a crate::pretrained::Baseline>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GateOutcome {
    pub passed: bool,
    pub reasons: Vec<String>,
    pub checks: Value,
}

pub fn runner_version_ok(identity: Option<&str>) -> bool {
    identity.is_some_and(|i| {
        i.split('|')
            .any(|p| p == format!("cmsq.runner.version={RUNNER_VERSION}"))
    })
}

/// Label AND image digest must match; the digest is the id recorded at build time.
pub fn runner_pinned(identity: Option<&str>, expected: Option<&str>) -> bool {
    runner_version_ok(identity)
        && expected.is_some_and(|e| identity.is_some_and(|i| i.split('|').next() == Some(e)))
}

pub fn evaluate(i: &GateInput, cfg: &GateConfig) -> GateOutcome {
    let mut checks = vec![];
    let mut reasons = vec![];
    let mut check = |name: &str, ok: bool, detail: String| {
        if !ok {
            reasons.push(format!("{name}: {detail}"));
        }
        checks.push(json!({"name": name, "ok": ok, "detail": detail}));
    };
    check(
        "job_succeeded",
        i.job_status == "succeeded",
        format!("job status {}", i.job_status),
    );
    check(
        "hardened_sandbox",
        i.hardened || !cfg.require_hardened_backend,
        if i.hardened {
            "ran in hardened Docker sandbox".into()
        } else {
            "ran on DEV process backend — re-run on Docker to promote".into()
        },
    );
    check(
        "runner_pinned",
        runner_pinned(i.runner_identity, i.expected_runner) || !cfg.require_hardened_backend,
        format!(
            "runner identity {:?}; expected image {} with cmsq.runner.version={RUNNER_VERSION}",
            i.runner_identity.unwrap_or("none"),
            i.expected_runner
                .unwrap_or("<not recorded: run scripts/build_runner.sh>")
        ),
    );
    check(
        "no_lookahead",
        !i.probe.detected && i.probe.containers >= i.probe.variants && i.probe.variants > 1,
        if i.probe.detected {
            format!(
                "positions changed when the future changed ({} variant(s))",
                i.probe.mismatches.len()
            )
        } else {
            format!(
                "{} variants in {} fresh containers, all consistent",
                i.probe.variants, i.probe.containers
            )
        },
    );
    {
        let r = crate::pretrained::check(i.model_use, cfg.model_gap_days);
        check("model_lookahead", r.is_ok(), r.unwrap_or_else(|e| e));
    }
    check(
        "static_risks",
        i.high_risks.is_empty(),
        if i.high_risks.is_empty() {
            "no high-severity static findings".into()
        } else {
            i.high_risks.join(", ")
        },
    );
    check(
        "data_adjusted",
        i.data_adjusted || !cfg.require_adjusted_data,
        if i.data_adjusted {
            "prices adjusted (or synthetic)".into()
        } else {
            "UNADJUSTED prices: ex-dividend gaps distort returns; import adjusted data (adj_factor)"
                .into()
        },
    );
    check(
        "data_cross_validated",
        i.data_validated.0,
        i.data_validated.1.clone(),
    );
    check(
        "real_market_data",
        !i.synthetic || cfg.allow_synthetic,
        if !i.synthetic {
            "real market data".into()
        } else if cfg.allow_synthetic {
            "NOT attested real data (sample/csv/unknown signed source), allowed by CMSQ_ALLOW_SYNTHETIC=1 (demo/test only)".into()
        } else {
            "signed data source is not an independent real-market fetch (sample/csv/unknown): cannot pass the gate".into()
        },
    );
    check(
        "futures_roll_ledger",
        i.roll_unpriced == 0,
        format!(
            "{} roll(s) without executable old/new legs; approximations are display-only",
            i.roll_unpriced
        ),
    );
    check(
        "capacity",
        i.capacity.is_some_and(|c| c >= cfg.min_capacity),
        match i.capacity {
            Some(c) => format!(
                "capacity ≈ ¥{:.0} at ≤5% of daily traded value on 90% of fills (min ¥{:.0})",
                c, cfg.min_capacity
            ),
            None => "no fills to estimate capacity".into(),
        },
    );
    check(
        "min_bars",
        i.dev.bars >= cfg.min_bars,
        format!("{} evaluation bars (min {})", i.dev.bars, cfg.min_bars),
    );
    check(
        "net_return_positive",
        i.dev.total_return > 0.0,
        format!("net return {:.2}%", i.dev.total_return * 100.0),
    );
    check(
        "sharpe",
        i.dev.sharpe >= cfg.min_sharpe,
        format!("Sharpe {:.2} (min {:.2})", i.dev.sharpe, cfg.min_sharpe),
    );
    check(
        "deflated_sharpe",
        i.dsr.dsr >= cfg.min_dsr,
        format!(
            "DSR {:.3} (min {:.2}) over N={} library trials, SR0/day {:.4}",
            i.dsr.dsr, cfg.min_dsr, i.dsr.trials, i.dsr.sr0_daily
        ),
    );
    let pos = i.folds.iter().filter(|f| f.total_return > 0.0).count();
    check(
        "walk_forward",
        pos >= cfg.min_positive_folds.min(i.folds.len()) && !i.folds.is_empty(),
        format!(
            "{pos}/{} walk-forward folds positive (min {})",
            i.folds.len(),
            cfg.min_positive_folds
        ),
    );
    {
        // two partitions of the same bars: trend (up/range/down) and volatility (high/low)
        let dim = |r: &str| {
            if r.starts_with("vol_") {
                "vol"
            } else {
                "trend"
            }
        };
        let q: Vec<_> = i
            .regimes
            .iter()
            .filter(|r| r.bars >= cfg.regime_min_bars)
            .collect();
        let mut bad = vec![];
        for d in ["trend", "vol"] {
            let qd: Vec<_> = q.iter().filter(|r| dim(&r.regime) == d).collect();
            let losing = qd.iter().filter(|r| r.total_return <= 0.0).count();
            if qd.len() < 2 {
                bad.push(format!(
                    "{d}: fewer than 2 regimes with ≥{} bars",
                    cfg.regime_min_bars
                ));
            } else if losing > cfg.regime_max_losing {
                bad.push(format!(
                    "{d}: {losing} of {} regimes lose money (max {})",
                    qd.len(),
                    cfg.regime_max_losing
                ));
            }
        }
        for r in &q {
            if r.total_return < cfg.regime_max_loss {
                bad.push(format!(
                    "{} {:+.1}% (floor {:.0}%)",
                    r.regime,
                    r.total_return * 100.0,
                    cfg.regime_max_loss * 100.0
                ));
            }
        }
        let summary = q
            .iter()
            .map(|r| format!("{} {:+.1}%", r.regime, r.total_return * 100.0))
            .collect::<Vec<_>>()
            .join(", ");
        check(
            "regime_stability",
            bad.is_empty(),
            if bad.is_empty() {
                summary
            } else {
                format!("{}; [{summary}]", bad.join("; "))
            },
        );
    }
    check(
        "cost_stress_2x",
        i.dev_cost_2x.total_return > 0.0,
        format!(
            "net return with 2x costs {:.2}%",
            i.dev_cost_2x.total_return * 100.0
        ),
    );
    check(
        "max_drawdown",
        i.dev.max_drawdown >= cfg.max_drawdown_floor,
        format!(
            "max DD {:.1}% (floor {:.0}%)",
            i.dev.max_drawdown * 100.0,
            cfg.max_drawdown_floor * 100.0
        ),
    );
    check(
        "beats_buy_and_hold",
        !cfg.require_beat_buy_hold || i.dev.sharpe > i.buy_hold.sharpe,
        format!(
            "Sharpe {:.2} vs buy-and-hold {:.2} (return {:+.1}% vs {:+.1}%)",
            i.dev.sharpe,
            i.buy_hold.sharpe,
            i.dev.total_return * 100.0,
            i.buy_hold.total_return * 100.0
        ),
    );
    check(
        "exposure",
        i.dev.exposure >= cfg.min_exposure,
        format!("avg exposure {:.1}%", i.dev.exposure * 100.0),
    );
    match i.kronos_baseline {
        Some(b) => check(
            "beats_kronos_baseline",
            i.dev.sharpe > b.dev.sharpe,
            format!(
                "Sharpe {:.2} vs Kronos zero-shot baseline {:.2} (return {:+.1}% vs {:+.1}%, baseline job {})",
                i.dev.sharpe,
                b.dev.sharpe,
                i.dev.total_return * 100.0,
                b.dev.total_return * 100.0,
                &b.job_id[..8.min(b.job_id.len())]
            ),
        ),
        None => check(
            "beats_kronos_baseline",
            !cfg.require_kronos_baseline,
            "Kronos baseline not run for this snapshot (cmsq baseline kronos --symbol …)".into(),
        ),
    }
    GateOutcome {
        passed: reasons.is_empty(),
        reasons,
        checks: json!({"checks": checks, "config": cfg, "dsr": i.dsr, "kronos_baseline": i.kronos_baseline}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn m(ret: f64, sharpe: f64) -> Metrics {
        Metrics {
            bars: 700,
            total_return: ret,
            sharpe,
            sharpe_daily: sharpe / 252f64.sqrt(),
            kurtosis: 3.0,
            max_drawdown: -0.1,
            exposure: 0.5,
            ..Default::default()
        }
    }
    struct Fx {
        dev: Metrics,
        c2: Metrics,
        folds: Vec<Metrics>,
        dsr: Dsr,
        probe: ProbeReport,
        risks: Vec<String>,
        bh: Metrics,
        regimes: Vec<crate::scorer::RegimeSlice>,
        mu: crate::pretrained::ModelUse,
        kb: Option<crate::pretrained::Baseline>,
    }
    fn rg(name: &str, bars: usize, ret: f64) -> crate::scorer::RegimeSlice {
        crate::scorer::RegimeSlice {
            regime: name.into(),
            bars,
            total_return: ret,
            market_return: 0.0,
        }
    }
    fn fx() -> Fx {
        let dev = m(0.3, 1.2);
        let dsr = crate::scorer::deflated_sharpe(&dev, &[dev.sharpe_daily]);
        Fx {
            c2: m(0.2, 1.0),
            folds: vec![m(0.05, 1.0); 4],
            dsr,
            probe: ProbeReport {
                detected: false,
                variants: 10,
                containers: 10,
                ..Default::default()
            },
            risks: vec![],
            dev,
            bh: m(0.1, 0.3),
            regimes: vec![
                rg("trend_up", 200, 0.1),
                rg("range", 200, 0.05),
                rg("trend_down", 100, -0.02),
                rg("vol_high", 250, 0.04),
                rg("vol_low", 250, 0.08),
            ],
            mu: Default::default(),
            kb: None,
        }
    }
    fn input(f: &Fx) -> GateInput<'_> {
        GateInput {
            job_status: "succeeded",
            hardened: true,
            runner_identity: Some("sha256:abc|cmsq.runner.version=2.2.2"),
            expected_runner: Some("sha256:abc"),
            buy_hold: &f.bh,
            dev: &f.dev,
            dev_cost_2x: &f.c2,
            folds: &f.folds,
            dsr: &f.dsr,
            probe: &f.probe,
            data_adjusted: true,
            data_validated: (true, "sample".into()),
            high_risks: &f.risks,
            synthetic: false,
            capacity: Some(5e7),
            roll_unpriced: 0,
            regimes: &f.regimes,
            model_use: &f.mu,
            kronos_baseline: f.kb.as_ref(),
        }
    }
    #[test]
    fn regime_thresholds_configurable() {
        let mut g = fx();
        g.regimes[1].total_return = -0.01; // second losing trend regime
        let mut cfg = GateConfig::default();
        assert!(!evaluate(&input(&g), &cfg).passed);
        cfg.regime_max_losing = 2;
        assert!(evaluate(&input(&g), &cfg).passed);
        cfg.regime_max_loss = -0.005;
        assert!(!evaluate(&input(&g), &cfg).passed);
        cfg.regime_max_loss = -0.10;
        cfg.regime_min_bars = 150; // trend_down (100 bars) no longer qualifies
        assert!(evaluate(&input(&g), &cfg).passed);
    }
    #[test]
    fn regime_stability_check() {
        let cfg = GateConfig::default();
        let fails = |f: &Fx| {
            let g = evaluate(&input(f), &cfg);
            !g.passed && g.reasons.iter().any(|r| r.starts_with("regime_stability"))
        };
        let f = fx();
        assert!(
            evaluate(&input(&f), &cfg).passed,
            "one losing trend regime is allowed"
        );
        // a second losing trend regime fails
        let mut g = fx();
        g.regimes[1].total_return = -0.01;
        assert!(fails(&g));
        // one losing regime in each partition is fine, a big loss is not
        let mut g = fx();
        g.regimes[3].total_return = -0.05;
        assert!(evaluate(&input(&g), &cfg).passed);
        g.regimes[3].total_return = -0.15;
        assert!(fails(&g));
        // a regime too short to qualify is ignored ...
        let mut g = fx();
        g.regimes[1] = rg("range", 59, -0.5);
        assert!(evaluate(&input(&g), &cfg).passed);
        // ... but each partition needs two qualifying regimes
        g.regimes[0].bars = 10;
        assert!(fails(&g));
        let mut g = fx();
        g.regimes.clear();
        assert!(fails(&g));
    }
    #[test]
    fn synthetic_and_capacity_checks() {
        let f = fx();
        let mut i = input(&f);
        i.synthetic = true;
        let cfg = GateConfig {
            allow_synthetic: false,
            min_capacity: 1e6,
            ..GateConfig::default()
        };
        assert!(evaluate(&i, &cfg)
            .reasons
            .iter()
            .any(|r| r.starts_with("real_market_data")));
        assert!(
            evaluate(
                &i,
                &GateConfig {
                    allow_synthetic: true,
                    ..cfg.clone()
                }
            )
            .passed
        );
        i.synthetic = false;
        i.capacity = Some(2e5);
        assert!(evaluate(&i, &cfg)
            .reasons
            .iter()
            .any(|r| r.starts_with("capacity")));
        i.capacity = None;
        assert!(!evaluate(&i, &cfg).passed);
    }

    #[test]
    fn passes_good() {
        let f = fx();
        let o = evaluate(&input(&f), &GateConfig::default());
        assert!(o.passed, "{:?}", o.reasons);
    }
    #[test]
    fn fails_closed_on_runner_status_probe_and_data() {
        let f = fx();
        let cfg = GateConfig::default();
        let mut i = input(&f);
        i.runner_identity = Some("sha256:abc|cmsq.runner.version=1.0.0");
        assert!(!evaluate(&i, &cfg).passed);
        let mut i = input(&f);
        i.runner_identity = None;
        assert!(!evaluate(&i, &cfg).passed);
        let mut i = input(&f);
        i.job_status = "error";
        assert!(!evaluate(&i, &cfg).passed);
        let mut i = input(&f);
        i.hardened = false;
        assert!(!evaluate(&i, &cfg).passed);
        let mut i = input(&f);
        i.data_adjusted = false;
        assert!(!evaluate(&i, &cfg).passed);
        let mut f2 = fx();
        f2.probe.containers = 1; // variants not each in their own container
        assert!(!evaluate(&input(&f2), &cfg).passed);
        let mut f3 = fx();
        f3.risks = vec!["network".into()];
        assert!(!evaluate(&input(&f3), &cfg).passed);
        // label right, digest wrong / not recorded
        let mut i = input(&f);
        i.expected_runner = Some("sha256:def");
        assert!(evaluate(&i, &cfg)
            .reasons
            .iter()
            .any(|r| r.starts_with("runner_pinned")));
        let mut i = input(&f);
        i.expected_runner = None;
        assert!(!evaluate(&i, &cfg).passed);
        let mut i = input(&f);
        i.data_validated = (false, "x".into());
        assert!(evaluate(&i, &cfg)
            .reasons
            .iter()
            .any(|r| r.starts_with("data_cross_validated")));
        let mut f4 = fx();
        f4.bh = m(0.5, 1.5);
        assert!(evaluate(&input(&f4), &cfg)
            .reasons
            .iter()
            .any(|r| r.starts_with("beats_buy_and_hold")));
    }
    #[test]
    fn walk_forward_and_cost_stress_and_dsr() {
        let cfg = GateConfig::default();
        let mut f = fx();
        f.folds = vec![m(0.1, 1.0), m(-0.1, -1.0), m(-0.01, -0.1), m(0.2, 2.0)];
        assert!(evaluate(&input(&f), &cfg)
            .reasons
            .iter()
            .any(|r| r.starts_with("walk_forward")));
        let mut f = fx();
        f.c2.total_return = -0.01;
        assert!(!evaluate(&input(&f), &cfg).passed);
        let mut f = fx();
        let many: Vec<f64> = (0..200).map(|k| (k as f64 - 100.0) / 300.0).collect();
        f.dsr = crate::scorer::deflated_sharpe(&f.dev, &many);
        let o = evaluate(&input(&f), &cfg);
        assert!(
            o.reasons.iter().any(|r| r.starts_with("deflated_sharpe")),
            "{:?}",
            f.dsr
        );
    }
    #[test]
    fn kronos_baseline_must_be_beaten_when_present() {
        let mut f = fx();
        let cfg = GateConfig::default();
        let o = evaluate(&input(&f), &cfg);
        assert!(o
            .reasons
            .iter()
            .all(|r| !r.starts_with("beats_kronos_baseline")));
        f.kb = Some(crate::pretrained::Baseline {
            job_id: "baseline-job".into(),
            dev: m(0.5, 2.0),
        });
        let o = evaluate(&input(&f), &cfg);
        assert!(
            o.reasons
                .iter()
                .any(|r| r.starts_with("beats_kronos_baseline")),
            "{:?}",
            o.reasons
        );
        assert_eq!(o.checks["kronos_baseline"]["dev"]["sharpe"], 2.0);
        f.kb.as_mut().unwrap().dev = m(0.01, 0.1);
        let o = evaluate(&input(&f), &cfg);
        assert!(o
            .reasons
            .iter()
            .all(|r| !r.starts_with("beats_kronos_baseline")));
        f.kb = None;
        let req = GateConfig {
            require_kronos_baseline: true,
            ..GateConfig::default()
        };
        assert!(evaluate(&input(&f), &req)
            .reasons
            .iter()
            .any(|r| r.starts_with("beats_kronos_baseline")));
    }

    #[test]
    fn pretrained_model_inside_training_window_is_rejected() {
        let mut f = fx();
        assert!(evaluate(&input(&f), &GateConfig::default())
            .reasons
            .iter()
            .all(|r| !r.starts_with("model_lookahead")));
        f.mu = crate::pretrained::ModelUse {
            models: crate::pretrained::detect("Kronos.from_pretrained('NeoQuasar/Kronos-mini')"),
            windows: vec![("dev".into(), "2025-06-02".into())],
            calendar: vec!["2025-06-02".into()],
        };
        let o = evaluate(&input(&f), &GateConfig::default());
        assert!(!o.passed);
        assert!(
            o.reasons.iter().any(|r| r.starts_with("model_lookahead")),
            "{:?}",
            o.reasons
        );
    }
}

/// Live-decay flags on the out-of-sample segment since promotion (display-only: the
/// strategy's status is NOT changed automatically). Thresholds: drawdown worse than
/// -20%, or (with >= 60 live bars) annualised Sharpe below -0.5.
pub fn live_decay(live: Option<&crate::scorer::Metrics>) -> serde_json::Value {
    let Some(m) = live else {
        return serde_json::json!({"flagged": false, "reasons": [], "note": "no live bars yet"});
    };
    let mut reasons = vec![];
    if m.max_drawdown < -0.20 {
        reasons.push(format!(
            "live max drawdown {:.1}% < -20%",
            m.max_drawdown * 100.0
        ));
    }
    if m.bars >= 60 && m.sharpe < -0.5 {
        reasons.push(format!(
            "live Sharpe {:.2} < -0.5 over {} bars",
            m.sharpe, m.bars
        ));
    }
    serde_json::json!({"flagged": !reasons.is_empty(), "reasons": reasons, "bars": m.bars,
        "policy": "display-only flag; status unchanged, a human decides (reject manually)"})
}

#[cfg(test)]
mod live_decay_tests {
    #[test]
    fn flags_drawdown_and_sharpe_only_with_enough_bars() {
        let mut m = crate::scorer::Metrics {
            bars: 30,
            sharpe: -2.0,
            max_drawdown: -0.05,
            ..Default::default()
        };
        assert_eq!(super::live_decay(Some(&m))["flagged"], false);
        m.bars = 60;
        assert_eq!(super::live_decay(Some(&m))["flagged"], true);
        m.sharpe = 1.0;
        m.max_drawdown = -0.25;
        assert_eq!(super::live_decay(Some(&m))["flagged"], true);
        assert_eq!(super::live_decay(None)["flagged"], false);
    }
}
