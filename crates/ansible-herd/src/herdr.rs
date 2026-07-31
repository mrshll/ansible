//! A client for Herdr's local socket: newline-delimited JSON over a Unix socket.
//!
//! # Why raw sockets and not the CLI
//!
//! Herdr's plugin guide recommends shelling out to `HERDR_BIN_PATH`, because that
//! hides the difference between a Unix socket and a Windows named pipe. This
//! daemon speaks the socket directly for two reasons: it needs a subscription,
//! which is a connection held open across many pushed lines rather than one
//! command, and it reconciles on a timer, where a process spawn per poll is real
//! cost for no benefit. The price is that the plugin declares
//! `platforms = ["linux", "macos"]`. Windows support means adding a named-pipe
//! transport here and nothing else.
//!
//! The CLI *is* used for one thing — `terminal session observe`, in
//! [`crate::teleport`] — because the live frame stream has no documented socket
//! method.
//!
//! # Defensive parsing
//!
//! Every reader below pulls named fields out of a [`Value`] with fallbacks and
//! tolerates absence, rather than deriving `Deserialize` for Herdr's response
//! types. That is a deliberate choice for code written against documentation
//! instead of against a running server: the documented advice is to "handle
//! unknown fields gracefully", a strict struct would turn one renamed field into
//! a dead daemon, and this repo's convention is that fixtures come from
//! recordings. `scripts/capture-herdr-fixtures.sh` records the real shapes; until
//! someone runs it, the parsers here are the honest interface.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::model::{Status, agent_key};

/// How long a single request may take before the daemon gives up and retries on
/// the next tick. Local socket calls are sub-millisecond in the normal case; this
/// only bounds a wedged server.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Resolve the socket the way Herdr documents it.
#[must_use]
pub fn socket_path() -> PathBuf {
    let env = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
    resolve_socket_path(
        env("HERDR_SOCKET_PATH").as_deref(),
        env("HERDR_SESSION").as_deref(),
        env("XDG_CONFIG_HOME").as_deref(),
        env("HOME").as_deref(),
    )
}

/// The resolution order, as a function of its inputs rather than of the process
/// environment.
///
/// Written this way because `std::env::set_var` is `unsafe` in edition 2024 and
/// this workspace forbids unsafe, so a test that swaps environment variables
/// cannot be written at all. Taking the values as arguments makes the rule
/// testable and, incidentally, states it in one place.
///
/// `HERDR_SOCKET_PATH` wins, because that is what Herdr injects into every plugin
/// process and it is authoritative for the session that launched us. Then
/// `HERDR_SESSION` for a named session, then the default session socket. The
/// explicit `--session` flag Herdr's own CLI accepts has no equivalent here; a
/// caller who needs it can set `HERDR_SOCKET_PATH`.
#[must_use]
fn resolve_socket_path(
    explicit: Option<&str>,
    session: Option<&str>,
    config_home: Option<&str>,
    home: Option<&str>,
) -> PathBuf {
    if let Some(path) = explicit {
        return PathBuf::from(path);
    }
    let config = config_home
        .map_or_else(|| PathBuf::from(home.unwrap_or(".")).join(".config"), PathBuf::from);
    let base = config.join("herdr");
    match session {
        Some(name) => base.join("sessions").join(name).join("herdr.sock"),
        None => base.join("herdr.sock"),
    }
}

/// One request/response connection.
pub struct Client {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: u64,
}

