//! Run a command under a PTY and publish its transcript to the Worker.
//!
//! The publisher half of the Spike B round trip. Spawns a real process on a real
//! PTY, redacts, chunks, and publishes both streams — ephemeral frames for the
//! relay and durable chunks for the archive.
//!
//! ```text
//! cargo run -p ansible-transport --example relay-publish -- s-1 bash script.sh
//! ANSIBLE_PUBLISH_SECONDS=30 cargo run -p ansible-transport --example relay-publish -- s-1 claude
//! ```
//!
//! Environment:
//!   `ANSIBLE_WORKER_URL`       default `http://localhost:8787`
//!   `ANSIBLE_PUBLISH_SECONDS`  wall-clock cap, default 60
//!   `ANSIBLE_SPOOL_DIR`        local spool, default a temp directory
//!   `ANSIBLE_REFERENCE_OUT`    write the redacted reference stream here
//!
//! The reference file is what the viewer's reconstruction is compared against, and
//! it is the *redacted* stream — the only one that is ever stored, and therefore
//! the only one byte-exactness can be claimed about.

use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ansible_capture::{Chunker, ChunkerConfig, Redactor, Ruleset};
use ansible_transport::{Publisher, PublisherConfig, now_ms};

/// Everything the publisher accumulated, for the summary and the assertions.
#[derive(Default)]
struct Totals {
    reads: u64,
    byte_offset: u64,
    frames_dropped: u64,
    upload_failures: u64,
}

/// A spawned child and the read side of its PTY.
struct PtySession {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    reader: Box<dyn Read + Send>,
}

