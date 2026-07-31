/**
 * The herd view: cards collapsed into one ordered list, and that list as text.
 *
 * Both halves are pure functions of `(world, me, now)`. Ordering *is* the product
 * here — a presence view that buries the one blocked session under nine idle ones
 * has failed at the only thing it exists to do — so the rule is a tested function
 * rather than a property of whatever loop draws the screen.
 */

import type { Card, World } from "./model.js";
import { formatAge, statusGlyph, statusLabel, statusRank, truncate, wantsAHuman } from "./model.js";

/** One line of the herd. */
export interface Row {
  /** 1-based selector the user types. */
  index: number;
  card: Card;
  isMe: boolean;
  /** The name to show. */
  who: string;
  /** Repo and branch, or the pane id as a last resort. */
  location: string;
  /** Heartbeat is late. Shown anyway, and sorted below everything fresh. */
  stale: boolean;
  /** Whether this row prints the owner's help note. Exactly one row per person. */
  showHelp: boolean;
  /** Unread comments addressed at this session. */
  unread: number;
}

export interface RosterOptions {
  me: string;
  now: number;
  /** Heartbeat age past which a card is marked stale. */
  staleAfterMs: number;
  /** Heartbeat age past which a card is dropped entirely. */
  forgetAfterMs: number;
}

/**
 * Collapse a world into the ordered herd.
 *
 * A card quiet for longer than `forgetAfterMs` disappears; one quiet for longer
 * than `staleAfterMs` is kept but marked, and sorts below everything fresh
 * regardless of status. A stale `blocked` row is a claim about the past and must
 * not outrank a live one.
 */
export function rows(world: World, options: RosterOptions): Row[] {
  const { me, now, staleAfterMs, forgetAfterMs } = options;

  const unreadBySession = new Map<string, number>();
  for (const comment of world.comments) {
    if (comment.readAt === undefined) {
      unreadBySession.set(comment.sessionId, (unreadBySession.get(comment.sessionId) ?? 0) + 1);
    }
  }

  const out: Row[] = [];
  for (const card of world.cards) {
    const age = now - card.heartbeatAt;
    if (age > forgetAfterMs) {
      continue;
    }
    // `share = off` should never have been published; a row is cheap to drop and
    // a leak is not.
    if (card.share === "off") {
      continue;
    }
    out.push({
      index: 0,
      card,
      isMe: card.login === me,
      who: card.displayName ?? card.login,
      location: locationOf(card),
      stale: age > staleAfterMs,
      showHelp: false,
      unread: unreadBySession.get(card.sessionId) ?? 0,
    });
  }

  out.sort(compareRows());

  const noteShown = new Set<string>();
  out.forEach((row, i) => {
    row.index = i + 1;
    if (row.card.help !== undefined && !noteShown.has(row.card.login)) {
      noteShown.add(row.card.login);
      row.showHelp = true;
    }
  });
  return out;
}

/**
 * The ordering rule, in one place.
 *
 * The last comparison is the one people notice: among equally urgent rows the
 * *oldest wait* comes first. Without it the list reshuffles whenever anyone's
 * status changes, and a queue that reorders under you is one you stop reading.
 */
function compareRows(): (a: Row, b: Row) => number {
  return (a, b) =>
    Number(a.stale) - Number(b.stale) ||
    Number(asks(b)) - Number(asks(a)) ||
    statusRank(a.card.status) - statusRank(b.card.status) ||
    // Ascending `statusSince` is descending wait, so the longest wait leads.
    a.card.statusSince - b.card.statusSince ||
    a.who.localeCompare(b.who) ||
    a.card.sessionId.localeCompare(b.card.sessionId);
}

/** Whether a row is asking for a person. */
export function asks(row: Row): boolean {
  return row.card.help !== undefined || wantsAHuman(row.card.status);
}

/** Whether a teammate can open this session and see it live. */
export function isWatchable(row: Row): boolean {
  return row.card.share === "live";
}

function locationOf(card: Card): string {
  if (card.repo !== undefined && card.branch !== undefined) return `${card.repo}@${card.branch}`;
  if (card.repo !== undefined) return card.repo;
  if (card.branch !== undefined) return card.branch;
  return card.paneId;
}

/**
 * Render the herd as text.
 *
 * Deliberately narrow — 80 columns of useful text — because it is displayed in an
 * overlay pane over whatever the user was doing.
 */
export function render(list: Row[], footer: string, now: number): string {
  const lines: string[] = [
    "  #  who          agent   state     for   what",
    "  ─  ───          ─────   ─────     ───   ────",
  ];
  if (list.length === 0) {
    lines.push("     (nobody is publishing yet)");
  }
  for (const row of list) {
    const state = `${statusGlyph(row.card.status)}${statusLabel(row.card.status)}`;
    // The gaps are part of the layout, not decoration: a single space after the
    // padded who/agent/state fields and two around the index and the headline, so
    // every column starts where its header says it does. Joining every field with
    // the same separator drifts the data one column right of `agent`, two of
    // `state`, three of `for` — which is what this did before, and what the golden
    // in roster.test.ts now holds still.
    lines.push(
      `${String(row.index).padStart(3)}  ${truncate(row.who, 12).padEnd(12)} ` +
        `${truncate(row.card.agent, 7).padEnd(7)} ${state.padEnd(9)} ` +
        `${formatAge(now - row.card.statusSince).padStart(4)}  ` +
        `${truncate(headlineOf(row), 40)}${marks(row)}`,
    );
    if (row.showHelp && row.card.help !== undefined) {
      lines.push(`       ↳ help: ${truncate(row.card.help.note, 60)}`);
    }
  }
  return `${lines.join("\n")}\n\n${footer}`;
}

function headlineOf(row: Row): string {
  return row.card.headline.length > 0 ? row.card.headline : row.location;
}

/** Trailing markers: mine, sharing, watchers, mail, staleness. */
function marks(row: Row): string {
  let out = "";
  if (row.isMe) out += " (you)";
  if (row.card.share === "live") out += " [live]";
  if (row.card.watchers.length > 0) out += ` 👀${row.card.watchers.length}`;
  // The hand is up, but the note is printed against a different row of theirs.
  if (row.card.help !== undefined && !row.showHelp) out += " [asked]";
  if (row.unread > 0) out += ` ✉${row.unread}`;
  if (row.stale) out += " (stale)";
  return out;
}
