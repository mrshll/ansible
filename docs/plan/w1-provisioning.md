# W1 — provisioning runbook

Everything that must exist outside this repository before the MVP can be built,
in the order to do it, with a command that proves each step worked.

This is [W1 of the Phase 1 execution plan](phase-1-execution.md#w1--provisioning).
It is mostly account admin rather than engineering, and it is the reason half of
Spike B is unfinished: the capture path is built and byte-exact, but no Worker,
no R2 bucket, and no SpacetimeDB module has ever existed
([capture-round-trip §6](../spikes/capture-round-trip.md#6-what-is-blocked-and-why)).

**Provisioned means verified, not created.** Every step below ends in a command
whose output is the evidence. [§7](#7-verification-checklist) collects them so
the whole thing can be re-run on a new machine or by a second person.

---

## 1. What each resource unblocks

| Resource | Unblocks | Without it |
|---|---|---|
| SpacetimeDB Maincloud database | W2, W3, W5, W7 | No grid, no presence, no status — the product |
| Cloudflare account + R2 bucket | W2, W6, W8 | No transcripts; the viewer has nothing to read |
| GitHub OAuth app (org-owned) | W2, W5, W7 | No identity, so no ownership check and no enforceable `Private` |
| A dev machine with interactive `claude` auth | W4 | The grid cannot show `AwaitingApproval` — the highest-value signal |
| Slack app | W9 | Mentions still work in-app; only DM delivery is missing |
| Durable Objects enabled | W10 only | Cursor-follow MVP still ships; sub-second live tail does not |

The first four are on the critical path. Slack and Durable Objects are not —
start them last, and do not let either block W2.

**One thing to know before you start:** [W2](phase-1-execution.md#w2--spike-c-identity-and-rls-the-last-gating-question)
may change what is needed here. If the identity hypothesis fails, all writes
route through the Worker under one service identity, and the per-user identity
plumbing looks different. Provision what is below, but do not build anything on
top of it until W2 reports.

---

## 2. Who has to do what

| Step | Needs |
|---|---|
| GitHub OAuth app | **Org owner.** An app created under a personal account cannot be trusted to represent org membership |
| Cloudflare account, R2, API tokens | Whoever holds billing. R2 requires a payment method even inside the free tier |
| SpacetimeDB Maincloud | Any dev, unless the team wants one shared org-owned account — decide now, because moving a published database later is worse than deciding it now |
| Slack app | A workspace admin, or a member if your workspace allows app creation |
| `claude` interactive auth | Any dev with a Claude subscription, on a real machine — see [§6](#6-step-5--a-machine-that-can-produce-a-real-approval-prompt) |

---

## 3. Names and environments, decided up front

Two of everything: a dev environment that W6's failure-injection tests can be
violent with, and a prod environment the team dogfoods on. Sharing one is a
false economy — W6's kill-the-Worker-mid-session tests will corrupt state on
purpose, and doing that to real transcripts is unrecoverable.

| Thing | dev | prod |
|---|---|---|
| SpacetimeDB database | `ansible-dev` | `ansible` |
| R2 bucket | `ansible-transcripts-dev` | `ansible-transcripts` |
| Worker | `ansible-transcript-worker-dev` | `ansible-transcript-worker` |
| GitHub OAuth app | one app, loopback redirect covers both | same |
| Slack app | one app; a private channel for dev | same |

The app never hardcodes any of these. They land in the `hub_config` singleton
(`github_org`, `worker_base_url`, feature flags) and in a local config file
naming the database — that is what `hub_config` is for.

---

## 4. Step 1 — SpacetimeDB Maincloud

Do this first. It is the one whose answers W2 depends on.

```bash
# The CLI, per install.spacetimedb.com — check for a current install command
# rather than assuming this one.
curl -sSf https://install.spacetimedb.com | sh
spacetime version
spacetime login                       # opens a browser
spacetime server list                 # confirm maincloud is present
```

Publish a throwaway module to prove the whole path end to end, rather than
trusting that login means deployable:

```bash
spacetime init --lang rust /tmp/hello && cd /tmp/hello
spacetime publish -s maincloud ansible-provision-check
spacetime logs -s maincloud ansible-provision-check
spacetime delete -s maincloud ansible-provision-check   # clean up
```

Then create the two real database names (empty is fine — W3 publishes into
them).

### Three things to find out while you are in there

These are W2's inputs, and none of them can be answered from this repo:

1. **Can a Maincloud database be configured to trust a third-party JWT issuer**
   (an issuer URL plus a JWKS endpoint)? This is the single fact
   [W2](phase-1-execution.md#w2--spike-c-identity-and-rls-the-last-gating-question)
   turns on. If yes, our Worker mints tokens and `Identity` derives from
   `(issuer, subject)`, so a GitHub login maps to a stable identity. If no, the
   Worker must intermediate every write and the architecture shifts.
2. **Can a reducer read claims from the caller's token**, or only `ctx.sender`?
   `upsert_member_from_token()` is specified to read *verified* claims and never
   a client-asserted login; if only the identity is available, membership has to
   be established by a trusted writer instead.
3. **What are the migration, backup, and pricing terms** at ~10 engineers
   (open question #10)? Note what you find; low design impact, real project
   risk.

Write the answers into a comment on the W2 issue or PR. They are the whole
input to that spike.

---

## 5. Step 2 — Cloudflare, R2, and the Worker

```bash
npm i -g wrangler         # or use npx wrangler throughout
wrangler login
wrangler whoami           # note the account id
```

Create both buckets:

```bash
wrangler r2 bucket create ansible-transcripts-dev
wrangler r2 bucket create ansible-transcripts
wrangler r2 bucket list
```

Prove a write and a read actually work, because an enabled R2 and a writable R2
are different states:

```bash
echo '{"provision":"check"}' > /tmp/check.json
wrangler r2 object put ansible-transcripts-dev/provision-check.json --file /tmp/check.json
wrangler r2 object get ansible-transcripts-dev/provision-check.json --file /tmp/out.json
diff /tmp/check.json /tmp/out.json && echo "R2 round trip OK"
wrangler r2 object delete ansible-transcripts-dev/provision-check.json
```

### Two things to confirm at the console

- **Durable Objects.** Needed only for the relay in W10, and the MVP ships
  without it, so confirm what your plan includes but do not block on it. If DOs
  need a paid Workers plan, that is a $5-scale decision to make when W10 starts,
  not now.
- **Object lifecycle rules.** Do not set any yet. Retention is open question #5
  and a lifecycle rule that deletes chunks under a live cursor breaks
  reassembly for anyone holding a later cursor
  ([capture-round-trip §8](../spikes/capture-round-trip.md#8-open-questions-this-moves)).
  Decide the policy first, then encode it.

### Cost sanity check, from measured numbers

Spike B estimated **~1.4M R2 writes/month** at 10 engineers × 5 sessions/day,
driven by the 1-second time-triggered flush — an idle-but-open session still
flushes every second
([capture-round-trip §4](../spikes/capture-round-trip.md#cost-at-team-scale)).
That is around or just past the usual free-tier allowance for class A
operations, on the *first* month of real use.

Two conclusions. Attach a payment method rather than relying on a free tier
this workload sits on the boundary of. And treat the adaptive flush (1s while
active, backing off when idle, estimated ~10× fewer writes with no fidelity
loss) as a W6/W10 deliverable rather than an optimization — the deployed numbers
from W6 are exactly what it needs to pick a curve.

---

## 6. Step 3 — the GitHub OAuth app

**Create it under the org, not a personal account.** Org membership *is* hub
membership, so the app that establishes identity has to be an org artifact.

Settings → Developer settings → OAuth Apps → New OAuth App:

| Field | Value |
|---|---|
| Application name | `ansible` |
| Homepage URL | the repo URL is fine |
| Authorization callback URL | `http://127.0.0.1/callback` — a loopback redirect for a desktop app. Confirm whether your setup needs the exact port registered; if it does, pin one port and record it here |

Scopes needed at authorization time: **`read:user` and `read:org`. Nothing
else.** The app never reads code, never opens PRs, and never calls the GitHub
API for anything but identity and org membership, so any repo scope would be
strictly extra exposure — worth stating in the consent screen review.

### The client secret does not go in the desktop app

A desktop binary cannot hold a secret; anyone with the app has it. So the
**Worker performs the code-for-token exchange**: the app opens the browser,
receives the code on loopback, posts it to the Worker, and the Worker — holding
the client secret — exchanges it, verifies org membership, and returns what the
app needs. That is the same Worker W2 needs for minting hub tokens, so this adds
no new component.

If loopback redirects turn out to be awkward, the GitHub **device flow** is the
fallback: no redirect URL and no client secret in the client, at the cost of a
code the user types. Either is fine; pick one in W2 and record it.

Prove the app exists and the scopes are right by completing one authorization
by hand and checking the org read works:

```bash
# after obtaining a token through the flow, with $TOKEN set:
curl -sS -H "Authorization: Bearer $TOKEN" https://api.github.com/user | jq '{login,id}'
curl -sS -H "Authorization: Bearer $TOKEN" https://api.github.com/user/orgs | jq '.[].login'
```

The second command must list your org. If it does not, the token lacks
`read:org` or the org restricts OAuth app access — an org owner has to approve
the app.

---

## 7. Step 4 — a machine that can produce a real approval prompt

Not a cloud resource, but [W4](phase-1-execution.md#w4--the-awaitingapproval-producer)
is blocked without it, and W4 is the grid's most valuable signal.

The container this project has been developed in **forces a permissive
permission mode** — `--permission-mode manual` was silently downgraded to
`default` and `bypassPermissions` is refused for root — so a genuine approval
prompt has never been observed, and the deny path had to be reached with a hook
returning `deny` instead
([hook-coverage §5](../spikes/hook-coverage.md#5-environment-limits)).

What is needed: a normal dev machine with interactive `claude` OAuth (not
`--print` with injected credentials) where a tool call actually stops and asks.
Verify with:

```bash
claude --version                       # v2.1.220 or later
claude                                 # interactive; ask it to run a shell command
                                       # a permission prompt must appear on screen
```

If the prompt appears, W4 can start. Also note whether `Notification` fires when
it does — that is one of the two hook questions W4 closes.

---

## 8. Step 5 — Slack (last, and not on the critical path)

api.slack.com/apps → Create New App → From scratch, in the team workspace.

Bot token scopes: `chat:write` to post, `im:write` to open a DM, and
`users:read.email` if the bridge maps GitHub identity to Slack user by email
(which is the cheapest mapping; the alternative is asking each person once and
storing it in `notification_route.slack_user_id`).

Install to the workspace, then prove delivery:

```bash
curl -sS -X POST https://slack.com/api/chat.postMessage \
  -H "Authorization: Bearer $SLACK_BOT_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"channel":"<your-slack-user-id>","text":"ansible provisioning check"}' | jq .ok
```

`true` and a DM in Slack means W9's delivery path exists.

---

## 9. Where each secret lives

Nothing in this table belongs in the repository, in a PR body, or in a chat
message. The rightmost column is the only correct home for each.

| Secret | Held by | Set with |
|---|---|---|
| GitHub OAuth client secret | the Worker | `wrangler secret put GITHUB_CLIENT_SECRET` |
| Hub token signing key | the Worker | `wrangler secret put HUB_JWT_SIGNING_KEY` (W2 decides the algorithm) |
| Slack bot token | the bridge Worker | `wrangler secret put SLACK_BOT_TOKEN` |
| Cloudflare API token (CI deploys) | GitHub Actions | repo secret `CLOUDFLARE_API_TOKEN`, scoped to Workers + R2 edit on this account only |
| SpacetimeDB deploy credentials | the deploying dev, later CI | `spacetime login`; a CI identity if W6 automates deploys |
| The user's GitHub token | the desktop app | OS keychain on macOS, Secret Service on Linux — never a file in the repo or a dotfile |
| Worker bearer token for chunk PUTs | the desktop app core | Derived per session; never reaches the webview (see [the ChunkSource correction](phase-1-execution.md#3-one-plan-level-correction-chunksource-belongs-in-rust)) |

The GitHub OAuth **client id** is not a secret and can live in config.

---

## 10. Verification checklist

Provisioning is done when every line passes. Paste the results (not the
secrets) into the W2 kickoff.

- [ ] `spacetime server list` includes maincloud, and `spacetime login` succeeded
- [ ] A throwaway module published to maincloud, logged, and deleted
- [ ] `ansible-dev` and `ansible` database names reserved
- [ ] The three Maincloud questions in [§4](#three-things-to-find-out-while-you-are-in-there) answered in writing
- [ ] `wrangler whoami` shows the right account
- [ ] Both R2 buckets exist; the put → get → diff → delete round trip passed
- [ ] Whether Durable Objects are available on this plan is recorded (needed for W10 only)
- [ ] No R2 lifecycle rule is set (retention is open question #5)
- [ ] Org-owned GitHub OAuth app exists; client id recorded, secret stored in the Worker
- [ ] A hand-run authorization returned a token whose `/user/orgs` lists the org
- [ ] A dev machine produces a real interactive approval prompt in `claude`
- [ ] Slack `chat.postMessage` returned `ok: true` and a DM arrived
- [ ] `CLOUDFLARE_API_TOKEN` set as a repo secret, scoped to this account

### Values to hand back

Not secret, and W3/W6 need them: the Cloudflare account id, both bucket names,
both Worker base URLs, both database names, the GitHub org login, the OAuth
client id, and the registered loopback port if one had to be pinned. These are
the initial contents of `hub_config`.

---

## 11. Deliberately not provisioned

- **Apple Developer account, signing, notarization** — W11, and gated on the
  platform decision in [§6 of the execution plan](phase-1-execution.md#6-decisions-that-need-a-person-not-an-experiment).
- **Durable Objects, if they need a plan change** — W10 only, decide with the
  measurements in hand.
- **Error reporting, analytics, crash reporting** — not in the MVP.
- **A custom domain for the Worker** — `*.workers.dev` is fine until it isn't.
- **Anything to do with a second plane** (open question #7).
