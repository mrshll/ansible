/**
 * Redaction. Everything published goes through here first.
 *
 * This is a port of `crates/ansible-capture`'s redactor, and the rules are not
 * guesses. They were derived by recording a real session with twelve planted
 * credentials: vendor-prefix rules alone caught **4 of 12**, so named values, URL
 * credentials, JWTs, and PEM blocks became rules too, taking coverage to 12 of 12
 * (`docs/spikes/capture-round-trip.md`). Deleting a rule here re-opens a hole
 * somebody already found.
 *
 * Two properties the Rust version has that this one keeps:
 *
 * 1. **Redaction happens before a byte can reach a chunk.** Offsets index the
 *    redacted stream, because that is the only stream that gets stored or watched.
 * 2. **It works across write boundaries.** A PTY splits writes wherever it likes,
 *    so a secret can straddle two of them. {@link Redactor} holds back a bounded
 *    tail until either a rule resolves or {@link Redactor.finish} releases it.
 *
 * And one it does not need: the Rust version is a byte-level state machine because
 * it must not assume valid UTF-8 mid-stream. This works on Latin-1-decoded strings
 * and re-encodes, which is lossless for arbitrary bytes and lets the rules be
 * ordinary regular expressions. Terminal output is frequently not valid UTF-8, so
 * decoding as UTF-8 would corrupt it; Latin-1 is a total function on bytes.
 */

/** A secret shape. */
export interface Rule {
  readonly name: string;
  readonly pattern: RegExp;
  /**
   * Capture group to keep verbatim, if any. Keeping `NAME=` or `scheme://user:`
   * is what lets a transcript still show *which* variable was set, at no cost to
   * safety.
   */
  readonly keep?: number;
}

/**
 * Identifier fragments that mark a value as secret-bearing.
 *
 * The half of the ruleset that recording added. `token`/`secret`/`password` catch
 * the credentials that have no vendor prefix at all — which was two thirds of the
 * planted set.
 */
const SECRET_NEEDLES = [
  "token",
  "secret",
  "password",
  "passwd",
  "apikey",
  "api_key",
  "credential",
  "private_key",
  "session_key",
  "access_key",
];

const NEEDLE_ALTERNATION = SECRET_NEEDLES.join("|");

/**
 * The rules applied to published text and terminal output.
 *
 * Order matters: the longest, most specific shapes go first so a JWT is not
 * partially eaten by a token rule.
 */
export const RULES: readonly Rule[] = [
  {
    name: "private-key",
    pattern: /-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----/gu,
  },
  { name: "jwt", pattern: /\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{4,}/gu },
  { name: "anthropic-key", pattern: /\bsk-ant-[A-Za-z0-9_-]{16,}/gu },
  { name: "openai-key", pattern: /\bsk-proj-[A-Za-z0-9_-]{16,}/gu },
  { name: "stripe-live", pattern: /\bsk_live_[A-Za-z0-9_-]{16,}/gu },
  { name: "github-fine-grained", pattern: /\bgithub_pat_[A-Za-z0-9_-]{20,}/gu },
  { name: "github-pat", pattern: /\bghp_[A-Za-z0-9_-]{20,}/gu },
  { name: "github-oauth", pattern: /\bgho_[A-Za-z0-9_-]{20,}/gu },
  { name: "github-server", pattern: /\bghs_[A-Za-z0-9_-]{20,}/gu },
  { name: "aws-access-key", pattern: /\bAKIA[A-Za-z0-9]{16,}/gu },
  { name: "aws-session-key", pattern: /\bASIA[A-Za-z0-9]{16,}/gu },
  { name: "slack-token", pattern: /\bxoxb-[A-Za-z0-9-]{16,}/gu },
  { name: "slack-app-token", pattern: /\bxapp-[A-Za-z0-9-]{16,}/gu },
  { name: "google-api-key", pattern: /\bAIza[A-Za-z0-9_-]{20,}/gu },
  { name: "npm-token", pattern: /\bnpm_[A-Za-z0-9]{20,}/gu },
  { name: "spacetime-token", pattern: /\bstdb_[A-Za-z0-9_-]{20,}/gu },
  // `scheme://user:` is kept; only the password goes. The `@` is a lookahead
  // rather than part of the match, so replacing the password does not eat the
  // host along with it.
  {
    name: "url-password",
    pattern: /([a-zA-Z][a-zA-Z0-9+.-]*:\/\/[^\s:/@]+:)([^\s@/]+)(?=@)/gu,
    keep: 1,
  },
  // `NAME=value` / `NAME: value`. The name is kept.
  {
    name: "named-value",
    pattern: new RegExp(
      String.raw`([A-Za-z0-9_.\-]*(?:${NEEDLE_ALTERNATION})[A-Za-z0-9_.\-]*\s*[=:]\s*)("[^"\n]{4,}"|'[^'\n]{4,}'|[^\s"'\n]{4,})`,
      "giu",
    ),
    keep: 1,
  },
];

/** What replaces a secret. Names the rule, so a reader knows what was removed. */
function marker(name: string): string {
  return `[redacted:${name}]`;
}

/** Ruleset version, so a stored chunk records what scanned it. */
export const RULESET_VERSION = 2;

