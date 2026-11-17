//! Framed message I/O on reliable QUIC streams.

use quinn::{ReadExactError, RecvStream, SendStream};
use ruststream_protocol::framing;
use serde::{Serialize, de::DeserializeOwned};

use crate::Result;

/// Writes one framed message.
pub async fn send_msg<T: Serialize>(send: &mut SendStream, msg: &T) -> Result<()> {
    let buf = framing::encode(msg)?;
    send.write_all(&buf).await?;
    Ok(())
}

/// Reads one framed message. Returns `Ok(None)` if the peer finished the
/// stream cleanly at a message boundary.
pub async fn recv_msg<T: DeserializeOwned>(
    recv: &mut RecvStream,
    scratch: &mut Vec<u8>,
) -> Result<Option<T>> {
    let mut prefix = [0u8; framing::PREFIX_LEN];
    match recv.read_exact(&mut prefix).await {
        Ok(()) => {}
        Err(ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = framing::payload_len(prefix)?;
    scratch.clear();
    scratch.resize(len, 0);
    recv.read_exact(scratch).await?;
    Ok(Some(framing::decode(scratch)?))
}
