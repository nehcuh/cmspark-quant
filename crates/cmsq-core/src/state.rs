use crate::models::StrategyStatus::{self, *};

/// Allowed transitions. `Failed`/`Rejected`/`Evaluated` → `Backtesting` is the
/// "re-run" path (each re-run increments the attempt counter used by the gate's
/// multiple-testing penalty).
pub fn is_valid_transition(from: StrategyStatus, to: StrategyStatus) -> bool {
    matches!(
        (from, to),
        (Ingested, Extracted)
            | (Ingested, Failed)
            | (Extracted, Backtesting)
            | (Backtesting, Evaluated)
            | (Backtesting, Failed)
            | (Evaluated, Promoted)
            | (Evaluated, Rejected)
            | (Evaluated, Backtesting)
            | (Rejected, Backtesting)
            | (Failed, Backtesting)
    )
}

pub fn check(from: StrategyStatus, to: StrategyStatus) -> anyhow::Result<()> {
    if is_valid_transition(from, to) {
        Ok(())
    } else {
        anyhow::bail!("invalid transition {from} -> {to}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_is_valid() {
        let path = [Ingested, Extracted, Backtesting, Evaluated, Promoted];
        for w in path.windows(2) {
            assert!(is_valid_transition(w[0], w[1]), "{:?}", w);
        }
    }

    #[test]
    fn cannot_skip_backtest_or_promote_from_rejected() {
        assert!(!is_valid_transition(Extracted, Promoted));
        assert!(!is_valid_transition(Ingested, Evaluated));
        assert!(!is_valid_transition(Rejected, Promoted));
        assert!(!is_valid_transition(Failed, Promoted));
        assert!(!is_valid_transition(Promoted, Backtesting));
        assert!(check(Backtesting, Promoted).is_err());
    }

    #[test]
    fn rerun_paths_exist() {
        assert!(is_valid_transition(Rejected, Backtesting));
        assert!(is_valid_transition(Failed, Backtesting));
    }
}
