//! Teleport: watching a teammate's session, live.
//!
//! # Where the bytes come from
//!
//! Herdr's CLI has `terminal session observe <target>`, which streams
//! newline-delimited JSON `terminal.frame` records carrying base64-encoded ANSI
//! bytes, read-only, with no takeover of input, scroll, or focus, and any number
//! of observers at once. That is exactly the shape Spike B built a pipeline for,
//! and it arrives without wrapping the agent's process, without a PTY of our own,
//! and without the terminal-embedding work of Spike A.
//!
//! So the publisher is a short pipeline of parts that already exist:
//!
//! ```text
//! herdr terminal session observe → base64 decode → Redactor → Chunker → hub
//! ```
//!
//! and the viewer is its inverse: chunks in sequence, payload to stdout, in a
//! Herdr pane. `ansible-capture` guarantees the middle — redaction before a byte
//! can reach a chunk, dense sequence numbers, contiguous byte ranges, and a refusal
//! to splice over a gap — and its golden round-trip test is what makes the claim
//! "byte-exact" rather than "looks right".
//!
//! # What is deliberately not here
//!
//! Writing. `terminal session control` exists and would let a watcher type into
//! someone else's agent, which is a different feature with a different consent
//! question. Support arrives as a [`crate::model::Message`] instead: it lands in
//! the owner's inbox, and the owner decides whether it reaches the agent. See
//! `docs/plan/herdr-plugin.md` for why that boundary is where it is.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, TryRecvError, channel};

use ansible_capture::{Chunk, Chunker, ChunkerConfig, Ruleset};
use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::Value;

use crate::hub::Hub;

/// Chunk flush parameters for a live stream.
///
/// Much smaller and much sooner than the transcript defaults (64 KiB / 1 s):
/// a watcher's latency floor is the flush interval, so this trades chunk overhead
/// for responsiveness. `ansible-capture`'s guarantees do not depend on the size.
const LIVE_CHUNKS: ChunkerConfig = ChunkerConfig { max_bytes: 8 * 1024, max_age_ms: 200 };

// The flush interval *is* the watcher's latency floor, so it is the number that
// decides whether teleport feels live. Asserted at compile time so a future
// tidy-up towards the transcript defaults fails the build rather than quietly
// making everyone's view lag.
const _: () = assert!(LIVE_CHUNKS.max_age_ms <= 250);
const _: () = assert!(LIVE_CHUNKS.max_bytes <= 16 * 1024);

/// How many chunks of history a live session keeps.
///
/// Enough for a watcher who joins mid-sentence to see recent context, not enough
/// to accumulate a transcript in a hub that is not a transcript store. Durable
/// history is the R2 path, not this one.
const LIVE_BACKLOG: u64 = 64;

/// Resolve the Herdr binary the way the plugin guide prescribes.
#[must_use]
pub fn herdr_bin() -> String {
    std::env::var("HERDR_BIN_PATH").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "herdr".into())
}

/// Publishes one pane's live output into the hub.
///
/// Owns a child `herdr` process and a reader thread. Dropping it kills the child,
/// so a pane whose share mode drops back to `title` stops being observed —
/// stopping the publisher is what makes revocation real rather than advisory.
pub struct LivePublisher {
    key: String,
    child: Child,
    frames: Receiver<Vec<u8>>,
    chunker: Chunker,
    published_seq: Option<u64>,
    closed: bool,
}

impl LivePublisher {
    /// Start observing `target` — a pane id or an agent name.
    ///
    /// # Errors
    /// When the `herdr` binary cannot be spawned. A missing binary is a
    /// configuration problem worth reporting; the daemon logs it and does not
    /// retry in a tight loop.
    pub fn start(key: &str, target: &str) -> Result<Self> {
        let mut child = Command::new(herdr_bin())
            .args(["terminal", "session", "observe", target])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("spawning `herdr terminal session observe {target}`"))?;

