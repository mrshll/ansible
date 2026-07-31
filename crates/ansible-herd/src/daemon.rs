//! The reconcile loop.
//!
//! One process per Herdr server, started by the plugin's `[[startup]]` hook. It
//! holds the only long-lived things in the design: a socket subscription, the hub
//! connection, and any live publishers.
//!
//! # Reconcile from state, subscribe for speed
//!
//! Every tick reads the *whole* current state from Herdr and rebuilds what should
//! be published, rather than mutating a local model from a stream of events. Events
//! are used only as a nudge to run that tick sooner. This costs a snapshot call per
//! second on a local socket and buys two things worth more than that: a daemon that
//! cannot drift out of sync with Herdr no matter which events it missed, and one
//! that keeps working if the event payload shape changes underneath it. Spike B's
//! hook work learned the same lesson from the other side — the state machine there
//! is authoritative precisely because it is a pure function of everything it has
//! seen, not of the last thing it saw.
//!
//! # What it will not do
//!
//! Report semantic agent state to Herdr. Herdr's docs are explicit that a pane has
//! one status authority, and for Claude Code that is its screen manifest. This
//! plugin adds *display* metadata beside it and never competes with it.

use std::collections::BTreeMap;

use anyhow::{Context, Result};

use crate::clock::now_ms;
use crate::config::{Config, METADATA_SOURCE, Paths};
use crate::herdr::{Client, Events, PaneAgent, socket_path};
use crate::hub::Hub;
use crate::model::{
    AgentCard, MAX_DISPLAY, MemberDoc, Message, MessageKind, Share, Status, normalize,
};
use crate::state::{Overrides, Store};

/// How long a teammate's comment stays on the owner's pane as a token.
const NOTE_TTL_MS: u64 = 600_000;

/// Sleep between ticks when nothing nudges us.
const TICK: std::time::Duration = std::time::Duration::from_millis(200);

pub struct Daemon {
    config: Config,
    store: Store,
    hub: Box<dyn Hub>,
    client: Option<Client>,
    login: String,
    host: String,

    doc_seq: u64,
    token_seq: u64,
    last_published: Option<Vec<AgentCard>>,
    last_publish_ms: u64,
    last_poll_ms: u64,
    last_reconcile_ms: u64,

    /// When each pane last changed status, so a card can say how long it has been
    /// blocked. Keyed by pane id, which is the only identity Herdr guarantees.
    since: BTreeMap<String, (Status, u64)>,
    /// Tokens last reported per pane, so an unchanged tick makes no socket call.
    reported: BTreeMap<String, BTreeMap<String, String>>,
    /// Live publishers, keyed by agent key.
    publishers: BTreeMap<String, crate::teleport::LivePublisher>,
    /// Repo names resolved from cwd, cached because it costs a process spawn.
    repos: BTreeMap<String, Option<String>>,
}

impl Daemon {
    /// # Errors
    /// When the config has no usable identity, or the hub cannot be opened.
    pub fn new(config: Config, paths: &Paths) -> Result<Self> {
        let (login, host) = config.identity()?;
        let hub = crate::hub::open(&config, &paths.state_dir)?;
        Ok(Self {
            config,
            store: Store::new(&paths.state_dir),
            hub,
            client: None,
            login,
            host,
            doc_seq: 0,
            token_seq: 0,
            last_published: None,
            last_publish_ms: 0,
            last_poll_ms: 0,
            last_reconcile_ms: 0,
            since: BTreeMap::new(),
            reported: BTreeMap::new(),
            publishers: BTreeMap::new(),
            repos: BTreeMap::new(),
        })
    }

