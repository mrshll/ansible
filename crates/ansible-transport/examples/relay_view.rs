//! The second process: backfill, tail, and reconstruct the stream.
//!
//! This is the half of the Spike B round trip that proves the design works. It
//! shares no memory with the publisher — it learns everything from the Worker — and
//! its output file must equal the publisher's reference byte for byte.
//!
//! ```text
//! cargo run -p ansible-transport --example relay-view -- s-1 out.bin
//! ANSIBLE_VIEW_MODE=cursor cargo run -p ansible-transport --example relay-view -- s-1 out.bin
//! ```
//!
//! Two modes, because assumption A2 is a choice between two transports and the
//! spike is supposed to measure both rather than assume one:
//!
//! - `relay` (default) — join the WebSocket, apply frames as they arrive, backfill
//!   through the archive for anything missed. This is the sub-second path.
//! - `cursor` — ignore frames entirely and poll the cursor, fetching chunks as they
//!   become durable. The simpler fallback, and the one whose measured delay has to
//!   be written down rather than hidden behind the UI.
//!
//! Environment:
//!   `ANSIBLE_WORKER_URL`  default `http://localhost:8787`
//!   `ANSIBLE_VIEW_MODE`   `relay` | `cursor`
//!   `ANSIBLE_VIEW_SECONDS` idle timeout before concluding the stream ended
//!   `ANSIBLE_LATENCY_OUT` write raw latency samples here, one per line

use std::io::BufWriter;
use std::time::{Duration, Instant};

use ansible_capture::Chunk;
use ansible_transport::{Applied, LiveViewer, RelayFrame, now_ms};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
enum Message {
    Hello { chunk_cursor: u64, byte_cursor: u64, live_byte_end: u64 },
    Frame(RelayFrame),
    // `byte_cursor` is deliberately ignored: the viewer trusts its own
    // `received_through` and backfills by chunk sequence, so taking the Worker's
    // byte offset as authoritative would add a second source of truth for the one
    // number that must not have two.
    Cursor { chunk_cursor: u64 },
    Stall { reason: String, expected_byte_start: u64, got_byte_start: u64 },
}

struct Archive {
    agent: ureq::Agent,
    base_url: String,
    session_id: String,
    token: String,
}

impl Archive {
    fn fetch_chunk(&self, seq: u64) -> Result<Chunk, Box<dyn std::error::Error>> {
        let text = self
            .agent
            .get(format!("{}/v1/session/{}/chunk/{seq}", self.base_url, self.session_id))
            .header("Authorization", &format!("Bearer {}", self.token))
            .call()?
            .body_mut()
            .read_to_string()?;
        Ok(Chunk::from_jsonl(&text)?)
    }

    fn cursor(&self) -> Result<(u64, u64), Box<dyn std::error::Error>> {
        #[derive(Deserialize)]
        struct Status {
            chunk_cursor: u64,
            byte_cursor: u64,
        }
        let status: Status = self
            .agent
            .get(format!("{}/v1/session/{}/status", self.base_url, self.session_id))
            .header("Authorization", &format!("Bearer {}", self.token))
            .call()?
            .body_mut()
            .read_json()?;
        Ok((status.chunk_cursor, status.byte_cursor))
    }
}

/// Pull durable chunks until the viewer has caught up to `chunk_cursor`.
///
/// Used both to backfill on join and to close a gap after a relay disconnect —
/// the same code path, because "I joined late" and "I fell behind" are the same
/// problem stated twice.
fn backfill(
    archive: &Archive,
    viewer: &mut LiveViewer,
    next_seq: &mut u64,
    chunk_cursor: u64,
    latencies: &mut Vec<i64>,
    record_latency: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    while *next_seq < chunk_cursor {
        let chunk = archive.fetch_chunk(*next_seq)?;
        // One sample per *record*, timed from when that record's bytes left the PTY.
        //
        // Timing from the chunk's `ended_at_ms` instead would quietly flatter this
        // transport: the last record in a chunk is fresh the moment the chunk
        // closes, while the first has been waiting the entire flush interval. Since
        // the whole point of the comparison is what a human waiting on the grid
        // experiences, every byte has to be counted from its own arrival, and the
        // relay is measured per frame the same way.
        let samples: Vec<i64> = chunk
            .records
            .iter()
            .map(|r| {
                let produced = chunk.started_at_ms + u64::from(r.at_delta_ms);
                i64::try_from(now_ms()).unwrap_or(i64::MAX) - i64::try_from(produced).unwrap_or(0)
            })
            .collect();
        match viewer.apply_chunk(&chunk) {
            Ok(Applied::Advanced(_)) if record_latency => latencies.extend(samples),
            Ok(_) => {}
            Err(e) => return Err(Box::new(e)),
        }
        *next_seq += 1;
    }
    Ok(())
}

