//! The read side: backfill through the cursor, tail the relay, splice by offset.
//!
//! This is the `ChunkSource` seam from the plan, in Rust. `RelaySource` consumes
//! ephemeral frames; `CursorFollowSource` retrieves durable chunks as the cursor
//! advances; [`LiveViewer`] joins them and the caller cannot tell which path
//! supplied a byte.
//!
//! # Why splicing is by absolute offset
//!
//! The obvious implementation deduplicates by sequence number and appends in
//! arrival order. It is wrong, because the two streams are not two views of the
//! same units: a frame is an arbitrary byte range and a chunk is a sequence, and
//! they overlap partially. A frame can deliver the first half of a chunk that
//! arrives whole moments later.
//!
//! So [`LiveViewer`] tracks exactly one number — `received_through`, the byte
//! offset it has contiguously accepted — and every message is spliced relative to
//! it. That makes the three cases fall out instead of needing to be special-cased:
//!
//! - Entirely behind `received_through`: already have it, drop it.
//! - Straddling it: take only the tail that is new.
//! - Starting beyond it: a **gap**. Rejected, because accepting it would produce a
//!   transcript that is quietly wrong at every later offset.

use std::io::Write;

use ansible_capture::Chunk;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;

use crate::{Error, Result};

/// A relay frame as it arrives on the wire.
#[derive(Debug, Clone, Deserialize)]
pub struct RelayFrame {
    pub byte_start: u64,
    pub byte_end: u64,
    /// Wall clock at the PTY. The viewer's own clock minus this is the perceived
    /// latency the spike exists to measure.
    pub at_ms: u64,
    pub b64: String,
}

impl RelayFrame {
    /// # Errors
    /// Errors if the payload is not valid base64, or if its length contradicts the
    /// declared byte range — the same self-description check the chunk envelope gets.
    pub fn bytes(&self) -> Result<Vec<u8>> {
        let bytes = STANDARD
            .decode(self.b64.as_bytes())
            .map_err(|e| Error::Protocol(format!("frame payload is not base64: {e}")))?;
        let declared = self.byte_end.saturating_sub(self.byte_start);
        if declared != bytes.len() as u64 {
            return Err(Error::Protocol(format!(
                "frame declares {declared} bytes but carries {}",
                bytes.len()
            )));
        }
        Ok(bytes)
    }
}

/// What happened to a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// Its bytes advanced the stream. Carries how many were new.
    Advanced(usize),
    /// Entirely bytes already held — a legitimate relay/archive overlap.
    Duplicate,
}

/// Reconstructs one session's redacted byte stream from frames and chunks.
pub struct LiveViewer {
    /// Bytes contiguously accepted. The single piece of state that matters.
    received_through: u64,
    out: Box<dyn Write + Send>,
    frames_applied: u64,
    chunks_applied: u64,
    duplicates: u64,
    bytes_written: u64,
}

impl LiveViewer {
    #[must_use]
    pub fn new(out: Box<dyn Write + Send>) -> Self {
        Self {
            received_through: 0,
            out,
            frames_applied: 0,
            chunks_applied: 0,
            duplicates: 0,
            bytes_written: 0,
        }
    }

    /// Splice a byte range at `received_through`, or refuse if it would leave a gap.
    ///
    /// The one place ordering is decided, deliberately shared by both the frame and
    /// chunk paths so they cannot drift apart.
    fn apply_range(&mut self, byte_start: u64, bytes: &[u8]) -> Result<Applied> {
        let byte_end = byte_start + bytes.len() as u64;

        if byte_end <= self.received_through {
            self.duplicates += 1;
            return Ok(Applied::Duplicate);
        }

        if byte_start > self.received_through {
            return Err(Error::Protocol(format!(
                "gap: have bytes through {}, but the next message starts at {byte_start}",
                self.received_through
            )));
        }

        // Straddles the boundary: keep only what is new. This is the case that makes
        // relay-plus-backfill work without coordination between the two paths.
        let skip = usize::try_from(self.received_through - byte_start).map_err(|_| {
            Error::Protocol("message overlap exceeds addressable memory".to_string())
        })?;
        let fresh = &bytes[skip..];
        self.out.write_all(fresh)?;
        self.received_through = byte_end;
        self.bytes_written += fresh.len() as u64;
        Ok(Applied::Advanced(fresh.len()))
    }

    /// Apply a live relay frame.
    ///
    /// # Errors
    /// Errors if the frame is malformed, or would leave a gap.
    pub fn apply_frame(&mut self, frame: &RelayFrame) -> Result<Applied> {
        let bytes = frame.bytes()?;
        let applied = self.apply_range(frame.byte_start, &bytes)?;
        if matches!(applied, Applied::Advanced(_)) {
            self.frames_applied += 1;
        }
        Ok(applied)
    }

    /// Apply a durable chunk fetched from the archive.
    ///
    /// # Errors
    /// Errors if the chunk fails self-validation, or would leave a gap.
    pub fn apply_chunk(&mut self, chunk: &Chunk) -> Result<Applied> {
        chunk.validate()?;
        let applied = self.apply_range(chunk.byte_start, &chunk.payload())?;
        if matches!(applied, Applied::Advanced(_)) {
            self.chunks_applied += 1;
        }
        Ok(applied)
    }

    /// # Errors
    /// Errors if the sink cannot be flushed.
    pub fn flush(&mut self) -> Result<()> {
        self.out.flush()?;
        Ok(())
    }

