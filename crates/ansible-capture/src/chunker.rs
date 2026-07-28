//! `(bytes, config) -> ordered chunks`, and the reassembly that must invert it.

use crate::chunk::{Chunk, Record};
use crate::redact::{Redactor, Ruleset};
use crate::{Error, Result};

/// Chunking parameters.
///
/// The defaults come from the architecture plan: flush at ~64 KiB or ~1s,
/// whichever comes first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkerConfig {
    pub max_bytes: usize,
    pub max_age_ms: u64,
}

impl Default for ChunkerConfig {
    fn default() -> Self {
        Self { max_bytes: 64 * 1024, max_age_ms: 1000 }
    }
}

/// Turns a redacted byte stream into ordered, self-describing chunks.
///
/// The caller supplies the clock. Time is never read internally, which is what
/// makes the whole capture path a pure function of `(bytes, timestamps, config)`
/// and therefore golden-testable and fuzzable.
pub struct Chunker {
    session_id: String,
    config: ChunkerConfig,
    redactor: Redactor,

    next_seq: u64,
    /// Offset in the redacted stream where the open chunk begins.
    byte_cursor: u64,
    open: Vec<Record>,
    open_bytes: usize,
    opened_at_ms: Option<u64>,
    last_record_ms: u64,
}

impl Chunker {
    #[must_use]
    pub fn new(session_id: impl Into<String>, config: ChunkerConfig, ruleset: Ruleset) -> Self {
        Self {
            session_id: session_id.into(),
            config,
            redactor: Redactor::new(ruleset),
            next_seq: 0,
            byte_cursor: 0,
            open: Vec::new(),
            open_bytes: 0,
            opened_at_ms: None,
            last_record_ms: 0,
        }
    }

    /// Feed raw PTY bytes observed at `now_ms`. Returns any chunks that closed.
    ///
    /// One push can close several chunks when a large write arrives, and can
    /// close none when redaction is still holding bytes back.
    pub fn push(&mut self, raw: &[u8], now_ms: u64) -> Vec<Chunk> {
        let safe = self.redactor.push(raw);
        self.append(&safe, now_ms)
    }

    /// Close the open chunk if it has aged past `max_age_ms`.
    ///
    /// Called from a timer. Without it a session that stops producing output
    /// would leave its tail unflushed and the cursor stalled.
    pub fn tick(&mut self, now_ms: u64) -> Option<Chunk> {
        let opened_at = self.opened_at_ms?;
        if now_ms.saturating_sub(opened_at) >= self.config.max_age_ms {
            self.close(now_ms)
        } else {
            None
        }
    }

    /// Release the redactor's tail and close the final chunk.
    ///
    /// Must be called at end of session, or the held-back bytes are lost and
    /// reassembly will not be byte-exact.
    pub fn finish(&mut self, now_ms: u64) -> Vec<Chunk> {
        let tail = self.redactor.finish();
        let mut chunks = self.append(&tail, now_ms);
        chunks.extend(self.close(now_ms));
        chunks
    }

    /// Offset one past the last byte handed to a chunk.
    #[must_use]
    pub fn byte_cursor(&self) -> u64 {
        self.byte_cursor + u64::try_from(self.open_bytes).unwrap_or(u64::MAX)
    }

    #[must_use]
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    #[must_use]
    pub fn redactions(&self) -> u64 {
        self.redactor.redactions()
    }

    fn append(&mut self, bytes: &[u8], now_ms: u64) -> Vec<Chunk> {
        let mut closed = Vec::new();
        if bytes.is_empty() {
            return closed;
        }

        let mut rest = bytes;
        while !rest.is_empty() {
            if self.opened_at_ms.is_none() {
                self.opened_at_ms = Some(now_ms);
            }

            let room = self.config.max_bytes.saturating_sub(self.open_bytes);
            // A single write larger than max_bytes is split across chunks rather
            // than allowed to blow past the limit. Splitting mid-escape-sequence
            // is fine: reassembly is exact and order is preserved, and the
            // viewer concatenates before feeding a terminal.
            let take = room.min(rest.len());
            let (head, tail) = rest.split_at(take);

            let delta = now_ms.saturating_sub(self.opened_at_ms.unwrap_or(now_ms));
            self.open.push(Record {
                at_delta_ms: u32::try_from(delta).unwrap_or(u32::MAX),
                bytes: head.to_vec(),
            });
            self.open_bytes += head.len();
            self.last_record_ms = now_ms;
            rest = tail;

            if self.open_bytes >= self.config.max_bytes {
                closed.extend(self.close(now_ms));
            }
        }
        closed
    }

    fn close(&mut self, now_ms: u64) -> Option<Chunk> {
        if self.open.is_empty() {
            self.opened_at_ms = None;
            return None;
        }

        let started_at_ms = self.opened_at_ms.unwrap_or(now_ms);
        let chunk = Chunk {
            session_id: self.session_id.clone(),
            seq: self.next_seq,
            byte_start: self.byte_cursor,
            byte_end: self.byte_cursor + u64::try_from(self.open_bytes).unwrap_or(u64::MAX),
            started_at_ms,
            ended_at_ms: self.last_record_ms.max(started_at_ms),
            redaction_version: self.redactor.ruleset_version(),
            records: std::mem::take(&mut self.open),
        };

        self.next_seq += 1;
        self.byte_cursor = chunk.byte_end;
        self.open_bytes = 0;
        self.opened_at_ms = None;
        Some(chunk)
    }
}

