//! The golden round-trip test: bytes in must equal bytes out, forever.
//!
//! The plan calls byte-exact reconstruction "the golden test that protects the
//! capture path forever," and names capture correctness the one thing in this
//! system with no acceptable failure mode. So these tests are deliberately
//! paranoid: they drive the full path — redact, chunk, serialize to the stored
//! JSONL form, parse back, reassemble — and compare against a reference.
//!
//! Two things are checked separately, because conflating them hides bugs:
//!
//! * **Fidelity** — the reassembled stream equals the redacted reference, byte
//!   for byte. Not the *raw* stream: redaction intentionally rewrites bytes, so
//!   raw-exactness would be the wrong invariant and would mean secrets survived.
//! * **Containment** — no secret from the input appears anywhere in the stored
//!   chunks.

use ansible_capture::{Chunk, Chunker, ChunkerConfig, Reassembler, Redactor, Ruleset};

/// Drive the full pipeline the way the app will: chunk, serialize, parse,
/// reassemble. Returns the stored wire form and the reconstructed stream.
fn round_trip(writes: &[(&[u8], u64)], config: ChunkerConfig) -> (Vec<String>, Vec<u8>) {
    let mut chunker = Chunker::new("session-golden", config, Ruleset::default());

    let mut chunks = Vec::new();
    let mut last_ms = 0;
    for (bytes, at_ms) in writes {
        chunks.extend(chunker.push(bytes, *at_ms));
        last_ms = *at_ms;
    }
    chunks.extend(chunker.finish(last_ms + 1));

    // Everything crosses the stored representation, so a serialization bug
    // cannot hide behind in-memory equality.
    let wire: Vec<String> = chunks.iter().map(|c| c.to_jsonl().expect("serialize")).collect();

    let mut reassembler = Reassembler::new();
    for text in &wire {
        let parsed = Chunk::from_jsonl(text).expect("parse");
        reassembler.accept(&parsed).expect("accept in order");
    }
    (wire, reassembler.into_bytes())
}

/// The same redaction the chunker applies, run standalone, to produce the
/// reference stream that reassembly must match.
fn redacted_reference(writes: &[(&[u8], u64)]) -> Vec<u8> {
    let mut redactor = Redactor::new(Ruleset::default());
    let mut out = Vec::new();
    for (bytes, _) in writes {
        out.extend(redactor.push(bytes));
    }
    out.extend(redactor.finish());
    out
}

/// Output shaped like a real Claude Code session: prompts, colored TUI frames,
/// box drawing, a diff, binary escape sequences, and a leaked credential.
fn realistic_session() -> Vec<(Vec<u8>, u64)> {
    let mut writes: Vec<(Vec<u8>, u64)> = vec![
        (b"\x1b[?1049h\x1b[2J\x1b[H".to_vec(), 0),
        ("\x1b[1mWelcome to Claude Code\x1b[0m\r\n".into(), 5),
        ("\x1b[38;2;215;119;87m╭──────────────╮\x1b[0m\r\n".into(), 10),
        (
            "\x1b[38;2;215;119;87m│ \x1b[0mrefactor this\x1b[38;2;215;119;87m │\x1b[0m\r\n".into(),
            12,
        ),
        ("\x1b[38;2;215;119;87m╰──────────────╯\x1b[0m\r\n".into(), 14),
        (b"\x1b[32m+ added line\x1b[0m\r\n".to_vec(), 40),
        (b"\x1b[31m- removed line\x1b[0m\r\n".to_vec(), 42),
        // A credential that must never reach a chunk.
        (b"env: ANTHROPIC_API_KEY=sk-ant-api03-SECRETSECRETSECRET1234\r\n".to_vec(), 60),
        (b"\xe2\x9c\x94 done\r\n".to_vec(), 90),
        (b"\x00\x01\x02\xff\xfe\r\n".to_vec(), 95),
    ];
    // A long build-log burst, to cross several chunk boundaries.
    for i in 0..400u64 {
        writes.push((format!("   Compiling crate-{i} v0.1.{i}\r\n").into_bytes(), 100 + i));
    }
    writes.push((b"\x1b[?1049l".to_vec(), 600));
    writes
}

fn as_refs(writes: &[(Vec<u8>, u64)]) -> Vec<(&[u8], u64)> {
    writes.iter().map(|(b, t)| (b.as_slice(), *t)).collect()
}

#[test]
fn realistic_session_round_trips_byte_exactly() {
    let owned = realistic_session();
    let writes = as_refs(&owned);
    let (wire, reconstructed) = round_trip(&writes, ChunkerConfig::default());

    assert_eq!(
        reconstructed,
        redacted_reference(&writes),
        "reassembled stream must equal the redacted reference byte for byte"
    );
    assert!(!wire.is_empty(), "a realistic session must produce chunks");
}

