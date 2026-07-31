/**
 * The reducer surface, grouped by caller.
 *
 * Every write checks `ctx.sender`. That check is the authorization story that
 * actually holds: it runs inside the transaction, on the host, and no client can
 * skip it. Read authorization is a different matter — see `rls.ts`.
 *
 * Reducer names on the wire come from the export name converted to snake_case, so
 * `updateSessionStatus` here is `update_session_status` on the wire — identical to
 * the Rust module's. That is what lets the Worker keep calling
 * `advance_transcript_cursor` across the port without a change.
 *
 * Errors are thrown, not returned. A throw aborts the transaction, so a failed
 * guard cannot leave a half-written pair of rows — which matters most for
 * `setSessionVisibility`, where the enum and its boolean mirror must move
 * together or not at all.
 */

import { ScheduleAt } from "spacetimedb";
import { SenderError, t } from "spacetimedb/server";
import type { ReducerCtx } from "spacetimedb/server";

import {
  Anchor,
  Focus,
  Role,
  SessionStatus,
  StatusSource,
  Visibility,
  pruneSchedule,
  reapSchedule,
  scheduleRow,
  spacetimedb,
} from "./schema.js";
import {
  HEARTBEAT_TIMEOUT_MICROS,
  HISTORY_PER_SESSION,
  PRUNE_INTERVAL_MICROS,
  REAP_INTERVAL_MICROS,
} from "./tunables.js";

type Ctx = ReducerCtx<typeof spacetimedb.schemaType>;

/**
 * Enum values on the wire are tagged unit objects — `{ tag: "Working" }`.
 *
 * Spelled out rather than derived from the builders because `t.enum()`'s declared
 * return type omits the variant constructors it creates at runtime (a 2.7.1
 * declaration bug, see `docs/adr/0005-typescript-and-the-herdr-host.md`). Writing
 * the unions here means every insert site is still checked against the exact set
 * of variants, and the wire form is visible in the source.
 */
type Status =
  | { tag: "Starting" }
  | { tag: "Working" }
  | { tag: "AwaitingInput" }
  | { tag: "AwaitingApproval" }
  | { tag: "Done" }
  | { tag: "Failed" }
  | { tag: "Detached" }
  | { tag: "Unknown" };
type Source =
  | { tag: "Hook" }
  | { tag: "Terminal" }
  | { tag: "Supervisor" }
  | { tag: "Reaper" }
  | { tag: "Herdr" };
type Coarse = { tag: "Active" } | { tag: "Done" };
type Vis = { tag: "Org" } | { tag: "Private" } | { tag: "Granted" };

/** Build a tagged unit variant with its literal type preserved. */
const v = <T extends string>(tag: T): { tag: T } => ({ tag });

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function requireSession(ctx: Ctx, sessionId: string) {
  const session = ctx.db.session.sessionId.find(sessionId);
  if (session === null) {
    throw new SenderError(`no such session: ${sessionId}`);
  }
  return session;
}

/** Fetch a session and confirm the caller owns it. */
function requireOwned(ctx: Ctx, sessionId: string) {
  const session = requireSession(ctx, sessionId);
  if (!session.owner.isEqual(ctx.sender)) {
    throw new SenderError(`session ${sessionId} is not owned by the caller`);
  }
  return session;
}

/**
 * Admin check, with a documented bootstrap hole.
 *
 * While `member` is empty the first caller is treated as an admin, because
 * otherwise a fresh database has no way to appoint one. Once anybody is a member
 * the hole closes.
 */
function requireAdmin(ctx: Ctx): void {
  if (ctx.db.member.count() === 0n) {
    return;
  }
  const me = ctx.db.member.identity.find(ctx.sender);
  if (me === null || me.role.tag !== "Admin") {
    throw new SenderError("caller is not an admin");
  }
}

function isTerminal(status: Status): boolean {
  return status.tag === "Done" || status.tag === "Failed" || status.tag === "Detached";
}

/**
 * Collapse a status to what the org-wide directory card is allowed to reveal.
 *
 * The listing must not disclose that a *private* session is awaiting approval or
 * input — that is activity detail. Lifecycle only.
 */
function coarse(status: Status): Coarse {
  return isTerminal(status) ? v("Done") : v("Active");
}