        let stdout = child.stdout.take().context("observe stdout")?;
        let (tx, frames) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(std::result::Result::ok) {
                match parse_frame(&line) {
                    Frame::Bytes(bytes) => {
                        if tx.send(bytes).is_err() {
                            return;
                        }
                    }
                    // An empty send marks the end of the stream, so the daemon can
                    // flush the chunker's tail rather than losing the last frames.
                    Frame::Closed => {
                        let _ = tx.send(Vec::new());
                        return;
                    }
                    Frame::Ignored => {}
                }
            }
        });

        Ok(Self {
            key: key.to_string(),
            child,
            frames,
            chunker: Chunker::new(key, LIVE_CHUNKS, Ruleset::default()),
            published_seq: None,
            closed: false,
        })
    }

    /// Drain whatever has arrived and publish it.
    ///
    /// Returns the highest chunk sequence published so far, which the daemon puts
    /// on the presence card as `live_seq` so a watcher knows there is something to
    /// read.
    ///
    /// # Errors
    /// When the hub rejects a chunk.
    pub fn pump(&mut self, hub: &mut dyn Hub, now_ms: u64) -> Result<Option<u64>> {
        let mut produced: Vec<Chunk> = Vec::new();
        loop {
            match self.frames.try_recv() {
                Ok(bytes) if bytes.is_empty() => {
                    self.closed = true;
                    produced.extend(self.chunker.finish(now_ms));
                    break;
                }
                Ok(bytes) => produced.extend(self.chunker.push(&bytes, now_ms)),
                Err(TryRecvError::Empty) => {
                    // The chunker holds an open chunk until it fills or ages out;
                    // without this tick a session that has gone quiet — which is
                    // exactly what a session awaiting approval looks like — would
                    // never publish its last screenful.
                    produced.extend(self.chunker.tick(now_ms));
                    break;
                }
                Err(TryRecvError::Disconnected) => {
                    self.closed = true;
                    produced.extend(self.chunker.finish(now_ms));
                    break;
                }
            }
        }

        for chunk in &produced {
            hub.put_chunk(&self.key, chunk)?;
            self.published_seq = Some(chunk.seq);
        }
        if let Some(seq) = self.published_seq {
            if seq >= LIVE_BACKLOG {
                hub.prune_chunks(&self.key, seq - LIVE_BACKLOG)?;
            }
        }
        Ok(self.published_seq)
    }

    /// Whether the observed terminal has gone away.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Highest chunk sequence published, for the presence card's `live_seq`.
    #[must_use]
    pub fn published_seq(&self) -> Option<u64> {
        self.published_seq
    }

    /// How many secrets the redactor caught on this stream. Surfaced by `doctor`
    /// because "redaction is on" is a claim worth being able to check.
    #[must_use]
    pub fn redactions(&self) -> u64 {
        self.chunker.redactions()
    }
}

