//! What crosses the hub.
//!
//! Two documents and nothing else. A [`MemberDoc`] is the whole of what one
//! person is publishing right now — replace-in-place, last write wins, authored
//! only by its owner. A [`Message`] is one addressed note, append-only, also
//! authored only by its sender.
//!
//! That split is deliberate: because every path in the hub is keyed by the login
//! of the only process allowed to write it, there is no shared mutable state, no
//! lock, and no merge. It is what lets the same schema sit on a shared directory,
//! on Git refs, or on a relay without any of them needing a transaction.
//!
//! # Status is Herdr's, not ours
//!
//! [`Status`] is a mirror of Herdr's semantic agent state, down to the names. The
//! repo's own [`ansible_hooks::SessionStatus`] has eight variants derived from
//! Claude Code hooks plus a screen detector; this has five, because Herdr already
//! did that derivation and publishing a second opinion about the same pane would
//! give the grid two sources of truth. In particular `blocked` is the state the
//! architecture plan calls the highest-value thing to surface, and it arrives
//! here for free — see `docs/adr/0004-herdr-plugin-host.md`.
//!
//! [`ansible_hooks::SessionStatus`]: https://docs.rs/ansible-hooks

use std::fmt;

use serde::{Deserialize, Serialize};

/// Schema version on every document. Readers refuse a version they do not know
/// rather than guessing at fields, because a teammate on a newer plugin is the
/// normal case in a team that installs at its own pace.
pub const SCHEMA_VERSION: u32 = 1;

/// Herdr's semantic agent state, mirrored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Recognised approval, question, or permission UI is on screen. The one a
    /// teammate can actually resolve.
    Blocked,
    /// Idle after unseen background work finished: ready to review.
    Done,
    Working,
    Idle,
    /// An agent is present but Herdr will not classify it. Not an error, and
    /// specifically not "finished successfully".
    Unknown,
}

impl Status {
    /// Parse the wire spelling Herdr uses in `agent_status`.
    ///
    /// Anything unrecognised becomes [`Status::Unknown`] rather than an error: a
    /// Herdr release that adds a state should degrade a roster row, not stop a
    /// daemon.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw {
            "blocked" => Self::Blocked,
            "done" => Self::Done,
            "working" => Self::Working,
            "idle" => Self::Idle,
            _ => Self::Unknown,
        }
    }

    /// Whether this state means a human is being waited on.
    ///
    /// The roster sorts on this and the daemon notifies on its rising edge, so it
    /// is one predicate rather than two lists that can drift apart.
    #[must_use]
    pub fn wants_a_human(self) -> bool {
        matches!(self, Self::Blocked | Self::Done)
    }

    /// Single-width marker for the roster's status column.
    #[must_use]
    pub fn glyph(self) -> char {
        match self {
            Self::Blocked => '!',
            Self::Done => '+',
            Self::Working => '>',
            Self::Idle => '.',
            Self::Unknown => '?',
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Working => "working",
            Self::Idle => "idle",
            Self::Unknown => "unknown",
        };
        f.write_str(s)
    }
}

/// How much of a session its owner is publishing.
///
/// Defaults to [`Share::Title`] everywhere, which matches the stance the README
/// already takes for the desktop app: a presence card is opt-out, a transcript is
/// opt-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Share {
    /// Publish nothing about this pane at all. It does not appear in the herd.
    Off,
    /// Headline, status, and repo. No terminal contents.
    #[default]
    Title,
    /// Headline plus a redacted live byte stream, so a teammate can watch.
    Live,
}

impl Share {
    /// Parse the spelling used on the command line and in config.
    ///
    /// # Errors
    /// The input string, when it is not one of the three modes.
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "off" => Ok(Self::Off),
            "title" => Ok(Self::Title),
            "live" => Ok(Self::Live),
            other => Err(other.to_string()),
        }
    }
}

impl fmt::Display for Share {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Off => "off",
            Self::Title => "title",
            Self::Live => "live",
        };
        f.write_str(s)
    }
}

/// A raised hand, with the note that makes it worth answering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelpWanted {
    /// Free text, already redacted and length-capped by the publisher.
    pub note: String,
    pub since_ms: u64,
}

/// One agent session as its owner is publishing it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCard {
    /// Globally unique and stable for the life of the pane: `login@host/pane_id`.
    /// Built by [`agent_key`] so the roster, the message address, and the live
    /// stream path all spell it the same way.
    pub key: String,
    pub pane_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab: Option<String>,
    /// Herdr's agent label, e.g. `claude`.
    pub agent: String,
    pub status: Status,
    /// What this session is working on, in one line. See
    /// `daemon::headline` for where it comes from.
    pub headline: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub share: Share,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<HelpWanted>,
    /// When this card last changed status, so the roster can say "blocked 4m".
    pub since_ms: u64,
    /// Highest live chunk sequence published for this pane, or `None` when the
    /// pane is not sharing live. A watcher polls from here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_seq: Option<u64>,
}

