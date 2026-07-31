/**
 * The hub: tables and types for the multiplayer presence layer.
 *
 * This is a port of `services/hub-module` (Rust) to a TypeScript SpacetimeDB
 * module, and it is deliberately a *faithful* one: same tables, same columns,
 * same reducer names, same row-level security rules. `CASE_CONVERSION_POLICY`
 * defaults to `SnakeCase`, so `sessionStatusHistory` registers as
 * `session_status_history` and `sharedWithOrg` as `shared_with_org` — which
 * means this module is wire-compatible with the Rust one. The Worker's
 * `advance_transcript_cursor` call and every RLS string keep working unchanged,
 * and the migration is a schema-compatible republish rather than a new database.
 *
 * Three things are new, all of them because presence now comes from Herdr rather
 * than from our own hooks: {@link SessionStatus} gains `Unknown`,
 * {@link StatusSource} gains `Herdr`, and there is a `helpRequest` table. See
 * `docs/adr/0005-typescript-and-the-herdr-host.md`.
 *
 * Two things it turns out we did *not* need to add, because the original schema
 * already modelled them:
 *
 * - **Watchers** are `presence` rows with `focus = Session`. Keyed by connection,
 *   so `clientDisconnected` cleans them up — a closed window stops being a
 *   watcher without anything having to expire.
 * - **Sharing** is `visibility`. `off` is no listing at all, `title` is
 *   `Private`, `live` is `Org`. So "a teammate may watch my session" and "a
 *   teammate may read my transcript" are one authorization decision instead of
 *   two that could disagree.
 */

import { schema, t, table } from "spacetimedb/server";

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/**
 * Where a status came from.
 *
 * Recorded on every transition because the statuses have different and
 * non-interchangeable sources. In the Rust design four came from Claude Code
 * hooks while `AwaitingApproval` could only come from the terminal — measured in
 * `docs/spikes/hook-coverage.md`, where a denied tool proved byte-for-byte
 * indistinguishable from a slow one. `Herdr` is now the normal source for all of
 * them, and keeping the older variants means a row recorded by either producer
 * still says which one it was.
 */
export const StatusSource = t.enum("StatusSource", [
  "Hook",
  "Terminal",
  "Supervisor",
  "Reaper",
  "Herdr",
]);

/**
 * Detailed session status.
 *
 * `Idle` is deliberately absent: nothing can set it. Idle is `AwaitingInput`
 * plus elapsed time, which a viewer derives from `lastEventAt`. Carrying a
 * status no producer can set is worse than not having it.
 */
export const SessionStatus = t.enum("SessionStatus", [
  "Starting",
  "Working",
  "AwaitingInput",
  /**
   * The interruption a teammate can actually resolve, and the highest-value
   * thing the herd surfaces. This is what Herdr's `blocked` maps to: its screen
   * manifests match known approval, question, and permission UI, which is the
   * same evidence class `docs/spikes/approval-producer.md` needed six
   * co-occurring signals to establish.
   */
  "AwaitingApproval",
  "Done",
  "Failed",
  /** Heartbeat lapsed with no live agent connection. */
  "Detached",
  /**
   * An agent is present and its state is not classified.
   *
   * New in this port, and it earns its place: Herdr reports `unknown` when it
   * sees an agent whose screen no manifest rule matches. Folding that into
   * `Starting` would claim a lifecycle position we have no evidence for, and
   * folding it into `AwaitingInput` would summon a human to a session that may
   * be working fine.
   */
  "Unknown",
]);

/** The only status distinction a title-only listing may expose. */
export const CoarseStatus = t.enum("CoarseStatus", ["Active", "Done"]);

export const Visibility = t.enum("Visibility", [
  /** Transcript readable by the whole org. The herd's `share = live`. */
  "Org",
  /** Transcript stays on the owner's machine. The default; `share = title`. */
  "Private",
  /** Readable by the subjects in `accessGrant`. */
  "Granted",
]);

/**
 * Which surface a viewer is looking at.
 *
 * Presence means "a human has this on screen", so it is bound to the viewer's
 * connection and never to the agent's. `Session` is what makes someone a
 * watcher.
 */
export const Focus = t.enum("Focus", ["Grid", "Session", "Replay"]);

export const Role = t.enum("Role", ["Member", "Admin"]);

/**
 * A moment in a transcript.
 *
 * A comment has to point at a moment, not a session, which is why the chunk
 * envelope carries byte offsets. Offsets index the **redacted** stream — the only
 * one that is stored.
 */
export const Anchor = t.object("Anchor", {
  chunkSeq: t.u64(),
  byteOffset: t.u64(),
});

