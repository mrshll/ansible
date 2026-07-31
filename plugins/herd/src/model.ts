/**
 * What crosses the hub, and the vocabulary the rest of the plugin shares.
 *
 * These types mirror the SpacetimeDB schema in `services/hub` rather than
 * inventing a parallel model: a {@link Card} is a `session_listing` joined to a
 * `session`, {@link Watcher} is a `presence` row, and {@link Share} is
 * `visibility` under a name a human would use. Where they differ from the wire
 * form it is only in flattening `{ tag: "Working" }` to `"Working"`, because
 * tagged unions are the database's ABI and a nuisance everywhere else.
 */

/** Herdr's semantic agent state, as it reports it. */
export type HerdrStatus = "idle" | "working" | "blocked" | "done" | "unknown";

/** The hub's status vocabulary — `SessionStatus` in the schema. */
export type Status =
  | "Starting"
  | "Working"
  | "AwaitingInput"
  | "AwaitingApproval"
  | "Done"
  | "Failed"
  | "Detached"
  | "Unknown";

/**
 * Map Herdr's five states onto the hub's eight.
 *
 * `blocked → AwaitingApproval` is the load-bearing line. Herdr marks `blocked`
 * only when the live bottom-buffer snapshot matches known approval, question, or
 * permission UI — the same evidence class `docs/spikes/approval-producer.md`
 * needed six co-occurring signals to establish, now maintained upstream for
 * nineteen agents. That is the whole argument for hosting on Herdr.
 *
 * `unknown → Unknown` rather than to a lifecycle position we have no evidence
 * for. Herdr says "there is an agent here and no rule matched"; claiming
 * `Starting` would be a guess and claiming `AwaitingInput` would summon a human.
 */
export function statusFromHerdr(status: HerdrStatus): Status {
  switch (status) {
    case "blocked":
      return "AwaitingApproval";
    case "working":
      return "Working";
    case "idle":
      return "AwaitingInput";
    case "done":
      return "Done";
    case "unknown":
      return "Unknown";
  }
}

/** Whether this state means a human is being waited on. */
export function wantsAHuman(status: Status): boolean {
  return status === "AwaitingApproval" || status === "Done";
}

/** Single-width marker for the roster's status column. */
export function statusGlyph(status: Status): string {
  switch (status) {
    case "AwaitingApproval":
      return "!";
    case "Done":
      return "+";
    case "Working":
      return ">";
    case "AwaitingInput":
      return ".";
    case "Starting":
      return "~";
    case "Failed":
      return "x";
    case "Detached":
      return "-";
    case "Unknown":
      return "?";
  }
}

/**
 * The short word a human reads.
 *
 * The schema's names are for the schema: `AwaitingApproval` is precise and eight
 * characters too wide for a roster column. These are the words people use.
 */
export function statusLabel(status: Status): string {
  switch (status) {
    case "AwaitingApproval":
      return "blocked";
    case "AwaitingInput":
      return "idle";
    case "Working":
      return "working";
    case "Done":
      return "done";
    case "Starting":
      return "starting";
    case "Failed":
      return "failed";
    case "Detached":
      return "gone";
    case "Unknown":
      return "unknown";
  }
}

/** Attention order: what a roster must put first. Lower sorts earlier. */
export function statusRank(status: Status): number {
  switch (status) {
    case "AwaitingApproval":
      return 0;
    case "Done":
      return 1;
    case "Working":
      return 2;
    case "Starting":
      return 3;
    case "AwaitingInput":
      return 4;
    case "Failed":
      return 5;
    case "Unknown":
      return 6;
    case "Detached":
      return 7;
  }
}

/**
 * How much of a session its owner is publishing.
 *
 * These are the three states of `visibility` under the names the CLI uses. `off`
 * is the absence of a listing, `title` is `Private`, `live` is `Org` — so "a
 * teammate may watch this" and "a teammate may read the transcript" are one
 * decision rather than two that can disagree.
 */
