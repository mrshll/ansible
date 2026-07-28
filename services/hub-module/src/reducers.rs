//! The reducer surface, grouped by caller.
//!
//! Every write checks `ctx.sender()`. That check is the authorization story that
//! actually holds today: it runs inside the transaction, on the host, and no
//! client can skip it. Read authorization is a different matter — see [`crate::rls`].

// A reducer's arguments are deserialized from the wire into owned values before
// the body runs, so taking `String` or `Identity` by value is the ABI, not a
// missed borrow. `&str` parameters do not compile here. The scheduled reducers
// take their whole schedule row by value for the same reason.
#![allow(clippy::needless_pass_by_value)]

use spacetimedb::{Identity, ReducerContext, Table, Timestamp};

use crate::{
    AccessGrant, Anchor, CoarseStatus, Focus, HEARTBEAT_TIMEOUT_MICROS, HISTORY_PER_SESSION,
    Member, Mention, NotificationRoute, Presence, PruneSchedule, ReapSchedule, Role, Session,
    SessionListing, SessionStatus, SessionStatusHistory, StatusSource, Visibility,
};
// The `#[table]` macro generates one trait per table, named after its accessor,
// and implements it for `ctx.db`. They have to be in scope for `ctx.db.session()`
// and friends to resolve — the lowercase names below are those traits, not the
// row structs imported above.
use crate::{
    access_grant, hub_config, member, mention, notification_route, presence, session,
    session_listing, session_status_history,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn require_session(ctx: &ReducerContext, session_id: &str) -> Result<Session, String> {
    ctx.db
        .session()
        .session_id()
        .find(session_id.to_string())
        .ok_or_else(|| format!("no such session: {session_id}"))
}

/// Fetch a session and confirm the caller owns it.
fn require_owned(ctx: &ReducerContext, session_id: &str) -> Result<Session, String> {
    let session = require_session(ctx, session_id)?;
    if session.owner != ctx.sender() {
        return Err(format!("session {session_id} is not owned by the caller"));
    }
    Ok(session)
}

/// Admin check, with a documented bootstrap hole.
///
/// While `member` is empty the first caller is treated as an admin, because
/// otherwise a fresh database has no way to appoint one. Once anybody is a
/// member the hole closes.
fn require_admin(ctx: &ReducerContext) -> Result<(), String> {
    if ctx.db.member().count() == 0 {
        return Ok(());
    }
    match ctx.db.member().identity().find(ctx.sender()) {
        Some(m) if m.role == Role::Admin => Ok(()),
        _ => Err("caller is not an admin".to_string()),
    }
}

/// Record a transition and update the listing's coarse status.
///
/// Only ever called when the status actually changed — see
/// [`update_session_status`].
fn record_transition(
    ctx: &ReducerContext,
    session_id: &str,
    status: SessionStatus,
    source: StatusSource,
) {
    ctx.db.session_status_history().insert(SessionStatusHistory {
        id: 0,
        session_id: session_id.to_string(),
        status,
        source,
        at: ctx.timestamp,
    });

    if let Some(mut listing) = ctx.db.session_listing().session_id().find(session_id.to_string()) {
        let coarse = status.coarse();
        let ended = status.is_terminal();
        // Only write the listing when something visible in it moved. The listing
        // is the row the whole org subscribes to, so a needless update fans out
        // to every connected client.
        if listing.coarse_status != coarse || (ended && listing.ended_at.is_none()) {
            listing.coarse_status = coarse;
            if ended && listing.ended_at.is_none() {
                listing.ended_at = Some(ctx.timestamp);
            }
            ctx.db.session_listing().session_id().update(listing);
        }
    }
}

// ---------------------------------------------------------------------------
// Lifecycle — agent connection (Rust core)
// ---------------------------------------------------------------------------

/// Create the title-only listing and the private detail row, atomically.
///
/// Idempotent on `session_id`, so a crash-restart re-attaches to its own session
/// instead of duplicating it. Called *before the first byte*, so the grid shows a
/// tile within one round trip and there is a row for the cursor to attach to. A
/// cursor bump for an unregistered session is an error case not worth designing
/// around.
///
/// The transcript defaults to [`Visibility::Private`]: output stays in the
/// owner's local spool until they explicitly share.
///
/// # Errors
/// Errors when `session_id` is empty, or when it already exists under a
/// different owner — a silent re-attach there would be a cross-account write.
#[spacetimedb::reducer]
pub fn register_session(
    ctx: &ReducerContext,
    session_id: String,
    title: String,
    host_label: String,
    repo: String,
    branch: String,
    model_label: String,
) -> Result<(), String> {
    if session_id.is_empty() {
        return Err("session_id must not be empty".to_string());
    }

    if let Some(existing) = ctx.db.session().session_id().find(&session_id) {
        // Re-attach, don't duplicate. Someone else's session id is still a
        // conflict, and a silent one would be a cross-account write.
        if existing.owner != ctx.sender() {
            return Err(format!("session {session_id} already exists under another owner"));
        }
        let mut existing = existing;
        existing.heartbeat_at = ctx.timestamp;
        existing.last_event_at = ctx.timestamp;
        ctx.db.session().session_id().update(existing);
        return Ok(());
    }

    ctx.db.session_listing().insert(SessionListing {
        session_id: session_id.clone(),
        owner: ctx.sender(),
        title: title.clone(),
        coarse_status: CoarseStatus::Active,
        started_at: ctx.timestamp,
        ended_at: None,
    });

    ctx.db.session().insert(Session {
        session_id: session_id.clone(),
        owner: ctx.sender(),
        host_label,
        repo,
        branch,
        status: SessionStatus::Starting,
        status_detail: String::new(),
        model_label,
        last_event_at: ctx.timestamp,
        exit_reason: None,
        visibility: Visibility::Private,
        shared_with_org: false,
        transcript_key: format!("transcripts/{session_id}"),
        chunk_cursor: 0,
        byte_cursor: 0,
        event_count: 0,
        heartbeat_at: ctx.timestamp,
    });

    record_transition(ctx, &session_id, SessionStatus::Starting, StatusSource::Supervisor);
    Ok(())
}

/// Owner-only sharing toggle.
///
/// Turning sharing off blocks new relay and archive reads immediately. It cannot
/// recall bytes a viewer already downloaded, and pretending otherwise would be
/// worse than saying so.
///
/// # Errors
/// Errors when the session does not exist or the caller does not own it.
#[spacetimedb::reducer]
pub fn set_session_visibility(
    ctx: &ReducerContext,
    session_id: String,
    visibility: Visibility,
) -> Result<(), String> {
    let mut session = require_owned(ctx, &session_id)?;
    session.visibility = visibility;
    // The two must move together or RLS and the app disagree about who may read
    // the transcript — and RLS is the one enforcing it. Setting them in one
    // reducer body means one transaction, so there is no window where they differ.
    session.shared_with_org = visibility == Visibility::Org;
    ctx.db.session().session_id().update(session);
    Ok(())
}

/// The hottest reducer in the system.
///
/// Must tolerate being called far more often than it changes anything: a status
/// report that matches the current status and detail is dropped without a write,
/// so the history table stays `O(transitions)` rather than `O(reports)`.
///
/// `AwaitingApproval` may only arrive from [`StatusSource::Terminal`]. Spike B
/// established that hooks cannot distinguish "awaiting a human" from "running a
/// slow tool" — a denied tool and an eight-second tool produce byte-identical
/// hook sequences — so accepting that status from the hook path would let the
/// guess ship. Rejecting it here means nobody can wire it up by accident.
///
/// # Errors
/// Errors when the caller does not own the session, or when a status is reported
/// by a source that cannot legitimately observe it: `AwaitingApproval` from
/// anything but the terminal, or `Failed` from anything but the supervisor.
#[spacetimedb::reducer]
pub fn update_session_status(
    ctx: &ReducerContext,
    session_id: String,
    status: SessionStatus,
    detail: String,
    source: StatusSource,
) -> Result<(), String> {
    if status == SessionStatus::AwaitingApproval && source != StatusSource::Terminal {
        return Err("AwaitingApproval may only be reported by StatusSource::Terminal; \
             hooks cannot distinguish it from a slow tool"
            .to_string());
    }
    if status == SessionStatus::Failed && source != StatusSource::Supervisor {
        return Err("Failed may only be reported by StatusSource::Supervisor; \
             SessionEnd.reason is 'other' even on a clean exit"
            .to_string());
    }

    let mut session = require_owned(ctx, &session_id)?;
    let changed = session.status != status || session.status_detail != detail;

    session.last_event_at = ctx.timestamp;
    session.heartbeat_at = ctx.timestamp;
    session.event_count += 1;

    if !changed {
        ctx.db.session().session_id().update(session);
        return Ok(());
    }

    let status_moved = session.status != status;
    session.status = status;
    session.status_detail = detail;
    ctx.db.session().session_id().update(session);

    // A detail-only change (`running: Bash` -> `running: Read`) is not a
    // transition. Recording one would put the history table back on the hot
    // path, which is exactly what it exists to avoid.
    if status_moved {
        record_transition(ctx, &session_id, status, source);
    }
    Ok(())
}

/// Set the title once the first prompt lands. Does not require sharing: the
/// owner can name a session the org can see without exposing its output.
///
/// # Errors
/// Errors when the caller does not own the session, or the listing is missing.
#[spacetimedb::reducer]
pub fn set_session_title(
    ctx: &ReducerContext,
    session_id: String,
    title: String,
) -> Result<(), String> {
    require_owned(ctx, &session_id)?;
    let mut listing = ctx
        .db
        .session_listing()
        .session_id()
        .find(&session_id)
        .ok_or_else(|| format!("no listing for session {session_id}"))?;
    listing.title = title;
    ctx.db.session_listing().session_id().update(listing);
    Ok(())
}

/// Liveness. Cheap on purpose: one row update, no history, no listing write.
///
/// # Errors
/// Errors when the session does not exist or the caller does not own it.
#[spacetimedb::reducer]
pub fn heartbeat_session(ctx: &ReducerContext, session_id: String) -> Result<(), String> {
    let mut session = require_owned(ctx, &session_id)?;
    session.heartbeat_at = ctx.timestamp;
    ctx.db.session().session_id().update(session);
    Ok(())
}

/// Final status, `ended_at`, and final cursor.
///
/// `exit_reason` comes from the supervisor's exit status, not from the
/// `SessionEnd` hook: Spike B observed `SessionEnd.reason == "other"` on a
/// perfectly clean exit, so the hook cannot tell success from failure.
///
/// # Errors
/// Errors when `status` is not terminal, or the caller does not own the session.
#[spacetimedb::reducer]
pub fn close_session(
    ctx: &ReducerContext,
    session_id: String,
    status: SessionStatus,
    exit_reason: String,
) -> Result<(), String> {
    if !status.is_terminal() {
        return Err(format!("{status:?} is not a terminal status"));
    }
    let mut session = require_owned(ctx, &session_id)?;
    let moved = session.status != status;
    session.status = status;
    session.exit_reason = Some(exit_reason);
    session.last_event_at = ctx.timestamp;
    ctx.db.session().session_id().update(session);
    if moved {
        record_transition(ctx, &session_id, status, StatusSource::Supervisor);
    } else if let Some(mut listing) = ctx.db.session_listing().session_id().find(&session_id) {
        // Status did not move, but the listing still needs its `ended_at`.
        if listing.ended_at.is_none() {
            listing.coarse_status = CoarseStatus::Done;
            listing.ended_at = Some(ctx.timestamp);
            ctx.db.session_listing().session_id().update(listing);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Archive — called by the Worker, never the app
// ---------------------------------------------------------------------------

/// Publish that chunks `..cursor` are durable in R2.
///
/// Two properties make this field trustworthy, and both are enforced here rather
/// than assumed:
///
/// 1. **Worker-only.** The caller must be `hub_config.worker_identity`. The app
///    knows what it *uploaded*; only the Worker knows what R2 *stored*. If the
///    app could bump this, the cursor would mean "a client claimed so" and every
///    viewer following it could read past the end of the archive.
/// 2. **Strictly monotonic.** Any value at or below the current cursor is
///    rejected. A cursor that could move backwards would break every viewer
///    holding a later offset, and would make byte-exact reassembly impossible
///    because offsets index the redacted stream.
///
/// # Errors
/// Errors when the caller is not the configured Worker identity, when no Worker
/// identity is configured at all, or when the cursor would not strictly advance.
/// All three are refusals to publish a cursor that might not describe R2.
#[spacetimedb::reducer]
pub fn advance_transcript_cursor(
    ctx: &ReducerContext,
    session_id: String,
    chunk_cursor: u64,
    byte_cursor: u64,
    event_count: u64,
) -> Result<(), String> {
    let config = ctx.db.hub_config().id().find(0).ok_or("hub_config missing")?;
    match config.worker_identity {
        Some(worker) if worker == ctx.sender() => {}
        Some(_) => return Err("only the Worker identity may advance the transcript cursor".into()),
        None => {
            return Err("hub_config.worker_identity is unset; refusing to trust a cursor".into());
        }
    }

    let mut session = require_session(ctx, &session_id)?;
    if chunk_cursor <= session.chunk_cursor {
        return Err(format!(
            "cursor for {session_id} must advance: have {}, got {chunk_cursor}",
            session.chunk_cursor
        ));
    }
    if byte_cursor < session.byte_cursor {
        return Err(format!(
            "byte cursor for {session_id} must not regress: have {}, got {byte_cursor}",
            session.byte_cursor
        ));
    }

    session.chunk_cursor = chunk_cursor;
    session.byte_cursor = byte_cursor;
    session.event_count = event_count;
    session.last_event_at = ctx.timestamp;
    ctx.db.session().session_id().update(session);
    Ok(())
}

// ---------------------------------------------------------------------------
// Presence — viewer connection (webview)
// ---------------------------------------------------------------------------

/// "A human has this on screen."
///
/// Bound to the *viewer* connection, never the agent's, so closing the window
/// drops presence while the session keeps streaming. One presence row per
/// connection: focus moves, it does not accumulate.
///
/// # Errors
/// Errors when called outside a client connection, or for a session with no
/// listing — a viewer may not announce presence on something it cannot see.
#[spacetimedb::reducer]
pub fn set_focus(
    ctx: &ReducerContext,
    session_id: Option<String>,
    focus: Focus,
) -> Result<(), String> {
    let connection_id = ctx.connection_id().ok_or("set_focus requires a client connection")?;

    // A viewer may only announce presence on a session it can see. The listing
    // is the right table to check: a title-only card is enough to be present on,
    // which is what lets teammates see each other on a private session without
    // seeing its output.
    if let Some(id) = &session_id
        && ctx.db.session_listing().session_id().find(id).is_none()
    {
        return Err(format!("no such session: {id}"));
    }

    let row =
        Presence { connection_id, identity: ctx.sender(), session_id, focus, since: ctx.timestamp };
    if ctx.db.presence().connection_id().find(connection_id).is_some() {
        ctx.db.presence().connection_id().update(row);
    } else {
        ctx.db.presence().insert(row);
    }
    Ok(())
}

/// Drop this connection's presence row without disconnecting.
///
/// The webview calls this when the window is hidden or backgrounded, so presence
/// keeps meaning "on screen" rather than "connected".
///
/// # Errors
/// Errors when called outside a client connection.
#[spacetimedb::reducer]
pub fn clear_focus(ctx: &ReducerContext) -> Result<(), String> {
    let connection_id = ctx.connection_id().ok_or("clear_focus requires a client connection")?;
    ctx.db.presence().connection_id().delete(connection_id);
    Ok(())
}

// ---------------------------------------------------------------------------
// Mentions
// ---------------------------------------------------------------------------

/// `@alice take this one`, against a specific moment.
///
/// # Errors
/// Errors when the body is empty, the session does not exist, or the sender
/// cannot see the session — otherwise a mention becomes an existence oracle for
/// private sessions.
#[spacetimedb::reducer]
pub fn create_mention(
    ctx: &ReducerContext,
    session_id: String,
    to: Identity,
    body: String,
    anchor: Anchor,
) -> Result<(), String> {
    if body.is_empty() {
        return Err("mention body must not be empty".to_string());
    }
    let session = require_session(ctx, &session_id)?;

    // The sender must be able to see the session, or a mention becomes an oracle
    // for the existence and contents of private sessions.
    let sender = ctx.sender();
    let visible = session.owner == sender
        || match session.visibility {
            Visibility::Org => true,
            Visibility::Private => false,
            Visibility::Granted => ctx
                .db
                .access_grant()
                .session_and_subject()
                .filter((&session_id, sender))
                .next()
                .is_some(),
        };
    if !visible {
        return Err(format!("cannot mention against a session you cannot see: {session_id}"));
    }

    ctx.db.mention().insert(Mention {
        id: 0,
        session_id,
        from: sender,
        to,
        body,
        anchor,
        created_at: ctx.timestamp,
        read_at: None,
        delivered_at: None,
        delivered_channel: None,
    });
    Ok(())
}

/// Recipient acknowledges a mention, typically via the deep link.
///
/// # Errors
/// Errors when the mention does not exist, or the caller is not its recipient.
#[spacetimedb::reducer]
pub fn mark_mention_read(ctx: &ReducerContext, id: u64) -> Result<(), String> {
    let mut mention = ctx.db.mention().id().find(id).ok_or_else(|| format!("no mention {id}"))?;
    if mention.to != ctx.sender() {
        return Err("only the recipient may mark a mention read".to_string());
    }
    mention.read_at = Some(ctx.timestamp);
    ctx.db.mention().id().update(mention);
    Ok(())
}

/// Called by the Slack bridge once a DM lands.
///
/// # Errors
/// Errors when the mention does not exist.
#[spacetimedb::reducer]
pub fn mark_mention_delivered(
    ctx: &ReducerContext,
    id: u64,
    channel: String,
) -> Result<(), String> {
    let mut mention = ctx.db.mention().id().find(id).ok_or_else(|| format!("no mention {id}"))?;
    mention.delivered_at = Some(ctx.timestamp);
    mention.delivered_channel = Some(channel);
    ctx.db.mention().id().update(mention);
    Ok(())
}

// ---------------------------------------------------------------------------
// Membership
// ---------------------------------------------------------------------------

/// Upsert the caller's own member row.
///
/// **The GitHub claims here are client-asserted, and that is a known hole.** Anyone
/// can claim any `github_login` that is not already taken — `#[unique]` prevents
/// impersonating an *existing* member and nothing more. **Phase 1 must not ship this
/// as written.**
///
/// The fix is smaller than it first appeared. A reducer *can* read verified claims:
/// [`whoami`] demonstrates that `ctx.sender_auth().jwt()` exposes `issuer()`,
/// `subject()`, and `raw_payload()` for custom claims, all validated by the host
/// before this body runs. So this should read the login from the token rather than
/// from its argument — a change to one reducer, not an architectural shift.
///
/// What it waits on is a token *worth* reading: our Worker completing GitHub OAuth
/// and minting a JWT that Maincloud is configured to trust. Whether Maincloud can
/// trust a third-party issuer is the last unverified step and the last thing gating
/// Phase 1. That `Identity` derives from `(issuer, subject)` is already confirmed —
/// see `docs/spikes/deployed-round-trip.md` §9.
///
/// # Errors
/// Errors when `github_login` is empty or already claimed by another identity.
#[spacetimedb::reducer]
pub fn upsert_member(
    ctx: &ReducerContext,
    github_login: String,
    github_id: u64,
    display_name: String,
    avatar_url: String,
) -> Result<(), String> {
    if github_login.is_empty() {
        return Err("github_login must not be empty".to_string());
    }
    if let Some(existing) = ctx.db.member().github_login().find(&github_login)
        && existing.identity != ctx.sender()
    {
        return Err(format!("github_login {github_login} is already claimed"));
    }

    if let Some(mut member) = ctx.db.member().identity().find(ctx.sender()) {
        member.github_login = github_login;
        member.github_id = github_id;
        member.display_name = display_name;
        member.avatar_url = avatar_url;
        member.last_seen = ctx.timestamp;
        ctx.db.member().identity().update(member);
        return Ok(());
    }

    // First member in an empty database is the admin, for the same bootstrap
    // reason `require_admin` documents.
    let role = if ctx.db.member().count() == 0 { Role::Admin } else { Role::Member };
    ctx.db.member().insert(Member {
        identity: ctx.sender(),
        github_login,
        github_id,
        display_name,
        avatar_url,
        role,
        joined_at: ctx.timestamp,
        last_seen: ctx.timestamp,
    });
    Ok(())
}

/// Promote or demote a member.
///
/// # Errors
/// Errors when the caller is not an admin, or the target is not a member.
#[spacetimedb::reducer]
pub fn admin_set_role(ctx: &ReducerContext, identity: Identity, role: Role) -> Result<(), String> {
    require_admin(ctx)?;
    let mut member =
        ctx.db.member().identity().find(identity).ok_or_else(|| "no such member".to_string())?;
    member.role = role;
    ctx.db.member().identity().update(member);
    Ok(())
}

/// Remove a member and their notification route.
///
/// Deliberately leaves their sessions and mentions in place: deleting an
/// engineer's transcripts because they changed teams is a retention decision, not
/// a membership one. See open question #5.
///
/// # Errors
/// Errors when the caller is not an admin.
#[spacetimedb::reducer]
pub fn remove_member(ctx: &ReducerContext, identity: Identity) -> Result<(), String> {
    require_admin(ctx)?;
    ctx.db.member().identity().delete(identity);
    ctx.db.notification_route().identity().delete(identity);
    Ok(())
}

/// Where to reach this member when they are mentioned or their session blocks.
///
/// Self-service only: the caller can set their own route and nobody else's.
///
/// # Errors
/// Infallible today; returns `Result` to keep the reducer signature uniform.
#[spacetimedb::reducer]
pub fn upsert_notification_route(
    ctx: &ReducerContext,
    slack_user_id: String,
    dm_channel: String,
    on_mention: bool,
    on_awaiting_approval: bool,
    enabled: bool,
) -> Result<(), String> {
    let row = NotificationRoute {
        identity: ctx.sender(),
        slack_user_id,
        dm_channel,
        on_mention,
        on_awaiting_approval,
        enabled,
    };
    if ctx.db.notification_route().identity().find(ctx.sender()).is_some() {
        ctx.db.notification_route().identity().update(row);
    } else {
        ctx.db.notification_route().insert(row);
    }
    Ok(())
}

/// Give one subject access to one session. Idempotent.
///
/// Unused by Phase 1's org-wide sharing, and present so that authorization does
/// not have to be retrofitted onto a live system.
///
/// # Errors
/// Errors when the caller does not own the session.
#[spacetimedb::reducer]
pub fn grant_access(
    ctx: &ReducerContext,
    session_id: String,
    subject: Identity,
    level: String,
) -> Result<(), String> {
    require_owned(ctx, &session_id)?;
    if ctx.db.access_grant().session_and_subject().filter((&session_id, subject)).next().is_some() {
        return Ok(());
    }
    ctx.db.access_grant().insert(AccessGrant {
        id: 0,
        session_id,
        subject,
        level,
        granted_by: ctx.sender(),
        at: ctx.timestamp,
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Set the org-wide configuration the app reads on connect.
///
/// # Errors
/// Errors when the caller is not an admin, or `hub_config` is missing.
#[spacetimedb::reducer]
pub fn set_hub_config(
    ctx: &ReducerContext,
    github_org: String,
    worker_base_url: String,
    relay_enabled: bool,
) -> Result<(), String> {
    require_admin(ctx)?;
    let mut config = ctx.db.hub_config().id().find(0).ok_or("hub_config missing")?;
    config.github_org = github_org;
    config.worker_base_url = worker_base_url;
    config.relay_enabled = relay_enabled;
    ctx.db.hub_config().id().update(config);
    Ok(())
}

/// Designate the identity allowed to advance transcript cursors.
///
/// Deliberately separate from [`set_hub_config`] so granting the cursor-writing
/// capability is its own audited act, not a field someone edits while changing a
/// URL.
///
/// # Errors
/// Errors when the caller is not an admin, or `hub_config` is missing.
#[spacetimedb::reducer]
pub fn set_worker_identity(ctx: &ReducerContext, worker: Identity) -> Result<(), String> {
    require_admin(ctx)?;
    let mut config = ctx.db.hub_config().id().find(0).ok_or("hub_config missing")?;
    config.worker_identity = Some(worker);
    ctx.db.hub_config().id().update(config);
    Ok(())
}

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

/// Log what the host knows about the caller's token, and nothing else.
///
/// This exists to answer the second of the three questions
/// `docs/plan/w1-provisioning.md` lists as inputs to the identity spike: *can a
/// reducer read claims from the caller's token, or only `ctx.sender`?* The answer is
/// yes — `ctx.sender_auth().jwt()` exposes the verified claims, and
/// `JwtClaims::raw_payload` exposes custom ones. That matters because
/// `upsert_member` is specified to read *verified* claims and currently cannot, so
/// this is the mechanism that fixes it.
///
/// It logs claim **names**, never claim values beyond issuer, subject, and audience.
/// A real token may carry an email or a display name, and a diagnostic that dumps a
/// token payload into a log is how those end up somewhere nobody meant to put them.
///
/// Safe to leave deployed: it reads only the caller's own token and writes no rows.
///
/// # Errors
/// Never; returns `Result` to keep the reducer signature uniform.
#[spacetimedb::reducer]
pub fn whoami(ctx: &ReducerContext) -> Result<(), String> {
    let auth = ctx.sender_auth();
    spacetimedb::log::info!(
        "whoami: sender={} is_internal={} has_jwt={}",
        ctx.sender().to_hex(),
        auth.is_internal(),
        auth.has_jwt()
    );

    if let Some(jwt) = auth.jwt() {
        // `Identity::from_claims(issuer, subject)` is how the host derives identity,
        // so logging both alongside `ctx.sender()` shows whether they agree — which
        // is the fact the identity design depends on.
        spacetimedb::log::info!(
            "whoami: issuer={:?} subject={:?} audience={:?} derived_identity={}",
            jwt.issuer(),
            jwt.subject(),
            jwt.audience(),
            jwt.identity().to_hex(),
        );

        let names: Vec<String> = serde_json::from_str::<serde_json::Value>(jwt.raw_payload())
            .ok()
            .and_then(|v| v.as_object().map(|o| o.keys().cloned().collect()))
            .unwrap_or_default();
        spacetimedb::log::info!("whoami: claim_names={names:?}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Scheduled
// ---------------------------------------------------------------------------

/// Stale heartbeat and no live agent connection -> `Detached`.
///
/// This is the crash path, and it is designed in from day one because it will
/// happen constantly during development: a killed app sends no `SessionEnd` and
/// closes no PTY, so nothing else would ever move the session off `Working`.
///
/// # Errors
/// Errors when invoked by anything other than the scheduler.
#[spacetimedb::reducer]
pub fn reap_stale_sessions(ctx: &ReducerContext, _arg: ReapSchedule) -> Result<(), String> {
    if ctx.sender() != ctx.database_identity() {
        return Err("reap_stale_sessions may only be invoked by the scheduler".to_string());
    }
    let now = ctx.timestamp.to_micros_since_unix_epoch();
    let stale: Vec<Session> = ctx
        .db
        .session()
        .iter()
        .filter(|s| !s.status.is_terminal())
        .filter(|s| now - s.heartbeat_at.to_micros_since_unix_epoch() > HEARTBEAT_TIMEOUT_MICROS)
        .collect();

    for mut session in stale {
        let session_id = session.session_id.clone();
        session.status = SessionStatus::Detached;
        ctx.db.session().session_id().update(session);
        record_transition(ctx, &session_id, SessionStatus::Detached, StatusSource::Reaper);
    }
    Ok(())
}

/// Enforce retention on the one table that grows with activity.
///
/// Keeps the most recent [`HISTORY_PER_SESSION`] transitions per session. Bound
/// per session rather than globally so a single chatty session cannot evict
/// every other session's history.
///
/// # Errors
/// Errors when invoked by anything other than the scheduler.
#[spacetimedb::reducer]
pub fn prune_status_history(ctx: &ReducerContext, _arg: PruneSchedule) -> Result<(), String> {
    if ctx.sender() != ctx.database_identity() {
        return Err("prune_status_history may only be invoked by the scheduler".to_string());
    }

    let mut by_session: std::collections::HashMap<String, Vec<(u64, Timestamp)>> =
        std::collections::HashMap::new();
    for row in ctx.db.session_status_history().iter() {
        by_session.entry(row.session_id).or_default().push((row.id, row.at));
    }

    for (_session_id, mut rows) in by_session {
        if rows.len() <= HISTORY_PER_SESSION {
            continue;
        }
        // Newest first, then drop everything past the retention bound.
        rows.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(b.0.cmp(&a.0)));
        for (id, _) in rows.drain(HISTORY_PER_SESSION..) {
            ctx.db.session_status_history().id().delete(id);
        }
    }
    Ok(())
}
