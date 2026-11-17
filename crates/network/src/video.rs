//! Encoded video frames over QUIC datagrams.

use quinn::Connection;
use ruststream_protocol::video::{FrameMeta, fragment};

use crate::{NetError, Result};

/// Sends one encoded frame as datagrams. Returns the number of datagrams.
///
/// Datagrams are queued in Quinn's bounded send buffer; if the path is
/// congested Quinn drops the oldest queued datagrams first, so a stale frame
/// is sacrificed rather than delaying the newest one.
pub fn send_frame(conn: &Connection, meta: FrameMeta, data: &[u8]) -> Result<usize> {
    let max = conn
        .max_datagram_size()
        .ok_or(NetError::DatagramsUnsupported)?;
    let mut err = None;
    let n = fragment(meta, data, max, |d| {
        if err.is_none()
            && let Err(e) = conn.send_datagram(d)
        {
            err = Some(e);
        }
    })?;
    match err {
        Some(e) => Err(e.into()),
        None => Ok(n),
    }
}
