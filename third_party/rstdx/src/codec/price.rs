//! Price encoding/decoding (variable-length integer).

use crate::error::{Error, Result};

/// Encode a price difference as variable-length integer.
///
/// Format (from pytdx):
/// - First byte: bits 0-5 = data, bit 6 = sign, bit 7 = continue
/// - Subsequent bytes: bits 0-6 = data, bit 7 = continue
pub fn encode_price(n: i32) -> Vec<u8> {
    if n == 0 {
        return vec![0];
    }

    let sign = n < 0;
    let mut value = n.unsigned_abs();

    // Check if we need more than one byte
    let needs_continuation = value >= 0x40;

    // First byte: 6 bits data + sign bit + optional continue bit
    let mut first_byte = ((value & 0x3F) as u8) | ((sign as u8) << 6);
    if needs_continuation {
        first_byte |= 0x80; // Set continue bit
    }
    value >>= 6;

    let mut result = vec![first_byte];

    // Subsequent bytes: 7 bits data + optional continue bit
    while value > 0 {
        let byte = ((value & 0x7F) as u8) | 0x80; // Set continue bit
        value >>= 7;
        result.push(byte);
    }

    // Clear continue bit on last byte
    if let Some(last) = result.last_mut() {
        *last &= 0x7F;
    }

    result
}

/// Decode a price difference from variable-length integer.
pub fn decode_price(buf: &[u8], pos: usize) -> Result<(i32, usize)> {
    if pos >= buf.len() {
        return Err(Error::Protocol("Buffer too short".into()));
    }

    let first = buf[pos];
    let sign = (first & 0x40) != 0;
    let mut result = (first & 0x3F) as u32;
    let mut pos_byte = 6;
    let mut read_pos = pos + 1;

    // Check for continuation
    if (first & 0x80) != 0 {
        loop {
            if read_pos >= buf.len() {
                return Err(Error::Protocol(
                    "Price decode: truncated continuation bytes".into(),
                ));
            }
            let byte = buf[read_pos];
            // Guard against shift overflow: i32 needs at most 5 continuation
            // bytes (6 bits + 4×7 bits = 34 bits). Beyond that the encoding is
            // either malformed or exceeds i32 range.
            if pos_byte >= 32 {
                return Err(Error::Protocol(
                    "Price decode: excessive continuation bytes".into(),
                ));
            }
            result |= ((byte & 0x7F) as u32) << pos_byte;
            pos_byte += 7;
            read_pos += 1;

            if (byte & 0x80) == 0 {
                break;
            }
        }
    }

    let value = if sign {
        (result as i32).wrapping_neg()
    } else {
        result as i32
    };

    Ok((value, read_pos))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn test_encode_zero() {
        assert_eq!(encode_price(0), vec![0]);
    }

    #[test]
    fn test_encode_small_positive() {
        // 63 = 0x3F fits in one byte (no sign bit)
        assert_eq!(encode_price(63), vec![0x3F]);
    }

    #[test]
    fn test_encode_small_negative() {
        // -63 = 0x7F (sign bit set, 6 bits data)
        assert_eq!(encode_price(-63), vec![0x7F]);
    }

    #[test]
    fn test_encode_64() {
        // 64 needs 2 bytes
        let encoded = encode_price(64);
        assert_eq!(encoded.len(), 2);
        assert_eq!(encoded[0] & 0x80, 0x80); // Continue bit set
        let (decoded, _) = decode_price(&encoded, 0).unwrap();
        assert_eq!(decoded, 64);
    }

    #[test]
    fn test_decode_encode_roundtrip() {
        for value in &[-10000, -100, -1, 0, 1, 100, 10000] {
            let encoded = encode_price(*value);
            let (decoded, _) = decode_price(&encoded, 0).unwrap();
            assert_eq!(decoded, *value, "Roundtrip failed for {}", value);
        }
    }

    #[test]
    fn test_values_0_to_100() {
        for value in 0..=100 {
            let encoded = encode_price(value);
            let (decoded, pos) = decode_price(&encoded, 0).unwrap();
            assert_eq!(decoded, value, "Failed for {}", value);
            assert_eq!(pos, encoded.len());
        }
    }

    #[test]
    fn test_multi_byte_values() {
        for value in [64, 100, 1000, 10000, 100000] {
            let encoded = encode_price(value);
            let (decoded, pos) = decode_price(&encoded, 0).unwrap();
            assert_eq!(decoded, value);
            assert_eq!(pos, encoded.len());
        }
    }

    #[test]
    fn test_decode_price_at_position() {
        let encoded = encode_price(100);
        let data = [0u8, 0u8];
        let mut combined = data.to_vec();
        combined.extend_from_slice(&encoded);
        let (decoded, pos) = decode_price(&combined, 2).unwrap();
        assert_eq!(decoded, 100);
        assert_eq!(pos, 2 + encoded.len());
    }

    #[test]
    fn test_decode_price_buffer_too_short() {
        let result = decode_price(&[], 0);
        assert!(result.is_err());
    }

    #[test]
    fn test_decode_price_excessive_continuation() {
        // 6 bytes all with continue bit set — exceeds i32 range and should
        // return an error rather than panic or silently truncate.
        let data = [0xC0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        let result = decode_price(&data, 0);
        assert!(result.is_err(), "Expected error for excessive continuation bytes");
    }

    #[test]
    fn test_decode_price_continuation_truncated() {
        // First byte has continue bit but no follow-up bytes
        let data = [0xC0];
        let result = decode_price(&data, 0);
        assert!(result.is_err(), "Expected error for truncated continuation");
    }

    proptest! {
        #[test]
        fn prop_encode_decode_roundtrip(value in -10_000_000i32..10_000_000i32) {
            let encoded = encode_price(value);
            let (decoded, pos) = decode_price(&encoded, 0).unwrap();
            prop_assert_eq!(decoded, value);
            prop_assert_eq!(pos, encoded.len());
        }

        #[test]
        fn prop_decode_at_position(
            value in -1_000_000i32..1_000_000i32,
            prefix_len in 0usize..100
        ) {
            let encoded = encode_price(value);
            let mut data = vec![0u8; prefix_len];
            data.extend_from_slice(&encoded);
            let (decoded, pos) = decode_price(&data, prefix_len).unwrap();
            prop_assert_eq!(decoded, value);
            prop_assert_eq!(pos, prefix_len + encoded.len());
        }
    }

    #[test]
    fn test_wrapping_neg_does_not_panic() {
        // Ensure that the decode path handles the edge case where
        // sign bit is set and result == i32::MAX as u32 + 1 (0x80000000)
        // This would panic with -(result as i32) but wrapping_neg handles it.
        let encoded = encode_price(-1);
        let (decoded, _) = decode_price(&encoded, 0).unwrap();
        assert_eq!(decoded, -1);

        let encoded = encode_price(i32::MIN + 1);
        let (decoded, _) = decode_price(&encoded, 0).unwrap();
        assert_eq!(decoded, i32::MIN + 1);
    }
}
