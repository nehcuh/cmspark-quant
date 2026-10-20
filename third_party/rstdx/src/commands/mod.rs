//! Command layer: TDX command implementations.

/// Ensure the buffer has at least `needed` bytes remaining.
#[macro_export]
macro_rules! ensure_bytes {
    ($buf:expr, $needed:expr) => {
        if $buf.remaining() < $needed {
            return Err($crate::error::Error::InvalidResponse(format!(
                "Need {} bytes, have {}",
                $needed,
                $buf.remaining()
            )));
        }
    };
}

pub mod finance;
pub mod minute_time;
pub mod security_bars;
pub mod security_list;
pub mod security_quotes;
pub mod setup;
pub mod transaction;

pub use finance::{FinanceInfo, get_finance_info};
pub use minute_time::{MinuteTimeData, get_minute_time_data};
pub use security_bars::{Kline, get_security_bars};
pub use security_list::{SecurityItem, get_security_list};
pub use security_quotes::{SecurityQuote, get_security_quotes};
pub use setup::setup_connection;
pub use transaction::{TransactionData, get_transaction_data};
