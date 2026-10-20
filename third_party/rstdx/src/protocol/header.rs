//! Protocol header definitions.

use bytes::Buf;

/// Response header (16 bytes).
#[derive(Debug, Clone, Copy)]
pub struct ResponseHeader {
    pub zip_size: u16,
    pub unzip_size: u16,
}

/// Parse response header from bytes.
pub fn parse_response_header(data: &[u8]) -> Option<ResponseHeader> {
    if data.len() < 16 {
        return None;
    }

    let mut cursor = &data[..16];
    let _unknown1 = cursor.get_u32_le();
    let _unknown2 = cursor.get_u32_le();
    let _unknown3 = cursor.get_u32_le();
    let zip_size = cursor.get_u16_le();
    let unzip_size = cursor.get_u16_le();

    Some(ResponseHeader {
        zip_size,
        unzip_size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_response_header() {
        let data = [
            0x00, 0x00, 0x00, 0x00, // unknown1
            0x00, 0x00, 0x00, 0x00, // unknown2
            0x00, 0x00, 0x00, 0x00, // unknown3
            0x10, 0x00, // zip_size = 16
            0x20, 0x00, // unzip_size = 32
        ];

        let header = parse_response_header(&data).unwrap();
        assert_eq!(header.zip_size, 16);
        assert_eq!(header.unzip_size, 32);
    }

    #[test]
    fn test_parse_response_header_too_short() {
        let data = [0x00; 15];
        assert!(parse_response_header(&data).is_none());
    }

    #[test]
    fn test_parse_response_header_zero() {
        let data = [0x00; 16];
        let header = parse_response_header(&data).unwrap();
        assert_eq!(header.zip_size, 0);
        assert_eq!(header.unzip_size, 0);
    }

    #[test]
    fn test_response_header_copy() {
        let header = ResponseHeader {
            zip_size: 100,
            unzip_size: 200,
        };
        let copied = header;
        assert_eq!(copied.zip_size, 100);
    }
}
