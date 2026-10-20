//! DateTime encoding/decoding.

use crate::error::{Error, Result};

/// DateTime format for encoding/decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateTimeFormat {
    /// Minute-level K-line: 4 bytes (date(2) + time(2))
    Minute,
    /// Daily and above: 4 bytes (YYYYMMDD)
    Daily,
}

/// Decode datetime from TDX format.
pub fn decode_datetime(
    buf: &[u8],
    pos: usize,
    format: DateTimeFormat,
) -> Result<(u16, u8, u8, u8, u8, usize)> {
    if pos + 4 > buf.len() {
        return Err(Error::Protocol("Buffer too short for datetime".into()));
    }

    let (year, month, day, hour, minute) = match format {
        DateTimeFormat::Minute => {
            // Format: 2 bytes date + 2 bytes time
            let zipday = u16::from_le_bytes([buf[pos], buf[pos + 1]]);
            let tminutes = u16::from_le_bytes([buf[pos + 2], buf[pos + 3]]);

            let year = (zipday >> 11) + 2004;
            let month_day = zipday % 2048;
            let month = (month_day / 100) as u8;
            let day = (month_day % 100) as u8;
            let hour = (tminutes / 60) as u8;
            let minute = (tminutes % 60) as u8;

            (year, month, day, hour, minute)
        }
        DateTimeFormat::Daily => {
            // Format: 4 bytes YYYYMMDD
            let yyyymmdd = u32::from_le_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]);

            let year = (yyyymmdd / 10000) as u16;
            let month = ((yyyymmdd % 10000) / 100) as u8;
            let day = (yyyymmdd % 100) as u8;
            let hour = 15; // Default for daily K-line
            let minute = 0;

            (year, month, day, hour, minute)
        }
    };

    Ok((year, month, day, hour, minute, pos + 4))
}

/// Encode datetime to TDX format.
pub fn encode_datetime(
    year: u16,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    format: DateTimeFormat,
) -> [u8; 4] {
    match format {
        DateTimeFormat::Minute => {
            let zipday = ((year - 2004) << 11) | ((month as u16) * 100 + (day as u16));
            let tminutes = (hour as u16) * 60 + (minute as u16);

            let mut result = [0u8; 4];
            result[0..2].copy_from_slice(&zipday.to_le_bytes());
            result[2..4].copy_from_slice(&tminutes.to_le_bytes());
            result
        }
        DateTimeFormat::Daily => {
            let yyyymmdd = (year as u32) * 10000 + (month as u32) * 100 + (day as u32);

            let mut result = [0u8; 4];
            result.copy_from_slice(&yyyymmdd.to_le_bytes());
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn test_encode_decode_minute() {
        let encoded = encode_datetime(2024, 4, 22, 10, 30, DateTimeFormat::Minute);
        let (year, month, day, hour, minute, _) =
            decode_datetime(&encoded, 0, DateTimeFormat::Minute).unwrap();

        assert_eq!(year, 2024);
        assert_eq!(month, 4);
        assert_eq!(day, 22);
        assert_eq!(hour, 10);
        assert_eq!(minute, 30);
    }

    #[test]
    fn test_encode_decode_daily() {
        let encoded = encode_datetime(2024, 4, 22, 15, 0, DateTimeFormat::Daily);
        let (year, month, day, hour, minute, _) =
            decode_datetime(&encoded, 0, DateTimeFormat::Daily).unwrap();

        assert_eq!(year, 2024);
        assert_eq!(month, 4);
        assert_eq!(day, 22);
        assert_eq!(hour, 15); // Default for daily
        assert_eq!(minute, 0);
    }

    #[test]
    fn test_encode_roundtrip_minute() {
        for (year, month, day, hour, minute) in &[
            (2024, 1, 1, 0, 0),
            (2024, 6, 15, 9, 30),
            (2024, 12, 31, 23, 59),
        ] {
            let encoded =
                encode_datetime(*year, *month, *day, *hour, *minute, DateTimeFormat::Minute);
            let (y, m, d, h, min, _) =
                decode_datetime(&encoded, 0, DateTimeFormat::Minute).unwrap();
            assert_eq!((y, m, d, h, min), (*year, *month, *day, *hour, *minute));
        }
    }

    #[test]
    fn test_decode_buffer_too_short() {
        let result = decode_datetime(&[0, 0], 0, DateTimeFormat::Minute);
        assert!(result.is_err());
    }

    #[test]
    fn test_encode_decode_year_boundary() {
        // Year 2004 (base year)
        let encoded = encode_datetime(2004, 1, 1, 0, 0, DateTimeFormat::Minute);
        let (year, month, day, hour, minute, _) =
            decode_datetime(&encoded, 0, DateTimeFormat::Minute).unwrap();
        assert_eq!(year, 2004);
        assert_eq!((month, day, hour, minute), (1, 1, 0, 0));
    }

    #[test]
    fn test_encode_decode_far_future() {
        // Year 2035 (max for minute format: 2004 + 31 = 2035, since 11 bits max = 2048-1)
        // Actually: zipday uses 11 bits for year offset, max = 2047 >> 11 = 31
        let encoded = encode_datetime(2035, 12, 31, 23, 59, DateTimeFormat::Minute);
        let (year, month, day, hour, minute, _) =
            decode_datetime(&encoded, 0, DateTimeFormat::Minute).unwrap();
        assert_eq!(year, 2035);
        assert_eq!((month, day, hour, minute), (12, 31, 23, 59));
    }

    #[test]
    fn test_encode_decode_leap_year() {
        let encoded = encode_datetime(2024, 2, 29, 14, 30, DateTimeFormat::Daily);
        let (year, month, day, hour, minute, _) =
            decode_datetime(&encoded, 0, DateTimeFormat::Daily).unwrap();
        assert_eq!((year, month, day), (2024, 2, 29));
        assert_eq!((hour, minute), (15, 0)); // daily defaults to 15:00
    }

    #[test]
    fn test_encode_decode_daily_roundtrip() {
        for (year, month, day) in &[(2000, 1, 1), (2024, 6, 15), (2099, 12, 31)] {
            let encoded = encode_datetime(*year, *month, *day, 0, 0, DateTimeFormat::Daily);
            let (y, m, d, _, _, _) = decode_datetime(&encoded, 0, DateTimeFormat::Daily).unwrap();
            assert_eq!((y, m, d), (*year, *month, *day));
        }
    }

    proptest! {
        #[test]
        fn prop_encode_decode_minute_roundtrip(
            year in 2004u16..2035u16,
            month in 1u8..12u8,
            day in 1u8..28u8,
            hour in 0u8..23u8,
            minute in 0u8..59u8,
        ) {
            let encoded = encode_datetime(year, month, day, hour, minute, DateTimeFormat::Minute);
            let (y, m, d, h, min, pos) = decode_datetime(&encoded, 0, DateTimeFormat::Minute).unwrap();
            prop_assert_eq!((y, m, d, h, min), (year, month, day, hour, minute));
            prop_assert_eq!(pos, 4);
        }

        #[test]
        fn prop_encode_decode_daily_roundtrip(
            year in 1990u16..2100u16,
            month in 1u8..12u8,
            day in 1u8..28u8,
        ) {
            let encoded = encode_datetime(year, month, day, 0, 0, DateTimeFormat::Daily);
            let (y, m, d, _, _, pos) = decode_datetime(&encoded, 0, DateTimeFormat::Daily).unwrap();
            prop_assert_eq!((y, m, d), (year, month, day));
            prop_assert_eq!(pos, 4);
        }
    }
}
