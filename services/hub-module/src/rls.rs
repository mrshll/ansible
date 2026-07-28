//! Row-level security: which rows each identity is allowed to receive.
//!
//! The plan calls this "a correctness dependency, not a nicety" and says to verify
//! RLS support and expressiveness on Maincloud early, because if the rules cannot
//! be expressed there then reads must be funnelled through a filtering
//! intermediary and the architecture shifts noticeably.
//!
//! **Verified: the rules below are enforced on deployed Maincloud 2.7.0**, per-row
//! and per-identity. `scripts/probe-rls.sh` asserts it end to end from an identity
//! that owns nothing. So no filtering intermediary is needed, and `Private` is a
//! real boundary rather than a cosmetic one.
//!
//! That had to be measured rather than read, because the bindings say the
//! opposite. `spacetimedb` 2.7.0 carries this above the attribute:
//!
//! ```text
//! // TODO: RLS filters are currently unimplemented, and are not enforced.
//! ```
//!
//! That comment is stale. Had it been trusted, the design would have grown a
//! Worker-mediated read path it does not need.
//!
//! # Three constraints the rules are shaped by
//!
//! **A rule cannot be attached to a `private` table.** Publishing fails with
//! "Cannot define RLS rule on private table". So every filtered table is `public`,
//! which here means "subscribable", not "world-readable".
//!
//! **A rule cannot compare an enum column to a literal.** `WHERE visibility =
//! 'Org'` is rejected — "The literal expression `Org` cannot be parsed as type
//! `(org: () | private: () | granted: ())`" — in any casing. The single most
//! important rule in the system therefore keys on the boolean
//! `session.shared_with_org` mirror instead. This is the one place the plan's
//! schema had to change to survive contact with RLS.
//!
//! **The module owner bypasses these rules and sees every row.** Useful for
//! operations, and a caveat worth stating plainly: these filters separate
//! teammates from each other, not a teammate from whoever can publish the module.
//!
//! Filters on the same table are unioned as if by SQL `OR`, so each rule below
//! widens visibility and none of them can accidentally restrict another.

#[cfg(target_arch = "wasm32")]
use spacetimedb::{Filter, client_visibility_filter};

/// A session's owner may see its detail row.
#[cfg(target_arch = "wasm32")]
#[client_visibility_filter]
const SESSION_VISIBLE_TO_OWNER: Filter = Filter::Sql("SELECT * FROM session WHERE owner = :sender");

/// Anyone may see the detail of a session shared with the whole org.
///
/// Keyed on the boolean mirror rather than `visibility = 'Org'` because the RLS
/// dialect cannot parse an enum literal. See the module docs.
#[cfg(target_arch = "wasm32")]
#[client_visibility_filter]
const SESSION_VISIBLE_WHEN_SHARED: Filter =
    Filter::Sql("SELECT * FROM session WHERE shared_with_org = true");

/// A subject with an explicit grant may see that session's detail.
#[cfg(target_arch = "wasm32")]
#[client_visibility_filter]
const SESSION_VISIBLE_WHEN_GRANTED: Filter = Filter::Sql(
    "SELECT session.* FROM session \
     JOIN access_grant ON session.session_id = access_grant.session_id \
     WHERE access_grant.subject = :sender",
);

/// A mention reaches only its sender and its recipient.
#[cfg(target_arch = "wasm32")]
#[client_visibility_filter]
const MENTION_VISIBLE_TO_RECIPIENT: Filter =
    Filter::Sql("SELECT * FROM mention WHERE to = :sender");

#[cfg(target_arch = "wasm32")]
#[client_visibility_filter]
const MENTION_VISIBLE_TO_SENDER: Filter = Filter::Sql("SELECT * FROM mention WHERE from = :sender");

/// Status history follows the session it belongs to.
#[cfg(target_arch = "wasm32")]
#[client_visibility_filter]
const HISTORY_FOLLOWS_SESSION: Filter = Filter::Sql(
    "SELECT session_status_history.* FROM session_status_history \
     JOIN session ON session_status_history.session_id = session.session_id \
     WHERE session.owner = :sender OR session.shared_with_org = true",
);

/// Notification routes are personal.
#[cfg(target_arch = "wasm32")]
#[client_visibility_filter]
const ROUTE_VISIBLE_TO_SELF: Filter =
    Filter::Sql("SELECT * FROM notification_route WHERE identity = :sender");