impl Drop for LivePublisher {
    fn drop(&mut self) {
        // Revoking `live` has to actually stop the observation, not merely stop
        // publishing it.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One line of the observe stream, classified.
#[derive(Debug, PartialEq, Eq)]
enum Frame {
    Bytes(Vec<u8>),
    Closed,
    Ignored,
}

/// Pull terminal bytes out of one `terminal.frame` record.
///
/// Field names are probed rather than assumed, for the reason given in
/// [`crate::herdr`]: this is written against documentation, and a renamed field
/// should cost a fixture update rather than a silent dead stream.
fn parse_frame(line: &str) -> Frame {
    let Ok(value) = serde_json::from_str::<Value>(line) else { return Frame::Ignored };
    let kind = value.get("type").and_then(Value::as_str).unwrap_or_default();
    if kind.contains("closed") {
        return Frame::Closed;
    }
    let candidates = [&value, value.get("frame").unwrap_or(&Value::Null)];
    for object in candidates {
        for field in ["bytes", "data", "data_base64", "base64", "b64"] {
            if let Some(encoded) = object.get(field).and_then(Value::as_str) {
                if let Ok(bytes) = BASE64.decode(encoded) {
                    return Frame::Bytes(bytes);
                }
            }
        }
    }
    Frame::Ignored
}

/// Orders and de-duplicates chunks arriving from the hub.
///
/// The same contract as `ansible_capture::Reassembler`, minus the buffer: a viewer
/// runs for as long as someone is watching, and a reassembler that accumulates
/// every byte of the session would grow without bound. The invariants that matter
/// downstream are the ordering ones, and they are checked here.
#[derive(Debug, Default)]
pub struct FrameGate {
    expected_seq: u64,
    expected_offset: Option<u64>,
}

impl FrameGate {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Start reading from the middle of a stream.
    ///
    /// A watcher joining a session in progress cannot see its beginning, so the
    /// first chunk it gets defines the origin instead of being rejected for
    /// starting at a non-zero offset.
    pub fn join_at(&mut self, seq: u64) {
        self.expected_seq = seq;
        self.expected_offset = None;
    }

    /// Accept a chunk, returning its payload when it is the next one.
    ///
    /// # Errors
    /// When the chunk would leave a gap, or fails its own self-description. A gap
    /// is refused rather than spliced over, because a viewer that silently drops a
    /// screenful shows a plausible screen that never existed.
    pub fn accept(&mut self, chunk: &Chunk) -> Result<Option<Vec<u8>>> {
        chunk.validate()?;
        if chunk.seq < self.expected_seq {
            return Ok(None);
        }
        if chunk.seq > self.expected_seq {
            bail!("live stream gap: expected chunk {} but got {}", self.expected_seq, chunk.seq);
        }
        match self.expected_offset {
            None => self.expected_offset = Some(chunk.byte_end),
            Some(offset) if offset == chunk.byte_start => {
                self.expected_offset = Some(chunk.byte_end);
            }
            Some(offset) => {
                bail!(
                    "live stream gap: chunk {} starts at {} but the stream is at {offset}",
                    chunk.seq,
                    chunk.byte_start
                );
            }
        }
        self.expected_seq = chunk.seq + 1;
        Ok(Some(chunk.payload()))
    }

    #[must_use]
    pub fn next_seq(&self) -> u64 {
        self.expected_seq
    }
}

/// Write live chunks to a sink until the stream ends or the process is killed.
///
/// The pane running this is a plain read-only view: Herdr owns the pane, so
/// closing it is the way to stop watching, and the daemon notices the watch
/// intent disappear on the next tick.
///
/// # Errors
/// When the hub cannot be read, or the stream develops a gap.
pub fn view(
    hub: &mut dyn Hub,
    key: &str,
    from_seq: u64,
    sink: &mut impl Write,
    poll: std::time::Duration,
    mut should_continue: impl FnMut() -> bool,
) -> Result<()> {
    if !hub.supports_live() {
        bail!("{} cannot carry live frames — see hub.kind in config.toml", hub.describe());
    }
    let mut gate = FrameGate::new();
    gate.join_at(from_seq);
    while should_continue() {
        let chunks = hub.chunks(key, gate.next_seq())?;
        let mut wrote = false;
        for chunk in &chunks {
            if let Some(payload) = gate.accept(chunk)? {
                sink.write_all(&payload)?;
                wrote = true;
            }
        }
        if wrote {
            sink.flush()?;
        }
        if chunks.is_empty() {
            std::thread::sleep(poll);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunks_of(bytes: &[u8], max_bytes: usize) -> Vec<Chunk> {
        let mut chunker =
            Chunker::new("k", ChunkerConfig { max_bytes, max_age_ms: 1_000 }, Ruleset::default());
        let mut out = chunker.push(bytes, 0);
        out.extend(chunker.finish(1));
        out
    }

    #[test]
    fn a_frame_record_yields_its_terminal_bytes() {
        let encoded = BASE64.encode(b"\x1b[32mok\x1b[0m");
        let line = format!(r#"{{"type":"terminal.frame","bytes":"{encoded}"}}"#);
        assert_eq!(parse_frame(&line), Frame::Bytes(b"\x1b[32mok\x1b[0m".to_vec()));
    }

    #[test]
    fn the_payload_field_is_probed_rather_than_assumed() {
        let encoded = BASE64.encode(b"hi");
        for field in ["bytes", "data", "data_base64", "base64", "b64"] {
            let line = format!(r#"{{"type":"terminal.frame","{field}":"{encoded}"}}"#);
            assert_eq!(parse_frame(&line), Frame::Bytes(b"hi".to_vec()), "field {field}");
        }
        // And one level of nesting, in case frames are wrapped.
        let line = format!(r#"{{"type":"terminal.frame","frame":{{"bytes":"{encoded}"}}}}"#);
        assert_eq!(parse_frame(&line), Frame::Bytes(b"hi".to_vec()));
    }

    #[test]
    fn a_closed_record_ends_the_stream() {
        assert_eq!(parse_frame(r#"{"type":"terminal.closed"}"#), Frame::Closed);
    }

    #[test]
    fn junk_lines_are_ignored_rather_than_fatal() {
        assert_eq!(parse_frame("not json"), Frame::Ignored);
        assert_eq!(parse_frame(r#"{"type":"terminal.frame"}"#), Frame::Ignored);
        assert_eq!(
            parse_frame(r#"{"type":"terminal.frame","bytes":"!!!not base64!!!"}"#),
            Frame::Ignored
        );
        assert_eq!(parse_frame(""), Frame::Ignored);
    }

    #[test]
    fn the_gate_passes_a_contiguous_stream_through_unchanged() {
        let source = b"the quick brown fox jumps over the lazy dog";
        let chunks = chunks_of(source, 8);
        assert!(chunks.len() > 3);

        let mut gate = FrameGate::new();
        let mut out = Vec::new();
        for chunk in &chunks {
            if let Some(payload) = gate.accept(chunk).expect("accepted") {
                out.extend(payload);
            }
        }
        assert_eq!(out, source, "byte-exact through the gate");
        assert_eq!(gate.next_seq(), u64::try_from(chunks.len()).expect("small"));
    }

    #[test]
    fn a_duplicate_chunk_is_dropped_not_replayed() {
        let chunks = chunks_of(b"abcdefgh", 4);
        let mut gate = FrameGate::new();
        assert!(gate.accept(&chunks[0]).expect("first").is_some());
        assert!(gate.accept(&chunks[0]).expect("duplicate").is_none(), "must not write twice");
        assert!(gate.accept(&chunks[1]).expect("next").is_some());
    }

    /// A viewer that silently skips a chunk shows a screen that never existed.
    #[test]
    fn a_gap_is_refused_rather_than_spliced_over() {
        let chunks = chunks_of(b"abcdefghijkl", 4);
        let mut gate = FrameGate::new();
        gate.accept(&chunks[0]).expect("first");
        let err = gate.accept(&chunks[2]).expect_err("gap");
        assert!(format!("{err}").contains("gap"), "got {err}");
    }

    #[test]
    fn a_watcher_can_join_mid_stream() {
        let chunks = chunks_of(b"abcdefghijkl", 4);
        let mut gate = FrameGate::new();
        gate.join_at(1);
        // Joining at 1 must not require chunk 1 to start at byte zero.
        assert!(chunks[1].byte_start > 0);
        assert!(gate.accept(&chunks[1]).expect("joined").is_some());
        assert!(gate.accept(&chunks[2]).expect("continues").is_some());
    }

    #[test]
    fn a_chunk_that_contradicts_itself_is_refused() {
        let mut chunks = chunks_of(b"abcd", 4);
        chunks[0].byte_end += 5;
        let mut gate = FrameGate::new();
        assert!(gate.accept(&chunks[0]).is_err());
    }

    #[test]
    fn the_herdr_binary_falls_back_to_the_name_on_path() {
        // Outside a plugin invocation there is no HERDR_BIN_PATH, and `ansible-herd`
        // has to keep working from a shell.
        let resolved = herdr_bin();
        assert!(!resolved.is_empty());
    }
}
