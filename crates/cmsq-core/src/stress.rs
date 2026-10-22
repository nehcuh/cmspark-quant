//! Synthetic stress test (Kronos P3). TRUSTED host side.
//!
//! A Kronos generator running in the sandbox (`cmsq-runner-kronos`) continues the
//! development window with N synthetic futures. The host validates every
//! generated bar, runs the candidate on `dev ++ synthetic` in fresh containers,
//! scores ONLY the synthetic tail with the Rust scorer, and applies this verdict.
//!
//! Rules: synthetic data is labelled SYNTHETIC everywhere, never enters
//! `real_market_data`, the DSR ledger or signals, and the verdict can only ADD a
//! rejection (a pass changes nothing). The vault is never used as context.
use crate::market::Bar;
use crate::scorer::Metrics;
use serde::{Deserialize, Serialize};

pub const LABEL: &str =
    "SYNTHETIC (Kronos-generated paths) — stress test only, can only add rejections";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StressConfig {
    /// a path "fails" when its synthetic-tail return is below this
    pub loss_floor: f64,
    /// … or its synthetic-tail max drawdown is worse than this
    pub dd_floor: f64,
    /// the strategy is rejected when more than this fraction of paths fail
    pub max_fail_frac: f64,
}

impl Default for StressConfig {
    fn default() -> Self {
        let env = |k: &str, d: f64| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d)
        };
        Self {
            loss_floor: env("CMSQ_STRESS_LOSS_FLOOR", -0.20),
            dd_floor: env("CMSQ_STRESS_DD_FLOOR", -0.35),
            max_fail_frac: env("CMSQ_STRESS_MAX_FAIL_FRAC", 0.5),
        }
    }
}

/// Parse + validate the generator's output (returned as `/out/positions.csv`)
/// (`path,step,open,high,low,close,volume`). Dates are assigned by the host
/// (weekdays after `last.date`); anything malformed rejects the whole file.
pub fn parse_synthetic(
    csv: &str,
    paths: usize,
    horizon: usize,
    last: &Bar,
) -> Result<Vec<Vec<Bar>>, String> {
    let mut out: Vec<Vec<Bar>> = vec![vec![]; paths];
    let mut lines = csv.lines();
    let head = lines.next().unwrap_or("").trim();
    if head != "path,step,open,high,low,close,volume" {
        return Err(format!("bad header {head:?}"));
    }
    let dates = next_weekdays(&last.date, horizon)?;
    for (n, l) in lines.enumerate() {
        if l.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = l.split(',').collect();
        if f.len() != 7 {
            return Err(format!("line {}: {} fields", n + 2, f.len()));
        }
        let p: usize = f[0].parse().map_err(|_| format!("line {}: path", n + 2))?;
        let s: usize = f[1].parse().map_err(|_| format!("line {}: step", n + 2))?;
        let v: Vec<f64> = f[2..]
            .iter()
            .map(|x| {
                x.parse::<f64>()
                    .map_err(|_| format!("line {}: number", n + 2))
            })
            .collect::<Result<_, _>>()?;
        if p >= paths || s != out[p].len() || s >= horizon {
            return Err(format!(
                "line {}: path {p} step {s} out of order/range",
                n + 2
            ));
        }
        let (o, h, lo, c, vol) = (v[0], v[1], v[2], v[3], v[4]);
        if !v.iter().all(|x| x.is_finite()) || o <= 0.0 || lo <= 0.0 || vol < 0.0 {
            return Err(format!("line {}: non-finite or non-positive value", n + 2));
        }
        // repair-free: inconsistent candles are rejected, not fixed
        if h + 1e-9 < o.max(c) || lo - 1e-9 > o.min(c) {
            return Err(format!(
                "line {}: high/low inconsistent with open/close",
                n + 2
            ));
        }
        let prev = out[p].last().map(|b| b.close).unwrap_or(last.close);
        if (c / prev).ln().abs() > 0.5 {
            return Err(format!(
                "line {}: implausible jump {:.0}%",
                n + 2,
                (c / prev - 1.0) * 100.0
            ));
        }
        out[p].push(Bar {
            date: dates[s].clone(),
            open: o,
            high: h,
            low: lo,
            close: c,
            volume: vol,
            adj_factor: None,
        });
    }
    if out.iter().any(|x| x.len() != horizon) {
        return Err(format!("expected {paths} paths × {horizon} steps"));
    }
    Ok(out)
}

