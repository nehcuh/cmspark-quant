//! Pretrained-model look-ahead guard (Kronos and other foundation models).
//!
//! A pretrained model has seen market data up to its (often unpublished)
//! training cutoff. Any scored bar on or before `cutoff + gap` trading days can
//! be "remembered" by the weights, which is look-ahead the position probes
//! cannot detect. Detection is a HOST-side static scan of the strategy code
//! (trusted); nothing the container writes is consulted.
use serde::{Deserialize, Serialize};

/// Conservative cutoff when a model's training data end is not published
/// (Kronos-small HF lastModified, 2025-09-09). `CMSQ_MODEL_CUTOFF_DEFAULT`.
pub const DEFAULT_UNKNOWN_CUTOFF: &str = "2025-09-09";
/// Minimum trading-day embargo after the cutoff. `CMSQ_GATE_MODEL_GAP_DAYS`.
pub const DEFAULT_GAP_DAYS: usize = 20;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelRef {
    pub name: String,
    /// YYYY-MM-DD; training-data cutoff used for the check
    pub cutoff: String,
    /// true when `cutoff` is the conservative default (not published)
    pub assumed: bool,
}

/// Known artifacts: name marker → published cutoff (None = unpublished).
const KNOWN: &[(&str, Option<&str>)] = &[
    ("kronos-mini", None),
    ("kronos-small", None),
    ("kronos-base", None),
    ("kronos-tokenizer", None),
    ("kronos", None),
];

/// Generic markers that mean "this strategy loads pretrained weights".
const GENERIC: &[&str] = &[
    "from_pretrained",
    "import torch",
    "from torch",
    "import transformers",
    "from transformers",
    "huggingface_hub",
    ".safetensors",
    "chronos",
    "timesfm",
    "moirai",
];

pub fn default_cutoff() -> String {
    std::env::var("CMSQ_MODEL_CUTOFF_DEFAULT")
        .ok()
        .filter(|v| chrono::NaiveDate::parse_from_str(v, "%Y-%m-%d").is_ok())
        .unwrap_or_else(|| DEFAULT_UNKNOWN_CUTOFF.into())
}

/// Static scan (host side). Returns every pretrained model the code appears to use.
pub fn detect(code: &str) -> Vec<ModelRef> {
    let lc = code.to_lowercase();
    let unknown = default_cutoff();
    let mut out: Vec<ModelRef> = vec![];
    for (marker, cut) in KNOWN {
        // the bare "kronos" marker only counts when no specific artifact matched
        if *marker == "kronos" && !out.is_empty() {
            continue;
        }
        if lc.contains(marker) {
            out.push(ModelRef {
                name: (*marker).into(),
                cutoff: cut.map(String::from).unwrap_or_else(|| unknown.clone()),
                assumed: cut.is_none(),
            });
        }
    }
    if out.is_empty() {
        if let Some(g) = GENERIC.iter().find(|g| lc.contains(*g)) {
            out.push(ModelRef {
                name: format!("unknown-pretrained({})", g.trim()),
                cutoff: unknown,
                assumed: true,
            });
        }
    }
    out
}

/// Trading days in `calendar` strictly after `cutoff` and strictly before `start`.
/// Dates must be YYYY-MM-DD (lexicographic order = chronological). Counting only
/// dates present in the dataset is conservative (gaps/holidays never inflate it).
pub fn trading_days_between(calendar: &[String], cutoff: &str, start: &str) -> usize {
    calendar
        .iter()
        .filter(|d| {
            let d = &d[..10.min(d.len())];
            d > cutoff && d < &start[..10.min(start.len())]
        })
        .count()
}

/// Everything the gate needs (all computed by trusted host code).
#[derive(Debug, Clone, Serialize, Default)]
pub struct ModelUse {
    pub models: Vec<ModelRef>,
    /// (label, first scored date) for every scored window: dev, each fold, vault
    pub windows: Vec<(String, String)>,
    /// dataset trading dates (for the gap count)
    pub calendar: Vec<String>,
}

