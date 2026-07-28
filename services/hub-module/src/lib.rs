//! The hub: schema and reducer surface for the multiplayer presence layer.
//!
//! This module is the schema source of truth. TypeScript and Rust client
//! bindings are generated from it with `spacetime generate`.
//!
//! # The one hard invariant
//!
//! **Nothing here may grow with transcript volume.** The row budget is
//! `O(sessions)` + `O(status transitions)`, both bounded. Transcript bytes live
//! in R2 and are addressed by [`Session::chunk_cursor`]; the hub stores the
//! cursor, never the content. Any change that adds a per-chunk or per-byte row
//! is a design error, not an optimization opportunity.
//!
//! # Authorization, and what is actually enforcing it
//!
//! **Writes** are checked here: every reducer compares `ctx.sender()` against the
//! row's owner, inside the transaction, on the host. No client can skip it.
//!
//! **Reads** are checked by row-level security — the `#[client_visibility_filter]`
//! rules in [`rls`] — and Spike B verified on deployed Maincloud that they are
//! genuinely enforced, per-row and per-identity. `scripts/probe-rls.sh` is the
//! standing evidence; it asserts from the viewpoint of an identity that owns
//! nothing, which is the only viewpoint that can be wrong in a way that matters.
//!
//! Two caveats that the schema is shaped around:
//!
//! - **RLS cannot compare an enum column to a literal**, so the visibility rule
//!   keys on [`Session::shared_with_org`] rather than on [`Session::visibility`].
//! - **The module owner bypasses RLS entirely.** `Private` is a boundary between
//!   teammates, not between a teammate and whoever holds the publish credential.
//!
//! One thing RLS deliberately does *not* do: keep private bytes out of R2. A
//! private session is never uploaded in the first place, so the archive has
//! nothing to leak. RLS governs the hub rows; the spool governs the bytes.

use spacetimedb::{ConnectionId, Identity, ReducerContext, SpacetimeType, Table, Timestamp};

pub mod reducers;
pub mod rls;

// `scheduled(...)` on a table resolves the reducer name at the table's
// definition site, so the two scheduled reducers have to be in scope here.
use reducers::{prune_status_history, reap_stale_sessions};

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// Where a status came from.
///
/// Recorded on every transition because Spike B found that the statuses have
/// *different* and non-interchangeable sources: four come from hooks, but
/// `AwaitingApproval` is not derivable from hooks at all and must come from the
/// terminal, and `Failed` must come from the supervisor's exit status because
/// `SessionEnd.reason` was `"other"` even on a clean exit. Storing the
/// provenance makes a mislabelled status debuggable after the fact instead of a
/// mystery. See `docs/spikes/hook-coverage.md` §4.
#[derive(SpacetimeType, Clone, Copy, PartialEq, Eq, Debug)]
pub enum StatusSource {
    /// A Claude Code hook event, via the localhost receiver.
    Hook,
    /// The terminal snapshot — the only source for `AwaitingApproval`.
    Terminal,
    /// The session supervisor: process exit, and therefore `Failed`.
    Supervisor,
    /// `reap_stale_sessions`, and therefore `Detached`.
    Reaper,
}

/// Detailed session status, as rendered on the grid.
///
/// `Idle` is deliberately absent. The plan listed it, but Spike B found nothing
/// can set it: `Stop` gives `AwaitingInput`, and idle is that same state plus
/// elapsed time, which the viewer derives from [`Session::last_event_at`].
/// Carrying a status no producer can set is worse than not having it.
#[derive(SpacetimeType, Clone, Copy, PartialEq, Eq, Debug)]
pub enum SessionStatus {
    Starting,
    Working,
    AwaitingInput,
    /// The interruption a teammate can actually resolve, and the highest-value
    /// thing the grid surfaces. Only [`StatusSource::Terminal`] may set it.
    AwaitingApproval,
    Done,
    Failed,
    /// Heartbeat lapsed with no live agent connection.
    Detached,
}

impl SessionStatus {
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Detached)
    }

    /// Collapse to what the org-wide directory card is allowed to reveal.
    ///
    /// The listing must not disclose that a *private* session is awaiting
    /// approval or input — that is activity detail. Lifecycle only.
    #[must_use]
    pub fn coarse(self) -> CoarseStatus {
        if self.is_terminal() { CoarseStatus::Done } else { CoarseStatus::Active }
    }
}

/// The only status distinction a title-only listing may expose.
#[derive(SpacetimeType, Clone, Copy, PartialEq, Eq, Debug)]
pub enum CoarseStatus {
    Active,
    Done,
}

#[derive(SpacetimeType, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Visibility {
    /// Transcript readable by the whole org.
    Org,
    /// Transcript stays in the owner's local spool. The default.
    Private,
    /// Readable by the subjects in `access_grant`.
    Granted,
}

/// Which surface a viewer is looking at. Presence means "a human has this on
/// screen", so it is bound to the viewer connection, never the agent's.
#[derive(SpacetimeType, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    Grid,
    Session,
    Replay,
}