    /// Run until killed, or for one tick when `once`.
    ///
    /// Herdr closes plugin processes by killing them, and installing a signal
    /// handler needs `unsafe`, so there is no graceful-shutdown path: everything
    /// this daemon owns is either idempotent on restart or reconstructed from
    /// Herdr on the next tick. `Drop` on the live publishers is what stops the
    /// observed streams.
    ///
    /// # Errors
    /// Only for failures that cannot be retried — a bad identity or an unopenable
    /// hub. Everything transient is logged to stderr and retried, because a daemon
    /// that exits when the hub blips takes the whole team's presence with it.
    pub fn run(&mut self, once: bool) -> Result<()> {
        let socket = socket_path();
        let (tx, rx) = std::sync::mpsc::channel();
        if let Err(err) = Events::spawn(&socket, tx) {
            // Not fatal, and worth saying out loud once: this is the difference
            // between sub-second and one-second reaction time, not between working
            // and not working.
            eprintln!(
                "herd: no event subscription ({err}); polling every {}ms",
                self.config.timing.reconcile_ms
            );
        }

        loop {
            let now = now_ms();
            self.tick(now);
            self.touch_alive(now);
            if once {
                return Ok(());
            }
            // Drain the nudge channel: several events arriving together should
            // cause one reconcile, not one each.
            if rx.recv_timeout(TICK).is_ok() {
                while rx.try_recv().is_ok() {}
                self.last_reconcile_ms = 0;
            }
        }
    }

    /// One pass. Every step is independent and every failure is logged rather than
    /// returned.
    ///
    /// This is the shape that makes the daemon survivable. Herdr can go away — a
    /// restart, a handoff — while the hub is fine, and the hub can go away while
    /// Herdr is fine. Either one short-circuiting the other means a teammate's
    /// comment sits undelivered because a socket blinked, or presence stops because
    /// a fetch timed out. Each step is retried on the next tick from whatever state
    /// it finds.
    fn tick(&mut self, now: u64) {
        if now.saturating_sub(self.last_reconcile_ms) >= self.config.timing.reconcile_ms {
            if let Err(err) = self.reconcile(now) {
                eprintln!("herd: reconcile: {err:#}");
            }
            self.last_reconcile_ms = now;
        }
        if now.saturating_sub(self.last_poll_ms) >= self.config.timing.poll_ms {
            self.poll(now);
            self.last_poll_ms = now;
        }
        self.pump_live(now);
    }

    /// Read Herdr, rebuild our presence document, publish it when it changed or a
    /// heartbeat is due.
    fn reconcile(&mut self, now: u64) -> Result<()> {
        let overrides = self.store.overrides();
        let panes = match self.client_mut()?.read_agents() {
            Ok(panes) => panes,
            Err(err) => {
                // The socket went away — Herdr restarted, or handed off. Drop the
                // client so the next tick reconnects.
                self.client = None;
                return Err(err).context("reading agents from Herdr");
            }
        };

        let cards = self.build_cards(&panes, &overrides, now);
        let changed = self.last_published.as_ref() != Some(&cards);
        let due = now.saturating_sub(self.last_publish_ms) >= self.config.timing.heartbeat_ms;
        if !changed && !due {
            return Ok(());
        }

        self.doc_seq += 1;
        let mut doc = MemberDoc::new(&self.login, &self.host);
        doc.display_name.clone_from(&self.config.display_name);
        doc.seq = self.doc_seq;
        doc.published_ms = now;
        doc.help.clone_from(&overrides.help);
        doc.agents.clone_from(&cards);
        doc.watching = self.store.watching(now);
        self.hub.publish(&doc).context("publishing presence")?;
        self.last_published = Some(cards);
        self.last_publish_ms = now;
        Ok(())
    }

    /// Turn Herdr's panes into cards, applying the human's overrides.
    fn build_cards(
        &mut self,
        panes: &[PaneAgent],
        overrides: &Overrides,
        now: u64,
    ) -> Vec<AgentCard> {
        let mut cards = Vec::with_capacity(panes.len());
        for pane in panes {
            let share = overrides
                .share
                .get(&pane.pane_id)
                .copied()
                .unwrap_or_else(|| self.config.default_share());
            if share == Share::Off {
                continue;
            }
            let since = self.since_for(&pane.pane_id, pane.status, now);
            let repo = pane.cwd.as_deref().and_then(|cwd| self.repo_of(cwd));
            cards.push(AgentCard {
                key: pane.key(&self.login, &self.host),
                pane_id: pane.pane_id.clone(),
                workspace: pane.workspace.clone().map(|w| scrub_display(&w)),
                tab: pane.tab.clone().map(|t| scrub_display(&t)),
                agent: scrub_display(&pane.agent),
                status: pane.status,
                headline: headline(pane, overrides.headline.as_deref()),
                repo,
                branch: pane.branch.clone().map(|b| scrub_display(&b)),
                share,
                // Member-level, published on the document — see `MemberDoc::help`.
                help: None,
                since_ms: since,
                live_seq: None,
            });
        }
        // `live_seq` is owned by the publishers, not by this pass.
        for card in &mut cards {
            if let Some(publisher) = self.publishers.get(&card.key) {
                card.live_seq = publisher.published_seq();
            }
        }
        cards
    }

