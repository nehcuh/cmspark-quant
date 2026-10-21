//! Setup commands for connection initialization.

use crate::client::TdxConnection;
use crate::error::Result;

/// Setup connection with handshake commands.
pub async fn setup_connection(conn: &mut TdxConnection) -> Result<()> {
    // SetupCmd1
    conn.send(SETUP_CMD_1).await?;
    conn.recv_response().await?;

    // SetupCmd2
    conn.send(SETUP_CMD_2).await?;
    conn.recv_response().await?;

    // SetupCmd3
    conn.send(SETUP_CMD_3).await?;
    conn.recv_response().await?;

    Ok(())
}

/// Setup command 1.
const SETUP_CMD_1: &[u8] = &[
    0x0c, 0x02, 0x18, 0x93, 0x00, 0x01, 0x03, 0x00, 0x03, 0x00, 0x0d, 0x00, 0x01,
];

/// Setup command 2.
const SETUP_CMD_2: &[u8] = &[
    0x0c, 0x02, 0x18, 0x94, 0x00, 0x01, 0x03, 0x00, 0x03, 0x00, 0x0d, 0x00, 0x02,
];

/// Setup command 3.
const SETUP_CMD_3: &[u8] = &[
    0x0c, 0x03, 0x18, 0x99, 0x00, 0x01, 0x20, 0x00, 0x20, 0x00, 0xdb, 0x0f, 0xd5, 0xd0, 0xc9, 0xcc,
    0xd6, 0xa4, 0xa8, 0xaf, 0x00, 0x00, 0x00, 0x8f, 0xc2, 0x25, 0x40, 0x13, 0x00, 0x00, 0xd5, 0x00,
    0xc9, 0xcc, 0xbd, 0xf0, 0xd7, 0xea, 0x00, 0x00, 0x00, 0x02,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_setup_cmd_lengths() {
        assert_eq!(SETUP_CMD_1.len(), 13);
        assert_eq!(SETUP_CMD_2.len(), 13);
        assert_eq!(SETUP_CMD_3.len(), 42);
    }

    #[test]
    fn test_setup_cmd_magic_bytes() {
        // All setup commands start with magic 0x0c
        assert_eq!(SETUP_CMD_1[0], 0x0c);
        assert_eq!(SETUP_CMD_2[0], 0x0c);
        assert_eq!(SETUP_CMD_3[0], 0x0c);
    }

    #[test]
    fn test_setup_cmd_distinct() {
        // Setup commands should be different from each other
        assert_ne!(SETUP_CMD_1, SETUP_CMD_2);
        assert_ne!(SETUP_CMD_2, SETUP_CMD_3);
    }

    #[test]
    fn test_setup_cmd_3_not_empty() {
        assert!(!SETUP_CMD_3.is_empty());
        assert!(SETUP_CMD_3.len() > SETUP_CMD_1.len());
    }

    #[test]
    fn test_setup_cmd_1_structure() {
        // First byte should be magic, last byte should be sequence number
        assert_eq!(SETUP_CMD_1.last().copied(), Some(0x01));
        assert_eq!(SETUP_CMD_2.last().copied(), Some(0x02));
    }
}