    #[must_use]
    pub fn received_through(&self) -> u64 {
        self.received_through
    }

    #[must_use]
    pub fn frames_applied(&self) -> u64 {
        self.frames_applied
    }

    #[must_use]
    pub fn chunks_applied(&self) -> u64 {
        self.chunks_applied
    }

    /// Overlapping messages dropped. Expected to be non-zero in any real session:
    /// it means relay and backfill genuinely overlapped and the join worked.
    #[must_use]
    pub fn duplicates(&self) -> u64 {
        self.duplicates
    }

    #[must_use]
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ansible_capture::Record;

    fn chunk(seq: u64, start: u64, payload: &[u8]) -> Chunk {
        Chunk {
            session_id: "s-1".into(),
            seq,
            byte_start: start,
            byte_end: start + payload.len() as u64,
            started_at_ms: 1_700_000_000_000,
            ended_at_ms: 1_700_000_000_010,
            redaction_version: 2,
            records: vec![Record { at_delta_ms: 0, bytes: payload.to_vec() }],
        }
    }

    fn frame(start: u64, payload: &[u8]) -> RelayFrame {
        RelayFrame {
            byte_start: start,
            byte_end: start + payload.len() as u64,
            at_ms: 1_700_000_000_000,
            b64: STANDARD.encode(payload),
        }
    }

    /// A sink that hands the bytes back, so tests assert on content not just counts.
    #[derive(Clone, Default)]
    struct Shared(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn viewer() -> (LiveViewer, Shared) {
        let sink = Shared::default();
        (LiveViewer::new(Box::new(sink.clone())), sink)
    }

    #[test]
    fn frames_in_order_reconstruct_the_stream() {
        let (mut v, sink) = viewer();
        v.apply_frame(&frame(0, b"hello ")).unwrap();
        v.apply_frame(&frame(6, b"world")).unwrap();
        assert_eq!(&*sink.0.lock().unwrap(), b"hello world");
        assert_eq!(v.received_through(), 11);
    }

    #[test]
    fn chunks_and_frames_interleave_to_the_same_stream() {
        let (mut v, sink) = viewer();
        v.apply_chunk(&chunk(0, 0, b"abc")).unwrap();
        v.apply_frame(&frame(3, b"def")).unwrap();
        v.apply_chunk(&chunk(1, 6, b"ghi")).unwrap();
        assert_eq!(&*sink.0.lock().unwrap(), b"abcdefghi");
    }

    #[test]
    fn a_fully_duplicate_message_is_dropped() {
        let (mut v, sink) = viewer();
        v.apply_frame(&frame(0, b"abcdef")).unwrap();
        // The archive redelivers the same range — the normal relay/backfill overlap.
        assert_eq!(v.apply_chunk(&chunk(0, 0, b"abcdef")).unwrap(), Applied::Duplicate);
        assert_eq!(&*sink.0.lock().unwrap(), b"abcdef");
        assert_eq!(v.duplicates(), 1);
    }

    #[test]
    fn a_straddling_message_contributes_only_its_new_tail() {
        let (mut v, sink) = viewer();
        v.apply_frame(&frame(0, b"abc")).unwrap();
        // The chunk covers 0..6; only 3..6 is new. This is the case that makes
        // "backfill through the cursor, then tail the relay" work at all.
        assert_eq!(v.apply_chunk(&chunk(0, 0, b"abcdef")).unwrap(), Applied::Advanced(3));
        assert_eq!(&*sink.0.lock().unwrap(), b"abcdef");
        assert_eq!(v.received_through(), 6);
    }

    #[test]
    fn a_gap_is_refused_rather_than_spliced_over() {
        let (mut v, sink) = viewer();
        v.apply_frame(&frame(0, b"abc")).unwrap();
        // Byte 3 is missing. Accepting this would silently corrupt every later
        // offset, including every mention anchor past it.
        let err = v.apply_frame(&frame(10, b"xyz")).unwrap_err();
        assert!(matches!(err, Error::Protocol(_)), "{err:?}");
        assert_eq!(&*sink.0.lock().unwrap(), b"abc");
        assert_eq!(v.received_through(), 3, "a refused message must not advance the stream");
    }

    #[test]
    fn backfill_then_live_tail_reconstructs_exactly_once() {
        // The real join: archive up to the cursor, then relay frames that overlap it.
        let (mut v, sink) = viewer();
        v.apply_chunk(&chunk(0, 0, b"0123456789")).unwrap();
        v.apply_frame(&frame(5, b"56789abcde")).unwrap();
        v.apply_frame(&frame(15, b"fghij")).unwrap();
        assert_eq!(&*sink.0.lock().unwrap(), b"0123456789abcdefghij");
    }

    #[test]
    fn a_frame_whose_payload_contradicts_its_range_is_refused() {
        let (mut v, _sink) = viewer();
        let mut bad = frame(0, b"abc");
        bad.byte_end = 99;
        assert!(v.apply_frame(&bad).is_err());
    }

    #[test]
    fn binary_payloads_survive_the_join() {
        let (mut v, sink) = viewer();
        // Not valid UTF-8; the reason payloads are base64 rather than JSON strings.
        let a: Vec<u8> = (0u8..=255).collect();
        let b: Vec<u8> = (0u8..=255).rev().collect();
        v.apply_frame(&frame(0, &a)).unwrap();
        v.apply_chunk(&chunk(0, 256, &b)).unwrap();
        let got = sink.0.lock().unwrap().clone();
        assert_eq!(got, [a, b].concat());
    }
}