/**
 * How many characters may be held back waiting for a rule to resolve.
 *
 * Bounds memory and latency: a live viewer's floor is this much unflushed text in
 * the worst case. A PEM block is longer than this, which is why its rule is
 * handled by holding from the BEGIN line rather than by lookahead.
 */
export const MAX_LOOKAHEAD = 512;

/** Redact a complete string. Use this for headlines, comments, and titles. */
export function redact(text: string): { text: string; hits: number } {
  let hits = 0;
  const count = (): void => {
    hits += 1;
  };
  const out = RULES.reduce((acc, rule) => applyRule(acc, rule, count), text);
  return { text: out, hits };
}

/** Apply one rule, keeping its `keep` group verbatim when it has one. */
function applyRule(text: string, rule: Rule, onHit: () => void): string {
  return text.replace(rule.pattern, (...args: unknown[]) => {
    onHit();
    if (rule.keep === undefined) {
      return marker(rule.name);
    }
    const kept = args[rule.keep];
    return `${typeof kept === "string" ? kept : ""}${marker(rule.name)}`;
  });
}

/**
 * Streaming redactor for terminal output.
 *
 * Feed with {@link push} and finish with {@link finish}. The held-back tail is
 * only released by `finish`, so a caller must call it before treating the stream
 * as complete — otherwise the last few hundred bytes are lost and a byte-exact
 * replay is not byte-exact.
 */
export class Redactor {
  #pending = "";
  #hits = 0;
  #inPem = false;

  /** How many secrets this redactor has caught. */
  get hits(): number {
    return this.#hits;
  }

  /**
   * Feed bytes observed now; returns the bytes that are safe to publish.
   *
   * Latin-1 in and Latin-1 out, so arbitrary binary survives: terminal output is
   * frequently not valid UTF-8 and must not be repaired into something else.
   */
  push(bytes: Uint8Array): Uint8Array {
    this.#pending += latin1Decode(bytes);
    return latin1Encode(this.#drain(false));
  }

  /** Release the held-back tail. */
  finish(): Uint8Array {
    const rest = this.#drain(true);
    this.#pending = "";
    return latin1Encode(rest);
  }

  /**
   * Emit everything that cannot still become part of a secret.
   *
   * The last {@link MAX_LOOKAHEAD} characters are held back unless flushing,
   * because a rule that matches there might match differently once more input
   * arrives. Inside a PEM block everything is held until the END line, since the
   * block is longer than any lookahead window.
   */
  #drain(flush: boolean): string {
    if (this.#inPem) {
      const end = this.#pending.search(/-----END [A-Z ]*PRIVATE KEY-----/u);
      if (end === -1) {
        if (!flush) {
          return "";
        }
        this.#pending = "";
        return "";
      }
      const endLine = /-----END [A-Z ]*PRIVATE KEY-----/u.exec(this.#pending);
      const after = end + (endLine?.[0].length ?? 0);
      this.#pending = this.#pending.slice(after);
      this.#inPem = false;
      this.#hits += 1;
      return marker("private-key") + this.#drain(flush);
    }

    const begin = this.#pending.search(/-----BEGIN [A-Z ]*PRIVATE KEY-----/u);
    if (begin !== -1) {
      const head = this.#pending.slice(0, begin);
      this.#pending = this.#pending.slice(begin);
      this.#inPem = true;
      const { text, hits } = redact(head);
      this.#hits += hits;
      return text + this.#drain(flush);
    }

    const holdFrom = flush
      ? this.#pending.length
      : Math.max(0, this.#pending.length - MAX_LOOKAHEAD);
    // Never split mid-line when holding back: a rule anchored on a line boundary
    // would otherwise see a partial line and decide wrongly.
    const cut = flush ? this.#pending.length : lastBoundary(this.#pending, holdFrom);
    const ready = this.#pending.slice(0, cut);
    this.#pending = this.#pending.slice(cut);
    const { text, hits } = redact(ready);
    this.#hits += hits;
    return text;
  }
}

/** The last newline at or before `limit`, or `limit` when there is none. */
function lastBoundary(text: string, limit: number): number {
  if (limit <= 0) return 0;
  const newline = text.lastIndexOf("\n", limit - 1);
  return newline === -1 ? 0 : newline + 1;
}

/** Bytes to a string, one code unit per byte. Total, and reversible. */
export function latin1Decode(bytes: Uint8Array): string {
  let out = "";
  // Chunked to stay well clear of the argument-count limit on large frames.
  for (let i = 0; i < bytes.length; i += 8192) {
    // `fromCharCode`, not `fromCodePoint`: one UTF-16 code *unit* per byte is
    // exactly the mapping that makes this reversible for all 256 values.
    // oxlint-disable-next-line unicorn/prefer-code-point
    out += String.fromCharCode(...bytes.subarray(i, i + 8192));
  }
  return out;
}

/** The inverse of {@link latin1Decode}. */
export function latin1Encode(text: string): Uint8Array {
  const out = new Uint8Array(text.length);
  for (let i = 0; i < text.length; i += 1) {
    // oxlint-disable-next-line unicorn/prefer-code-point
    out[i] = text.charCodeAt(i) & 0xff;
  }
  return out;
}