    /// When this pane entered its current status.
    fn since_for(&mut self, pane_id: &str, status: Status, now: u64) -> u64 {
        match self.since.get(pane_id) {
            Some((previous, at)) if *previous == status => *at,
            _ => {
                self.since.insert(pane_id.to_string(), (status, now));
                now
            }
        }
    }

    /// `owner/repo` from a working directory, via the origin remote.
    ///
    /// Cached including the negative answer: most panes are not in a repo with an
    /// origin, and re-asking Git about them every second would be the most
    /// expensive thing this daemon does.
    fn repo_of(&mut self, cwd: &str) -> Option<String> {
        if let Some(cached) = self.repos.get(cwd) {
            return cached.clone();
        }
        let resolved = std::process::Command::new("git")
            .args(["-C", cwd, "config", "--get", "remote.origin.url"])
            .output()
            .ok()
            .filter(|out| out.status.success())
            .and_then(|out| repo_from_url(String::from_utf8_lossy(&out.stdout).trim()));
        self.repos.insert(cwd.to_string(), resolved.clone());
        resolved
    }

    /// Read the hub: teammates' presence, watchers of our own sessions, and mail.
    fn poll(&mut self, now: u64) {
        let members = match self.hub.members() {
            Ok(members) => members,
            Err(err) => {
                eprintln!("herd: reading the hub: {err:#}");
                return;
            }
        };
        if let Err(err) = self.handle_watchers(&members, now) {
            eprintln!("herd: watchers: {err:#}");
        }
        if let Err(err) = self.announce(&members, now) {
            eprintln!("herd: announce: {err:#}");
        }
        if let Err(err) = self.receive_mail() {
            eprintln!("herd: mail: {err:#}");
        }
    }

    /// Reflect watchers onto our own panes, and start or stop live publishing.
    fn handle_watchers(&mut self, members: &[MemberDoc], now: u64) -> Result<()> {
        let mine: Vec<AgentCard> = self.last_published.clone().unwrap_or_default();
        let mut wanted: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for member in members {
            if member.login == self.login || member.is_stale(now, self.config.hub.forget_after_ms) {
                continue;
            }
            for key in &member.watching {
                if mine.iter().any(|c| &c.key == key) {
                    wanted.entry(key.clone()).or_default().push(member.login.clone());
                }
            }
        }

        for card in &mine {
            let watchers = wanted.get(&card.key).cloned().unwrap_or_default();
            let live = card.share == Share::Live;

            match (live, watchers.is_empty()) {
                // Someone wants in and the owner has opted the pane into `live`.
                (true, false) => self.ensure_publisher(&card.key, &card.pane_id),
                // Sharing was revoked, or the last watcher left. Dropping the
                // publisher kills the observe process: revocation has to stop the
                // observation, not just stop the upload.
                _ => {
                    self.publishers.remove(&card.key);
                }
            }

            let mut tokens: BTreeMap<String, String> = BTreeMap::new();
            if !watchers.is_empty() {
                tokens.insert("herd".into(), normalize(&watchers.join(", "), MAX_DISPLAY));
            }
            if live {
                tokens.insert("live".into(), "sharing".into());
            } else if !watchers.is_empty() {
                // The consent handshake, made visible: they are asking, and the
                // owner has not said yes. Nothing streams until they do.
                tokens.insert("live".into(), "asked".into());
            }
            if self.config.default_share() != Share::Off && self.store.overrides().help.is_some() {
                tokens.insert("ask".into(), "help wanted".into());
            }
            self.report_tokens(&card.pane_id, tokens)?;
        }
        Ok(())
    }