/// Rebuilds a stream from chunks, refusing anything that would hide a gap.
///
/// Order is the one invariant in this system, so this is strict on purpose: a
/// missing, duplicated, reordered, or non-contiguous chunk is an error rather
/// than a best-effort splice.
#[derive(Debug, Default)]
pub struct Reassembler {
    expected_seq: u64,
    expected_offset: u64,
    output: Vec<u8>,
}

impl Reassembler {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Accept the next chunk in sequence.
    ///
    /// # Errors
    /// [`Error::Protocol`] when the chunk is out of sequence, non-contiguous,
    /// or fails its own [`Chunk::validate`].
    pub fn accept(&mut self, chunk: &Chunk) -> Result<()> {
        chunk.validate()?;

        if chunk.seq != self.expected_seq {
            return Err(Error::Protocol(format!(
                "expected chunk {} but got {}",
                self.expected_seq, chunk.seq
            )));
        }
        if chunk.byte_start != self.expected_offset {
            return Err(Error::Protocol(format!(
                "chunk {} starts at {} but the stream is at {}",
                chunk.seq, chunk.byte_start, self.expected_offset
            )));
        }

        self.output.extend_from_slice(&chunk.payload());
        self.expected_seq += 1;
        self.expected_offset = chunk.byte_end;
        Ok(())
    }

