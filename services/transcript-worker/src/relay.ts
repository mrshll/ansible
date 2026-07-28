/**
 * `SessionRelay` — one Durable Object per session.
 *
 * A session is the coordination atom here, which is why the DO is keyed by
 * session id: every viewer of one session must see the same ordered byte stream,
 * and no viewer of another session shares any state with them. A single global DO
 * would serialize every session in the org through one object.
 *
 * It owns three jobs that have to agree with each other:
 *
 * 1. **Fan out frames** to viewers as they arrive, for the real-time feel.
 * 2. **Persist chunks** to R2 and advance the hub's cursor, so the archive is
 *    authoritative and backfill is possible.
 * 3. **Refuse to hide a gap.** Order is the one invariant. A frame or chunk that
 *    does not continue the stream is rejected and the viewers are told the stream
 *    stalled, because a visible stall is strictly better than a silent gap.
 *
 * The ordering state lives in SQLite, not memory. A DO can be evicted between two
 * frames; if `expected_byte` were in memory only, the next frame after an eviction
 * would look like a gap (or worse, be accepted at the wrong offset).
 */

import { DurableObject } from "cloudflare:workers";

import {
  type Chunk,
  type CursorMessage,
  type FrameMessage,
  type HelloMessage,
  ProtocolError,
  type StallMessage,
  chunkByteLength,
  chunkKey,
  manifestKey,
  parseChunkJsonl,
} from "./protocol";
import { advanceTranscriptCursor } from "./hub";
import type { Env } from "./types";

/** Ordering state, mirrored in SQLite so it survives eviction. */
interface RelayState {
  sessionId: string;
  /** Next chunk sequence the archive does not yet hold. */
  chunkCursor: number;
  /** Byte offset matching `chunkCursor` — durable in R2. */
  byteCursor: number;
  /** Byte offset the publisher has reached, which runs ahead of `byteCursor`. */
  liveByteEnd: number;
  chunksWritten: number;
}

