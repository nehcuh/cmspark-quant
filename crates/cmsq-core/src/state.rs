use crate::error::CoreError;
use crate::models::StrategyStatus::{self, *};

/// Allowed transitions.
///
/// ```text
/// Ingested -> Extracted | Failed
/// Extracted -> Backtesting
/// Backtesting -> Evaluated | Failed          (Failed also via crash recovery)
/// Evaluated -> GatePassed | Rejected         (gate on development data, trusted metrics)
/// GatePassed -> Promoted | Rejected          (manual promote = one-time vault holdout check)
/// Rejected | Failed | GatePassed -> Backtesting   (re-run; every run is a library trial)
/// ```
pub fn is_valid_transition(from: StrategyStatus, to: StrategyStatus) -> bool {
    matches!(
        (from, to),
        (Ingested, Extracted)
            | (Ingested, Failed)
            | (Extracted, Backtesting)
            | (Backtesting, Evaluated)
            | (Backtesting, Failed)
            | (Evaluated, GatePassed)
            | (Evaluated, Rejected)
            | (GatePassed, Promoted)
            | (GatePassed, Rejected)
            | (GatePassed, Backtesting)
            | (Rejected, Backtesting)
            | (Failed, Backtesting)
    )
}

pub fn check(from: StrategyStatus, to: StrategyStatus) -> anyhow::Result<()> {
    if is_valid_transition(from, to) {
        Ok(())
    } else {
        Err(CoreError::InvalidTransition { from, to }.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_is_valid() {
        let path = [
            Ingested,
            Extracted,
            Backtesting,
            Evaluated,
            GatePassed,
            Promoted,
        ];
        for w in path.windows(2) {
            assert!(is_valid_transition(w[0], w[1]), "{:?}", w);
        }
    }

    #[test]
    fn cannot_skip_backtest_gate_or_promote_from_rejected() {
        assert!(!is_valid_transition(Extracted, Promoted));
        assert!(!is_valid_transition(Ingested, Evaluated));
        assert!(
            !is_valid_transition(Evaluated, Promoted),
            "must pass the gate first"
        );
        assert!(!is_valid_transition(Rejected, Promoted));
        assert!(!is_valid_transition(Failed, Promoted));
        assert!(!is_valid_transition(Promoted, Backtesting));
        let e = check(Backtesting, Promoted).unwrap_err();
        assert_eq!(
            e.downcast_ref::<CoreError>(),
            Some(&CoreError::InvalidTransition {
                from: Backtesting,
                to: Promoted
            })
        );
    }

    #[test]
    fn rerun_paths_exist() {
        assert!(is_valid_transition(Rejected, Backtesting));
        assert!(is_valid_transition(Failed, Backtesting));
        assert!(is_valid_transition(GatePassed, Backtesting));
    }
}