// ---------------------------------------------------------------------------
// Tables
// ---------------------------------------------------------------------------

const member = table(
  { public: true },
  {
    identity: t.identity().primaryKey(),
    githubLogin: t.string().unique(),
    githubId: t.u64(),
    displayName: t.string(),
    avatarUrl: t.string(),
    role: Role,
    joinedAt: t.timestamp(),
    lastSeen: t.timestamp(),
  },
);

/**
 * The title-only org directory card. Exists even while the transcript is
 * private, which is what lets a teammate see *that* you have a session and who
 * is watching it without seeing any output.
 *
 * Splitting this from `session` is an authorization boundary, not a view-model
 * convenience: this row is org-visible unconditionally while `session` is
 * filtered per-identity. Without the split the two would need contradictory
 * rules on one table. It is also the row the whole herd subscribes to, which is
 * why the reducers are careful not to write it needlessly.
 */
const sessionListing = table(
  { public: true },
  {
    sessionId: t.string().primaryKey(),
    owner: t.identity().index(),
    /** The headline. What this session is working on, in one line. */
    title: t.string(),
    coarseStatus: CoarseStatus,
    startedAt: t.timestamp(),
    endedAt: t.timestamp().optional(),
  },
);

/**
 * Full session detail, including the transcript cursor.
 *
 * `public`, and filtered per-identity by the rules in `rls.ts`. `public` here
 * means "clients may subscribe", not "everyone sees every row" — and it is the
 * prerequisite for being filtered at all, since RLS on a private table is a
 * publish-time error.
 */
const session = table(
  { public: true },
  {
    sessionId: t.string().primaryKey(),
    owner: t.identity().index(),
    hostLabel: t.string(),
    repo: t.string(),
    branch: t.string(),
    /** Herdr's pane id, so a teammate's comment can be addressed at a pane. */
    paneId: t.string(),
    /** Herdr's agent label, e.g. `claude`. */
    agent: t.string(),
    status: SessionStatus,
    /**
     * Short human string rendered verbatim, e.g. `awaiting approval: Bash`.
     * Unstructured on purpose: `tool_name` was measured to be the only reliable
     * thing worth putting here.
     */
    statusDetail: t.string(),
    modelLabel: t.string(),
    lastEventAt: t.timestamp(),
    /** When the status last actually changed, so the herd can say "blocked 4m". */
    statusSince: t.timestamp(),
    exitReason: t.string().optional(),
    visibility: Visibility,
    /**
     * Redundant mirror of `visibility === Org`, maintained by the same reducers
     * in the same transaction.
     *
     * It exists because **RLS cannot compare an enum column to a literal.**
     * Maincloud rejects `WHERE visibility = 'Org'` — "The literal expression
     * `Org` cannot be parsed as type `(org: () | private: () | granted: ())`" —
     * in any casing, so the single most important visibility rule in the system
     * is inexpressible against the typed column. A bool is comparable.
     *
     * The enum stays the source of truth because it is the honest type; this is a
     * projection of it for the query planner. Nothing may write one without the
     * other — see `setSessionVisibility`.
     */
    sharedWithOrg: t.bool(),
    transcriptKey: t.string(),
    /**
     * Next chunk sequence the archive does **not** yet hold. Strictly monotonic,
     * advanced only by the Worker, and therefore means "durably in R2" rather
     * than "a client claimed so".
     */
    chunkCursor: t.u64(),
    /** Byte offset in the redacted stream matching `chunkCursor`. */
    byteCursor: t.u64(),
    eventCount: t.u64(),
    heartbeatAt: t.timestamp(),
  },
);

/**
 * Status **transitions only** — never a row per status report.
 *
 * `updateSessionStatus` is the hottest reducer in the system and must tolerate
 * being called far more often than it changes anything, so it writes here only
 * when the status actually moves. This is the one table that grows with activity,
 * which is why `pruneStatusHistory` exists.
 */
const sessionStatusHistory = table(
  { public: true },
  {
    id: t.u64().primaryKey().autoInc(),
    sessionId: t.string().index(),
    status: SessionStatus,
    source: StatusSource,
    at: t.timestamp(),
  },
);

/**
 * Who has what on screen — and therefore, who is watching whom.
 *
 * Keyed by connection, so `clientDisconnected` cleans it up for free. That is the
 * only reason presence can be trusted rather than guessed: close the pane and you
 * correctly stop being a watcher, while the *session* stays live and keeps
 * streaming under the agent's own connection.
 */