impl Client {
    /// Connect to a Herdr server.
    ///
    /// # Errors
    /// When the socket is missing or refuses the connection, which is the normal
    /// state when Herdr is not running.
    pub fn connect(path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(path)
            .with_context(|| format!("connecting to Herdr at {}", path.display()))?;
        stream.set_read_timeout(Some(CALL_TIMEOUT))?;
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Self { stream, reader, next_id: 1 })
    }

    /// Send one request and return its `result` object.
    ///
    /// # Errors
    /// Transport failure, or a Herdr `error` response, which is reported with its
    /// code so a caller can branch on `feature_disabled` or `not_found`.
    pub fn call(&mut self, method: &str, params: &Value) -> Result<Value> {
        let id = format!("ah{}", self.next_id);
        self.next_id += 1;

        let line = serde_json::to_string(&json!({"id": &id, "method": method, "params": params}))?;
        self.stream.write_all(line.as_bytes())?;
        self.stream.write_all(b"\n")?;
        self.stream.flush()?;

        // Skip anything that is not our reply. Nothing subscribes on this
        // connection, so in practice the first line is the answer; being tolerant
        // here costs nothing and survives a server that annotates streams.
        for _ in 0..64 {
            let mut line = String::new();
            if self.reader.read_line(&mut line)? == 0 {
                bail!("Herdr closed the connection during {method}");
            }
            let value: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if value.get("id").and_then(Value::as_str) != Some(id.as_str()) {
                continue;
            }
            if let Some(err) = value.get("error") {
                let code = err.get("code").and_then(Value::as_str).unwrap_or("error");
                let message = err.get("message").and_then(Value::as_str).unwrap_or("");
                bail!("{method} failed: {code}: {message}");
            }
            return Ok(value.get("result").cloned().unwrap_or(Value::Null));
        }
        bail!("no response to {method}")
    }

    /// Check the server is alive and describe it.
    ///
    /// Observed on 0.7.5 (`scripts/probe-herdr.sh` check A3): `pong` carries
    /// `version` as a string, `protocol` as a **number**, and a `capabilities`
    /// object. The first draft looked for `protocol_version` and found nothing —
    /// harmless, since only `doctor` printed it, but a good illustration of why
    /// every one of these readers probes rather than assumes.
    ///
    /// `capabilities` is worth surfacing: `live_handoff` says a server can be
    /// replaced without killing panes, which is exactly the event that makes the
    /// daemon's socket go away underneath it.
    ///
    /// # Errors
    /// Whatever [`Client::call`] reports.
    pub fn ping(&mut self) -> Result<Option<String>> {
        let result = self.call("ping", &json!({}))?;
        let version = field(&result, &["version"]);
        let protocol = result
            .get("protocol")
            .and_then(|p| {
                p.as_u64().map(|n| n.to_string()).or_else(|| p.as_str().map(String::from))
            })
            .or_else(|| field(&result, &["protocol_version"]));
        let capabilities = result
            .get("capabilities")
            .and_then(Value::as_object)
            .map(|caps| {
                caps.iter()
                    .filter(|(_, v)| v.as_bool() == Some(true))
                    .map(|(k, _)| k.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .filter(|caps| !caps.is_empty());

        Ok(match (version, protocol, capabilities) {
            (None, None, _) => None,
            (v, p, caps) => Some(format!(
                "{}{}{}",
                v.unwrap_or_else(|| "?".into()),
                p.map(|p| format!(" protocol {p}")).unwrap_or_default(),
                caps.map(|c| format!(" [{c}]")).unwrap_or_default(),
            )),
        })
    }

    /// Read every agent Herdr knows about.
    ///
    /// `session.snapshot` is the documented bootstrap call and carries workspace
    /// and tab records too, which is where a pane's human-readable labels live.
    /// When it is unavailable — an older server, or a renamed field — this falls
    /// back to `agent.list`, which loses the labels but keeps the statuses. A
    /// degraded roster beats no roster.
    ///
    /// # Errors
    /// When both calls fail.
    pub fn read_agents(&mut self) -> Result<Vec<PaneAgent>> {
        match self.call("session.snapshot", &json!({})) {
            Ok(snapshot) => Ok(parse_snapshot(&snapshot)),
            Err(snapshot_err) => {
                let agents = self.call("agent.list", &json!({})).map_err(|list_err| {
                    anyhow!("session.snapshot: {snapshot_err}; agent.list: {list_err}")
                })?;
                Ok(parse_agent_list(&agents, &Labels::default()))
            }
        }
    }

    /// Patch display-only pane metadata.
    ///
    /// Presence lands in Herdr's own sidebar this way rather than only in our
    /// roster: tokens render as `$name` in an Agent row, so "two people are
    /// watching this" shows up where the owner is already looking. Semantic state
    /// is deliberately untouched — Herdr owns `blocked`, and reporting a second
    /// opinion would create the two-sources-of-truth problem its docs warn about.
    ///
    /// # Errors
    /// Whatever [`Client::call`] reports.
    pub fn report_tokens(
        &mut self,
        pane_id: &str,
        source: &str,
        tokens: &[(&str, Option<&str>)],
        ttl_ms: Option<u64>,
        seq: u64,
    ) -> Result<()> {
        let mut map = serde_json::Map::new();
        for (name, value) in tokens {
            map.insert(
                (*name).to_string(),
                value.map_or(Value::Null, |v| Value::String(v.to_string())),
            );
        }
        let mut params = json!({
            "pane_id": pane_id,
            "source": source,
            "tokens": Value::Object(map),
            "seq": seq,
        });
        if let (Some(ttl), Some(obj)) = (ttl_ms, params.as_object_mut()) {
            obj.insert("ttl_ms".into(), json!(ttl));
        }
        self.call("pane.report_metadata", &params).map(|_| ())
    }

    /// Show a toast through whatever delivery the user configured.
    ///
    /// Returns the reason Herdr gives, so a caller can tell "shown" from
    /// "`rate_limited`" instead of assuming a teammate was told.
    ///
    /// # Errors
    /// Whatever [`Client::call`] reports.
    pub fn notify(&mut self, title: &str, body: &str, urgent: bool) -> Result<String> {
        let result = self.call(
            "notification.show",
            &json!({
                "title": title,
                "body": body,
                "sound": if urgent { "request" } else { "none" },
            }),
        )?;
        Ok(field(&result, &["reason"]).unwrap_or_else(|| "unknown".into()))
    }

    /// Type text into a pane without submitting it.
    ///
    /// This is the consent-preserving half of "a teammate can help": their words
    /// land in the composer and the owner presses Enter. See
    /// [`Client::submit_prompt`] for the half that does not wait.
    ///
    /// # Errors
    /// Whatever [`Client::call`] reports.
    pub fn send_text(&mut self, pane_id: &str, text: &str) -> Result<()> {
        self.call("pane.send_text", &json!({"pane_id": pane_id, "text": text})).map(|_| ())
    }

    /// Submit text to an agent as a prompt.
    ///
    /// Only ever reached through an explicit opt-in — see `inbox --submit` and
    /// `allow_submit` in the config.
    ///
    /// # Errors
    /// Whatever [`Client::call`] reports.
    pub fn submit_prompt(&mut self, target: &str, text: &str) -> Result<()> {
        self.call("agent.prompt", &json!({"target": target, "text": text})).map(|_| ())
    }

    /// Install a declarative Agents-view projection that floats attention to the
    /// top.
    ///
    /// # Errors
    /// Whatever [`Client::call`] reports.
    pub fn set_attention_view(&mut self, source: &str) -> Result<()> {
        self.call(
            "agent.view.set",
            &json!({
                "source": source,
                "label": "herd",
                "sort": [
                    {"field": "attention", "order": "desc"},
                    {"field": "state_change_seq", "order": "desc"},
                ],
            }),
        )
        .map(|_| ())
    }

    /// Clear a view this plugin owns, leaving another owner's view alone.
    ///
    /// # Errors
    /// Whatever [`Client::call`] reports.
    pub fn clear_attention_view(&mut self, source: &str) -> Result<()> {
        self.call("agent.view.clear", &json!({"source": source})).map(|_| ())
    }

    /// Open one of this plugin's own manifest pane entrypoints.
    ///
    /// Used by the roster to open a teleport view *beside itself* rather than
    /// replacing itself: Herdr owns the split, the focus, and the close, so the
    /// viewer is an ordinary pane the user can move, zoom, or shut like any other.
    ///
    /// # Errors
    /// Whatever [`Client::call`] reports — including `plugin_disabled` and
    /// `platform_unsupported`, which are worth showing verbatim.
    /// `placement` of `None` leaves the manifest's own placement in force, which
    /// is the right default: the manifest is where a pane's shape is declared, and
    /// overriding it from a caller should be a deliberate exception.
    pub fn open_plugin_pane(
        &mut self,
        plugin_id: &str,
        entrypoint: &str,
        placement: Option<&str>,
        env: &[(&str, &str)],
    ) -> Result<()> {
        let mut map = serde_json::Map::new();
        for (key, value) in env {
            map.insert((*key).to_string(), Value::String((*value).to_string()));
        }
        let mut params = json!({
            "plugin_id": plugin_id,
            "entrypoint": entrypoint,
            "env": Value::Object(map),
            "focus": true,
        });
        if let (Some(placement), Some(object)) = (placement, params.as_object_mut()) {
            object.insert("placement".into(), Value::String(placement.to_string()));
        }
        self.call("plugin.pane.open", &params).map(|_| ())
    }
}

/// One Herdr pane that currently hosts an agent, flattened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneAgent {
    pub pane_id: String,
    pub workspace_id: Option<String>,
    pub workspace: Option<String>,
    pub tab: Option<String>,
    pub agent: String,
    pub status: Status,
    /// Herdr's `terminal_title_stripped`: the OSC title with a leading spinner
    /// glyph removed. For Claude Code this is a short summary of the current task,
    /// which makes it the best free source for a headline.
    pub terminal_title: Option<String>,
    /// The cwd of the process actually controlling the PTY when Herdr can resolve
    /// it, else the pane cwd. Used to name the repo.
    pub cwd: Option<String>,
    pub branch: Option<String>,
    /// Herdr's own status-transition counter, present on agent records.
    ///
    /// Better than comparing status strings for deciding "the status moved": it
    /// also catches a transition that lands back on the same state, which is
    /// exactly what a permission prompt answered and immediately re-raised looks
    /// like.
    pub state_change_seq: Option<u64>,
}

impl PaneAgent {
    #[must_use]
    pub fn key(&self, login: &str, host: &str) -> String {
        agent_key(login, host, &self.pane_id)
    }
}

/// Workspace and tab labels, keyed by id, so a pane row can be named.
#[derive(Debug, Default, Clone)]
struct Labels {
    workspaces: std::collections::BTreeMap<String, String>,
    tabs: std::collections::BTreeMap<String, String>,
    branches: std::collections::BTreeMap<String, String>,
    cwds: std::collections::BTreeMap<String, String>,
}

fn parse_snapshot(result: &Value) -> Vec<PaneAgent> {
    // Observed on 0.7.5: `session.snapshot` returns
    // `{"type": "...", "snapshot": {agents, panes, tabs, workspaces, ...}}`, one
    // level deeper than this code first assumed. Every *field* name inside was
    // right; only the envelope was wrong — and that single level was enough to
    // empty the roster and cascade into five other checks reporting "no pane to
    // work with". `scripts/probe-herdr.sh` check B2.
    let snapshot = result.get("snapshot").unwrap_or(result);
    parse_snapshot_body(snapshot)
}

fn parse_snapshot_body(snapshot: &Value) -> Vec<PaneAgent> {
    let mut labels = Labels::default();
    for ws in array(snapshot, &["workspaces", "workspace_records"]) {
        let Some(id) = field(ws, &["workspace_id", "id"]) else { continue };
        if let Some(label) = field(ws, &["label", "name"]) {
            labels.workspaces.insert(id.clone(), label);
        }
        // Observed: a workspace row is {workspace_id, label, number, focused,
        // agent_status, pane_count, tab_count, active_tab_id} — no cwd, and no
        // worktree provenance. So the repo and branch have to come from the pane's
        // own `cwd`/`foreground_cwd`, which are both present there.
        if let Some(cwd) = field(ws, &["cwd"]) {
            labels.cwds.insert(id.clone(), cwd);
        }
        // Worktree provenance is on the workspace record, and it is the cheapest
        // true answer to "what is this session working on".
        if let Some(branch) = ws
            .get("worktree")
            .and_then(|w| field(w, &["branch"]))
            .or_else(|| field(ws, &["branch"]))
        {
            labels.branches.insert(id, branch);
        }
    }
    for tab in array(snapshot, &["tabs", "tab_records"]) {
        if let (Some(id), Some(label)) =
            (field(tab, &["tab_id", "id"]), field(tab, &["label", "name"]))
        {
            labels.tabs.insert(id, label);
        }
    }
    parse_agent_list(snapshot, &labels)
}

fn parse_agent_list(root: &Value, labels: &Labels) -> Vec<PaneAgent> {
    array(root, &["agents", "agent_records"])
        .into_iter()
        .filter_map(|agent| parse_agent(agent, labels))
        .collect()
}

fn parse_agent(agent: &Value, labels: &Labels) -> Option<PaneAgent> {
    let pane_id = field(agent, &["pane_id"])?;
    let workspace_id = field(agent, &["workspace_id"]);
    let status = field(agent, &["agent_status", "status", "state"])
        .map_or(Status::Unknown, |raw| Status::parse(&raw));

    let workspace = field(agent, &["workspace_label"])
        .or_else(|| workspace_id.as_ref().and_then(|id| labels.workspaces.get(id).cloned()));
    let tab = field(agent, &["tab_label"])
        .or_else(|| field(agent, &["tab_id"]).and_then(|id| labels.tabs.get(&id).cloned()));
    let branch = workspace_id.as_ref().and_then(|id| labels.branches.get(id).cloned());
    let cwd = field(agent, &["foreground_cwd", "cwd"])
        .or_else(|| workspace_id.as_ref().and_then(|id| labels.cwds.get(id).cloned()));

    Some(PaneAgent {
        pane_id,
        workspace_id,
        workspace,
        tab,
        // A pane in the agents list always has an agent; `display_agent` is the
        // presentation override and wins when a user set one.
        agent: field(agent, &["display_agent", "agent", "kind", "agent_kind"])
            .unwrap_or_else(|| "agent".into()),
        status,
        terminal_title: field(agent, &["terminal_title_stripped", "terminal_title"]),
        cwd,
        branch,
        state_change_seq: agent.get("state_change_seq").and_then(Value::as_u64),
    })
}

/// Read the first present string among `keys`.
fn field(value: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(found) = value.get(*key).and_then(Value::as_str) {
            if !found.is_empty() {
                return Some(found.to_string());
            }
        }
    }
    None
}