/// Cursor-follow only: the simpler transport, measured on its own terms so its
/// delay is a number in the writeup rather than an assumption.
fn follow_cursor(
    archive: &Archive,
    viewer: &mut LiveViewer,
    next_seq: &mut u64,
    latencies: &mut Vec<i64>,
    idle_secs: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut idle_since = Instant::now();
    let mut last_cursor = 0;
    loop {
        let (chunk_cursor, _byte_cursor) = archive.cursor()?;
        if chunk_cursor > last_cursor {
            backfill(archive, viewer, next_seq, chunk_cursor, latencies, true)?;
            last_cursor = chunk_cursor;
            idle_since = Instant::now();
        }
        if idle_since.elapsed() > Duration::from_secs(idle_secs) {
            return Ok(());
        }
        // The polling interval is part of the measurement: it is this transport's
        // inherent cost, not an implementation detail to tune away.
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Join the relay and apply frames live, backfilling from the archive for anything
/// the relay could not deliver in order. Returns the number of stalls observed.
#[allow(clippy::too_many_arguments)]
fn follow_relay(
    archive: &Archive,
    viewer: &mut LiveViewer,
    next_seq: &mut u64,
    latencies: &mut Vec<i64>,
    base_url: &str,
    session_id: &str,
    token: &str,
    idle_secs: u64,
) -> Result<u64, Box<dyn std::error::Error>> {
    let mut stalls = 0_u64;
    let ws_url = format!(
        "{}/v1/session/{session_id}/relay?token={token}",
        base_url.replacen("http", "ws", 1)
    );
    let (mut socket, _response) = tungstenite::connect(&ws_url)?;
    // A read timeout is what lets the loop notice the stream has gone quiet without
    // hanging forever on a session that already ended.
    if let tungstenite::stream::MaybeTlsStream::Plain(stream) = socket.get_mut() {
        stream.set_read_timeout(Some(Duration::from_millis(250)))?;
    }

    let mut idle_since = Instant::now();
    loop {
        match socket.read() {
            Ok(tungstenite::Message::Text(text)) => {
                idle_since = Instant::now();
                match serde_json::from_str::<Message>(&text)? {
                    Message::Hello { chunk_cursor, byte_cursor, live_byte_end } => {
                        eprintln!(
                            "viewer: joined at chunk_cursor={chunk_cursor} byte_cursor={byte_cursor} live_byte_end={live_byte_end}"
                        );
                        // Joined mid-stream: catch up through the archive before
                        // applying any frame, or the first frame looks like a gap.
                        backfill(archive, viewer, next_seq, chunk_cursor, latencies, false)?;
                    }
                    Message::Frame(frame) => {
                        let at_ms = frame.at_ms;
                        match viewer.apply_frame(&frame) {
                            Ok(Applied::Advanced(_)) => latencies.push(
                                i64::try_from(now_ms()).unwrap_or(i64::MAX)
                                    - i64::try_from(at_ms).unwrap_or(0),
                            ),
                            Ok(Applied::Duplicate) => {}
                            Err(_) => {
                                // A frame that would leave a gap is not applied. The
                                // archive is authoritative, so close the gap from
                                // there rather than dropping the byte range.
                                stalls += 1;
                                let (chunk_cursor, _) = archive.cursor()?;
                                backfill(
                                    archive,
                                    viewer,
                                    next_seq,
                                    chunk_cursor,
                                    latencies,
                                    false,
                                )?;
                                // Retry once the archive has caught us up.
                                let _ = viewer.apply_frame(&frame);
                            }
                        }
                    }
                    Message::Cursor { chunk_cursor } => {
                        // Confirms relayed bytes are durable, and closes any gap the
                        // relay could not.
                        backfill(archive, viewer, next_seq, chunk_cursor, latencies, false)?;
                    }
                    Message::Stall { reason, expected_byte_start, got_byte_start } => {
                        stalls += 1;
                        eprintln!(
                            "viewer: relay stalled: {reason} (expected {expected_byte_start}, got {got_byte_start})"
                        );
                    }
                }
            }
            Ok(tungstenite::Message::Close(_)) => break,
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if idle_since.elapsed() > Duration::from_secs(idle_secs) {
                    break;
                }
            }
            Err(e) => {
                eprintln!("viewer: relay closed: {e}");
                break;
            }
        }
    }

    // Final catch-up: the tail chunk may have landed after the last frame.
    let (chunk_cursor, _) = archive.cursor()?;
    backfill(archive, viewer, next_seq, chunk_cursor, latencies, false)?;
    Ok(stalls)
}

fn percentile(sorted: &[i64], p: f64) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss, clippy::cast_sign_loss)]
    let idx = (((sorted.len() - 1) as f64) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let session_id = args.next().ok_or("usage: relay-view <session-id> <out-file>")?;
    let out_path = args.next().ok_or("usage: relay-view <session-id> <out-file>")?;

    let base_url =
        std::env::var("ANSIBLE_WORKER_URL").unwrap_or_else(|_| "http://localhost:8787".into());
    let mode = std::env::var("ANSIBLE_VIEW_MODE").unwrap_or_else(|_| "relay".into());
    let idle_secs: u64 =
        std::env::var("ANSIBLE_VIEW_SECONDS").ok().and_then(|v| v.parse().ok()).unwrap_or(10);
    let token = std::env::var("ANSIBLE_VIEW_TOKEN").unwrap_or_else(|_| "spike-view-token".into());

    let archive = Archive {
        agent: ureq::Agent::new_with_defaults(),
        base_url: base_url.clone(),
        session_id: session_id.clone(),
        token: token.clone(),
    };

    let file = std::fs::File::create(&out_path)?;
    let mut viewer = LiveViewer::new(Box::new(BufWriter::new(file)));
    let mut latencies: Vec<i64> = Vec::new();
    let mut next_seq: u64 = 0;
    let mut stalls = 0_u64;

    if mode == "cursor" {
        follow_cursor(&archive, &mut viewer, &mut next_seq, &mut latencies, idle_secs)?;
    } else {
        stalls = follow_relay(
            &archive,
            &mut viewer,
            &mut next_seq,
            &mut latencies,
            &base_url,
            &session_id,
            &token,
            idle_secs,
        )?;
    }

    viewer.flush()?;

    latencies.sort_unstable();
    let n = latencies.len();
    println!("viewer: mode={mode}");
    println!(
        "viewer: bytes={} frames_applied={} chunks_applied={} duplicates={} stalls={stalls}",
        viewer.bytes_written(),
        viewer.frames_applied(),
        viewer.chunks_applied(),
        viewer.duplicates(),
    );
    if n > 0 {
        println!(
            "viewer: latency_ms n={n} p50={} p95={} p99={} max={}",
            percentile(&latencies, 0.50),
            percentile(&latencies, 0.95),
            percentile(&latencies, 0.99),
            latencies[n - 1],
        );
    } else {
        println!("viewer: latency_ms n=0 (nothing was applied live)");
    }

    if let Ok(path) = std::env::var("ANSIBLE_LATENCY_OUT") {
        let text: String = latencies.iter().map(|l| format!("{l}\n")).collect::<Vec<_>>().concat();
        std::fs::write(path, text)?;
    }
    Ok(())
}