    fn ensure_publisher(&mut self, key: &str, pane_id: &str) {
        if self.publishers.get(key).is_some_and(|p| !p.is_closed()) {
            return;
        }
        if !self.hub.supports_live() {
            return;
        }
        match crate::teleport::LivePublisher::start(key, pane_id) {
            Ok(publisher) => {
                self.publishers.insert(key.to_string(), publisher);
            }
            Err(err) => eprintln!("herd: cannot observe {pane_id}: {err:#}"),
        }
    }

    fn pump_live(&mut self, now: u64) {
        let keys: Vec<String> = self.publishers.keys().cloned().collect();
        for key in keys {
            let Some(publisher) = self.publishers.get_mut(&key) else { continue };
            if let Err(err) = publisher.pump(self.hub.as_mut(), now) {
                eprintln!("herd: live publish for {key} failed: {err:#}");
                self.publishers.remove(&key);
                continue;
            }
            if publisher.is_closed() {
                // Worth one line: "redaction is on" is a claim, and this is the
                // only place that can say how often it fired on a stream that
                // teammates actually saw.
                eprintln!(
                    "herd: live stream for {key} ended ({} redactions)",
                    publisher.redactions()
                );
                self.publishers.remove(&key);
            }
        }
    }

    /// Notify on the rising edge of a teammate needing a human.
    ///
    /// Rising edge and not level, because `blocked` persists for as long as the
    /// prompt is on screen. A toast per poll would train everyone to ignore the one
    /// status meant to summon them — the same failure the approval-producer spike
    /// warned about from the detection side.
    fn announce(&mut self, members: &[MemberDoc], now: u64) -> Result<()> {
        let mut announced = self.store.announced();
        let mut changed = false;

        for member in members {
            if member.login == self.login || member.is_stale(now, self.config.hub.stale_after_ms) {
                continue;
            }
            for card in &member.agents {
                let state = announce_state(card);
                let previous = announced.get(&card.key).cloned();
                if previous.as_deref() == state.as_deref() {
                    continue;
                }
                match &state {
                    Some(reason) => {
                        announced.insert(card.key.clone(), reason.clone());
                        let who = member.display_name.as_ref().unwrap_or(&member.login);
                        let title = format!("{who}: {reason}");
                        let body = if card.headline.is_empty() {
                            card.pane_id.clone()
                        } else {
                            card.headline.clone()
                        };
                        if let Ok(client) = self.client_mut() {
                            let _ = client.notify(&title, &body, true);
                        }
                    }
                    None => {
                        announced.remove(&card.key);
                    }
                }
                changed = true;
            }
        }

        if changed {
            self.store.put_announced(&announced)?;
        }
        Ok(())
    }

    /// Deliver mail addressed to us: inbox, toast, and a token on the pane it is
    /// about.
    fn receive_mail(&mut self) -> Result<()> {
        let messages = self.hub.messages_for(&self.login).context("reading mail")?;
        for message in messages {
            if !self.store.deliver(&message)? {
                continue;
            }
            let title = match message.kind {
                MessageKind::Comment => format!("{} commented", message.from),
                MessageKind::Nudge => format!("{} is looking at your session", message.from),
            };
            let pane =
                crate::model::split_key(&message.to_key).map(|(_, _, pane)| pane.to_string());
            if let Ok(client) = self.client_mut() {
                let _ = client.notify(&title, &message.body, true);
            }
            if let Some(pane_id) = pane {
                let note = normalize(&format!("{}: {}", message.from, message.body), MAX_DISPLAY);
                self.report_note(&pane_id, &note)?;
            }
        }
        Ok(())
    }