/// Spawn `command` on a PTY and hand back the child and its output side.
fn spawn_on_pty(command: &str, args: &[String]) -> Result<PtySession, Box<dyn std::error::Error>> {
    let pty = portable_pty::native_pty_system();
    let pair = pty.openpty(portable_pty::PtySize {
        rows: 40,
        cols: 120,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut builder = portable_pty::CommandBuilder::new(command);
    for arg in args {
        builder.arg(arg);
    }
    builder.env("LC_ALL", "C.UTF-8");
    builder.env("TERM", "xterm-256color");
    let child = pair.slave.spawn_command(builder)?;
    // Dropping the slave is what lets `read` see EOF when the child exits; holding
    // it would keep the PTY open and the loop below would run to its deadline
    // instead of finishing with the process.
    drop(pair.slave);
    let reader = pair.master.try_clone_reader()?;
    Ok(PtySession { child, reader })
}

/// Drain the redactor's held-back tail and close the final chunk.
///
/// Separate from the read loop because the ordering here is subtle and easy to get
/// wrong: the redactor's tail must be pushed *into* the chunker before the chunker
/// is finished, or the last few bytes of the session never reach a chunk. Skipping
/// the redactor's `finish` entirely would store a secret in the clear whenever a
/// session ends right after printing one — a bug the capture crate's own history
/// records.
fn finish_stream(
    publisher: &mut Publisher,
    redactor: &mut Redactor,
    chunker: &mut Chunker,
    reference: &mut Vec<u8>,
    totals: &mut Totals,
) -> Result<(), Box<dyn std::error::Error>> {
    let tail = redactor.finish();
    if !tail.is_empty() {
        reference.extend_from_slice(&tail);
        publisher.publish_frame(totals.byte_offset, &tail, now_ms())?;
        totals.byte_offset += tail.len() as u64;
        for chunk in chunker.push(&tail, now_ms()) {
            if publisher.publish_chunk(&chunk).is_err() {
                totals.upload_failures += 1;
            }
        }
    }
    for chunk in chunker.finish(now_ms()) {
        if publisher.publish_chunk(&chunk).is_err() {
            totals.upload_failures += 1;
        }
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let session_id = args.next().ok_or("usage: relay-publish <session-id> <command> [args...]")?;
    let command = args.next().ok_or("usage: relay-publish <session-id> <command> [args...]")?;
    let command_args: Vec<String> = args.collect();

    let base_url =
        std::env::var("ANSIBLE_WORKER_URL").unwrap_or_else(|_| "http://localhost:8787".into());
    let seconds: u64 =
        std::env::var("ANSIBLE_PUBLISH_SECONDS").ok().and_then(|v| v.parse().ok()).unwrap_or(60);
    let spool_dir = std::env::var("ANSIBLE_SPOOL_DIR")
        .map_or_else(|_| std::env::temp_dir().join("ansible-spool"), PathBuf::from);

    let mut config = PublisherConfig::new(&base_url, &session_id);
    if let Ok(token) = std::env::var("ANSIBLE_PUBLISH_TOKEN") {
        config.publish_token = token;
    }
    let mut publisher = Publisher::new(config, &spool_dir)?;

    // One redaction pass, owned here rather than inside the chunker, because frames
    // must be published the moment bytes are safe — long before a chunk closes. The
    // chunker therefore gets an empty ruleset: redacting the same bytes twice would
    // be wasted work on the hot path and a second place for the two streams to
    // disagree about what a byte offset means.
    let ruleset = Ruleset::default();
    let redaction_version = ruleset.version;
    let mut redactor = Redactor::new(ruleset);
    let mut chunker = Chunker::new(
        session_id.clone(),
        ChunkerConfig::default(),
        Ruleset { version: redaction_version, rules: Vec::new() },
    );

    let PtySession { mut child, mut reader } = spawn_on_pty(&command, &command_args)?;

    let mut totals = Totals::default();
    let mut reference: Vec<u8> = Vec::new();

    let deadline = Instant::now() + Duration::from_secs(seconds);
    // Heap, not stack: 64 KiB matches the chunker's size threshold, which is far
    // past what belongs in a stack frame.
    let mut buf = vec![0u8; 64 * 1024].into_boxed_slice();

    // Reading on this thread and publishing inline keeps the measurement honest:
    // the frame's `at_ms` is stamped between the read returning and the POST, so
    // the viewer's latency number includes everything the design actually costs.
    loop {
        if Instant::now() >= deadline {
            break;
        }
        let n = match reader.read(&mut buf) {
            Ok(0) => break, // EOF: the child closed the PTY.
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        totals.reads += 1;
        let at_ms = now_ms();
        let safe = redactor.push(&buf[..n]);
        if !safe.is_empty() {
            reference.extend_from_slice(&safe);
            if !publisher.publish_frame(totals.byte_offset, &safe, at_ms)? {
                totals.frames_dropped += 1;
            }
            totals.byte_offset += safe.len() as u64;
            for chunk in chunker.push(&safe, at_ms) {
                // A failed upload must not end the session. `publish_chunk` spools
                // before it uploads, so the chunk is already safe on disk; the
                // cursor simply stops advancing and viewers see a stalled tail.
                // Aborting here instead would turn a recoverable stall into a
                // truncated transcript — the exact thing the spool exists to
                // prevent.
                if publisher.publish_chunk(&chunk).is_err() {
                    totals.upload_failures += 1;
                }
            }
        }
        // Age-based flush, so an idle-but-open session still advances the cursor.
        if let Some(chunk) = chunker.tick(now_ms())
            && publisher.publish_chunk(&chunk).is_err()
        {
            totals.upload_failures += 1;
        }
    }

    finish_stream(&mut publisher, &mut redactor, &mut chunker, &mut reference, &mut totals)?;

    let _ = child.kill();
    let _ = child.wait();

    // The reference is written *here* — after the stream is complete, before any
    // recovery is attempted. It describes what the PTY produced, which is settled
    // once the redactor has been finished, and is deliberately independent of
    // whether uploading succeeded. Writing it after the flush would mean a failed
    // upload left nothing to compare a later recovery against, which is precisely
    // the case worth comparing.
    if let Ok(path) = std::env::var("ANSIBLE_REFERENCE_OUT") {
        std::fs::write(&path, &reference)?;
    }

    // Anything still spooled means the Worker refused or was unreachable; retry
    // before declaring the transcript complete.
    let flushed = publisher.flush_spool().unwrap_or(0);
    let finalized = publisher.finalize(redaction_version).is_ok();

    // From the redactor that actually ran; the chunker's own ruleset is empty by
    // design, so asking it would always report zero.
    println!(
        "publisher: reads={} redacted_bytes={} redactions={}",
        totals.reads,
        totals.byte_offset,
        redactor.redactions()
    );
    println!(
        "publisher: frames_sent={} frames_dropped={} chunks_confirmed={} retries={} upload_failures={} late_flush={flushed} finalized={finalized}",
        publisher.frames_sent(),
        totals.frames_dropped,
        publisher.chunks_confirmed(),
        publisher.retries(),
        totals.upload_failures,
    );

    let still_spooled = publisher.spool().pending()?;
    if !still_spooled.is_empty() {
        // Loud, and a non-zero exit: a transcript with unspooled chunks is not
        // byte-exact, and no downstream rigor can recover it.
        eprintln!("publisher: {} chunks NEVER uploaded: {still_spooled:?}", still_spooled.len());
        std::process::exit(1);
    }
    Ok(())
}