    /// Accept chunks that may arrive duplicated or out of order.
    ///
    /// This is the relay path: frames can overlap with the durable backfill, so
    /// an already-seen chunk is skipped rather than treated as an error. A chunk
    /// from the future is still an error, because accepting it would leave a gap.
    ///
    /// # Errors
    /// [`Error::Protocol`] when the chunk would leave a gap.
    pub fn accept_deduplicated(&mut self, chunk: &Chunk) -> Result<bool> {
        if chunk.seq < self.expected_seq {
            return Ok(false);
        }
        self.accept(chunk)?;
        Ok(true)
    }

    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.output
    }

    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.output
    }

    #[must_use]
    pub fn chunks_accepted(&self) -> u64 {
        self.expected_seq
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunker() -> Chunker {
        Chunker::new("s-1", ChunkerConfig { max_bytes: 16, max_age_ms: 1000 }, Ruleset::default())
    }

    fn reassemble(chunks: &[Chunk]) -> Vec<u8> {
        let mut r = Reassembler::new();
        for c in chunks {
            r.accept(c).expect("accept");
        }
        r.into_bytes()
    }

    #[test]
    fn small_writes_stay_in_one_chunk_until_flushed() {
        let mut c = chunker();
        assert!(c.push(b"abc ", 0).is_empty());
        assert!(c.push(b"def ", 10).is_empty());
        let chunks = c.finish(20);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].payload(), b"abc def ");
        assert_eq!(chunks[0].seq, 0);
    }

    #[test]
    fn flushes_when_the_size_threshold_is_reached() {
        let mut c = chunker();
        // 16-byte limit, and the write ends on a word boundary so redaction
        // releases all of it. 20 bytes must close at least one chunk.
        let chunks = c.push(b"aaaaaaaaaaaaaaaaaaa\n", 0);
        assert!(!chunks.is_empty(), "expected a size-triggered flush");
        assert_eq!(chunks[0].len(), 16);
    }

    #[test]
    fn flushes_when_the_age_threshold_is_reached() {
        let mut c = chunker();
        c.push(b"abc\n", 0);
        assert!(c.tick(500).is_none(), "should not flush before max_age");
        let chunk = c.tick(1000).expect("age-triggered flush");
        assert_eq!(chunk.payload(), b"abc\n");
    }

    #[test]
    fn tick_on_an_idle_chunker_produces_nothing() {
        let mut c = chunker();
        assert!(c.tick(10_000).is_none());
    }

    #[test]
    fn sequence_numbers_are_contiguous_from_zero() {
        let mut c = chunker();
        let mut all = Vec::new();
        for i in 0..10 {
            all.extend(c.push(b"0123456789", i));
        }
        all.extend(c.finish(100));
        for (i, chunk) in all.iter().enumerate() {
            assert_eq!(chunk.seq, i as u64, "seq must be dense and ordered");
        }
    }

    #[test]
    fn byte_ranges_are_contiguous_and_non_overlapping() {
        let mut c = chunker();
        let mut all = Vec::new();
        for i in 0..10 {
            all.extend(c.push(b"0123456789", i));
        }
        all.extend(c.finish(100));

        let mut offset = 0u64;
        for chunk in &all {
            assert_eq!(chunk.byte_start, offset, "gap or overlap at seq {}", chunk.seq);
            offset = chunk.byte_end;
        }
        assert_eq!(offset, c.byte_cursor());
    }

    #[test]
    fn a_write_larger_than_max_bytes_is_split_not_oversized() {
        let mut c = chunker();
        let big = vec![b'x'; 100];
        let mut all = c.push(&big, 0);
        all.extend(c.finish(1));
        assert!(all.len() >= 6, "100 bytes at 16 per chunk should span many chunks");
        for chunk in &all {
            assert!(chunk.len() <= 16, "chunk {} exceeded max_bytes", chunk.seq);
        }
        assert_eq!(reassemble(&all), big);
    }

    #[test]
    fn round_trip_is_byte_exact_for_plain_output() {
        let mut c = chunker();
        let input: &[u8] = b"line one\nline two\nline three\n";
        let mut all = c.push(input, 0);
        all.extend(c.finish(1));
        assert_eq!(reassemble(&all), input);
    }

    #[test]
    fn round_trip_is_byte_exact_for_binary_output() {
        let mut c = chunker();
        let input: Vec<u8> = (0u8..=255).cycle().take(1000).collect();
        let mut all = c.push(&input, 0);
        all.extend(c.finish(1));
        assert_eq!(reassemble(&all), input);
    }

    #[test]
    fn reassembly_rejects_a_missing_chunk() {
        let mut c = chunker();
        let mut all = c.push(&[b'x'; 100], 0);
        all.extend(c.finish(1));

        let mut r = Reassembler::new();
        r.accept(&all[0]).unwrap();
        // Skipping one must be an error, never a silent splice.
        assert!(r.accept(&all[2]).is_err());
    }

    #[test]
    fn reassembly_rejects_a_duplicate_chunk() {
        let mut c = chunker();
        let mut all = c.push(&[b'x'; 100], 0);
        all.extend(c.finish(1));

        let mut r = Reassembler::new();
        r.accept(&all[0]).unwrap();
        assert!(r.accept(&all[0]).is_err());
    }

    #[test]
    fn reassembly_rejects_reordering() {
        let mut c = chunker();
        let mut all = c.push(&[b'x'; 100], 0);
        all.extend(c.finish(1));

        let mut r = Reassembler::new();
        assert!(r.accept(&all[1]).is_err());
    }

    /// The relay path: overlap with the durable backfill is expected.
    #[test]
    fn deduplicating_reassembly_skips_chunks_already_seen() {
        let mut c = chunker();
        let mut all = c.push(&[b'y'; 64], 0);
        all.extend(c.finish(1));

        let mut r = Reassembler::new();
        for chunk in &all {
            assert!(r.accept_deduplicated(chunk).unwrap(), "first pass accepts");
        }
        for chunk in &all {
            assert!(!r.accept_deduplicated(chunk).unwrap(), "replay is skipped");
        }
        assert_eq!(r.bytes().len(), 64);
    }

    #[test]
    fn deduplicating_reassembly_still_refuses_a_gap() {
        let mut c = chunker();
        let mut all = c.push(&[b'z'; 100], 0);
        all.extend(c.finish(1));

        let mut r = Reassembler::new();
        r.accept_deduplicated(&all[0]).unwrap();
        assert!(r.accept_deduplicated(&all[3]).is_err(), "a gap is never acceptable");
    }

    #[test]
    fn secrets_are_redacted_before_chunking() {
        let mut c = chunker();
        let mut all = c.push(b"tok=ghp_ABCDEFGHIJKLMNOPQRSTUV done", 0);
        all.extend(c.finish(1));
        let out = String::from_utf8(reassemble(&all)).unwrap();
        assert!(!out.contains("ghp_ABCDEFGHIJKLMNOPQRSTUV"), "secret reached a chunk: {out}");
        assert!(out.contains("[redacted:github-pat]"));
        assert_eq!(c.redactions(), 1);
    }

    #[test]
    fn byte_offsets_index_the_redacted_stream() {
        let mut c = chunker();
        let mut all = c.push(b"k=ghp_ABCDEFGHIJKLMNOPQRSTUV", 0);
        all.extend(c.finish(1));
        let total: u64 = all.last().unwrap().byte_end;
        assert_eq!(
            usize::try_from(total).unwrap(),
            reassemble(&all).len(),
            "offsets must match stored bytes"
        );
    }

    #[test]
    fn records_carry_relative_timing_for_replay() {
        let mut c = chunker();
        // Writes end on word boundaries so each is released as its own record;
        // a write ending mid-identifier would be held and merged with the next.
        c.push(b"a ", 1000);
        c.push(b"b ", 1250);
        let chunks = c.finish(1500);
        let records = &chunks[0].records;
        assert_eq!(records[0].at_delta_ms, 0);
        assert_eq!(records[1].at_delta_ms, 250);
        assert_eq!(chunks[0].started_at_ms, 1000);
        assert_eq!(chunks[0].ended_at_ms, 1250);
    }

    #[test]
    fn finishing_twice_is_harmless() {
        let mut c = chunker();
        c.push(b"abc\n", 0);
        assert_eq!(c.finish(1).len(), 1);
        assert!(c.finish(2).is_empty());
    }

    #[test]
    fn an_empty_session_produces_no_chunks() {
        let mut c = chunker();
        assert!(c.finish(0).is_empty());
    }
}