/**
 * Record a transition and update the listing's coarse status.
 *
 * Only ever called when the status actually changed — see
 * {@link updateSessionStatus}.
 */
function recordTransition(ctx: Ctx, sessionId: string, status: Status, source: Source): void {
  ctx.db.sessionStatusHistory.insert({
    id: 0n,
    sessionId,
    status,
    source,
    at: ctx.timestamp,
  });

  const listing = ctx.db.sessionListing.sessionId.find(sessionId);
  if (listing === null) {
    return;
  }
  const next = coarse(status);
  const ended = isTerminal(status);
  // Only write the listing when something visible in it moved. The listing is the
  // row the whole herd subscribes to, so a needless update fans out to every
  // connected client.
  if (listing.coarseStatus.tag !== next.tag || (ended && listing.endedAt === undefined)) {
    ctx.db.sessionListing.sessionId.update({
      ...listing,
      coarseStatus: next,
      endedAt: ended && listing.endedAt === undefined ? ctx.timestamp : listing.endedAt,
    });
  }
}

/** Keep `visibility` and its boolean mirror in step. Never write one alone. */
function shareFlag(visibility: Vis): boolean {
  return visibility.tag === "Org";
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

/**
 * Seed config and start the timers.
 *
 * Scheduling from `init` is transactional: if this reducer fails, neither timer
 * is created.
 */
export const init = spacetimedb.init((ctx) => {
  ctx.db.hubConfig.insert({
    id: 0,
    githubOrg: "",
    workerBaseUrl: "",
    schemaVersion: 2,
    relayEnabled: false,
    workerIdentity: undefined,
  });
  ctx.db.reapSchedule.insert({
    scheduledId: 0n,
    scheduledAt: ScheduleAt.interval(REAP_INTERVAL_MICROS),
  });
  ctx.db.pruneSchedule.insert({
    scheduledId: 0n,
    scheduledAt: ScheduleAt.interval(PRUNE_INTERVAL_MICROS),
  });
});

/**
 * A client connected. Nothing to do: presence is created by an explicit
 * `setFocus`, because a connection is not the same as a human looking at
 * something.
 */
export const clientConnected = spacetimedb.clientConnected(() => {});

/**
 * A client went away. Drop its presence rows.
 *
 * This is the whole reason presence — and therefore the watcher list — can be
 * trusted rather than guessed: close the pane and you correctly stop being a
 * watcher, while the *session* stays live and keeps streaming under the agent's
 * own connection. It is also why the plugin needs no lease expiry of its own.
 */
export const clientDisconnected = spacetimedb.clientDisconnected((ctx) => {
  if (ctx.connectionId !== null) {
    ctx.db.presence.connectionId.delete(ctx.connectionId);
  }
});

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

/**
 * Create the title-only listing and the private detail row, atomically.
 *
 * Idempotent on `sessionId`, so a crash-restart re-attaches to its own session
 * instead of duplicating it. The transcript defaults to `Private`: output stays on
 * the owner's machine until they explicitly share.
 */
export const registerSession = spacetimedb.reducer(
  {
    sessionId: t.string(),
    hostLabel: t.string(),
    paneId: t.string(),
    agent: t.string(),
    title: t.string(),
    repo: t.string(),
    branch: t.string(),
    modelLabel: t.string(),
    transcriptKey: t.string(),
  },
  (ctx, args) => {
    if (args.sessionId.length === 0) {
      throw new SenderError("sessionId must not be empty");
    }

    const existing = ctx.db.session.sessionId.find(args.sessionId);
    if (existing !== null) {
      // A silent re-attach under a different owner would be a cross-account write.
      if (!existing.owner.isEqual(ctx.sender)) {
        throw new SenderError(`session ${args.sessionId} belongs to another identity`);
      }
      ctx.db.session.sessionId.update({
        ...existing,
        hostLabel: args.hostLabel,
        paneId: args.paneId,
        agent: args.agent,
        repo: args.repo,
        branch: args.branch,
        modelLabel: args.modelLabel,
        lastEventAt: ctx.timestamp,
        heartbeatAt: ctx.timestamp,
      });
      return;
    }

    ctx.db.sessionListing.insert({
      sessionId: args.sessionId,
      owner: ctx.sender,
      title: args.title,
      coarseStatus: v("Active"),
      startedAt: ctx.timestamp,
      endedAt: undefined,
    });
    ctx.db.session.insert({
      sessionId: args.sessionId,
      owner: ctx.sender,
      hostLabel: args.hostLabel,
      repo: args.repo,
      branch: args.branch,
      paneId: args.paneId,
      agent: args.agent,
      status: v("Starting"),
      statusDetail: "",
      modelLabel: args.modelLabel,
      lastEventAt: ctx.timestamp,
      statusSince: ctx.timestamp,
      exitReason: undefined,
      visibility: v("Private"),
      sharedWithOrg: false,
      transcriptKey: args.transcriptKey,
      chunkCursor: 0n,
      byteCursor: 0n,
      eventCount: 0n,
      heartbeatAt: ctx.timestamp,
    });
    recordTransition(ctx, args.sessionId, v("Starting"), v("Herdr"));
  },
);

/**
 * Move a session between `off`, `title`, and `live` sharing.
 *
 * The enum and its boolean mirror are written together, in one transaction, and
 * this is the only reducer allowed to write either. See `session.sharedWithOrg`
 * for why the mirror exists at all.
 */
export const setSessionVisibility = spacetimedb.reducer(
  { sessionId: t.string(), visibility: Visibility },
  (ctx, { sessionId, visibility }) => {
    const session = requireOwned(ctx, sessionId);
    ctx.db.session.sessionId.update({
      ...session,
      visibility,
      sharedWithOrg: shareFlag(visibility),
    });
  },
);

/**
 * Report a status.
 *
 * The hottest reducer in the system: it is called far more often than the status
 * changes, and it writes history only on an actual transition. Callers are
 * expected to fire it freely.
 */
export const updateSessionStatus = spacetimedb.reducer(
  {
    sessionId: t.string(),
    status: SessionStatus,
    statusDetail: t.string(),
    source: StatusSource,
  },
  (ctx, args) => {
    const session = requireOwned(ctx, args.sessionId);

    // A terminal status is final: a late report must not resurrect a finished
    // session on the herd.
    if (isTerminal(session.status) && !isTerminal(args.status)) {
      return;
    }

    const moved = session.status.tag !== args.status.tag;
    const detailMoved = session.statusDetail !== args.statusDetail;
    if (!moved && !detailMoved) {
      ctx.db.session.sessionId.update({ ...session, lastEventAt: ctx.timestamp });
      return;
    }

    ctx.db.session.sessionId.update({
      ...session,
      status: args.status,
      statusDetail: args.statusDetail,
      lastEventAt: ctx.timestamp,
      statusSince: moved ? ctx.timestamp : session.statusSince,
      eventCount: session.eventCount + 1n,
    });
    if (moved) {
      recordTransition(ctx, args.sessionId, args.status, args.source);
    }
  },
);

/** Set the headline. Lives on the listing, so the whole herd can read it. */
export const setSessionTitle = spacetimedb.reducer(
  { sessionId: t.string(), title: t.string() },
  (ctx, { sessionId, title }) => {
    requireOwned(ctx, sessionId);
    const listing = ctx.db.sessionListing.sessionId.find(sessionId);
    if (listing === null) {
      throw new SenderError(`no listing for session ${sessionId}`);
    }
    if (listing.title !== title) {
      ctx.db.sessionListing.sessionId.update({ ...listing, title });
    }
  },
);

/** Keep a session out of the reaper's hands. */
export const heartbeatSession = spacetimedb.reducer(
  { sessionId: t.string() },
  (ctx, { sessionId }) => {
    const session = requireOwned(ctx, sessionId);
    ctx.db.session.sessionId.update({ ...session, heartbeatAt: ctx.timestamp });
  },
);

export const closeSession = spacetimedb.reducer(
  { sessionId: t.string(), exitReason: t.string() },
  (ctx, { sessionId, exitReason }) => {
    const session = requireOwned(ctx, sessionId);
    const status = exitReason.length === 0 ? v("Done") : v("Failed");
    ctx.db.session.sessionId.update({
      ...session,
      status,
      exitReason: exitReason.length === 0 ? undefined : exitReason,
      lastEventAt: ctx.timestamp,
      statusSince: ctx.timestamp,
    });
    // The supervisor's exit status is the authority on Failed, which is why the
    // source is not Herdr here.
    recordTransition(ctx, sessionId, status, v("Supervisor"));
  },
);

/**
 * Advance the durable transcript cursor.
 *
 * Callable only by the Worker identity in `hubConfig`. That restriction is what
 * makes the cursor mean "durably in R2" instead of "a client said so" — without
 * it the field is a claim; with it, a receipt.
 */
export const advanceTranscriptCursor = spacetimedb.reducer(
  { sessionId: t.string(), chunkCursor: t.u64(), byteCursor: t.u64() },
  (ctx, args) => {
    const config = ctx.db.hubConfig.id.find(0);
    const worker = config?.workerIdentity;
    if (worker === undefined || !worker.isEqual(ctx.sender)) {
      throw new SenderError("only the Worker identity may advance the transcript cursor");
    }
    const session = requireSession(ctx, args.sessionId);
    // Strictly monotonic. A cursor that could move backwards would let a viewer
    // re-read bytes it had already spliced, or skip a chunk entirely.
    if (args.chunkCursor < session.chunkCursor || args.byteCursor < session.byteCursor) {
      throw new SenderError(
        `cursor must not move backwards: have ${session.chunkCursor}/${session.byteCursor}, got ${args.chunkCursor}/${args.byteCursor}`,
      );
    }
    ctx.db.session.sessionId.update({
      ...session,
      chunkCursor: args.chunkCursor,
      byteCursor: args.byteCursor,
    });
  },
);

// ---------------------------------------------------------------------------
// Presence — who is watching what
// ---------------------------------------------------------------------------

/**
 * Say what this connection has on screen.
 *
 * `focus = Session` with a `sessionId` is what makes the caller a watcher of that
 * session, and it is the entire teleport handshake from the watcher's side: the
 * owner's plugin sees the row and their own `visibility` decides whether frames
 * follow.
 */
export const setFocus = spacetimedb.reducer(
  { sessionId: t.string().optional(), focus: Focus },
  (ctx, { sessionId, focus }) => {
    if (ctx.connectionId === null) {
      throw new SenderError("presence needs a connection");
    }
    const existing = ctx.db.presence.connectionId.find(ctx.connectionId);
    const row = {
      connectionId: ctx.connectionId,
      identity: ctx.sender,
      sessionId,
      focus,
      // `since` only moves when the target does, so "watching for 4m" is true.
      since:
        existing !== null && existing.sessionId === sessionId && existing.focus.tag === focus.tag
          ? existing.since
          : ctx.timestamp,
    };
    if (existing === null) {
      ctx.db.presence.insert(row);
    } else {
      ctx.db.presence.connectionId.update(row);
    }
  },
);

export const clearFocus = spacetimedb.reducer((ctx) => {
  if (ctx.connectionId !== null) {
    ctx.db.presence.connectionId.delete(ctx.connectionId);
  }
});

// ---------------------------------------------------------------------------
// Asking for help
// ---------------------------------------------------------------------------

/**
 * Raise a hand.
 *
 * Keyed by identity, so raising it twice replaces the note rather than stacking.
 * `since` is preserved across a note edit: "stuck for 20 minutes" should not reset
 * because the wording improved.
 */
export const raiseHand = spacetimedb.reducer(
  { note: t.string(), sessionId: t.string().optional() },
  (ctx, { note, sessionId }) => {
    const existing = ctx.db.helpRequest.identity.find(ctx.sender);
    const row = { identity: ctx.sender, note, sessionId, since: existing?.since ?? ctx.timestamp };
    if (existing === null) {
      ctx.db.helpRequest.insert(row);
    } else {
      ctx.db.helpRequest.identity.update(row);
    }
  },
);

export const lowerHand = spacetimedb.reducer((ctx) => {
  ctx.db.helpRequest.identity.delete(ctx.sender);
});

// ---------------------------------------------------------------------------
// Comments
// ---------------------------------------------------------------------------

/**
 * Leave a comment against a moment in a session.
 *
 * The recipient is derived from the session's owner rather than passed in, so a
 * comment cannot be addressed at someone who has nothing to do with the session.
 */
export const createMention = spacetimedb.reducer(
  { sessionId: t.string(), body: t.string(), anchor: Anchor },
  (ctx, { sessionId, body, anchor }) => {
    if (body.length === 0) {
      throw new SenderError("a comment needs a body");
    }
    const session = requireSession(ctx, sessionId);
    ctx.db.mention.insert({
      id: 0n,
      sessionId,
      from: ctx.sender,
      to: session.owner,
      body,
      anchor,
      createdAt: ctx.timestamp,
      readAt: undefined,
      deliveredAt: undefined,
      deliveredChannel: undefined,
      appliedAs: undefined,
    });
  },
);

export const markMentionRead = spacetimedb.reducer({ id: t.u64() }, (ctx, { id }) => {
  const mention = ctx.db.mention.id.find(id);
  if (mention === null) {
    throw new SenderError(`no such mention: ${id}`);
  }
  if (!mention.to.isEqual(ctx.sender)) {
    throw new SenderError("only the recipient may mark a comment read");
  }
  ctx.db.mention.id.update({ ...mention, readAt: ctx.timestamp });
});

export const markMentionDelivered = spacetimedb.reducer(
  { id: t.u64(), channel: t.string() },
  (ctx, { id, channel }) => {
    const mention = ctx.db.mention.id.find(id);
    if (mention === null) {
      throw new SenderError(`no such mention: ${id}`);
    }
    if (!mention.to.isEqual(ctx.sender)) {
      throw new SenderError("only the recipient may record delivery");
    }
    ctx.db.mention.id.update({
      ...mention,
      deliveredAt: ctx.timestamp,
      deliveredChannel: channel,
    });
  },
);

/**
 * Record that a comment reached the agent, and how.
 *
 * `typed` means it was placed in the pane's composer unsent; `submitted` means it
 * was sent as a prompt. Only the recipient can say so, which is the point: this is
 * the audit trail for the one path by which another human's words enter your
 * agent's context.
 */
export const markMentionApplied = spacetimedb.reducer(
  { id: t.u64(), appliedAs: t.string() },
  (ctx, { id, appliedAs }) => {
    if (appliedAs !== "typed" && appliedAs !== "submitted") {
      throw new SenderError('appliedAs must be "typed" or "submitted"');
    }
    const mention = ctx.db.mention.id.find(id);
    if (mention === null) {
      throw new SenderError(`no such mention: ${id}`);
    }
    if (!mention.to.isEqual(ctx.sender)) {
      throw new SenderError("only the recipient may apply a comment to their agent");
    }
    ctx.db.mention.id.update({ ...mention, appliedAs, readAt: mention.readAt ?? ctx.timestamp });
  },
);

// ---------------------------------------------------------------------------
// Membership and config
// ---------------------------------------------------------------------------

/** Register or refresh the caller's own member row. Self-service by design. */
export const upsertMember = spacetimedb.reducer(
  {
    githubLogin: t.string(),
    githubId: t.u64(),
    displayName: t.string(),
    avatarUrl: t.string(),
  },
  (ctx, args) => {
    if (args.githubLogin.length === 0) {
      throw new SenderError("githubLogin must not be empty");
    }
    const existing = ctx.db.member.identity.find(ctx.sender);
    if (existing === null) {
      // The first member is an admin, because a fresh database needs one.
      const role = ctx.db.member.count() === 0n ? v("Admin") : v("Member");
      ctx.db.member.insert({
        identity: ctx.sender,
        githubLogin: args.githubLogin,
        githubId: args.githubId,
        displayName: args.displayName,
        avatarUrl: args.avatarUrl,
        role,
        joinedAt: ctx.timestamp,
        lastSeen: ctx.timestamp,
      });
      return;
    }
    ctx.db.member.identity.update({
      ...existing,
      githubLogin: args.githubLogin,
      githubId: args.githubId,
      displayName: args.displayName,
      avatarUrl: args.avatarUrl,
      lastSeen: ctx.timestamp,
    });
  },
);

export const adminSetRole = spacetimedb.reducer(
  { identity: t.identity(), role: Role },
  (ctx, { identity, role }) => {
    requireAdmin(ctx);
    const member = ctx.db.member.identity.find(identity);
    if (member === null) {
      throw new SenderError("no such member");
    }
    ctx.db.member.identity.update({ ...member, role });
  },
);

export const removeMember = spacetimedb.reducer({ identity: t.identity() }, (ctx, { identity }) => {
  requireAdmin(ctx);
  ctx.db.member.identity.delete(identity);
});

export const upsertNotificationRoute = spacetimedb.reducer(
  {
    slackUserId: t.string(),
    dmChannel: t.string(),
    onMention: t.bool(),
    onAwaitingApproval: t.bool(),
    enabled: t.bool(),
  },
  (ctx, args) => {
    const row = { identity: ctx.sender, ...args };
    if (ctx.db.notificationRoute.identity.find(ctx.sender) === null) {
      ctx.db.notificationRoute.insert(row);
    } else {
      ctx.db.notificationRoute.identity.update(row);
    }
  },
);

export const grantAccess = spacetimedb.reducer(
  { sessionId: t.string(), subject: t.identity(), level: t.string() },
  (ctx, { sessionId, subject, level }) => {
    requireOwned(ctx, sessionId);
    ctx.db.accessGrant.insert({
      id: 0n,
      sessionId,
      subject,
      level,
      grantedBy: ctx.sender,
      at: ctx.timestamp,
    });
  },
);

export const setHubConfig = spacetimedb.reducer(
  { githubOrg: t.string(), workerBaseUrl: t.string(), relayEnabled: t.bool() },
  (ctx, args) => {
    requireAdmin(ctx);
    const config = ctx.db.hubConfig.id.find(0);
    if (config === null) {
      throw new SenderError("hub config is missing; republish the module");
    }
    ctx.db.hubConfig.id.update({ ...config, ...args });
  },
);

export const setWorkerIdentity = spacetimedb.reducer(
  { worker: t.identity() },
  (ctx, { worker }) => {
    requireAdmin(ctx);
    const config = ctx.db.hubConfig.id.find(0);
    if (config === null) {
      throw new SenderError("hub config is missing; republish the module");
    }
    ctx.db.hubConfig.id.update({ ...config, workerIdentity: worker });
  },
);

/**
 * Log the caller's identity.
 *
 * Exists so a human can discover the identity a machine is publishing under
 * without a client that already knows — which is exactly the problem when setting
 * `workerIdentity` for the first time.
 */
export const whoami = spacetimedb.reducer((ctx) => {
  console.log(`whoami: ${ctx.sender.toHexString()}`);
});

// ---------------------------------------------------------------------------
// Scheduled
// ---------------------------------------------------------------------------

/**
 * Detach sessions whose heartbeat has lapsed.
 *
 * A session whose machine went to sleep must stop claiming to be `blocked`: a
 * stale summons is worse than no summons. The reaper is the only writer of
 * `Detached`.
 */
export const reapStaleSessions = spacetimedb.reducer(
  { onSchedule: reapSchedule },
  { schedule: scheduleRow },
  (ctx) => {
    const cutoff = ctx.timestamp.microsSinceUnixEpoch - HEARTBEAT_TIMEOUT_MICROS;
    for (const session of ctx.db.session.iter()) {
      if (isTerminal(session.status)) {
        continue;
      }
      if (session.heartbeatAt.microsSinceUnixEpoch >= cutoff) {
        continue;
      }
      ctx.db.session.sessionId.update({
        ...session,
        status: v("Detached"),
        statusSince: ctx.timestamp,
      });
      recordTransition(ctx, session.sessionId, v("Detached"), v("Reaper"));
    }
  },
);

/**
 * Bound the one table that grows with activity.
 *
 * History is the only table whose size tracks how much work the team has done
 * rather than how much of it is live, so it needs a ceiling per session.
 */
export const pruneStatusHistory = spacetimedb.reducer(
  { onSchedule: pruneSchedule },
  { schedule: scheduleRow },
  (ctx) => {
    const perSession = new Map<string, bigint[]>();
    for (const row of ctx.db.sessionStatusHistory.iter()) {
      const ids = perSession.get(row.sessionId) ?? [];
      ids.push(row.id);
      perSession.set(row.sessionId, ids);
    }
    for (const ids of perSession.values()) {
      if (ids.length <= HISTORY_PER_SESSION) {
        continue;
      }
      // Auto-increment ids are monotonic, so sorting by id is chronological and
      // the oldest are at the front.
      ids.sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
      for (const id of ids.slice(0, ids.length - HISTORY_PER_SESSION)) {
        ctx.db.sessionStatusHistory.id.delete(id);
      }
    }
  },
);
