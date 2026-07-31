/**
 * Row-level security: which rows each identity is allowed to receive.
 *
 * The architecture plan calls this "a correctness dependency, not a nicety". It
 * was verified once and the finding survives the port unchanged:
 *
 * **The rules below are enforced on deployed Maincloud**, per-row and
 * per-identity — `scripts/probe-rls.sh` asserts it end to end from an identity
 * that owns nothing. So no filtering intermediary is needed and `Private` is a
 * real boundary rather than a cosmetic one. That had to be measured rather than
 * read, because the 2.7.0 Rust bindings carried `// TODO: RLS filters are
 * currently unimplemented, and are not enforced.` above the attribute. The comment
 * was stale; trusting it would have grown a Worker-mediated read path we do not
 * need. See [ADR 0003](../../../docs/adr/0003-read-authorization.md).
 *
 * # Three constraints these rules are shaped by
 *
 * **A rule cannot be attached to a private table.** Publishing fails with "Cannot
 * define RLS rule on private table", so every filtered table is `public` — which
 * here means "subscribable", not "world-readable".
 *
 * **A rule cannot compare an enum column to a literal.** `WHERE visibility =
 * 'Org'` is rejected: "The literal expression `Org` cannot be parsed as type
 * `(org: () | private: () | granted: ())`", in any casing. The single most
 * important rule in the system therefore keys on the boolean
 * `session.shared_with_org` mirror. This is the one place the schema had to change
 * to survive contact with RLS, and it is why `setSessionVisibility` is the only
 * reducer allowed to write either field.
 *
 * **The module owner bypasses these rules and sees every row.** Useful for
 * operations, and worth stating plainly: these filters separate teammates from
 * each other, not a teammate from whoever can publish the module.
 *
 * Filters on the same table are unioned as if by SQL `OR`, so each rule widens
 * visibility and none can accidentally restrict another.
 *
 * # The SQL is unchanged from the Rust module
 *
 * Identifiers here are snake_case because `CASE_CONVERSION_POLICY` defaults to
 * `SnakeCase`, so the TypeScript `sessionStatusHistory` table is
 * `session_status_history` in SQL exactly as before. These strings are byte-identical
 * to the Rust module's, which is the strongest evidence available that the port did
 * not quietly change who can read what.
 */

import { spacetimedb } from "./schema.js";

const filter = spacetimedb.clientVisibilityFilter;

/** A session's owner may see its detail row. */
export const sessionVisibleToOwner = filter.sql("SELECT * FROM session WHERE owner = :sender");

/**
 * Anyone may see the detail of a session shared with the whole org.
 *
 * Keyed on the boolean mirror rather than `visibility = 'Org'` because the RLS
 * dialect cannot parse an enum literal. See the module docs.
 */
export const sessionVisibleWhenShared = filter.sql(
  "SELECT * FROM session WHERE shared_with_org = true",
);

/** A subject with an explicit grant may see that session's detail. */
export const sessionVisibleWhenGranted = filter.sql(
  "SELECT session.* FROM session " +
    "JOIN access_grant ON session.session_id = access_grant.session_id " +
    "WHERE access_grant.subject = :sender",
);

/** A comment reaches only its sender and its recipient. */
export const mentionVisibleToRecipient = filter.sql("SELECT * FROM mention WHERE to = :sender");

export const mentionVisibleToSender = filter.sql("SELECT * FROM mention WHERE from = :sender");

/** Status history follows the session it belongs to. */
export const historyFollowsSession = filter.sql(
  "SELECT session_status_history.* FROM session_status_history " +
    "JOIN session ON session_status_history.session_id = session.session_id " +
    "WHERE session.owner = :sender OR session.shared_with_org = true",
);

/** Notification routes are personal. */
export const routeVisibleToSelf = filter.sql(
  "SELECT * FROM notification_route WHERE identity = :sender",
);

// Deliberately unfiltered, and each for a reason:
//
// `member`         — the team roster is the team's.
// `session_listing` — the whole point is that a teammate can see *that* you have a
//                    session while its transcript stays private. It carries only a
//                    headline and a coarse Active/Done, never activity detail.
// `presence`       — who is watching what, visible to everyone including the person
//                    being watched. Observation you cannot see is the failure mode
//                    worth designing against.
// `help_request`   — asking for help is not a secret from the team you are asking.
// `hub_config`     — org name, Worker URL, and the Worker's identity. Public config.
