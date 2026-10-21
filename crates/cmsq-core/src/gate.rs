//! Promotion gate. Every input is produced by TRUSTED code: job status and runner
//! identity come from the orchestrator / sandbox backend, metrics from the Rust
//! scorer, the look-ahead verdict from the host-side probe comparison. Nothing
//! written by the strategy container is read here.
use crate::probe::ProbeReport;
use crate::scorer::{Dsr, Metrics};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const RUNNER_VERSION: &str = "2.2.1";

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
}

impl Default for GateConfig {
    fn default() -> Self {
        Self { min_bars: 250, min_sharpe: 0.5, min_dsr: 0.95, min_positive_folds: 3, max_drawdown_floor: -0.35, min_exposure: 0.05,
            require_hardened_backend: true, require_adjusted_data: true,
            dsr_min_trials: std::env::var("CMSQ_DSR_MIN_TRIALS").ok().and_then(|v| v.parse().ok()).unwrap_or(20), require_beat_buy_hold: true }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct GateInput<'a> {
    pub job_status: &'a str,
    pub hardened: bool,
    /// from the backend (docker image inspect), e.g. "sha256:…|cmsq.runner.version=2.2.1"
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
}

#[derive(Debug, Clone, Serialize)]
pub struct GateOutcome {
    pub passed: bool,
    pub reasons: Vec<String>,
    pub checks: Value,
}

pub fn runner_version_ok(identity: Option<&str>) -> bool {
    identity.is_some_and(|i| i.split('|').any(|p| p == format!("cmsq.runner.version={RUNNER_VERSION}")))
}

/// Label AND image digest must match; the digest is the id recorded at build time.
pub fn runner_pinned(identity: Option<&str>, expected: Option<&str>) -> bool {
    runner_version_ok(identity) && expected.is_some_and(|e| identity.is_some_and(|i| i.split('|').next() == Some(e)))
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
    check("job_succeeded", i.job_status == "succeeded", format!("job status {}", i.job_status));
    check("hardened_sandbox", i.hardened || !cfg.require_hardened_backend,
        if i.hardened { "ran in hardened Docker sandbox".into() } else { "ran on DEV process backend — re-run on Docker to promote".into() });
    check("runner_pinned", runner_pinned(i.runner_identity, i.expected_runner) || !cfg.require_hardened_backend,
        format!("runner identity {:?}; expected image {} with cmsq.runner.version={RUNNER_VERSION}", i.runner_identity.unwrap_or("none"),
            i.expected_runner.unwrap_or("<not recorded: run scripts/build_runner.sh>")));
    check("no_lookahead", !i.probe.detected && i.probe.containers >= i.probe.variants && i.probe.variants > 1,
        if i.probe.detected { format!("positions changed when the future changed ({} variant(s))", i.probe.mismatches.len()) }
        else { format!("{} variants in {} fresh containers, all consistent", i.probe.variants, i.probe.containers) });
    check("static_risks", i.high_risks.is_empty(), if i.high_risks.is_empty() { "no high-severity static findings".into() } else { i.high_risks.join(", ") });
    check("data_adjusted", i.data_adjusted || !cfg.require_adjusted_data,
        if i.data_adjusted { "prices adjusted (or synthetic)".into() } else { "UNADJUSTED prices: ex-dividend gaps distort returns; import adjusted data (adj_factor)".into() });
    check("data_cross_validated", i.data_validated.0, i.data_validated.1.clone());
    check("min_bars", i.dev.bars >= cfg.min_bars, format!("{} evaluation bars (min {})", i.dev.bars, cfg.min_bars));
    check("net_return_positive", i.dev.total_return > 0.0, format!("net return {:.2}%", i.dev.total_return * 100.0));
    check("sharpe", i.dev.sharpe >= cfg.min_sharpe, format!("Sharpe {:.2} (min {:.2})", i.dev.sharpe, cfg.min_sharpe));
    check("deflated_sharpe", i.dsr.dsr >= cfg.min_dsr,
        format!("DSR {:.3} (min {:.2}) over N={} library trials, SR0/day {:.4}", i.dsr.dsr, cfg.min_dsr, i.dsr.trials, i.dsr.sr0_daily));
    let pos = i.folds.iter().filter(|f| f.total_return > 0.0).count();
    check("walk_forward", pos >= cfg.min_positive_folds.min(i.folds.len()) && !i.folds.is_empty(),
        format!("{pos}/{} walk-forward folds positive (min {})", i.folds.len(), cfg.min_positive_folds));
    check("cost_stress_2x", i.dev_cost_2x.total_return > 0.0, format!("net return with 2x costs {:.2}%", i.dev_cost_2x.total_return * 100.0));
    check("max_drawdown", i.dev.max_drawdown >= cfg.max_drawdown_floor, format!("max DD {:.1}% (floor {:.0}%)", i.dev.max_drawdown * 100.0, cfg.max_drawdown_floor * 100.0));
    check("beats_buy_and_hold", !cfg.require_beat_buy_hold || i.dev.sharpe > i.buy_hold.sharpe,
        format!("Sharpe {:.2} vs buy-and-hold {:.2} (return {:+.1}% vs {:+.1}%)", i.dev.sharpe, i.buy_hold.sharpe, i.dev.total_return * 100.0, i.buy_hold.total_return * 100.0));
    check("exposure", i.dev.exposure >= cfg.min_exposure, format!("avg exposure {:.1}%", i.dev.exposure * 100.0));
    GateOutcome { passed: reasons.is_empty(), reasons, checks: json!({"checks": checks, "config": cfg, "dsr": i.dsr}) }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn m(ret: f64, sharpe: f64) -> Metrics {
        Metrics { bars: 700, total_return: ret, sharpe, sharpe_daily: sharpe / 252f64.sqrt(), kurtosis: 3.0, max_drawdown: -0.1, exposure: 0.5, ..Default::default() }
    }
    struct Fx {
        dev: Metrics,
        c2: Metrics,
        folds: Vec<Metrics>,
        dsr: Dsr,
        probe: ProbeReport,
        risks: Vec<String>,
        bh: Metrics,
    }
    fn fx() -> Fx {
        let dev = m(0.3, 1.2);
        let dsr = crate::scorer::deflated_sharpe(&dev, &[dev.sharpe_daily]);
        Fx { c2: m(0.2, 1.0), folds: vec![m(0.05, 1.0); 4], dsr, probe: ProbeReport { detected: false, variants: 10, containers: 10, ..Default::default() }, risks: vec![], dev, bh: m(0.1, 0.3) }
    }
    fn input(f: &Fx) -> GateInput<'_> {
        GateInput { job_status: "succeeded", hardened: true, runner_identity: Some("sha256:abc|cmsq.runner.version=2.2.1"), expected_runner: Some("sha256:abc"), buy_hold: &f.bh, dev: &f.dev, dev_cost_2x: &f.c2,
            folds: &f.folds, dsr: &f.dsr, probe: &f.probe, data_adjusted: true, data_validated: (true, "sample".into()), high_risks: &f.risks }
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
        assert!(evaluate(&i, &cfg).reasons.iter().any(|r| r.starts_with("runner_pinned")));
        let mut i = input(&f);
        i.expected_runner = None;
        assert!(!evaluate(&i, &cfg).passed);
        let mut i = input(&f);
        i.data_validated = (false, "x".into());
        assert!(evaluate(&i, &cfg).reasons.iter().any(|r| r.starts_with("data_cross_validated")));
        let mut f4 = fx();
        f4.bh = m(0.5, 1.5);
        assert!(evaluate(&input(&f4), &cfg).reasons.iter().any(|r| r.starts_with("beats_buy_and_hold")));
    }
    #[test]
    fn walk_forward_and_cost_stress_and_dsr() {
        let cfg = GateConfig::default();
        let mut f = fx();
        f.folds = vec![m(0.1, 1.0), m(-0.1, -1.0), m(-0.01, -0.1), m(0.2, 2.0)];
        assert!(evaluate(&input(&f), &cfg).reasons.iter().any(|r| r.starts_with("walk_forward")));
        let mut f = fx();
        f.c2.total_return = -0.01;
        assert!(!evaluate(&input(&f), &cfg).passed);
        let mut f = fx();
        let many: Vec<f64> = (0..200).map(|k| (k as f64 - 100.0) / 300.0).collect();
        f.dsr = crate::scorer::deflated_sharpe(&f.dev, &many);
        let o = evaluate(&input(&f), &cfg);
        assert!(o.reasons.iter().any(|r| r.starts_with("deflated_sharpe")), "{:?}", f.dsr);
    }
}
