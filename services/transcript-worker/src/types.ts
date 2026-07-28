import type { SessionRelay } from "./relay";

export interface Env {
  /** One instance per session, keyed by session id. */
  SESSION_RELAY: DurableObjectNamespace<SessionRelay>;

  /** Durable transcript chunks: `transcripts/{session_id}/{seq}.jsonl`. */
  TRANSCRIPTS: R2Bucket;

  /**
   * Shared secret the app presents to publish.
   *
   * Spike-grade on purpose — the plan says "no auth beyond a hardcoded token" for
   * Spike B. Phase 1 replaces this with a per-session token minted at
   * `register_session` time, so a leaked token cannot publish to someone else's
   * session.
   */
  PUBLISH_TOKEN: string;

  /**
   * Shared secret a viewer presents to read.
   *
   * Also spike-grade. Phase 1 must check the *hub* for the session's visibility on
   * every read instead, because a token cannot express "sharing was just turned
   * off" — see the note in README.md.
   */
  VIEW_TOKEN: string;

  HUB_URL: string;
  HUB_DB: string;
  /** Bearer token for the identity registered as `hub_config.worker_identity`. */
  HUB_TOKEN: string;
}