const presence = table(
  { public: true },
  {
    connectionId: t.connectionId().primaryKey(),
    identity: t.identity().index(),
    /** `undefined` means "on the grid, not in any session". */
    sessionId: t.string().optional(),
    focus: Focus,
    since: t.timestamp(),
  },
);

/**
 * A raised hand.
 *
 * Keyed by identity rather than by session, because "I am stuck" is a fact about
 * a person: the first roster that attached it to each session printed the same
 * note under three rows. Public and unfiltered — asking for help is not a secret
 * from the team you are asking.
 */
const helpRequest = table(
  { public: true },
  {
    identity: t.identity().primaryKey(),
    /** Already redacted and length-capped by the publisher. */
    note: t.string(),
    since: t.timestamp(),
    /** Optional session the question is about. */
    sessionId: t.string().optional(),
  },
);

/**
 * A comment addressed at a moment in a session.
 *
 * Named `mention` rather than `comment` to keep the Rust module's table name, so
 * the port stays wire-compatible.
 */
const mention = table(
  { public: true },
  {
    id: t.u64().primaryKey().autoInc(),
    sessionId: t.string().index(),
    from: t.identity(),
    to: t.identity().index(),
    body: t.string(),
    anchor: Anchor,
    createdAt: t.timestamp(),
    readAt: t.timestamp().optional(),
    deliveredAt: t.timestamp().optional(),
    deliveredChannel: t.string().optional(),
    /**
     * Whether the recipient let this reach the agent's input, and how.
     *
     * `typed` means it was placed in the composer unsent; `submitted` means it
     * was sent as a prompt. Recorded because "a teammate's words entered my
     * agent's context" is a thing worth being able to audit later.
     */
    appliedAs: t.string().optional(),
  },
);

const notificationRoute = table(
  { public: true },
  {
    identity: t.identity().primaryKey(),
    slackUserId: t.string(),
    dmChannel: t.string(),
    onMention: t.bool(),
    onAwaitingApproval: t.bool(),
    enabled: t.bool(),
  },
);

/**
 * Unused while sharing is org-wide, and present anyway: retrofitting
 * authorization onto a live system is miserable, and an unused table is free.
 */
const accessGrant = table(
  {
    public: true,
    indexes: [
      { accessor: "sessionAndSubject", algorithm: "btree", columns: ["sessionId", "subject"] },
    ],
  },
  {
    id: t.u64().primaryKey().autoInc(),
    sessionId: t.string(),
    subject: t.identity(),
    level: t.string(),
    grantedBy: t.identity(),
    at: t.timestamp(),
  },
);

const hubConfig = table(
  { public: true },
  {
    /** Always 0. A singleton needs a key, and a key needs a name. */
    id: t.u32().primaryKey(),
    githubOrg: t.string(),
    workerBaseUrl: t.string(),
    schemaVersion: t.u32(),
    /** Set by the Worker at deploy time; the app refuses to upload without it. */
    relayEnabled: t.bool(),
    /**
     * The only identity permitted to call `advanceTranscriptCursor`.
     *
     * This is what makes the cursor mean "durably in R2" instead of "a client
     * said so". Without it the field is a claim; with it, a receipt.
     */
    workerIdentity: t.identity().optional(),
  },
);

// ---------------------------------------------------------------------------
// Scheduled tables
// ---------------------------------------------------------------------------

/**
 * Both timers carry the same row, so the columns are declared once.
 *
 * Note what is *not* here: the `scheduled` table option that names the reducer.
 * It is deprecated precisely so tables and reducers can live in separate
 * modules, and using it would make this file and `reducers.ts` circular. The
 * reducer names the table instead — `reducer({ onSchedule: reapSchedule }, ...)`.
 */
const scheduleColumns = {
  scheduledId: t.u64().primaryKey().autoInc(),
  scheduledAt: t.scheduleAt(),
};

export const reapSchedule = table({}, scheduleColumns);
export const pruneSchedule = table({}, scheduleColumns);

/**
 * The schema, and the handle every reducer and filter registers against.
 *
 * `CASE_CONVERSION_POLICY` is left at its `SnakeCase` default on purpose: it is
 * what keeps the SQL identifiers — and therefore the RLS strings and every
 * existing client — identical to the Rust module's.
 */
export const spacetimedb = schema({
  member,
  sessionListing,
  session,
  sessionStatusHistory,
  presence,
  helpRequest,
  mention,
  notificationRoute,
  accessGrant,
  hubConfig,
  reapSchedule,
  pruneSchedule,
});

/** The row a scheduled reducer receives. */
export const scheduleRow = t.row(scheduleColumns);
