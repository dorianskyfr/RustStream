//! Length-prefixed framing for messages carried on reliable QUIC streams.
//!
//! Layout: `u32` little-endian payload length, followed by the postcard
//! payload. The length is checked against [`MAX_MESSAGE_SIZE`] before any
//! buffer is allocated, so a malicious peer cannot make us allocate gigabytes.

use serde::{Serialize, de::DeserializeOwned};

use crate::{MAX_MESSAGE_SIZE, ProtocolError};

/// Size of the length prefix.
pub const PREFIX_LEN: usize = 4;

/// Appends one framed message to `out`.
pub fn encode_into<T: Serialize>(msg: &T, out: &mut Vec<u8>) -> Result<(), ProtocolError> {
    let start = out.len();
    out.extend_from_slice(&[0; PREFIX_LEN]);
    let mut out_ext = postcard::to_extend(msg, std::mem::take(out))?;
    let len = out_ext.len() - start - PREFIX_LEN;
    if len > MAX_MESSAGE_SIZE {
        out_ext.truncate(start);
        *out = out_ext;
        return Err(ProtocolError::TooLarge(len));
    }
    // `len <= MAX_MESSAGE_SIZE` fits in u32.
    out_ext[start..start + PREFIX_LEN].copy_from_slice(&(len as u32).to_le_bytes());
    *out = out_ext;
    Ok(())
}

/// Encodes one framed message into a new buffer.
pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, ProtocolError> {
    let mut out = Vec::with_capacity(64);
    encode_into(msg, &mut out)?;
    Ok(out)
}

/// Parses and validates a length prefix.
pub fn payload_len(prefix: [u8; PREFIX_LEN]) -> Result<usize, ProtocolError> {
    let len = u32::from_le_bytes(prefix) as usize;
    if len > MAX_MESSAGE_SIZE {
        return Err(ProtocolError::TooLarge(len));
    }
    Ok(len)
}

/// Decodes a payload (without its prefix). Trailing bytes are rejected.
pub fn decode<T: DeserializeOwned>(payload: &[u8]) -> Result<T, ProtocolError> {
    let (msg, rest) = postcard::take_from_bytes(payload)?;
    if !rest.is_empty() {
        return Err(ProtocolError::Malformed("trailing bytes"));
    }
    Ok(msg)
}

/// Incremental decoder for a byte stream containing several framed messages.
/// Used by tests and by transports that deliver arbitrary chunks.
#[derive(Debug, Default)]
pub struct FrameBuffer {
    buf: Vec<u8>,
}

impl FrameBuffer {
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Returns the next complete message, if any.
    pub fn next_message<T: DeserializeOwned>(&mut self) -> Result<Option<T>, ProtocolError> {
        if self.buf.len() < PREFIX_LEN {
            return Ok(None);
        }
        let mut prefix = [0; PREFIX_LEN];
        prefix.copy_from_slice(&self.buf[..PREFIX_LEN]);
        let len = payload_len(prefix)?;
        if self.buf.len() < PREFIX_LEN + len {
            return Ok(None);
        }
        let msg = decode(&self.buf[PREFIX_LEN..PREFIX_LEN + len])?;
        self.buf.drain(..PREFIX_LEN + len);
        Ok(Some(msg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ControlMessage, InputEvent};

    #[test]
    fn several_messages_in_chunks() {
        let msgs = vec![
            ControlMessage::Ping { nonce: 1 },
            ControlMessage::RequestKeyframe,
            ControlMessage::Goodbye {
                reason: "bye".into(),
            },
        ];
        let mut wire = Vec::new();
        for m in &msgs {
            encode_into(m, &mut wire).expect("encode");
        }
        let mut fb = FrameBuffer::default();
        let mut got = Vec::new();
        for chunk in wire.chunks(3) {
            fb.push(chunk);
            while let Some(m) = fb.next_message::<ControlMessage>().expect("decode") {
                got.push(m);
            }
        }
        assert_eq!(got, msgs);
    }

    #[test]
    fn rejects_oversized_prefix() {
        let prefix = ((MAX_MESSAGE_SIZE + 1) as u32).to_le_bytes();
        assert!(matches!(
            payload_len(prefix),
            Err(ProtocolError::TooLarge(_))
        ));
    }

    #[test]
    fn rejects_trailing_bytes_and_garbage() {
        let mut payload = postcard::to_allocvec(&InputEvent::ReleaseAll).expect("encode");
        payload.push(0);
        assert!(decode::<InputEvent>(&payload).is_err());
        assert!(decode::<ControlMessage>(&[0xff, 0xff, 0xff]).is_err());
    }

    #[test]
    fn appends_after_existing_data() {
        let mut out = vec![9, 9];
        encode_into(&InputEvent::ReleaseAll, &mut out).expect("encode");
        assert_eq!(&out[..2], &[9, 9]);
        let mut prefix = [0; PREFIX_LEN];
        prefix.copy_from_slice(&out[2..6]);
        let len = payload_len(prefix).expect("len");
        assert_eq!(out.len(), 6 + len);
    }
}