fn next_weekdays(after: &str, n: usize) -> Result<Vec<String>, String> {
    use chrono::Datelike;
    let mut d = chrono::NaiveDate::parse_from_str(&after[..10.min(after.len())], "%Y-%m-%d")
        .map_err(|e| format!("bad date {after}: {e}"))?;
    let mut v = vec![];
    while v.len() < n {
        d = d.succ_opt().ok_or("date overflow")?;
        if d.weekday().number_from_monday() <= 5 {
            v.push(d.format("%Y-%m-%d").to_string());
        }
    }
    Ok(v)
}

/// Ok(detail) = no added rejection; Err(detail) = reject.
pub fn verdict(tails: &[Metrics], cfg: &StressConfig) -> Result<String, String> {
    if tails.is_empty() {
        return Ok("no synthetic paths scored (nothing added)".into());
    }
    let failed: Vec<usize> = tails
        .iter()
        .enumerate()
        .filter(|(_, m)| m.total_return < cfg.loss_floor || m.max_drawdown < cfg.dd_floor)
        .map(|(i, _)| i)
        .collect();
    let mut rets: Vec<f64> = tails.iter().map(|m| m.total_return).collect();
    rets.sort_by(|a, b| a.total_cmp(b));
    let med = rets[rets.len() / 2];
    let d = format!(
        "{}/{} synthetic paths breach (return < {:.0}% or maxDD < {:.0}%), median tail return {:+.1}% [{LABEL}]",
        failed.len(),
        tails.len(),
        cfg.loss_floor * 100.0,
        cfg.dd_floor * 100.0,
        med * 100.0
    );
    if failed.len() as f64 > cfg.max_fail_frac * tails.len() as f64 {
        Err(d)
    } else {
        Ok(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn last() -> Bar {
        Bar {
            date: "2026-10-16".into(),
            open: 10.0,
            high: 10.0,
            low: 10.0,
            close: 10.0,
            volume: 1.0,
            adj_factor: None,
        }
    }
    fn m(r: f64, dd: f64) -> Metrics {
        Metrics {
            total_return: r,
            max_drawdown: dd,
            ..Default::default()
        }
    }

    #[test]
    fn parses_valid_paths_and_assigns_host_dates() {
        let csv = "path,step,open,high,low,close,volume\n0,0,10,10.5,9.8,10.2,100\n0,1,10.2,10.4,10,10.1,90\n1,0,10,10.1,9.5,9.6,80\n1,1,9.6,9.9,9.4,9.8,70\n";
        let p = parse_synthetic(csv, 2, 2, &last()).unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p[0][0].date, "2026-10-19"); // Fri → Mon
        assert_eq!(p[1][1].close, 9.8);
    }

    #[test]
    fn rejects_malformed_generator_output() {
        let h = "path,step,open,high,low,close,volume\n";
        for bad in [
            "0,0,10,9,9.8,10.2,100\n",    // high < close
            "0,0,10,10.5,9.8,NaN,100\n",  // non-finite
            "0,0,10,30,9.8,25,100\n",     // +150% jump
            "0,1,10,10.5,9.8,10.2,100\n", // out of order
            "3,0,10,10.5,9.8,10.2,100\n", // unknown path
        ] {
            assert!(
                parse_synthetic(&format!("{h}{bad}"), 1, 1, &last()).is_err(),
                "{bad}"
            );
        }
        assert!(parse_synthetic("x\n", 1, 1, &last()).is_err());
        // missing steps
        assert!(parse_synthetic(&format!("{h}0,0,10,10.5,9.8,10.2,100\n"), 1, 2, &last()).is_err());
    }

    #[test]
    fn verdict_only_rejects_when_most_paths_breach() {
        let cfg = StressConfig {
            loss_floor: -0.2,
            dd_floor: -0.35,
            max_fail_frac: 0.5,
        };
        assert!(verdict(&[], &cfg).is_ok());
        assert!(verdict(&[m(0.1, -0.1), m(-0.3, -0.3), m(0.0, -0.05)], &cfg).is_ok());
        let e = verdict(&[m(-0.3, -0.3), m(0.05, -0.5), m(0.1, -0.1)], &cfg).unwrap_err();
        assert!(e.contains("2/3") && e.contains("SYNTHETIC"), "{e}");
    }
}