#[derive(SpacetimeType, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Member,
    Admin,
}

/// A moment in a transcript.
///
/// A mention has to point at a *moment*, not a session, which is why the chunk
/// envelope carries byte offsets. Offsets index the **redacted** stream — the
/// only one that is stored.
#[derive(SpacetimeType, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Anchor {
    pub chunk_seq: u64,
    pub byte_offset: u64,
}

// ---------------------------------------------------------------------------
// Tables
// ---------------------------------------------------------------------------

#[spacetimedb::table(accessor = member, public)]
#[derive(Clone, Debug)]
pub struct Member {
    #[primary_key]
    pub identity: Identity,
    #[unique]
    pub github_login: String,
    pub github_id: u64,
    pub display_name: String,
    pub avatar_url: String,
    pub role: Role,
    pub joined_at: Timestamp,
    pub last_seen: Timestamp,
}

/// The title-only org directory card. Exists even while the transcript is
/// private, which is what lets a teammate see *that* you have a session and who
/// is watching it without seeing any output.
///
/// Splitting this from [`Session`] is an authorization boundary, not a view-model
/// convenience. It is what makes "discoverable but not readable" a state the
/// schema can represent at all: this row is org-visible unconditionally, while
/// [`Session`] is filtered per-identity. Without the split, the two would need
/// contradictory rules on one table.
#[spacetimedb::table(accessor = session_listing, public)]
#[derive(Clone, Debug)]
pub struct SessionListing {
    #[primary_key]
    pub session_id: String,
    #[index(btree)]
    pub owner: Identity,
    pub title: String,
    pub coarse_status: CoarseStatus,
    pub started_at: Timestamp,
    pub ended_at: Option<Timestamp>,
}

/// Full session detail, including the transcript cursor.
///
/// **`public`, and filtered per-identity by RLS** — the owner and authorized
/// viewers see it, nobody else does. Note that `public` here means "clients may
/// subscribe", not "everyone sees every row": the rules in [`rls`] decide which
/// rows each identity receives, and `scripts/probe-rls.sh` verifies they do.
///
/// RLS on a `private` table is a publish-time error ("Cannot define RLS rule on
/// private table"), so `public` is not a relaxation here — it is the prerequisite
/// for being filtered at all.
#[spacetimedb::table(accessor = session, public)]
#[derive(Clone, Debug)]
pub struct Session {
    #[primary_key]
    pub session_id: String,
    #[index(btree)]
    pub owner: Identity,
    pub host_label: String,
    pub repo: String,
    pub branch: String,
    pub status: SessionStatus,
    /// Short human string the grid renders verbatim, e.g. `awaiting approval:
    /// Bash`. Spike B measured that `tool_name` is the only reliable thing worth
    /// putting here, so this stays unstructured on purpose.
    pub status_detail: String,
    pub model_label: String,
    pub last_event_at: Timestamp,
    pub exit_reason: Option<String>,
    pub visibility: Visibility,
    /// Redundant mirror of `visibility == Visibility::Org`, maintained by the
    /// same reducers in the same transaction.
    ///
    /// It exists because **RLS cannot compare an enum column to a literal.**
    /// Maincloud rejects `WHERE visibility = 'Org'` with "The literal expression
    /// `Org` cannot be parsed as type `(org: () | private: () | granted: ())`",
    /// in any casing — so the single most important visibility rule in the
    /// system, "this session is shared with the org", is inexpressible against
    /// the typed column. A `bool` is comparable, so the rule keys on this
    /// instead.
    ///
    /// The enum stays the source of truth for the app because it is the honest
    /// type; this is a projection of it for the query planner's benefit. Nothing
    /// may write one without the other — see `set_session_visibility`.
    pub shared_with_org: bool,
    pub transcript_key: String,
    /// Next chunk sequence the archive does **not** yet hold. Strictly
    /// monotonic, advanced only by the Worker, and therefore means "durably in
    /// R2" rather than "a client claimed so". This single field is the live-tail
    /// signal every viewer follows.
    pub chunk_cursor: u64,
    /// Byte offset in the redacted stream matching `chunk_cursor`.
    pub byte_cursor: u64,
    pub event_count: u64,
    pub heartbeat_at: Timestamp,
}

/// Status **transitions only** — never a row per status report.
///
/// `update_session_status` is the hottest reducer in the system and must
/// tolerate being called far more often than it changes anything, so it writes
/// here only when the status actually moves. This is the one table that grows
/// with activity, which is why `prune_status_history` exists.
#[spacetimedb::table(accessor = session_status_history, public)]
#[derive(Clone, Debug)]
pub struct SessionStatusHistory {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    #[index(btree)]
    pub session_id: String,
    pub status: SessionStatus,
    pub source: StatusSource,
    pub at: Timestamp,
}

