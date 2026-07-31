/**
 * The transcript Worker: ingest, relay, archive.
 *
 * Routes, all under one session id:
 *
 *   POST /v1/session/:id/chunk      publish a durable chunk (JSONL body)
 *   POST /v1/session/:id/frame      publish an ephemeral relay frame
 *   GET  /v1/session/:id/relay      viewer WebSocket
 *   GET  /v1/session/:id/chunk/:seq read one chunk back from the archive
 *   POST /v1/session/:id/finalize   write the manifest
 *   GET  /v1/session/:id/status     relay + cursor state, for the harness
 *
 * The Worker itself is a router and an authorization boundary; all ordering state
 * lives in the per-session Durable Object, because ordering is exactly the thing
 * that cannot be decided by whichever isolate happened to receive the request.
 */

import { chunkKey, manifestKey, type FrameMessage } from "./protocol";
import { SessionRelay } from "./relay";
import type { Env } from "./types";

export { SessionRelay };

function unauthorized(): Response {
  return new Response("unauthorized\n", { status: 401 });
}

/**
 * Constant-time-ish bearer check.
 *
 * The comparison below is not constant time, and for a spike shared secret that is
 * acceptable; it is called out because it must not survive into Phase 1 unnoticed.
 */
function bearer(request: Request): string | null {
  const header = request.headers.get("Authorization");
  if (!header?.startsWith("Bearer ")) return null;
  return header.slice("Bearer ".length);
}

function relayFor(env: Env, sessionId: string) {
  // Deterministic routing: the same session id always reaches the same DO, which
  // is what lets ordering be checked at all.
  return env.SESSION_RELAY.getByName(sessionId);
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    const parts = url.pathname.split("/").filter(Boolean);

    // /v1/session/:id/...
    if (parts.length < 4 || parts[0] !== "v1" || parts[1] !== "session") {
      return new Response("not found\n", { status: 404 });
    }
    const sessionId = decodeURIComponent(parts[2]);
    const action = parts[3];
    const token = bearer(request);

    switch (`${request.method} ${action}`) {
      case "POST chunk": {
        if (token !== env.PUBLISH_TOKEN) return unauthorized();
        const body = await request.text();
        const result = await relayFor(env, sessionId).publishChunk(body);
        if (!result.ok) {
          return new Response(`${result.error}\n`, { status: result.status ?? 400 });
        }
        return new Response(null, { status: 204 });
      }

      case "POST frame": {
        if (token !== env.PUBLISH_TOKEN) return unauthorized();
        let frame: FrameMessage;
        try {
          frame = (await request.json()) as FrameMessage;
        } catch (e) {
          return new Response(`frame is not JSON: ${String(e)}\n`, { status: 400 });
        }
        const result = await relayFor(env, sessionId).publishFrame(frame);
        if (!result.ok) return new Response(`${result.error}\n`, { status: 409 });
        return new Response(null, { status: 204 });
      }

      case "GET relay": {
        // A browser WebSocket cannot set headers, so the token rides in the query
        // string here. That is a real weakness (it lands in logs), and Phase 1
        // should use a short-lived ticket fetched over HTTP first.
        const viewToken = url.searchParams.get("token") ?? token;
        if (viewToken !== env.VIEW_TOKEN) return unauthorized();
        // `fetch` rather than an RPC call, because a WebSocket cannot be returned
        // across the RPC boundary. See the note on `SessionRelay.fetch`.
        return relayFor(env, sessionId).fetch(request);
      }

      case "GET chunk": {
        if (token !== env.VIEW_TOKEN) return unauthorized();
        const seq = Number(parts[4]);
        if (!Number.isInteger(seq) || seq < 0) {
          return new Response("chunk sequence must be a non-negative integer\n", { status: 400 });
        }
        const object = await env.TRANSCRIPTS.get(chunkKey(sessionId, seq));
        if (!object) return new Response("no such chunk\n", { status: 404 });
        return new Response(object.body, {
          headers: { "Content-Type": "application/jsonl" },
        });
      }

      case "GET manifest": {
        if (token !== env.VIEW_TOKEN) return unauthorized();
        const object = await env.TRANSCRIPTS.get(manifestKey(sessionId));
        if (!object) return new Response("not finalized\n", { status: 404 });
        return new Response(object.body, { headers: { "Content-Type": "application/json" } });
      }

      case "POST finalize": {
        if (token !== env.PUBLISH_TOKEN) return unauthorized();
        const redactionVersion = Number(url.searchParams.get("redaction_version") ?? "2");
        return relayFor(env, sessionId).finalize(redactionVersion);
      }

      case "GET status": {
        if (token !== env.VIEW_TOKEN && token !== env.PUBLISH_TOKEN) return unauthorized();
        return relayFor(env, sessionId).status();
      }

      default:
        return new Response("not found\n", { status: 404 });
    }
  },
} satisfies ExportedHandler<Env>;