/// Ok(detail) or Err(detail). No models ⇒ ok.
pub fn check(u: &ModelUse, gap: usize) -> Result<String, String> {
    if u.models.is_empty() {
        return Ok("no pretrained model detected".into());
    }
    let mut bad = vec![];
    for m in &u.models {
        for (label, start) in &u.windows {
            let n = trading_days_between(&u.calendar, &m.cutoff, start);
            if start.as_str() <= m.cutoff.as_str() || n < gap {
                bad.push(format!(
                    "{label} starts {start}: {n} trading day(s) after {} cutoff {}{} (need ≥{gap})",
                    m.name,
                    m.cutoff,
                    if m.assumed {
                        " [unpublished, assumed]"
                    } else {
                        ""
                    }
                ));
            }
        }
    }
    let names = u
        .models
        .iter()
        .map(|m| format!("{}≤{}", m.name, m.cutoff))
        .collect::<Vec<_>>()
        .join(", ");
    if u.windows.is_empty() {
        return Err(format!(
            "pretrained model(s) {names} but no scored window recorded"
        ));
    }
    if bad.is_empty() {
        Ok(format!(
            "{names}: all {} scored window(s) start ≥{gap} trading days after cutoff",
            u.windows.len()
        ))
    } else {
        Err(format!("model look-ahead: {}", bad.join("; ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cal(from: &str, n: usize) -> Vec<String> {
        let mut d = chrono::NaiveDate::parse_from_str(from, "%Y-%m-%d").unwrap();
        let mut v = vec![];
        while v.len() < n {
            use chrono::Datelike;
            if d.weekday().number_from_monday() <= 5 {
                v.push(d.format("%Y-%m-%d").to_string());
            }
            d = d.succ_opt().unwrap();
        }
        v
    }

    #[test]
    fn detects_kronos_with_assumed_cutoff() {
        let m = detect("from model import Kronos, KronosTokenizer\nKronos.from_pretrained('NeoQuasar/Kronos-small')");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].name, "kronos-small");
        assert_eq!(m[0].cutoff, DEFAULT_UNKNOWN_CUTOFF);
        assert!(m[0].assumed);
    }

    #[test]
    fn generic_torch_is_treated_as_pretrained() {
        let m = detect("import torch\ndef generate_signals(df): ...");
        assert_eq!(m.len(), 1);
        assert!(m[0].name.starts_with("unknown-pretrained"));
    }

    #[test]
    fn plain_strategy_is_unaffected() {
        assert!(detect(
            "def generate_signals(df, fast=5):\n    return df.close.rolling(fast).mean()"
        )
        .is_empty());
        assert!(check(&ModelUse::default(), 20).is_ok());
    }

    #[test]
    fn rejects_window_inside_training_data() {
        let c = cal("2024-01-01", 600);
        let u = ModelUse {
            models: detect("Kronos-mini"),
            windows: vec![("dev".into(), "2025-03-03".into())],
            calendar: c,
        };
        let e = check(&u, 20).unwrap_err();
        assert!(e.contains("model look-ahead"), "{e}");
    }

    #[test]
    fn gap_is_counted_in_trading_days_and_every_window_checked() {
        let c = cal("2025-01-01", 400);
        let models = detect("kronos-small");
        // 2025-09-10 .. : first 20 weekdays after 09-09 end on 2025-10-07
        let ok_start = "2025-10-08";
        let short = "2025-10-07"; // only 19 trading days strictly between
        assert_eq!(trading_days_between(&c, "2025-09-09", ok_start), 20);
        let good = ModelUse {
            models: models.clone(),
            windows: vec![
                ("dev".into(), ok_start.into()),
                ("fold 2".into(), "2026-01-05".into()),
                ("vault".into(), "2026-03-02".into()),
            ],
            calendar: c.clone(),
        };
        assert!(check(&good, 20).is_ok());
        let bad = ModelUse {
            windows: vec![
                ("dev".into(), ok_start.into()),
                ("fold 1".into(), short.into()),
            ],
            ..good
        };
        let e = check(&bad, 20).unwrap_err();
        assert!(e.contains("fold 1") && !e.contains("dev starts"), "{e}");
    }

    #[test]
    fn no_windows_is_a_rejection() {
        let u = ModelUse {
            models: detect("kronos"),
            ..Default::default()
        };
        assert!(check(&u, 20).is_err());
    }
}