/// Read the first present array among `keys`.
fn array<'a>(value: &'a Value, keys: &[&str]) -> Vec<&'a Value> {
    for key in keys {
        if let Some(items) = value.get(*key).and_then(Value::as_array) {
            return items.iter().collect();
        }
    }
    Vec::new()
}

/// A held-open subscription, feeding pushed events into a channel as nudges.
///
/// # Every subscription needs a `pane_id`
///
/// Measured, not assumed. An unfiltered subscription is rejected outright:
///
/// ```text
/// {"error":{"code":"invalid_request","message":"invalid request: missing field `pane_id`"}}
/// ```
///
/// So there is no wildcard event stream, and the shape of the daemon follows from
/// that: it **polls the snapshot to discover panes** and **subscribes per pane** to
/// hear about their transitions immediately. Discovery is a snapshot call on a local
/// socket, which is cheap; the transitions are the part where a second of latency
/// would be felt, and those are pushed.
///
/// The daemon does not read event *contents* — an event only means "reconcile now
/// rather than on the next tick", so a payload shape that drifts costs nothing.
/// Reconcile from state, subscribe for speed.
pub struct Events {
    stream: UnixStream,
    /// The panes this subscription covers, so the daemon can tell when the set has
    /// moved and it needs a new one.
    panes: Vec<String>,
}

