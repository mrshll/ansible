/**
 * The Worker's one write to SpacetimeDB: advancing the transcript cursor.
 *
 * Why the Worker and not the app: the app knows what it *uploaded*, but only the
 * Worker knows what R2 *stored*. Keeping this call Worker-only is what makes
 * `session.chunk_cursor` mean "durably in R2" rather than "a client claimed so",
 * and the hub enforces it by comparing the caller against
 * `hub_config.worker_identity`. See `services/hub-module/src/reducers.rs`.
 */

import type { Env } from "./types";

export interface CursorAdvance {
  sessionId: string;
  chunkCursor: number;
  byteCursor: number;
  eventCount: number;
}

/**
 * Advance the cursor. Returns `null` on success, or a message describing the
 * failure.
 *
 * Deliberately returns rather than throws. A failed cursor advance is a
 * *freshness* problem, not a correctness one — the bytes are already in R2, and
 * the next chunk will publish a later cursor — so it must not fail the publisher's
 * request and provoke a retry of a write that already landed.
 */
export async function advanceTranscriptCursor(
  env: Env,
  advance: CursorAdvance,
): Promise<string | null> {
  if (!env.HUB_URL || !env.HUB_DB || !env.HUB_TOKEN) {
    return "hub is not configured (HUB_URL, HUB_DB, HUB_TOKEN)";
  }

  const url = `${env.HUB_URL}/v1/database/${env.HUB_DB}/call/advance_transcript_cursor`;
  let response: Response;
  try {
    response = await fetch(url, {
      method: "POST",
      headers: {
        Authorization: `Bearer ${env.HUB_TOKEN}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify([
        advance.sessionId,
        advance.chunkCursor,
        advance.byteCursor,
        advance.eventCount,
      ]),
    });
  } catch (e) {
    return `hub unreachable: ${String(e)}`;
  }

  if (!response.ok) {
    const detail = await response.text().catch(() => "");
    return `hub returned ${response.status}: ${detail.slice(0, 300)}`;
  }

  // The reducer rejects a non-advancing cursor with a 200 and an error in the
  // body, so the status alone is not proof it applied.
  const body = await response.text().catch(() => "");
  if (body.includes("must advance") || body.includes("must not regress")) {
    return `hub refused the cursor: ${body.slice(0, 300)}`;
  }
  return null;
}
