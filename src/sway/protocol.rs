//! Sway/i3 IPC wire protocol.
//!
//! Every message is `"i3-ipc"` + payload length (u32 LE) + message type (u32
//! LE) + payload. Events arrive on a subscribed connection with the high bit
//! of the type set.

use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const MAGIC: &[u8; 6] = b"i3-ipc";
pub const HEADER_LEN: usize = MAGIC.len() + 8;

/// Message types Suede sends.
pub mod message {
    pub const RUN_COMMAND: u32 = 0;
    pub const SUBSCRIBE: u32 = 2;
    pub const GET_OUTPUTS: u32 = 3;
    pub const GET_TREE: u32 = 4;
    pub const GET_VERSION: u32 = 7;
}

/// The high bit distinguishes an event from a reply.
pub const EVENT_BIT: u32 = 0x8000_0000;

/// Event types Suede subscribes to.
pub mod event {
    use super::EVENT_BIT;
    pub const WORKSPACE: u32 = EVENT_BIT;
    pub const OUTPUT: u32 = EVENT_BIT | 1;
    pub const WINDOW: u32 = EVENT_BIT | 3;
    pub const SHUTDOWN: u32 = EVENT_BIT | 6;
}

/// Guards against a desynchronized stream allocating absurd buffers.
const MAX_PAYLOAD: u32 = 64 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid IPC magic header")]
    BadMagic,
    #[error("payload of {0} bytes exceeds the maximum")]
    PayloadTooLarge(u32),
}

/// Write one framed message.
pub async fn write_message<W>(
    stream: &mut W,
    message_type: u32,
    payload: &[u8],
) -> Result<(), ProtocolError>
where
    W: AsyncWriteExt + Unpin,
{
    let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
    frame.extend_from_slice(MAGIC);
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&message_type.to_le_bytes());
    frame.extend_from_slice(payload);
    stream.write_all(&frame).await?;
    stream.flush().await?;
    Ok(())
}

/// Read one framed message, returning its type and raw payload.
pub async fn read_message<R>(stream: &mut R) -> Result<(u32, Vec<u8>), ProtocolError>
where
    R: AsyncReadExt + Unpin,
{
    let mut header = [0u8; HEADER_LEN];
    stream.read_exact(&mut header).await?;
    if &header[..MAGIC.len()] != MAGIC {
        return Err(ProtocolError::BadMagic);
    }
    let length = u32::from_le_bytes(header[MAGIC.len()..MAGIC.len() + 4].try_into().unwrap());
    let message_type = u32::from_le_bytes(header[MAGIC.len() + 4..].try_into().unwrap());
    if length > MAX_PAYLOAD {
        return Err(ProtocolError::PayloadTooLarge(length));
    }
    let mut payload = vec![0u8; length as usize];
    stream.read_exact(&mut payload).await?;
    Ok((message_type, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trips_a_message() {
        let mut buffer = Vec::new();
        write_message(&mut buffer, message::RUN_COMMAND, b"output HDMI-A-1 enable")
            .await
            .unwrap();
        assert_eq!(&buffer[..6], MAGIC);

        let mut cursor = std::io::Cursor::new(buffer);
        let (message_type, payload) = read_message(&mut cursor).await.unwrap();
        assert_eq!(message_type, message::RUN_COMMAND);
        assert_eq!(payload, b"output HDMI-A-1 enable");
    }

    #[tokio::test]
    async fn round_trips_an_empty_payload() {
        let mut buffer = Vec::new();
        write_message(&mut buffer, message::GET_OUTPUTS, b"")
            .await
            .unwrap();
        let mut cursor = std::io::Cursor::new(buffer);
        let (message_type, payload) = read_message(&mut cursor).await.unwrap();
        assert_eq!(message_type, message::GET_OUTPUTS);
        assert!(payload.is_empty());
    }

    #[tokio::test]
    async fn rejects_bad_magic() {
        let mut frame = Vec::new();
        frame.extend_from_slice(b"x3-ipc");
        frame.extend_from_slice(&0u32.to_le_bytes());
        frame.extend_from_slice(&0u32.to_le_bytes());
        let mut cursor = std::io::Cursor::new(frame);
        assert!(matches!(
            read_message(&mut cursor).await,
            Err(ProtocolError::BadMagic)
        ));
    }

    #[test]
    fn event_types_have_the_high_bit() {
        assert_eq!(event::WINDOW, 0x8000_0003);
        assert_eq!(event::OUTPUT, 0x8000_0001);
    }
}