export class SessionRelay extends DurableObject<Env> {
  private state: RelayState;

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    // `sql.exec` is synchronous, so the schema is set up inline rather than inside
    // `blockConcurrencyWhile`. That is not a shortcut — it is the only correct
    // order here. `blockConcurrencyWhile` takes an async callback that the
    // constructor cannot await, so `load()` below would run before the table
    // existed and every request would fail with "no such table".
    this.ctx.storage.sql.exec(`
      CREATE TABLE IF NOT EXISTS relay_state (
        id INTEGER PRIMARY KEY,
        session_id TEXT NOT NULL DEFAULT '',
        chunk_cursor INTEGER NOT NULL DEFAULT 0,
        byte_cursor INTEGER NOT NULL DEFAULT 0,
        live_byte_end INTEGER NOT NULL DEFAULT 0,
        chunks_written INTEGER NOT NULL DEFAULT 0
      )
    `);
    this.ctx.storage.sql.exec("INSERT OR IGNORE INTO relay_state (id) VALUES (0)");
    this.state = this.load();
  }

  private load(): RelayState {
    const row = this.ctx.storage.sql
      .exec<{
        session_id: string;
        chunk_cursor: number;
        byte_cursor: number;
        live_byte_end: number;
        chunks_written: number;
      }>("SELECT * FROM relay_state WHERE id = 0")
      .one();
    return {
      sessionId: row.session_id,
      chunkCursor: row.chunk_cursor,
      byteCursor: row.byte_cursor,
      liveByteEnd: row.live_byte_end,
      chunksWritten: row.chunks_written,
    };
  }

  /** Persist first, then cache — an eviction after this point loses nothing. */
  private save(): void {
    this.ctx.storage.sql.exec(
      `UPDATE relay_state SET session_id = ?, chunk_cursor = ?, byte_cursor = ?,
         live_byte_end = ?, chunks_written = ? WHERE id = 0`,
      this.state.sessionId,
      this.state.chunkCursor,
      this.state.byteCursor,
      this.state.liveByteEnd,
      this.state.chunksWritten,
    );
  }

  // -------------------------------------------------------------------------
  // Viewers
  // -------------------------------------------------------------------------

  /**
   * Attach a viewer socket.
   *
   * This is a `fetch` handler rather than an RPC method, and it has to be: a
   * `WebSocket` cannot cross the RPC boundary, so returning one from an RPC method
   * fails with "Web Socket request did not return status 101". Every other entry
   * point here is RPC, which is the better interface — this one is the exception
   * the platform forces.
   *
   * Uses the hibernation API (`acceptWebSocket`) rather than holding the socket in
   * memory, so a session that streams for hours with idle viewers does not keep
   * the DO resident between frames.
   */
  override async fetch(request: Request): Promise<Response> {
    if (request.headers.get("Upgrade") !== "websocket") {
      return new Response("expected a websocket upgrade\n", { status: 426 });
    }

    // The session id is in the path; the Worker already routed us by it, so this
    // only records it for the state row.
    const parts = new URL(request.url).pathname.split("/").filter(Boolean);
    const sessionId = decodeURIComponent(parts[2] ?? "");
    if (this.state.sessionId === "") {
      this.state.sessionId = sessionId;
      this.save();
    }

    const pair = new WebSocketPair();
    this.ctx.acceptWebSocket(pair[1]);

    // Tell the viewer where the stream is before any frame arrives, so it can
    // distinguish "nothing yet" from "joined mid-stream, must backfill".
    const hello: HelloMessage = {
      t: "hello",
      session_id: sessionId,
      chunk_cursor: this.state.chunkCursor,
      byte_cursor: this.state.byteCursor,
      live_byte_end: this.state.liveByteEnd,
    };
    pair[1].send(JSON.stringify(hello));

    return new Response(null, { status: 101, webSocket: pair[0] });
  }

  /**
   * Viewers are read-only. The plan's scope is a read-only transcript, and input
   * handoff would bring input authority, conflict, and audit with it (open
   * question #9), so an inbound message is a protocol error rather than something
   * to interpret.
   */
  async webSocketMessage(ws: WebSocket, _message: string | ArrayBuffer): Promise<void> {
    ws.close(1003, "viewers are read-only");
  }

  private broadcast(message: FrameMessage | CursorMessage | StallMessage): void {
    const text = JSON.stringify(message);
    for (const ws of this.ctx.getWebSockets()) {
      try {
        ws.send(text);
      } catch {
        // A socket that has gone away must not stop the others from being served,
        // and must not fail the publisher's request either.
      }
    }
  }

  // -------------------------------------------------------------------------
  // Publishing
  // -------------------------------------------------------------------------

  /**
   * Publish an ephemeral frame: fan out immediately, persist nothing.
   *
   * Frames are checked for byte contiguity but are *not* the durability path. A
   * rejected frame is reported as a stall and the publisher is expected to keep
   * its spool and let the chunk path catch up.
   */
  async publishFrame(frame: FrameMessage): Promise<{ ok: boolean; error?: string }> {
    if (frame.byte_end < frame.byte_start) {
      return { ok: false, error: "frame byte range is inverted" };
    }

    // Frames may legitimately re-send bytes the viewer already has (after a
    // publisher reconnect), but they may never skip ahead: that would leave a gap
    // no later frame can fill.
    if (frame.byte_start > this.state.liveByteEnd) {
      const stall: StallMessage = {
        t: "stall",
        reason: "frame skipped ahead of the live stream",
        expected_byte_start: this.state.liveByteEnd,
        got_byte_start: frame.byte_start,
      };
      this.broadcast(stall);
      return { ok: false, error: stall.reason };
    }

    this.broadcast(frame);

    if (frame.byte_end > this.state.liveByteEnd) {
      this.state.liveByteEnd = frame.byte_end;
      this.save();
    }
    return { ok: true };
  }

  /**
   * Accept a durable chunk: validate, write to R2, then advance the hub cursor.
   *
   * The order of those three is the whole point. The cursor is what every viewer
   * follows, and it must mean "durably in R2" — so it is advanced only *after* the
   * R2 write returns. Advancing first would publish an offset that readers could
   * race to, and they would get a 404 from the archive.
   */
  async publishChunk(body: string): Promise<{ ok: boolean; error?: string; status?: number }> {
    let chunk: Chunk;
    try {
      chunk = parseChunkJsonl(body);
    } catch (e) {
      if (e instanceof ProtocolError) return { ok: false, error: e.message, status: 400 };
      throw e;
    }

    if (this.state.sessionId === "") {
      this.state.sessionId = chunk.session_id;
      this.save();
    } else if (chunk.session_id !== this.state.sessionId) {
      return {
        ok: false,
        status: 400,
        error: `chunk belongs to session ${chunk.session_id}, not ${this.state.sessionId}`,
      };
    }

    // Idempotent replay: a publisher that retried after a timeout may resend a
    // chunk that already landed. Accepting it as a no-op is correct; treating it
    // as an error would turn a successful retry into a stalled session.
    if (chunk.seq < this.state.chunkCursor) {
      return { ok: true };
    }

    // Anything other than the next chunk is a gap. Refusing is the invariant:
    // splicing over it would produce an archive that reassembles to the wrong
    // bytes, and no downstream rigor could detect that.
    if (chunk.seq !== this.state.chunkCursor) {
      return {
        ok: false,
        status: 409,
        error: `expected chunk ${this.state.chunkCursor} but got ${chunk.seq}`,
      };
    }
    if (chunk.byte_start !== this.state.byteCursor) {
      return {
        ok: false,
        status: 409,
        error:
          `chunk ${chunk.seq} starts at byte ${chunk.byte_start}, ` +
          `but the archive ends at ${this.state.byteCursor}`,
      };
    }

    await this.env.TRANSCRIPTS.put(chunkKey(chunk.session_id, chunk.seq), body, {
      httpMetadata: { contentType: "application/jsonl" },
    });

    this.state.chunkCursor = chunk.seq + 1;
    this.state.byteCursor = chunk.byte_end;
    this.state.chunksWritten += 1;
    if (chunk.byte_end > this.state.liveByteEnd) this.state.liveByteEnd = chunk.byte_end;
    this.save();

    // The hub call is deliberately after the state write and outside any
    // concurrency block. If it fails the bytes are still durable and the next
    // chunk re-publishes a later cursor, so a transient hub outage costs freshness
    // rather than correctness.
    const hubError = await advanceTranscriptCursor(this.env, {
      sessionId: chunk.session_id,
      chunkCursor: this.state.chunkCursor,
      byteCursor: this.state.byteCursor,
      eventCount: this.state.chunksWritten,
    });

    const cursor: CursorMessage = {
      t: "cursor",
      chunk_cursor: this.state.chunkCursor,
      byte_cursor: this.state.byteCursor,
    };
    this.broadcast(cursor);

    if (hubError) {
      // Reported, not thrown: the chunk *is* durable, and failing the publisher's
      // request would make it retry a write that already succeeded.
      console.error(`hub cursor advance failed for ${chunk.session_id}: ${hubError}`);
    }
    return { ok: true };
  }

  /**
   * Write the manifest that closes the transcript.
   *
   * Later replays read this first and never touch the hub beyond the session row,
   * which is what keeps replay independent of SpacetimeDB availability.
   */
  async finalize(redactionVersion: number): Promise<Response> {
    const manifest = {
      session_id: this.state.sessionId,
      chunk_count: this.state.chunkCursor,
      total_bytes: this.state.byteCursor,
      redaction_version: redactionVersion,
      finalized_at_ms: Date.now(),
    };
    await this.env.TRANSCRIPTS.put(manifestKey(this.state.sessionId), JSON.stringify(manifest), {
      httpMetadata: { contentType: "application/json" },
    });
    return Response.json(manifest);
  }

  async status(): Promise<Response> {
    return Response.json({
      session_id: this.state.sessionId,
      chunk_cursor: this.state.chunkCursor,
      byte_cursor: this.state.byteCursor,
      live_byte_end: this.state.liveByteEnd,
      chunks_written: this.state.chunksWritten,
      viewers: this.ctx.getWebSockets().length,
    });
  }
}