export type Share = "off" | "title" | "live";

export type Visibility = "Org" | "Private" | "Granted";

export function visibilityOf(share: Share): Visibility {
  return share === "live" ? "Org" : "Private";
}

export function shareOf(visibility: Visibility): Share {
  return visibility === "Org" ? "live" : "title";
}

export function parseShare(raw: string): Share | undefined {
  return raw === "off" || raw === "title" || raw === "live" ? raw : undefined;
}

/** One agent session, as the herd sees it. */
export interface Card {
  sessionId: string;
  /** GitHub login of the owner. */
  login: string;
  /** Friendlier name when the member row has one. */
  displayName?: string;
  host: string;
  paneId: string;
  agent: string;
  status: Status;
  statusDetail: string;
  /** What this session is working on, in one line. */
  headline: string;
  repo?: string;
  branch?: string;
  share: Share;
  /** Milliseconds since the epoch when the status last changed. */
  statusSince: number;
  /** Last heartbeat, so staleness is a property of the data and not a guess. */
  heartbeatAt: number;
  /** Logins watching this session right now. */
  watchers: string[];
  /** A raised hand belonging to this session's owner. */
  help?: HelpRequest;
  /** Highest chunk the relay holds, so a viewer joins at the current moment. */
  chunkCursor?: number;
}

export interface HelpRequest {
  login: string;
  note: string;
  since: number;
}

/** A `presence` row: who has what on screen. */
export interface Watcher {
  login: string;
  sessionId?: string;
  focus: "Grid" | "Session" | "Replay";
  since: number;
}

/** A comment addressed at a moment in a session. */
export interface Comment {
  id: number;
  sessionId: string;
  from: string;
  to: string;
  body: string;
  chunkSeq: number;
  byteOffset: number;
  createdAt: number;
  readAt?: number;
  /** `typed` or `submitted` once the recipient let it reach the agent. */
  appliedAs?: string;
}

/** Everything a roster needs, in one value. */
export interface World {
  cards: Card[];
  comments: Comment[];
  /** When this snapshot was taken. */
  at: number;
  /** Where it came from, for the roster's footer. */
  source: string;
}

/**
 * Herdr caps presentation strings at 80 characters and strips control
 * characters. Doing the same before publishing means the hub carries what will
 * actually be displayed.
 */
export const MAX_DISPLAY = 80;

/**
 * Collapse whitespace, drop control characters, and cap at `max` *characters*.
 *
 * Code points, not bytes and not UTF-16 units: slicing a string by `.length`
 * splits surrogate pairs, so an emoji in a terminal title would publish as half a
 * character.
 */
export function normalize(raw: string, max = MAX_DISPLAY): string {
  const collapsed = [...raw]
    .map((ch) => (isControl(ch) ? " " : ch))
    .join("")
    .split(/\s+/u)
    .filter((word) => word.length > 0);

  const out: string[] = [];
  let length = 0;
  for (const word of collapsed) {
    const chars = [...word];
    const cost = length === 0 ? chars.length : chars.length + 1;
    if (length + cost > max) {
      if (length === 0) {
        return chars.slice(0, max).join("");
      }
      break;
    }
    out.push(word);
    length += cost;
  }
  return out.join(" ");
}

function isControl(ch: string): boolean {
  const code = ch.codePointAt(0) ?? 0;
  return code < 0x20 || (code >= 0x7f && code <= 0x9f);
}

/** Truncate to `max` code points, marking the cut. */
export function truncate(text: string, max: number): string {
  const chars = [...text];
  if (chars.length <= max) {
    return text;
  }
  return chars.slice(0, Math.max(0, max - 1)).join("") + "…";
}

/** Compact duration: `12s`, `4m`, `2h`, `3d`. */
export function formatAge(ms: number): string {
  const seconds = Math.max(0, Math.floor(ms / 1000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h`;
  return `${Math.floor(hours / 24)}d`;
}
