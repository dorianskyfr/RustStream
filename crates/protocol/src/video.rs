//! Video datagram framing: fragmentation of encoded frames into QUIC
//! datagrams and loss-tolerant reassembly.
//!
//! Policy: **the newest frame wins**. A frame is delivered as soon as all its
//! fragments arrived; any older, still incomplete frame is discarded at that
//! moment instead of being waited for. Gaps are reported to the caller
//! (`lost_before`) so it can request a keyframe rather than decode a frame
//! whose reference is missing.

use bytes::{BufMut, Bytes, BytesMut};

use crate::ProtocolError;

/// Datagram type tag for video fragments.
pub const KIND_VIDEO: u8 = 0x01;
/// Fixed header size of a video fragment.
pub const HEADER_LEN: usize = 18;
/// Upper bound on fragments per frame (≈ 4.7 MB at 1200-byte datagrams).
pub const MAX_FRAGMENTS: u16 = 4096;

const FLAG_KEYFRAME: u8 = 0b0000_0001;

/// Metadata attached to every fragment of a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameMeta {
    /// Monotonically increasing per session (wraps after ~200 days at 240 fps;
    /// sessions are restarted long before that).
    pub frame_id: u32,
    pub keyframe: bool,
    /// Host capture timestamp in microseconds (host monotonic clock).
    pub timestamp_us: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Header {
    meta: FrameMeta,
    index: u16,
    count: u16,
}

impl Header {
    fn write(&self, out: &mut BytesMut) {
        out.put_u8(KIND_VIDEO);
        out.put_u32_le(self.meta.frame_id);
        out.put_u16_le(self.index);
        out.put_u16_le(self.count);
        out.put_u8(if self.meta.keyframe { FLAG_KEYFRAME } else { 0 });
        out.put_u64_le(self.meta.timestamp_us);
    }

    fn parse(d: &[u8]) -> Result<Self, ProtocolError> {
        if d.len() < HEADER_LEN {
            return Err(ProtocolError::Malformed("datagram shorter than header"));
        }
        if d[0] != KIND_VIDEO {
            return Err(ProtocolError::Malformed("unknown datagram kind"));
        }
        let u16_at = |i: usize| u16::from_le_bytes([d[i], d[i + 1]]);
        let frame_id = u32::from_le_bytes([d[1], d[2], d[3], d[4]]);
        let index = u16_at(5);
        let count = u16_at(7);
        let flags = d[9];
        let mut ts = [0u8; 8];
        ts.copy_from_slice(&d[10..18]);
        if count == 0 || count > MAX_FRAGMENTS || index >= count {
            return Err(ProtocolError::Malformed("invalid fragment index/count"));
        }
        if flags & !FLAG_KEYFRAME != 0 {
            return Err(ProtocolError::Malformed("unknown fragment flags"));
        }
        Ok(Self {
            meta: FrameMeta {
                frame_id,
                keyframe: flags & FLAG_KEYFRAME != 0,
                timestamp_us: u64::from_le_bytes(ts),
            },
            index,
            count,
        })
    }
}

/// Splits `data` into datagrams of at most `max_datagram` bytes and calls
/// `emit` for each. Uses a single allocation for the whole frame.
/// Returns the number of datagrams produced.
pub fn fragment(
    meta: FrameMeta,
    data: &[u8],
    max_datagram: usize,
    mut emit: impl FnMut(Bytes),
) -> Result<usize, ProtocolError> {
    if data.is_empty() {
        return Err(ProtocolError::Malformed("empty frame"));
    }
    if max_datagram <= HEADER_LEN {
        return Err(ProtocolError::Malformed("datagram size too small"));
    }
    let payload = max_datagram - HEADER_LEN;
    let count = data.len().div_ceil(payload);
    if count > usize::from(MAX_FRAGMENTS) {
        return Err(ProtocolError::TooLarge(data.len()));
    }
    let mut buf = BytesMut::with_capacity(data.len() + count * HEADER_LEN);
    for (index, chunk) in data.chunks(payload).enumerate() {
        Header {
            meta,
            // Both bounded by MAX_FRAGMENTS (checked above).
            index: index as u16,
            count: count as u16,
        }
        .write(&mut buf);
        buf.put_slice(chunk);
        emit(buf.split().freeze());
    }
    Ok(count)
}

/// A fully reassembled frame. `data` borrows the reassembler's output buffer.
#[derive(Debug, PartialEq, Eq)]
pub struct Frame<'a> {
    pub meta: FrameMeta,
    pub data: &'a [u8],
    /// Number of frames between the previously delivered frame and this one
    /// that were never delivered (lost or dropped). Non-zero means the decoder
    /// reference chain is broken unless this frame is a keyframe.
    pub lost_before: u32,
}

