# ADR 0003 — Read authorization for hub rows

- **Status:** accepted
- **Date:** 2026-07-28
- **Deciders:** Spike B
- **Evidence:** [docs/spikes/deployed-round-trip.md](../spikes/deployed-round-trip.md),
  `scripts/probe-rls.sh`

## Context

The product's consent model (assumption A4) is that a transcript is **private by
default** and the owner explicitly shares it. That only means something if a
non-owner genuinely cannot read a private session's rows. The architecture plan
called this out as *"a correctness dependency, not a nicety"* and required verifying
SpacetimeDB's row-level security on Maincloud early, because if the rules could not
be expressed there, *"reads must be funneled through a filtering intermediary and the
architecture shifts noticeably."*

Reads are the open half. Write authorization was never in doubt: every reducer
compares `ctx.sender()` against the row's owner, inside the transaction, on the host,
and no client can skip it.

The `spacetimedb` 2.7.0 bindings state plainly that reads are *not* protected:

```rust
#[cfg(feature = "unstable")]
#[doc(inline, hidden)] // TODO: RLS filters are currently unimplemented, and are not enforced.
pub use spacetimedb_bindings_macro::client_visibility_filter;
```

Taken at face value, that forces the intermediary the plan feared. Spike B measured
it against deployed Maincloud instead.

## Decision

**Use `#[client_visibility_filter]` RLS as the read-authorization mechanism. Do not
build a filtering intermediary.**

The bindings' comment is stale. The filters are enforced, per-row and
per-identity, on Maincloud 2.7.0. `scripts/probe-rls.sh` is the standing evidence and
should be treated as a regression test on the platform, not a one-off: it asserts
**only** from the viewpoint of an identity that owns nothing, because that is the only
viewpoint that can be wrong in a way that matters.

Three constraints follow, and the schema is shaped around them:

**A filtered table must be `public`.** RLS on a `private` table is a publish-time
error — *"Cannot define RLS rule on private table"*. So `public` in this codebase
means "subscribable, then filtered per-identity", **not** "world-readable". That
reads like the opposite of what it means, so it is worth stating in review.

**A rule cannot compare an enum column to a literal.** `WHERE visibility = 'Org'` is
rejected: *"The literal expression `Org` cannot be parsed as type `(org: () |
private: () | granted: ())`"*, in any casing. There is no literal syntax for a unit
variant. The single most important rule in the system is therefore keyed on a
redundant boolean, `session.shared_with_org`, written in the same reducer body — and
so the same transaction — as `visibility`. The enum stays the source of truth for the
app because it is the honest type; the boolean is a projection of it for the query
planner.

**The module owner bypasses RLS entirely.** `Private` is a boundary between
teammates, not between a teammate and whoever holds the publish credential.

## Consequences

**Accepted.**

- **A two-field invariant on `session`.** `visibility` and `shared_with_org` must
  never disagree, because RLS reads one and the app reads the other. Only
  `set_session_visibility` may write either, and it writes both.
- **`Private` does not mean private from the operator.** This must be said out loud
  when describing the consent model to the team; implying otherwise would be a
  promise the platform does not keep.
- **A dependency on unstable, undocumented-as-working behaviour.** The feature is
  behind `features = ["unstable"]` and its own source says it does not work. An
  upstream release could regress it, and the failure mode is silent disclosure rather
  than an error. This is exactly why the probe exists and why it belongs in CI.
- **Filters are unioned as `OR`.** Each rule can only widen visibility, so a new rule
  can never accidentally restrict an existing one — convenient, but it also means a
  too-broad rule cannot be fenced in by adding a narrower one.

**Gained.**

- No filtering intermediary. Viewers subscribe to the hub directly, which is what
  keeps presence and the grid cheap and real-time, and keeps the Worker on the
  transcript path only.
- `Private` is genuinely enforceable, so assumption A4's consent model is real rather
  than cosmetic.
- The `session_listing` / `session` split earns its keep: the listing is org-visible
  unconditionally while `session` is filtered per-identity, which is how
  "discoverable but not readable" becomes a state the schema can represent. One table
  would have needed contradictory rules.

## Alternatives rejected

**Funnel all reads through the Worker.** The plan's stated fallback, and what the
bindings' comment would have forced. Rejected because it is unnecessary: it would put
a Cloudflare hop in front of every grid subscription, give up SpacetimeDB's live
subscription semantics for polling or a bespoke fan-out, and make the Worker a
mandatory dependency of presence — which has nothing to do with transcripts.

**Keep the sensitive tables `private` and read detail through reducers.** Enforced by
the host, and genuinely safe. Rejected because `private` is all-or-nothing: the owner
could not subscribe to their own session row either, so live grid detail would become
request/response and every status change would need polling. It solves read
authorization by removing the feature that needed it.

**Represent `Visibility` as a `u8` or a bool instead of an enum.** Would let RLS
compare the real column and remove the two-field invariant. Rejected because the enum
is the correct type for a three-state concept and appears throughout the reducer
surface and generated bindings; trading type safety across the whole codebase to
satisfy one query dialect is the wrong direction. The redundancy is contained to one
field and one reducer.

## Revisit if

- A SpacetimeDB upgrade changes RLS behaviour in either direction. If enforcement
  regresses, `scripts/probe-rls.sh` fails and `Private` must be treated as broken
  immediately. If the dialect gains enum literals, `shared_with_org` can be deleted
  and the rule keyed on `visibility` directly.
- The GitHub-OAuth-to-`Identity` trust path lands (open question #3's other half).
  These filters authorize *an identity*; they say nothing about whether that identity
  is really who it claims to be. `upsert_member` currently trusts client-asserted
  GitHub claims, and until that is fixed, RLS is enforcing rules about identities that
  are not themselves verified — which bounds what this ADR actually buys.

  The mechanism for fixing it is now known: a reducer can read **verified** claims
  via `ctx.sender_auth().jwt()`, and `Identity` is confirmed to be
  `from_claims(issuer, subject)`. So `upsert_member` reading its login from the token
  rather than its argument is a small change, gated only on whether Maincloud can be
  configured to trust our Worker as a JWT issuer. See
  [deployed-round-trip.md §9](../spikes/deployed-round-trip.md#9-identity-a-question-answered-early).
- `Granted` sharing becomes load-bearing. Its rule is a `JOIN` against
  `access_grant`, which is the most complex filter here and the least exercised; it
  deserves its own probe assertions before Phase 1 relies on it.