#[test]
fn the_leaked_credential_never_reaches_a_chunk() {
    let owned = realistic_session();
    let writes = as_refs(&owned);
    let (wire, reconstructed) = round_trip(&writes, ChunkerConfig::default());

    let secret = "sk-ant-api03-SECRETSECRETSECRET1234";
    for (i, text) in wire.iter().enumerate() {
        // The wire form is base64, so decode before searching: a substring
        // check on the encoded text would pass even if the secret were stored.
        let payload = Chunk::from_jsonl(text).unwrap().payload();
        let decoded = String::from_utf8_lossy(&payload);
        assert!(!decoded.contains(secret), "chunk {i} contains the credential");
    }
    let all = String::from_utf8_lossy(&reconstructed);
    assert!(!all.contains(secret), "the reconstructed stream contains the credential");
    // `ANTHROPIC_API_KEY=` matches the named-value rule, which spans the whole
    // value and therefore wins over the narrower vendor-token rule. Assert the
    // secret is gone and *a* marker replaced it, not which rule fired.
    assert!(all.contains("[redacted:"), "the redaction marker is missing");
    assert!(all.contains("ANTHROPIC_API_KEY="), "the variable name should survive");
}

/// The property that matters most: chunk boundaries must not depend on how the
/// PTY happened to split its writes.
#[test]
fn reconstruction_is_independent_of_write_boundaries() {
    let owned = realistic_session();
    let flat: Vec<u8> = owned.iter().flat_map(|(b, _)| b.clone()).collect();

    let one_write = round_trip(&[(flat.as_slice(), 0)], ChunkerConfig::default()).1;

    for step in [1usize, 2, 3, 7, 64, 997] {
        let split: Vec<(&[u8], u64)> =
            flat.chunks(step).enumerate().map(|(i, c)| (c, i as u64)).collect();
        let many_writes = round_trip(&split, ChunkerConfig::default()).1;
        assert_eq!(
            many_writes, one_write,
            "reconstruction differed when the stream was split every {step} bytes"
        );
    }
}

/// Chunk size must not change the reconstructed bytes either.
#[test]
fn reconstruction_is_independent_of_chunk_size() {
    let owned = realistic_session();
    let writes = as_refs(&owned);
    let baseline = round_trip(&writes, ChunkerConfig::default()).1;

    for max_bytes in [1usize, 7, 64, 1024, 64 * 1024, 1 << 20] {
        let config = ChunkerConfig { max_bytes, max_age_ms: 1000 };
        let (_, reconstructed) = round_trip(&writes, config);
        assert_eq!(reconstructed, baseline, "differed at max_bytes={max_bytes}");
    }
}

#[test]
fn every_byte_value_survives_the_round_trip() {
    // 64 KiB covering all 256 byte values, so base64, JSONL framing, and the
    // record split are all exercised on non-text data.
    let data: Vec<u8> = (0..=255u8).cycle().take(64 * 1024).collect();
    let (_, reconstructed) =
        round_trip(&[(data.as_slice(), 0)], ChunkerConfig { max_bytes: 700, max_age_ms: 1000 });
    assert_eq!(reconstructed, data);
}

#[test]
fn chunk_sequence_and_offsets_are_dense_and_contiguous() {
    let owned = realistic_session();
    let writes = as_refs(&owned);
    let (wire, reconstructed) =
        round_trip(&writes, ChunkerConfig { max_bytes: 512, max_age_ms: 1000 });

    let chunks: Vec<Chunk> = wire.iter().map(|t| Chunk::from_jsonl(t).unwrap()).collect();
    assert!(chunks.len() > 1, "expected several chunks at 512 bytes each");

    let mut offset = 0u64;
    for (i, chunk) in chunks.iter().enumerate() {
        assert_eq!(chunk.seq, u64::try_from(i).unwrap(), "sequence must be dense");
        assert_eq!(chunk.byte_start, offset, "byte ranges must be contiguous");
        assert!(chunk.byte_end > chunk.byte_start, "chunk {i} is empty");
        chunk.validate().expect("self-consistent envelope");
        offset = chunk.byte_end;
    }
    assert_eq!(
        usize::try_from(offset).unwrap(),
        reconstructed.len(),
        "final offset must equal stored length"
    );
}

