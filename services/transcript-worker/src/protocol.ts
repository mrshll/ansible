/**
 * The wire contract between the app, the relay, and the viewer.
 *
 * This file is a port, not a design. `crates/ansible-capture` owns the chunk
 * envelope and its golden round-trip test; the shapes here must match it byte
 * for byte or the archive stops being byte-exact. When the Rust changes, this
 * changes with it.
 *
 * Two streams leave one session, and conflating them is the mistake this file
 * exists to prevent:
 *
 * - **Frames** are sent the moment bytes leave the PTY. They are ephemeral, exist
 *   only to make the relay feel live, and are addressed by *byte range*.
 * - **Chunks** are sent when the chunker closes one (~64 KiB or ~1s). They are
 *   durable, land in R2, and are addressed by *sequence number*.
 *
 * A viewer receives both and must not double-apply the overlap. Byte ranges are
 * what make that safe: they are absolute offsets into the redacted stream, so a
 * viewer that tracks "received through byte N" can accept either kind of message
 * and splice it at the right place. Sequence numbers alone could not do this,
 * because a frame is not a chunk and has no sequence of its own.
 */

/** One record inside a durable chunk. `bytes` is base64: PTY output is binary. */
export interface Record {
  at_delta_ms: number;
  bytes: string;
}

/**
 * A durable chunk of transcript.
 *
 * Byte offsets index the **redacted** stream — the only stream that is stored,
 * and therefore the only one a viewer or a mention anchor can address.
 */
export interface Chunk {
  session_id: string;
  seq: number;
  byte_start: number;
  /** Exclusive. */
  byte_end: number;
  started_at_ms: number;
  ended_at_ms: number;
  redaction_version: number;
  records: Record[];
}

/** The stored form's first line. `record_count` is what makes truncation visible. */
interface ChunkHeader {
  session_id: string;
  seq: number;
  byte_start: number;
  byte_end: number;
  started_at_ms: number;
  ended_at_ms: number;
  redaction_version: number;
  record_count: number;
}

export class ProtocolError extends Error {}

/**
 * Parse the stored JSONL form.
 *
 * JSONL rather than one document so a truncated upload loses only its last line.
 * The header's `record_count` is checked against the records actually present,
 * which is how a truncated body is rejected instead of silently accepted as a
 * short chunk.
 */
export function parseChunkJsonl(text: string): Chunk {
  const lines = text.split("\n").filter((l) => l.trim() !== "");
  if (lines.length === 0) throw new ProtocolError("chunk is empty");

  let header: ChunkHeader;
  try {
    header = JSON.parse(lines[0]) as ChunkHeader;
  } catch (e) {
    throw new ProtocolError(`chunk header is not JSON: ${String(e)}`);
  }

  const records: Record[] = [];
  for (const line of lines.slice(1)) {
    try {
      records.push(JSON.parse(line) as Record);
    } catch (e) {
      throw new ProtocolError(`chunk record is not JSON: ${String(e)}`);
    }
  }

  if (records.length !== header.record_count) {
    throw new ProtocolError(
      `chunk ${header.seq} declares ${header.record_count} records but carries ${records.length}`,
    );
  }

  const chunk: Chunk = { ...header, records };
  validateChunk(chunk);
  return chunk;
}

/** Total payload length, in bytes of decoded output. */
export function chunkByteLength(chunk: Chunk): number {
  let total = 0;
  for (const r of chunk.records) total += base64ByteLength(r.bytes);
  return total;
}

/**
 * Check the envelope describes itself consistently.
 *
 * Cheap enough to run on every chunk, and skipping it would be false economy: a
 * chunk whose declared range disagrees with its payload silently corrupts every
 * downstream offset, including every mention anchor past it.
 */
export function validateChunk(chunk: Chunk): void {
  if (chunk.byte_end < chunk.byte_start) {
    throw new ProtocolError(
      `chunk ${chunk.seq} has byte_end ${chunk.byte_end} before byte_start ${chunk.byte_start}`,
    );
  }
  const declared = chunk.byte_end - chunk.byte_start;
  const actual = chunkByteLength(chunk);
  if (declared !== actual) {
    throw new ProtocolError(`chunk ${chunk.seq} declares ${declared} bytes but carries ${actual}`);
  }
  if (chunk.ended_at_ms < chunk.started_at_ms) {
    throw new ProtocolError(`chunk ${chunk.seq} ends before it starts`);
  }
}

/** Decoded length of a base64 string, without decoding it. */
function base64ByteLength(b64: string): number {
  const padding = b64.endsWith("==") ? 2 : b64.endsWith("=") ? 1 : 0;
  return (b64.length / 4) * 3 - padding;
}

/** Where a chunk lives in R2. Must match `Chunk::object_key()` in Rust. */
export function chunkKey(sessionId: string, seq: number): string {
  return `transcripts/${sessionId}/${seq}.jsonl`;
}

export function manifestKey(sessionId: string): string {
  return `transcripts/${sessionId}/manifest.json`;
}

// ---------------------------------------------------------------------------
// Relay messages
// ---------------------------------------------------------------------------

/**
 * An ephemeral frame, published the instant bytes leave the PTY.
 *
 * Deliberately not durable and deliberately not a chunk. Waiting for a chunk to
 * close would put the chunker's flush interval (~1s) into the perceived latency,
 * which is the entire thing the relay exists to avoid.
 */
export interface FrameMessage {
  t: "frame";
  byte_start: number;
  byte_end: number;
  /** Wall clock at the PTY, for latency accounting. */
  at_ms: number;
  b64: string;
}

/**
 * The durable cursor moved: chunks below `chunk_cursor` are in R2.
 *
 * Sent to viewers so they can confirm that relayed bytes are now durable, and so
 * a viewer that missed frames knows there is something to backfill.
 */
export interface CursorMessage {
  t: "cursor";
  chunk_cursor: number;
  byte_cursor: number;
}

/**
 * The relay's opening message, so a joining viewer knows where the stream is
 * before any frame arrives. Without it a viewer cannot tell "nothing has happened
 * yet" from "I joined mid-stream and must backfill".
 */
export interface HelloMessage {
  t: "hello";
  session_id: string;
  chunk_cursor: number;
  byte_cursor: number;
  /** Bytes the publisher has sent, which may be ahead of `byte_cursor`. */
  live_byte_end: number;
}

/** The relay refuses to paper over a gap; it says so instead. */
export interface StallMessage {
  t: "stall";
  reason: string;
  expected_byte_start: number;
  got_byte_start: number;
}

export type RelayMessage = FrameMessage | CursorMessage | HelloMessage | StallMessage;