/// Who has what on screen. Keyed by connection, so
/// `client_disconnected` cleans it up for free — which is the only reason
/// presence can be trusted.
#[spacetimedb::table(accessor = presence, public)]
#[derive(Clone, Debug)]
pub struct Presence {
    #[primary_key]
    pub connection_id: ConnectionId,
    #[index(btree)]
    pub identity: Identity,
    /// `None` means "on the grid, not in any session".
    pub session_id: Option<String>,
    pub focus: Focus,
    pub since: Timestamp,
}

#[spacetimedb::table(accessor = mention, public)]
#[derive(Clone, Debug)]
pub struct Mention {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    #[index(btree)]
    pub session_id: String,
    pub from: Identity,
    #[index(btree)]
    pub to: Identity,
    pub body: String,
    pub anchor: Anchor,
    pub created_at: Timestamp,
    pub read_at: Option<Timestamp>,
    pub delivered_at: Option<Timestamp>,
    pub delivered_channel: Option<String>,
}

#[spacetimedb::table(accessor = notification_route, public)]
#[derive(Clone, Debug)]
pub struct NotificationRoute {
    #[primary_key]
    pub identity: Identity,
    pub slack_user_id: String,
    pub dm_channel: String,
    pub on_mention: bool,
    pub on_awaiting_approval: bool,
    pub enabled: bool,
}

/// Unused in Phase 1's org-wide sharing, and present anyway: retrofitting
/// authorization onto a live system is miserable, and an unused table is free.
#[spacetimedb::table(accessor = access_grant, public,
    index(accessor = session_and_subject, btree(columns = [session_id, subject])))]
#[derive(Clone, Debug)]
pub struct AccessGrant {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    pub session_id: String,
    pub subject: Identity,
    pub level: String,
    pub granted_by: Identity,
    pub at: Timestamp,
}

#[spacetimedb::table(accessor = hub_config, public)]
#[derive(Clone, Debug)]
pub struct HubConfig {
    /// Always 0. A singleton needs a key, and a key needs a name.
    #[primary_key]
    pub id: u32,
    pub github_org: String,
    pub worker_base_url: String,
    pub schema_version: u32,
    /// Set by the Worker at deploy time; the app refuses to upload without it.
    pub relay_enabled: bool,
    /// The only identity permitted to call `advance_transcript_cursor`.
    ///
    /// This is what makes the cursor mean "durably in R2" instead of "a client
    /// said so". Without it the field is a claim; with it, it is a receipt.
    pub worker_identity: Option<Identity>,
}

// ---------------------------------------------------------------------------
// Scheduled tables
// ---------------------------------------------------------------------------

#[spacetimedb::table(accessor = reap_schedule, private, scheduled(reap_stale_sessions))]
pub struct ReapSchedule {
    #[primary_key]
    #[auto_inc]
    pub scheduled_id: u64,
    pub scheduled_at: spacetimedb::ScheduleAt,
}

#[spacetimedb::table(accessor = prune_schedule, private, scheduled(prune_status_history))]
pub struct PruneSchedule {
    #[primary_key]
    #[auto_inc]
    pub scheduled_id: u64,
    pub scheduled_at: spacetimedb::ScheduleAt,
}

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

/// How long a session may go without a heartbeat before the reaper detaches it.
pub const HEARTBEAT_TIMEOUT_MICROS: i64 = 90_000_000;

/// How often the reaper runs.
pub const REAP_INTERVAL: std::time::Duration = std::time::Duration::from_mins(1);

/// How often history pruning runs.
pub const PRUNE_INTERVAL: std::time::Duration = std::time::Duration::from_hours(1);

/// Transitions retained per session. Bounds the one table that grows.
pub const HISTORY_PER_SESSION: usize = 200;

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

/// Seed config and start the timers.
///
/// Scheduling from `init` is transactional: if this reducer fails, neither timer
/// is created.
#[spacetimedb::reducer(init)]
pub fn init(ctx: &ReducerContext) {
    ctx.db.hub_config().insert(HubConfig {
        id: 0,
        github_org: String::new(),
        worker_base_url: String::new(),
        schema_version: 1,
        relay_enabled: false,
        worker_identity: None,
    });
    ctx.db
        .reap_schedule()
        .insert(ReapSchedule { scheduled_id: 0, scheduled_at: REAP_INTERVAL.into() });
    ctx.db
        .prune_schedule()
        .insert(PruneSchedule { scheduled_id: 0, scheduled_at: PRUNE_INTERVAL.into() });
}

/// A client connected. Nothing to do: presence is created by an explicit
/// `set_focus`, because a connection is not the same as a human looking at
/// something.
#[spacetimedb::reducer(client_connected)]
pub fn client_connected(_ctx: &ReducerContext) {}

/// A client went away. Drop its presence rows.
///
/// This is the whole reason presence can be trusted rather than guessed: close
/// the window and presence correctly disappears, while the *session* stays live
/// and keeps streaming under the agent connection.
#[spacetimedb::reducer(client_disconnected)]
pub fn client_disconnected(ctx: &ReducerContext) {
    if let Some(connection_id) = ctx.connection_id() {
        ctx.db.presence().connection_id().delete(connection_id);
    }
}