/// Everything one member is publishing, replaced whole on every heartbeat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberDoc {
    pub v: u32,
    pub login: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub host: String,
    /// Monotonic per member. A reader that sees a lower `seq` than it already
    /// holds is looking at a stale replica — which a Git-backed hub produces
    /// routinely between fetches.
    pub seq: u64,
    pub published_ms: u64,
    /// A raised hand.
    ///
    /// On the member rather than on each card, because "I am stuck" is a fact
    /// about a person. Putting it on every card is what made the first roster
    /// print the same note three times.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<HelpWanted>,
    #[serde(default)]
    pub agents: Vec<AgentCard>,
    /// Keys this member is watching right now.
    ///
    /// This is the whole of the teleport handshake. Watching is not a request
    /// that needs an answer: the watcher publishes intent, the owner's daemon
    /// sees it, and the owner's own `share` mode decides whether frames follow.
    #[serde(default)]
    pub watching: Vec<String>,
}

impl MemberDoc {
    #[must_use]
    pub fn new(login: impl Into<String>, host: impl Into<String>) -> Self {
        Self {
            v: SCHEMA_VERSION,
            login: login.into(),
            display_name: None,
            host: host.into(),
            seq: 0,
            published_ms: 0,
            help: None,
            agents: Vec::new(),
            watching: Vec::new(),
        }
    }

    /// Whether this member has not been heard from for `stale_after_ms`.
    #[must_use]
    pub fn is_stale(&self, now_ms: u64, stale_after_ms: u64) -> bool {
        now_ms.saturating_sub(self.published_ms) > stale_after_ms
    }
}

/// What a message is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    /// Text meant for the human at the other end, and — with consent — for the
    /// agent itself.
    Comment,
    /// "Look at this" with no body worth reading. Raises a notification only.
    Nudge,
}

/// One addressed note from one member to one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub v: u32,
    /// `from` login plus the sender's own counter. Unique because only the sender
    /// mints it, which is also what makes delivery idempotent across a hub that
    /// can hand the same file back twice.
    pub id: String,
    pub from: String,
    /// The recipient's login, so a reader can skip other people's mail without
    /// parsing the body.
    pub to: String,
    /// The [`AgentCard::key`] this is about.
    pub to_key: String,
    pub kind: MessageKind,
    #[serde(default)]
    pub body: String,
    /// Optional anchor into what the sender was looking at: the line number in
    /// the live view. Enough for "the error on line 42", not a byte-exact
    /// transcript anchor — see the mentions note in the plan doc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_line: Option<u32>,
    pub created_ms: u64,
}

/// The one spelling of an agent's identity across the hub.
#[must_use]
pub fn agent_key(login: &str, host: &str, pane_id: &str) -> String {
    format!("{login}@{host}/{pane_id}")
}

/// Split a key back into `(login, host, pane_id)`.
///
/// Returns `None` for anything that is not in [`agent_key`] form, which is how a
/// bad `--target` on the command line is caught before it reaches the hub.
#[must_use]
pub fn split_key(key: &str) -> Option<(&str, &str, &str)> {
    let (owner, pane) = key.split_once('/')?;
    let (login, host) = owner.split_once('@')?;
    if login.is_empty() || host.is_empty() || pane.is_empty() {
        return None;
    }
    Some((login, host, pane))
}

/// Herdr caps presentation strings at 80 characters and strips control
/// characters. Doing the same before publishing means the hub carries what will
/// actually be displayed, so a headline cannot be one thing in the roster and a
/// truncated other thing in a Herdr sidebar row.
pub const MAX_DISPLAY: usize = 80;