#[derive(Debug, Default)]
struct Slot {
    active: bool,
    meta: Option<FrameMeta>,
    count: u16,
    received: u16,
    parts: Vec<Option<Bytes>>,
}

impl Slot {
    fn reset(&mut self) {
        self.active = false;
        self.meta = None;
        self.count = 0;
        self.received = 0;
        self.parts.clear();
    }

    fn frame_id(&self) -> Option<u32> {
        self.meta.filter(|_| self.active).map(|m| m.frame_id)
    }
}

/// Counters exposed for telemetry.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReassemblyStats {
    pub frames_completed: u64,
    /// Frames that started arriving but were abandoned incomplete.
    pub frames_incomplete: u64,
    /// Datagrams that arrived after their frame was delivered or superseded.
    pub stale_datagrams: u64,
    pub malformed_datagrams: u64,
}

/// Reassembles fragments into frames with bounded memory.
#[derive(Debug)]
pub struct Reassembler {
    slots: Vec<Slot>,
    last_delivered: Option<u32>,
    out: Vec<u8>,
    stats: ReassemblyStats,
}

impl Default for Reassembler {
    fn default() -> Self {
        Self::new(4)
    }
}

impl Reassembler {
    /// `max_in_flight` bounds the number of frames reassembled concurrently.
    pub fn new(max_in_flight: usize) -> Self {
        let mut slots = Vec::with_capacity(max_in_flight.max(1));
        slots.resize_with(max_in_flight.max(1), Slot::default);
        Self {
            slots,
            last_delivered: None,
            out: Vec::new(),
            stats: ReassemblyStats::default(),
        }
    }

    pub fn stats(&self) -> ReassemblyStats {
        self.stats
    }

    /// Forgets all state (e.g. after a stream reconfiguration).
    pub fn reset(&mut self) {
        self.slots.iter_mut().for_each(Slot::reset);
        self.last_delivered = None;
    }

    /// Feeds one datagram. Returns a frame when this datagram completed one.
    /// Malformed datagrams are counted and reported as errors; the reassembler
    /// stays usable.
    pub fn push(&mut self, datagram: Bytes) -> Result<Option<Frame<'_>>, ProtocolError> {
        let header = match Header::parse(&datagram) {
            Ok(h) => h,
            Err(e) => {
                self.stats.malformed_datagrams += 1;
                return Err(e);
            }
        };
        let id = header.meta.frame_id;
        if self.last_delivered.is_some_and(|last| id <= last) {
            self.stats.stale_datagrams += 1;
            return Ok(None);
        }

        let slot_idx = match self.slots.iter().position(|s| s.frame_id() == Some(id)) {
            Some(i) => i,
            None => match self.claim_slot(id) {
                Some(i) => i,
                None => {
                    self.stats.stale_datagrams += 1;
                    return Ok(None);
                }
            },
        };

        let slot = &mut self.slots[slot_idx];
        if !slot.active {
            slot.active = true;
            slot.meta = Some(header.meta);
            slot.count = header.count;
            slot.received = 0;
            slot.parts.clear();
            slot.parts.resize(usize::from(header.count), None);
        } else if slot.count != header.count || slot.meta != Some(header.meta) {
            slot.reset();
            self.stats.frames_incomplete += 1;
            self.stats.malformed_datagrams += 1;
            return Err(ProtocolError::Malformed("inconsistent fragment header"));
        }

        let part = &mut slot.parts[usize::from(header.index)];
        if part.is_none() {
            *part = Some(datagram.slice(HEADER_LEN..));
            slot.received += 1;
        }
        if slot.received < slot.count {
            return Ok(None);
        }