#[test]
fn records_preserve_arrival_order_and_timing() {
    // Each write ends on a word boundary so redaction releases it immediately
    // and it becomes its own record; a write ending mid-identifier is held for
    // correctness and would merge with the next one.
    let writes: Vec<(&[u8], u64)> =
        vec![(b"first\n", 1000), (b"second\n", 1200), (b"third\n", 1500)];
    let (wire, reconstructed) = round_trip(&writes, ChunkerConfig::default());

    assert_eq!(reconstructed, b"first\nsecond\nthird\n");
    let chunk = Chunk::from_jsonl(&wire[0]).unwrap();
    let deltas: Vec<u32> = chunk.records.iter().map(|r| r.at_delta_ms).collect();
    assert!(deltas.windows(2).all(|w| w[0] <= w[1]), "timing must be monotonic: {deltas:?}");
    assert_eq!(chunk.started_at_ms, 1000);
}

/// Failure injection, per the plan: killing the uploader mid-session must not
/// produce a stream that silently omits the lost chunks.
#[test]
fn a_lost_chunk_is_detected_rather_than_silently_skipped() {
    let owned = realistic_session();
    let writes = as_refs(&owned);
    let (wire, _) = round_trip(&writes, ChunkerConfig { max_bytes: 512, max_age_ms: 1000 });
    let chunks: Vec<Chunk> = wire.iter().map(|t| Chunk::from_jsonl(t).unwrap()).collect();
    assert!(chunks.len() >= 3, "need enough chunks to drop one from the middle");

    let mut reassembler = Reassembler::new();
    reassembler.accept(&chunks[0]).expect("first chunk");
    let err = reassembler.accept(&chunks[2]).expect_err("a gap must be an error");
    assert!(err.to_string().contains("expected chunk 1"), "unhelpful error: {err}");
}

/// Recovery after a crash: the app re-uploads from its spool, so the viewer sees
/// chunks it already has. Overlap must be tolerated; a gap still must not be.
#[test]
fn replayed_chunks_are_deduplicated_but_gaps_still_fail() {
    let owned = realistic_session();
    let writes = as_refs(&owned);
    let (wire, reference) = round_trip(&writes, ChunkerConfig { max_bytes: 512, max_age_ms: 1000 });
    let chunks: Vec<Chunk> = wire.iter().map(|t| Chunk::from_jsonl(t).unwrap()).collect();

    let mut reassembler = Reassembler::new();
    for chunk in &chunks {
        // Deliver each chunk twice, as a relay overlapping a backfill would.
        assert!(reassembler.accept_deduplicated(chunk).unwrap(), "first delivery accepted");
        assert!(!reassembler.accept_deduplicated(chunk).unwrap(), "replay skipped");
    }
    assert_eq!(reassembler.bytes(), reference.as_slice(), "dedup must not alter the stream");

    let mut gapped = Reassembler::new();
    gapped.accept_deduplicated(&chunks[0]).unwrap();
    assert!(gapped.accept_deduplicated(&chunks[2]).is_err(), "a gap is never acceptable");
}

/// A truncated upload must fail to parse rather than yield a short chunk, which
/// would look like valid data with a hole in it.
#[test]
fn a_truncated_chunk_upload_fails_to_parse() {
    let owned = realistic_session();
    let writes = as_refs(&owned);
    let (wire, _) = round_trip(&writes, ChunkerConfig { max_bytes: 512, max_age_ms: 1000 });

    let full = &wire[0];
    let lines: Vec<&str> = full.lines().collect();
    assert!(lines.len() > 2, "need multiple records to truncate");

    let truncated = lines[..lines.len() - 1].join("\n") + "\n";
    assert!(
        Chunk::from_jsonl(&truncated).is_err(),
        "a chunk missing a record must not parse as valid"
    );
}

#[test]
fn an_empty_session_produces_nothing_and_reassembles_to_nothing() {
    let (wire, reconstructed) = round_trip(&[], ChunkerConfig::default());
    assert!(wire.is_empty());
    assert!(reconstructed.is_empty());
}

/// Guards the wire format itself. If this breaks, stored transcripts from older
/// builds may no longer be readable, which is a migration, not a refactor.
#[test]
fn the_stored_format_is_stable() {
    let mut chunker = Chunker::new(
        "session-fixed",
        ChunkerConfig { max_bytes: 1024, max_age_ms: 1000 },
        Ruleset::default(),
    );
    chunker.push(b"hello", 1_700_000_000_000);
    let chunks = chunker.finish(1_700_000_000_010);
    let text = chunks[0].to_jsonl().unwrap();

    let header: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    for field in [
        "session_id",
        "seq",
        "byte_start",
        "byte_end",
        "started_at_ms",
        "ended_at_ms",
        "redaction_version",
        "record_count",
    ] {
        assert!(header.get(field).is_some(), "header lost the `{field}` field");
    }

    let record: serde_json::Value = serde_json::from_str(text.lines().nth(1).unwrap()).unwrap();
    assert_eq!(record["at_delta_ms"], 0);
    assert_eq!(record["bytes"], "aGVsbG8=", "record payloads must stay base64");
}