impl Events {
    /// Subscribe to status changes for `panes`, reading in a background thread.
    ///
    /// The thread exits when the connection drops, and [`Events::shutdown`] drops
    /// it deliberately. The caller's poll loop keeps working either way.
    ///
    /// # Errors
    /// When the connection cannot be opened, or Herdr rejects the subscription.
    pub fn spawn(path: &Path, panes: &[String], tx: Sender<()>) -> Result<Self> {
        if panes.is_empty() {
            bail!("no panes to subscribe to");
        }
        let stream = UnixStream::connect(path)?;
        let mut writer = stream.try_clone()?;

        // One entry per pane per event type. `pane.agent_status_changed` is the one
        // that matters; `pane.updated` catches a terminal-title change, which is
        // where a headline comes from.
        let subscriptions: Vec<Value> = panes
            .iter()
            .flat_map(|pane| {
                ["pane.agent_status_changed", "pane.updated"]
                    .into_iter()
                    .map(move |kind| json!({"type": kind, "pane_id": pane}))
            })
            .collect();
        let request = json!({
            "id": "ah-sub",
            "method": "events.subscribe",
            "params": {"subscriptions": subscriptions},
        });
        writer.write_all(serde_json::to_string(&request)?.as_bytes())?;
        writer.write_all(b"\n")?;
        writer.flush()?;

        let mut reader = BufReader::new(stream.try_clone()?);
        let mut ack = String::new();
        reader.read_line(&mut ack)?;
        let parsed: Value = serde_json::from_str(&ack).unwrap_or(Value::Null);
        if let Some(err) = parsed.get("error") {
            bail!("events.subscribe rejected: {err}");
        }

        std::thread::spawn(move || {
            for line in reader.lines() {
                match line {
                    Ok(_) => {
                        if tx.send(()).is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
        });

        Ok(Self { stream, panes: panes.to_vec() })
    }

    /// Whether this subscription still covers exactly `panes`.
    #[must_use]
    pub fn covers(&self, panes: &[String]) -> bool {
        self.panes == panes
    }

    /// Close the connection, which ends the reader thread.
    pub fn shutdown(&self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

impl Drop for Events {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shaped from the documented `session.snapshot` description: version
    /// metadata, workspace/tab/pane records, agent records. Doc-derived, not
    /// recorded — `scripts/capture-herdr-fixtures.sh` replaces it with the real
    /// thing.
    /// Recorded from Herdr 0.7.5 by `scripts/probe-herdr.sh` — no longer
    /// doc-derived. Note the `snapshot` envelope, which the first version of this
    /// parser missed, and that workspace rows carry no `cwd` or worktree.
    fn snapshot() -> Value {
        json!({
          "type": "session_snapshot",
          "snapshot": {
            "protocol": 17,
            "version": "0.7.5",
            "focused_pane_id": "w1:p1",
            "workspaces": [
                {"workspace_id": "w1", "label": "ansible", "cwd": "/repo",
                 "worktree": {"branch": "claude/herd", "path": "/repo"}},
                {"workspace_id": "w2", "label": "docs"}
            ],
            "tabs": [
                {"tab_id": "w1:t1", "workspace_id": "w1", "label": "main"}
            ],
            "panes": [
                {"pane_id": "w1:p1", "workspace_id": "w1", "tab_id": "w1:t1"}
            ],
            "agents": [
                {
                    "pane_id": "w1:p1", "workspace_id": "w1", "tab_id": "w1:t1",
                    "agent": "claude", "agent_status": "blocked",
                    "terminal_title": "✳ Refactor auth middleware",
                    "terminal_title_stripped": "Refactor auth middleware",
                    "cwd": "/repo", "foreground_cwd": "/repo/crates",
                    "state_change_seq": 43, "focused": true,
                    "terminal_id": "term_657da34abec2e7", "revision": 8
                },
                {
                    "pane_id": "w2:p1", "workspace_id": "w2",
                    "agent": "codex", "agent_status": "working"
                }
            ]
          }
        })
    }

    /// The bug the telemetry caught: the payload is one level down, under
    /// `snapshot`. Every field name inside was already right, so this single level
    /// was the whole of it — and it emptied the roster.
    #[test]
    fn the_snapshot_envelope_is_unwrapped() {
        let agents = parse_snapshot(&snapshot());
        assert_eq!(agents.len(), 2, "the `snapshot` envelope must be unwrapped");
        // And a response that is *not* wrapped still parses, so an older or newer
        // server that flattens it keeps working.
        let flat = snapshot()["snapshot"].clone();
        assert_eq!(parse_snapshot(&flat).len(), 2, "an unwrapped payload also parses");
    }

    #[test]
    fn herdrs_own_transition_counter_is_carried() {
        let agents = parse_snapshot(&snapshot());
        assert_eq!(agents[0].state_change_seq, Some(43));
        assert_eq!(agents[1].state_change_seq, None, "absent is fine");
    }

    #[test]
    fn a_snapshot_flattens_into_pane_agents_with_labels_resolved() {
        let agents = parse_snapshot(&snapshot());
        assert_eq!(agents.len(), 2);

        let first = &agents[0];
        assert_eq!(first.pane_id, "w1:p1");
        assert_eq!(first.status, Status::Blocked);
        assert_eq!(first.agent, "claude");
        assert_eq!(first.workspace.as_deref(), Some("ansible"));
        assert_eq!(first.tab.as_deref(), Some("main"));
        assert_eq!(first.branch.as_deref(), Some("claude/herd"), "from worktree provenance");
        // The stripped title wins: the raw one carries a spinner glyph that
        // animates, and publishing it would rewrite the headline every frame.
        assert_eq!(first.terminal_title.as_deref(), Some("Refactor auth middleware"));
        // `foreground_cwd` is preferred over the workspace cwd.
        assert_eq!(first.cwd.as_deref(), Some("/repo/crates"));
    }

    #[test]
    fn missing_labels_degrade_to_none_rather_than_dropping_the_agent() {
        let agents = parse_snapshot(&snapshot());
        let second = &agents[1];
        assert_eq!(second.pane_id, "w2:p1");
        assert_eq!(second.workspace.as_deref(), Some("docs"));
        assert_eq!(second.tab, None, "no tab record for this pane");
        assert_eq!(second.terminal_title, None);
        assert_eq!(second.status, Status::Working);
    }

    /// The fallback path: `agent.list` alone, with no workspace or tab records to
    /// resolve names from.
    #[test]
    fn an_agent_list_without_a_snapshot_still_yields_rows() {
        let list = json!({"type": "agent_list", "agents": [
            {"pane_id": "w1:p1", "agent": "claude", "agent_status": "idle"}
        ]});
        let agents = parse_agent_list(&list, &Labels::default());
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].status, Status::Idle);
        assert_eq!(agents[0].workspace, None);
    }

    /// A record with no `pane_id` cannot be addressed, watched, or messaged, so it
    /// is skipped rather than published as a row nobody can act on.
    #[test]
    fn an_agent_without_a_pane_id_is_skipped() {
        let list = json!({"agents": [
            {"agent": "claude", "agent_status": "working"},
            {"pane_id": "w1:p2", "agent": "claude", "agent_status": "working"}
        ]});
        let agents = parse_agent_list(&list, &Labels::default());
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].pane_id, "w1:p2");
    }

    #[test]
    fn an_unknown_status_string_does_not_drop_the_row() {
        let list = json!({"agents": [
            {"pane_id": "w1:p1", "agent": "claude", "agent_status": "reticulating"}
        ]});
        let agents = parse_agent_list(&list, &Labels::default());
        assert_eq!(agents[0].status, Status::Unknown);
    }

    #[test]
    fn a_display_agent_override_wins_over_the_agent_kind() {
        let list = json!({"agents": [
            {"pane_id": "w1:p1", "agent": "claude", "display_agent": "Claude: auth",
             "agent_status": "working"}
        ]});
        let agents = parse_agent_list(&list, &Labels::default());
        assert_eq!(agents[0].agent, "Claude: auth");
    }

    #[test]
    fn empty_strings_are_treated_as_absent() {
        // Herdr normalizes presentation text, and an empty normalized value clears
        // a token. A field present but empty must not become a blank label.
        let value = json!({"label": "", "name": "real"});
        assert_eq!(field(&value, &["label", "name"]), Some("real".into()));
    }

    #[test]
    fn the_injected_socket_path_wins_over_everything_else() {
        // Herdr injects HERDR_SOCKET_PATH into every plugin process, and it is
        // authoritative — a stale HERDR_SESSION must not redirect us to a
        // different server than the one that launched the plugin.
        assert_eq!(
            resolve_socket_path(Some("/run/herdr-test.sock"), Some("review"), Some("/cfg"), None),
            PathBuf::from("/run/herdr-test.sock")
        );
    }

    #[test]
    fn a_named_session_lands_under_the_sessions_directory() {
        assert_eq!(
            resolve_socket_path(None, Some("review"), Some("/cfg"), None),
            PathBuf::from("/cfg/herdr/sessions/review/herdr.sock")
        );
        assert_eq!(
            resolve_socket_path(None, None, Some("/cfg"), None),
            PathBuf::from("/cfg/herdr/herdr.sock")
        );
    }

    #[test]
    fn without_xdg_config_home_the_default_is_under_home() {
        assert_eq!(
            resolve_socket_path(None, None, None, Some("/home/sam")),
            PathBuf::from("/home/sam/.config/herdr/herdr.sock")
        );
    }
}
