//! The chunk envelope: the unit written to R2 and followed by the cursor.

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// One contiguous run of output with the time it arrived.
///
/// Records exist so replay is time-accurate rather than dumped all at once. The
/// bytes are base64 because PTY output is arbitrary binary — it is frequently
/// not valid UTF-8, so it cannot be a JSON string directly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// Milliseconds after the chunk's `started_at`.
    pub at_delta_ms: u32,
    #[serde(with = "base64_bytes")]
    pub bytes: Vec<u8>,
}

/// A chunk of a session transcript.
///
/// Byte offsets index the **redacted** stream, not the raw PTY stream. That is
/// the stream that gets stored, so it is the only one a viewer or a mention
/// anchor can address. Redaction changes length, so raw offsets would not
/// survive a round trip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chunk {
    pub session_id: String,
    /// Strictly increasing from 0, with no gaps. This is what the Worker checks
    /// before writing and what `advance_transcript_cursor` publishes.
    pub seq: u64,
    pub byte_start: u64,
    /// Exclusive.
    pub byte_end: u64,
    /// Wall clock of the first record, ms since the Unix epoch.
    pub started_at_ms: u64,
    /// Wall clock of the last record.
    pub ended_at_ms: u64,
    /// Which redaction ruleset produced these bytes, so a later scrub pass can
    /// tell what was already scanned.
    pub redaction_version: u32,
    pub records: Vec<Record>,
}

impl Chunk {
    /// Total payload bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.iter().map(|r| r.bytes.len()).sum()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.iter().all(|r| r.bytes.is_empty())
    }

    /// Payload with record framing removed.
    #[must_use]
    pub fn payload(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.len());
        for record in &self.records {
            out.extend_from_slice(&record.bytes);
        }
        out
    }

    /// Check the envelope describes itself consistently.
    ///
    /// Cheap enough to run on every chunk. A chunk whose declared range does not
    /// match its payload would silently corrupt every downstream offset.
    ///
    /// # Errors
    /// [`Error::Protocol`] when the declared byte range, payload length, or
    /// timestamps contradict each other.
    pub fn validate(&self) -> Result<()> {
        if self.byte_end < self.byte_start {
            return Err(Error::Protocol(format!(
                "chunk {} has byte_end {} before byte_start {}",
                self.seq, self.byte_end, self.byte_start
            )));
        }
        let declared = usize::try_from(self.byte_end - self.byte_start).map_err(|_| {
            Error::Protocol(format!("chunk {} range exceeds addressable memory", self.seq))
        })?;
        if declared != self.len() {
            return Err(Error::Protocol(format!(
                "chunk {} declares {declared} bytes but carries {}",
                self.seq,
                self.len()
            )));
        }
        if self.ended_at_ms < self.started_at_ms {
            return Err(Error::Protocol(format!("chunk {} ends before it starts", self.seq)));
        }
        Ok(())
    }

    /// Serialize to the stored form: one JSON object per line.
    ///
    /// JSONL rather than one JSON document so a truncated upload loses only its
    /// last line, and so the Worker can append without reparsing.
    ///
    /// # Errors
    /// [`Error::Serde`] if a record cannot be encoded.
    pub fn to_jsonl(&self) -> Result<String> {
        let header = Header {
            session_id: &self.session_id,
            seq: self.seq,
            byte_start: self.byte_start,
            byte_end: self.byte_end,
            started_at_ms: self.started_at_ms,
            ended_at_ms: self.ended_at_ms,
            redaction_version: self.redaction_version,
            record_count: self.records.len(),
        };
        let mut out = serde_json::to_string(&header).map_err(Error::from)?;
        out.push('\n');
        for record in &self.records {
            out.push_str(&serde_json::to_string(record).map_err(Error::from)?);
            out.push('\n');
        }
        Ok(out)
    }

    /// Parse the stored form.
    ///
    /// # Errors
    /// [`Error::Serde`] on malformed JSON, or [`Error::Protocol`] when the
    /// header and record list disagree — which is how a truncated upload is
    /// caught rather than silently accepted.
    pub fn from_jsonl(text: &str) -> Result<Self> {
        let mut lines = text.lines();
        let header_line = lines.next().ok_or_else(|| Error::Protocol("chunk is empty".into()))?;
        let header: OwnedHeader = serde_json::from_str(header_line)?;

        let records: Vec<Record> = lines
            .filter(|l| !l.trim().is_empty())
            .map(serde_json::from_str)
            .collect::<std::result::Result<_, _>>()?;

        if records.len() != header.record_count {
            return Err(Error::Protocol(format!(
                "chunk {} declares {} records but carries {}",
                header.seq,
                header.record_count,
                records.len()
            )));
        }

        let chunk = Self {
            session_id: header.session_id,
            seq: header.seq,
            byte_start: header.byte_start,
            byte_end: header.byte_end,
            started_at_ms: header.started_at_ms,
            ended_at_ms: header.ended_at_ms,
            redaction_version: header.redaction_version,
            records,
        };
        chunk.validate()?;
        Ok(chunk)
    }

    /// Object key under the session's prefix.
    #[must_use]
    pub fn object_key(&self) -> String {
        format!("transcripts/{}/{}.jsonl", self.session_id, self.seq)
    }
}