    /// Patch pane tokens, skipping the call when nothing changed.
    fn report_tokens(&mut self, pane_id: &str, tokens: BTreeMap<String, String>) -> Result<()> {
        let previous = self.reported.get(pane_id).cloned().unwrap_or_default();
        if previous == tokens {
            return Ok(());
        }
        // A key that was set and is now absent has to be cleared explicitly, or a
        // watcher who left stays on the owner's sidebar forever.
        let mut patch: Vec<(String, Option<String>)> = Vec::new();
        for (name, value) in &tokens {
            if previous.get(name) != Some(value) {
                patch.push((name.clone(), Some(value.clone())));
            }
        }
        for name in previous.keys() {
            if !tokens.contains_key(name) {
                patch.push((name.clone(), None));
            }
        }

        let borrowed: Vec<(&str, Option<&str>)> =
            patch.iter().map(|(n, v)| (n.as_str(), v.as_deref())).collect();
        self.token_seq += 1;
        let seq = self.token_seq;
        let result =
            self.client_mut()?.report_tokens(pane_id, METADATA_SOURCE, &borrowed, None, seq);
        match result {
            Ok(()) => {
                self.reported.insert(pane_id.to_string(), tokens);
                Ok(())
            }
            Err(err) => {
                // A failed call means the connection is suspect. Dropping it here
                // is what lets the daemon walk back up after a Herdr restart or a
                // live handoff without a restart of its own.
                self.client = None;
                Err(err)
            }
        }
    }

    fn report_note(&mut self, pane_id: &str, note: &str) -> Result<()> {
        self.token_seq += 1;
        let seq = self.token_seq;
        self.client_mut()?.report_tokens(
            pane_id,
            METADATA_SOURCE,
            &[("note", Some(note))],
            Some(NOTE_TTL_MS),
            seq,
        )
    }

    /// A socket connection, reconnecting when the last one died.
    fn client_mut(&mut self) -> Result<&mut Client> {
        if self.client.is_none() {
            self.client = Some(Client::connect(&socket_path())?);
        }
        self.client.as_mut().context("client")
    }

    /// Heartbeat file, so `startup` can tell a running daemon from a stale pidfile
    /// without sending a signal — which would need `unsafe` — and without `/proc`,
    /// which macOS does not have.
    fn touch_alive(&self, now: u64) {
        let _ = crate::state::write_atomic(
            &self.store.root().join("daemon.alive"),
            now.to_string().as_bytes(),
        );
    }
}

/// Why a card would raise a toast, or `None` when it would not.
///
/// A raised hand wins over a status, because someone typed it.
#[must_use]
fn announce_state(card: &AgentCard) -> Option<String> {
    if let Some(help) = &card.help {
        return Some(if help.note.is_empty() {
            "needs help".to_string()
        } else {
            format!("needs help — {}", help.note)
        });
    }
    match card.status {
        Status::Blocked => Some("blocked".into()),
        Status::Done => Some("ready to review".into()),
        _ => None,
    }
}

/// What this session is working on, best source first.
///
/// The explicit override wins because a human typed it. Then Herdr's stripped
/// terminal title, which for Claude Code is a short summary of the task and is the
/// best thing available for free. Then the workspace and tab, which at least says
/// where. Never empty: a blank headline is a row a teammate cannot read.
#[must_use]
fn headline(pane: &PaneAgent, explicit: Option<&str>) -> String {
    if let Some(text) = explicit.map(str::trim).filter(|t| !t.is_empty()) {
        return scrub_display(text);
    }
    if let Some(title) = pane.terminal_title.as_deref().filter(|t| !t.trim().is_empty()) {
        return scrub_display(title);
    }
    match (&pane.workspace, &pane.tab) {
        (Some(ws), Some(tab)) => scrub_display(&format!("{ws}/{tab}")),
        (Some(ws), None) => scrub_display(ws),
        _ => scrub_display(&pane.pane_id),
    }
}

/// Redact, then normalize, anything on its way out of this machine.
///
/// Terminal titles are set by whatever is running in the pane, and a title is
/// under no obligation to be free of secrets — `curl -H "Authorization: ..."` as a
/// window title is a thing that happens. So headlines go through exactly the same
/// redactor as transcript bytes. It costs a few microseconds on a string that is
/// at most 80 characters, and it means there is one answer to "what redacts
/// published text" instead of two.
#[must_use]
pub fn scrub_display(text: &str) -> String {
    normalize(&scrub(text), MAX_DISPLAY)
}

/// Run text through the redaction ruleset.
#[must_use]
pub fn scrub(text: &str) -> String {
    let mut redactor = ansible_capture::Redactor::new(ansible_capture::Ruleset::default());
    let mut out = redactor.push(text.as_bytes());
    out.extend(redactor.finish());
    String::from_utf8_lossy(&out).into_owned()
}