        // Complete: assemble into the reusable output buffer.
        self.out.clear();
        for p in slot.parts.iter().flatten() {
            self.out.extend_from_slice(p);
        }
        let meta = header.meta;
        slot.reset();
        for s in &mut self.slots {
            if s.frame_id().is_some_and(|other| other < id) {
                s.reset();
                self.stats.frames_incomplete += 1;
            }
        }
        let lost_before = self
            .last_delivered
            .map_or(0, |last| id.saturating_sub(last).saturating_sub(1));
        self.last_delivered = Some(id);
        self.stats.frames_completed += 1;
        Ok(Some(Frame {
            meta,
            data: &self.out,
            lost_before,
        }))
    }

    /// Finds a slot for a new frame, evicting the oldest in-flight frame if
    /// needed. Returns `None` if the new frame is older than everything held.
    fn claim_slot(&mut self, id: u32) -> Option<usize> {
        if let Some(i) = self.slots.iter().position(|s| !s.active) {
            return Some(i);
        }
        let (oldest_idx, oldest_id) = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.frame_id().map(|f| (i, f)))
            .min_by_key(|&(_, f)| f)?;
        if id < oldest_id {
            return None;
        }
        self.slots[oldest_idx].reset();
        self.stats.frames_incomplete += 1;
        Some(oldest_idx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(id: u32, key: bool) -> FrameMeta {
        FrameMeta {
            frame_id: id,
            keyframe: key,
            timestamp_us: u64::from(id) * 16_666,
        }
    }

    fn frags(m: FrameMeta, data: &[u8], mtu: usize) -> Vec<Bytes> {
        let mut v = Vec::new();
        fragment(m, data, mtu, |b| v.push(b)).expect("fragment");
        v
    }

    fn payload(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(31) ^ seed)
            .collect()
    }

    #[test]
    fn roundtrip_out_of_order() {
        let data = payload(10_000, 7);
        let mut parts = frags(meta(1, true), &data, 1200);
        assert_eq!(parts.len(), 10_000usize.div_ceil(1200 - HEADER_LEN));
        assert!(parts.iter().all(|p| p.len() <= 1200));
        parts.reverse();
        let mut r = Reassembler::default();
        let last = parts.pop().expect("non-empty");
        for p in parts {
            assert!(r.push(p).expect("push").is_none());
        }
        let f = r.push(last).expect("push").expect("complete");
        assert_eq!(f.data, &data[..]);
        assert!(f.meta.keyframe);
        assert_eq!(f.lost_before, 0);
    }

    #[test]
    fn duplicate_fragments_are_ignored() {
        let data = payload(2900, 1);
        let parts = frags(meta(1, false), &data, 1000);
        assert_eq!(parts.len(), 3);
        let mut r = Reassembler::default();
        assert!(r.push(parts[0].clone()).expect("push").is_none());
        assert!(r.push(parts[0].clone()).expect("push").is_none());
        assert!(r.push(parts[1].clone()).expect("push").is_none());
        let f = r.push(parts[2].clone()).expect("push").expect("complete");
        assert_eq!(f.data, &data[..]);
    }

    #[test]
    fn loss_is_reported_and_old_frames_dropped() {
        let mut r = Reassembler::default();
        let f1 = frags(meta(1, true), &payload(500, 1), 1200);
        let f2 = frags(meta(2, false), &payload(3000, 2), 1200);
        let f3 = frags(meta(3, false), &payload(500, 3), 1200);
        r.push(f1[0].clone()).expect("push").expect("frame 1");
        // Frame 2 loses its last fragment.
        r.push(f2[0].clone()).expect("push");
        r.push(f2[1].clone()).expect("push");
        let f = r.push(f3[0].clone()).expect("push").expect("frame 3");
        assert_eq!(f.meta.frame_id, 3);
        assert_eq!(f.lost_before, 1);
        // The late fragment of frame 2 is now stale.
        assert!(r.push(f2[2].clone()).expect("push").is_none());
        let s = r.stats();
        assert_eq!(s.frames_completed, 2);
        assert_eq!(s.frames_incomplete, 1);
        assert_eq!(s.stale_datagrams, 1);
    }

    #[test]
    fn eviction_keeps_memory_bounded() {
        let mut r = Reassembler::new(2);
        for id in 1..=10 {
            let parts = frags(meta(id, false), &payload(2000, 0), 1200);
            assert!(r.push(parts[0].clone()).expect("push").is_none());
        }
        assert_eq!(r.slots.len(), 2);
        assert_eq!(r.stats().frames_incomplete, 8);
        // An old frame cannot evict newer ones.
        let old = frags(meta(3, false), &payload(2000, 0), 1200);
        assert!(r.push(old[0].clone()).expect("push").is_none());
    }

    #[test]
    fn malformed_datagrams_are_rejected() {
        let mut r = Reassembler::default();
        assert!(r.push(Bytes::from_static(&[1, 2, 3])).is_err());
        let mut bad = frags(meta(1, false), &payload(10, 0), 1200)[0].to_vec();
        bad[7] = 0; // count = 0
        bad[8] = 0;
        assert!(r.push(Bytes::from(bad)).is_err());
        let mut bad = frags(meta(1, false), &payload(10, 0), 1200)[0].to_vec();
        bad[0] = 0x7f;
        assert!(r.push(Bytes::from(bad)).is_err());
        assert_eq!(r.stats().malformed_datagrams, 3);
        // Still usable afterwards.
        let ok = frags(meta(2, true), &payload(10, 0), 1200);
        assert!(r.push(ok[0].clone()).expect("push").is_some());
    }

    #[test]
    fn fragment_rejects_bad_input() {
        assert!(fragment(meta(1, false), &[], 1200, |_| {}).is_err());
        assert!(fragment(meta(1, false), &[1], HEADER_LEN, |_| {}).is_err());
    }
}