#[derive(Serialize)]
struct Header<'a> {
    session_id: &'a str,
    seq: u64,
    byte_start: u64,
    byte_end: u64,
    started_at_ms: u64,
    ended_at_ms: u64,
    redaction_version: u32,
    record_count: usize,
}

#[derive(Deserialize)]
struct OwnedHeader {
    session_id: String,
    seq: u64,
    byte_start: u64,
    byte_end: u64,
    started_at_ms: u64,
    ended_at_ms: u64,
    redaction_version: u32,
    record_count: usize,
}

/// Base64 for record payloads. Terminal output is binary, so a JSON string
/// would corrupt it and `Vec<u8>` as a JSON array would triple the size.
mod base64_bytes {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        STANDARD.decode(text.as_bytes()).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(records: Vec<Record>) -> Chunk {
        let len: usize = records.iter().map(|r| r.bytes.len()).sum();
        Chunk {
            session_id: "s-1".into(),
            seq: 7,
            byte_start: 100,
            byte_end: 100 + len as u64,
            started_at_ms: 1_700_000_000_000,
            ended_at_ms: 1_700_000_000_500,
            redaction_version: 1,
            records,
        }
    }

    fn record(at: u32, bytes: &[u8]) -> Record {
        Record { at_delta_ms: at, bytes: bytes.to_vec() }
    }

    #[test]
    fn payload_concatenates_records_in_order() {
        let c = chunk(vec![record(0, b"abc"), record(10, b"def")]);
        assert_eq!(c.payload(), b"abcdef");
        assert_eq!(c.len(), 6);
    }

    #[test]
    fn jsonl_round_trip_preserves_everything() {
        let c = chunk(vec![record(0, b"hello"), record(42, b"\x1b[31m!\x00\xff")]);
        let text = c.to_jsonl().unwrap();
        let back = Chunk::from_jsonl(&text).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn jsonl_survives_non_utf8_payloads() {
        // The reason records are base64: this is not valid UTF-8.
        let c = chunk(vec![record(0, &[0xff, 0xfe, 0x00, 0x80])]);
        let back = Chunk::from_jsonl(&c.to_jsonl().unwrap()).unwrap();
        assert_eq!(back.payload(), vec![0xff, 0xfe, 0x00, 0x80]);
    }

    #[test]
    fn jsonl_is_one_line_per_record_plus_a_header() {
        let c = chunk(vec![record(0, b"a"), record(1, b"b"), record(2, b"c")]);
        assert_eq!(c.to_jsonl().unwrap().lines().count(), 4);
    }

    #[test]
    fn validate_rejects_a_declared_range_that_does_not_match_the_payload() {
        let mut c = chunk(vec![record(0, b"abc")]);
        c.byte_end += 1;
        assert!(c.validate().is_err());
    }

    #[test]
    fn validate_rejects_an_inverted_range() {
        let mut c = chunk(vec![record(0, b"abc")]);
        c.byte_start = 999;
        assert!(c.validate().is_err());
    }

    #[test]
    fn validate_rejects_time_running_backwards() {
        let mut c = chunk(vec![record(0, b"abc")]);
        c.ended_at_ms = c.started_at_ms - 1;
        assert!(c.validate().is_err());
    }

    #[test]
    fn parsing_rejects_a_truncated_record_list() {
        let c = chunk(vec![record(0, b"a"), record(1, b"b")]);
        let text = c.to_jsonl().unwrap();
        let truncated = text.lines().take(2).fold(String::new(), |mut acc, line| {
            acc.push_str(line);
            acc.push('\n');
            acc
        });
        assert!(Chunk::from_jsonl(&truncated).is_err());
    }

    #[test]
    fn object_key_is_stable_and_sortable() {
        assert_eq!(chunk(vec![]).object_key(), "transcripts/s-1/7.jsonl");
    }
}