/// `owner/repo` from any of the URL forms Git accepts.
#[must_use]
fn repo_from_url(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    // `git@host:owner/repo`, `ssh://host/owner/repo`, `https://host/owner/repo`.
    let tail = trimmed.rsplit_once(':').map_or(trimmed, |(_, after)| after);
    let parts: Vec<&str> = tail.rsplit('/').take(2).collect();
    if parts.len() < 2 || parts.iter().any(|p| p.is_empty()) {
        return None;
    }
    Some(format!("{}/{}", parts[1], parts[0]))
}

/// Build a message addressed at a key.
///
/// Lives here rather than in the `comment` command so the sender and the daemon
/// agree on the id scheme, which is what makes delivery idempotent.
///
/// # Errors
/// When the target key is malformed, or the outbox counter cannot be advanced.
pub fn compose(
    store: &Store,
    from: &str,
    to_key: &str,
    kind: MessageKind,
    body: &str,
    anchor_line: Option<u32>,
    now: u64,
) -> Result<Message> {
    let (to, _, _) = crate::model::split_key(to_key)
        .context("target is not a herd key (expected login@host/pane)")?;
    let seq = store.next_outbox_seq()?;
    Ok(Message {
        v: crate::model::SCHEMA_VERSION,
        id: format!("{from}-{seq}"),
        from: from.to_string(),
        to: to.to_string(),
        to_key: to_key.to_string(),
        kind,
        // A comment is the one published string a human wrote on purpose, so it
        // gets a longer cap than a display field — but the same redactor.
        body: scrub(body).chars().take(500).collect(),
        anchor_line,
        created_ms: now,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{HelpWanted, agent_key};

    fn pane(pane_id: &str, title: Option<&str>) -> PaneAgent {
        PaneAgent {
            pane_id: pane_id.into(),
            workspace_id: Some("w1".into()),
            workspace: Some("ansible".into()),
            tab: Some("main".into()),
            agent: "claude".into(),
            status: Status::Working,
            terminal_title: title.map(str::to_string),
            cwd: None,
            branch: None,
        }
    }

    #[test]
    fn an_explicit_headline_wins_over_the_terminal_title() {
        let pane = pane("w1:p1", Some("Refactor auth middleware"));
        assert_eq!(headline(&pane, Some("waiting on review")), "waiting on review");
    }

    #[test]
    fn the_terminal_title_is_the_free_fallback() {
        let pane = pane("w1:p1", Some("Refactor auth middleware"));
        assert_eq!(headline(&pane, None), "Refactor auth middleware");
        assert_eq!(
            headline(&pane, Some("   ")),
            "Refactor auth middleware",
            "blank is not a choice"
        );
    }

    #[test]
    fn a_headline_is_never_empty() {
        let mut pane = pane("w1:p1", None);
        assert_eq!(headline(&pane, None), "ansible/main");
        pane.tab = None;
        assert_eq!(headline(&pane, None), "ansible");
        pane.workspace = None;
        assert_eq!(headline(&pane, None), "w1:p1");
    }

    /// A window title is set by whatever runs in the pane, and it is published to
    /// the whole team.
    #[test]
    fn a_secret_in_a_terminal_title_never_reaches_the_hub() {
        let pane = pane("w1:p1", Some("deploy with ghp_0123456789abcdefghij now"));
        let line = headline(&pane, None);
        assert!(!line.contains("ghp_0123456789abcdefghij"), "got {line}");
        assert!(line.contains("deploy with"), "the rest of the title survives: {line}");
    }

    #[test]
    fn an_explicit_headline_is_redacted_too() {
        let pane = pane("w1:p1", None);
        let line = headline(&pane, Some("token=sk-ant-0123456789abcdef pasted"));
        assert!(!line.contains("sk-ant-0123456789abcdef"), "got {line}");
    }

    #[test]
    fn a_comment_body_is_redacted_and_length_capped() {
        let store = Store::new(
            std::env::temp_dir().join(format!("ansible-herd-compose-{}", std::process::id())),
        );
        let _ = std::fs::remove_dir_all(store.root());
        std::fs::create_dir_all(store.root()).expect("mkdir");

        let key = agent_key("alice", "box", "w1:p1");
        let message = compose(
            &store,
            "mrshll",
            &key,
            MessageKind::Comment,
            &format!("try ghp_0123456789abcdefghij {}", "x".repeat(1_000)),
            Some(42),
            1_000,
        )
        .expect("compose");

        assert_eq!(message.to, "alice");
        assert_eq!(message.id, "mrshll-1");
        assert_eq!(message.anchor_line, Some(42));
        assert!(!message.body.contains("ghp_0123456789abcdefghij"));
        assert!(message.body.chars().count() <= 500);
    }

    #[test]
    fn composing_to_a_malformed_key_is_refused_before_it_reaches_the_hub() {
        let store = Store::new(
            std::env::temp_dir().join(format!("ansible-herd-badkey-{}", std::process::id())),
        );
        let _ = std::fs::remove_dir_all(store.root());
        std::fs::create_dir_all(store.root()).expect("mkdir");
        let err = compose(&store, "me", "not-a-key", MessageKind::Comment, "hi", None, 0)
            .expect_err("refused");
        assert!(format!("{err}").contains("herd key"), "got {err}");
    }

    #[test]
    fn a_raised_hand_announces_itself_before_a_status_does() {
        let mut card = AgentCard {
            key: agent_key("alice", "box", "w1:p1"),
            pane_id: "w1:p1".into(),
            workspace: None,
            tab: None,
            agent: "claude".into(),
            status: Status::Working,
            headline: "refactor auth".into(),
            repo: None,
            branch: None,
            share: Share::Title,
            help: None,
            since_ms: 0,
            live_seq: None,
        };
        assert_eq!(announce_state(&card), None, "working alone is not a summons");

        card.status = Status::Blocked;
        assert_eq!(announce_state(&card).as_deref(), Some("blocked"));

        card.status = Status::Done;
        assert_eq!(announce_state(&card).as_deref(), Some("ready to review"));

        card.help = Some(HelpWanted { note: "cannot get RLS to deny".into(), since_ms: 0 });
        assert_eq!(
            announce_state(&card).as_deref(),
            Some("needs help — cannot get RLS to deny"),
            "the note a human typed is the whole value of the summons"
        );

        card.help = Some(HelpWanted { note: String::new(), since_ms: 0 });
        assert_eq!(announce_state(&card).as_deref(), Some("needs help"));
    }

    #[test]
    fn idle_and_unknown_never_announce() {
        for status in [Status::Idle, Status::Unknown, Status::Working] {
            let card = AgentCard {
                key: "k".into(),
                pane_id: "w1:p1".into(),
                workspace: None,
                tab: None,
                agent: "claude".into(),
                status,
                headline: String::new(),
                repo: None,
                branch: None,
                share: Share::Title,
                help: None,
                since_ms: 0,
                live_seq: None,
            };
            assert_eq!(announce_state(&card), None, "{status}");
        }
    }

    #[test]
    fn repo_names_come_out_of_every_url_form_git_accepts() {
        assert_eq!(
            repo_from_url("git@github.com:mrshll/ansible.git"),
            Some("mrshll/ansible".into())
        );
        assert_eq!(
            repo_from_url("https://github.com/mrshll/ansible.git"),
            Some("mrshll/ansible".into())
        );
        assert_eq!(
            repo_from_url("https://github.com/mrshll/ansible"),
            Some("mrshll/ansible".into())
        );
        assert_eq!(
            repo_from_url("ssh://git@github.com/mrshll/ansible.git"),
            Some("mrshll/ansible".into())
        );
        assert_eq!(repo_from_url("/srv/git/ansible.git"), Some("git/ansible".into()));
        assert_eq!(repo_from_url(""), None);
        assert_eq!(repo_from_url("ansible"), None);
    }

    #[test]
    fn scrubbing_leaves_ordinary_text_alone() {
        assert_eq!(scrub_display("  refactor  auth middleware "), "refactor auth middleware");
        assert_eq!(scrub_display("100% done"), "100% done");
    }
}