/// Collapse whitespace, drop control characters, and cap at `max` characters.
///
/// Character-counted, not byte-counted: a byte cap would split a multi-byte
/// grapheme and hand the hub invalid-looking text.
#[must_use]
pub fn normalize(raw: &str, max: usize) -> String {
    let mut out = String::with_capacity(raw.len().min(max));
    let mut pending_space = false;
    let mut chars = 0;
    for ch in raw.chars() {
        if ch.is_control() || ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            if chars + 1 >= max {
                break;
            }
            out.push(' ');
            chars += 1;
            pending_space = false;
        }
        if chars >= max {
            break;
        }
        out.push(ch);
        chars += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_parses_herdrs_spellings_and_degrades_unknown() {
        assert_eq!(Status::parse("blocked"), Status::Blocked);
        assert_eq!(Status::parse("done"), Status::Done);
        assert_eq!(Status::parse("working"), Status::Working);
        assert_eq!(Status::parse("idle"), Status::Idle);
        // A future Herdr state must not stop a daemon.
        assert_eq!(Status::parse("hibernating"), Status::Unknown);
    }

    #[test]
    fn only_blocked_and_done_want_a_human() {
        assert!(Status::Blocked.wants_a_human());
        assert!(Status::Done.wants_a_human(), "unseen finished work is a review request");
        assert!(!Status::Working.wants_a_human());
        assert!(!Status::Idle.wants_a_human());
        // `unknown` means Herdr will not classify the screen. Treating it as a
        // summons would make the noisiest state the loudest one.
        assert!(!Status::Unknown.wants_a_human());
    }

    #[test]
    fn status_sorts_attention_first() {
        let mut v =
            vec![Status::Idle, Status::Unknown, Status::Blocked, Status::Working, Status::Done];
        v.sort_unstable();
        assert_eq!(
            v,
            vec![Status::Blocked, Status::Done, Status::Working, Status::Idle, Status::Unknown]
        );
    }

    #[test]
    fn share_defaults_to_title_not_live() {
        // The default decides what happens to someone who installs the plugin and
        // reads no documentation. It must not be `live`.
        assert_eq!(Share::default(), Share::Title);
    }

    #[test]
    fn keys_round_trip() {
        let key = agent_key("mrshll", "sams-box", "w1:p1");
        assert_eq!(key, "mrshll@sams-box/w1:p1");
        assert_eq!(split_key(&key), Some(("mrshll", "sams-box", "w1:p1")));
    }

    #[test]
    fn malformed_keys_are_rejected_rather_than_guessed_at() {
        assert_eq!(split_key("mrshll/w1:p1"), None, "no host");
        assert_eq!(split_key("mrshll@box"), None, "no pane");
        assert_eq!(split_key("@box/w1:p1"), None, "empty login");
        assert_eq!(split_key("mrshll@/w1:p1"), None, "empty host");
        assert_eq!(split_key("mrshll@box/"), None, "empty pane");
    }

    #[test]
    fn normalize_collapses_and_caps() {
        assert_eq!(
            normalize("  refactor   auth\tmiddleware\n", MAX_DISPLAY),
            "refactor auth middleware"
        );
        assert_eq!(normalize("", MAX_DISPLAY), "");
        assert_eq!(normalize("   ", MAX_DISPLAY), "");
        assert_eq!(normalize("abcdef", 3), "abc");
    }

    /// Truncation must never split a character, or the hub carries broken text.
    #[test]
    fn normalize_counts_characters_not_bytes() {
        let out = normalize("ααααα", 3);
        assert_eq!(out.chars().count(), 3);
        assert_eq!(out, "ααα");
    }

    /// A cap that lands exactly on a word gap must not leave a trailing space,
    /// which would show up as a ragged right edge in every roster row.
    #[test]
    fn normalize_does_not_end_on_a_space() {
        let out = normalize("aaa bbb", 4);
        assert!(!out.ends_with(' '), "got {out:?}");
        assert_eq!(out, "aaa");
    }

    #[test]
    fn a_member_doc_round_trips_through_json() {
        let mut doc = MemberDoc::new("mrshll", "sams-box");
        doc.published_ms = 1_780_000_000_000;
        doc.seq = 7;
        doc.agents.push(AgentCard {
            key: agent_key("mrshll", "sams-box", "w1:p1"),
            pane_id: "w1:p1".into(),
            workspace: Some("ansible".into()),
            tab: Some("main".into()),
            agent: "claude".into(),
            status: Status::Blocked,
            headline: "refactor auth middleware".into(),
            repo: Some("mrshll/ansible".into()),
            branch: Some("claude/herd".into()),
            share: Share::Live,
            help: Some(HelpWanted { note: "stuck on RLS".into(), since_ms: 1_780_000_000_000 }),
            since_ms: 1_780_000_000_000,
            live_seq: Some(12),
        });

        let json = serde_json::to_string(&doc).expect("serializes");
        let back: MemberDoc = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(back, doc);
    }

    /// Absent optional fields must not be an error: the whole point of the
    /// version field is that everything else can be additive.
    #[test]
    fn a_minimal_member_doc_deserializes() {
        let doc: MemberDoc =
            serde_json::from_str(r#"{"v":1,"login":"a","host":"b","seq":1,"published_ms":10}"#)
                .expect("minimal doc parses");
        assert!(doc.agents.is_empty());
        assert!(doc.watching.is_empty());
    }

    #[test]
    fn staleness_is_measured_from_the_publish_time() {
        let mut doc = MemberDoc::new("a", "b");
        doc.published_ms = 1_000;
        assert!(!doc.is_stale(20_000, 20_000));
        assert!(doc.is_stale(21_001, 20_000));
        // A clock that runs backwards must not report an infinitely fresh member.
        assert!(!doc.is_stale(0, 20_000));
    }
}
