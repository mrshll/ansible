//! Claude Code hook payload types.
//!
//! Every field here was observed in a real session, not taken from
//! documentation. `docs/spikes/hook-coverage.md` records the capture method and
//! the payload shapes; `tests/fixtures/` holds the recordings these types are
//! parsed against.
//!
//! Two properties of the real payloads shape this module:
//!
//! * **Every payload carries `hook_event_name`.** The event is self-describing,
//!   so a receiver does not need one endpoint per event and cannot mislabel one.
//! * **Every payload carries `session_id`, `cwd`, and `transcript_path`.** That
//!   is enough to route an event to a session and to find Claude Code's own
//!   transcript without any additional bookkeeping.

use serde::{Deserialize, Serialize};

/// Fields present in every hook payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Common {
    pub session_id: String,
    /// Path to Claude Code's own session JSONL. The authoritative record, and
    /// the only place a tool denial is visible — see [`crate::status`].
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// Present on prompt- and tool-scoped events, absent on `SessionStart`.
    #[serde(default)]
    pub permission_mode: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStart {
    #[serde(flatten)]
    pub common: Common,
    /// `"startup"`, `"resume"`, `"clear"`, … Observed: `"startup"`.
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserPromptSubmit {
    #[serde(flatten)]
    pub common: Common,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub prompt_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreToolUse {
    #[serde(flatten)]
    pub common: Common,
    pub tool_name: String,
    /// Correlates with the matching [`PostToolUse`]. Required for correct
    /// bracket tracking: several tools can be in flight at once, so pairing by
    /// tool name alone would mismatch them.
    #[serde(default)]
    pub tool_use_id: Option<String>,
    #[serde(default)]
    pub tool_input: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostToolUse {
    #[serde(flatten)]
    pub common: Common,
    pub tool_name: String,
    #[serde(default)]
    pub tool_use_id: Option<String>,
    #[serde(default)]
    pub tool_input: Option<serde_json::Value>,
    #[serde(default)]
    pub tool_response: Option<serde_json::Value>,
    /// How long the tool actually ran. Observed at 9,155 ms for a legitimate
    /// slow command, which is why no timeout can separate "slow tool" from
    /// "blocked on a human".
    #[serde(default)]
    pub duration_ms: Option<u64>,
}

/// Claude Code wants the user's attention.
///
/// Modelled but **never observed firing** in this environment, whose policy
/// forces a permissive permission mode. See `docs/spikes/hook-coverage.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    #[serde(flatten)]
    pub common: Common,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stop {
    #[serde(flatten)]
    pub common: Common,
    #[serde(default)]
    pub last_assistant_message: Option<String>,
    /// True when the stop hook itself caused this invocation. Guards against a
    /// hook that re-triggers itself.
    #[serde(default)]
    pub stop_hook_active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEnd {
    #[serde(flatten)]
    pub common: Common,
    /// Observed: `"other"`. Not reliably an error indicator.
    #[serde(default)]
    pub reason: Option<String>,
}

/// A parsed hook payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookEvent {
    SessionStart(SessionStart),
    UserPromptSubmit(UserPromptSubmit),
    PreToolUse(PreToolUse),
    PostToolUse(PostToolUse),
    Notification(Notification),
    Stop(Stop),
    SessionEnd(SessionEnd),
    /// An event this build does not model.
    ///
    /// Deliberately not an error. Claude Code adds hook events, and a receiver
    /// that rejected unknown ones would start failing on upgrade — losing the
    /// events it *does* understand along with the ones it does not.
    Unknown {
        hook_event_name: String,
        session_id: Option<String>,
    },
}

impl HookEvent {
    /// Parse one payload.
    ///
    /// Dispatches on `hook_event_name` rather than relying on structural
    /// inference, because several events differ only by which optional fields
    /// they carry and would otherwise be ambiguous.
    ///
    /// # Errors
    /// [`Error::Json`] on malformed JSON, or [`Error::MissingEventName`] when
    /// the payload has no `hook_event_name` and so cannot be dispatched.
    pub fn parse(json: &str) -> crate::Result<Self> {
        let value: serde_json::Value = serde_json::from_str(json)?;
        Self::from_value(value)
    }

    /// Parse one already-decoded payload.
    ///
    /// # Errors
    /// As [`parse`](Self::parse).
    pub fn from_value(value: serde_json::Value) -> crate::Result<Self> {
        let name = value
            .get("hook_event_name")
            .and_then(serde_json::Value::as_str)
            .ok_or(crate::Error::MissingEventName)?
            .to_string();

        Ok(match name.as_str() {
            "SessionStart" => Self::SessionStart(serde_json::from_value(value)?),
            "UserPromptSubmit" => Self::UserPromptSubmit(serde_json::from_value(value)?),
            "PreToolUse" => Self::PreToolUse(serde_json::from_value(value)?),
            "PostToolUse" => Self::PostToolUse(serde_json::from_value(value)?),
            "Notification" => Self::Notification(serde_json::from_value(value)?),
            "Stop" => Self::Stop(serde_json::from_value(value)?),
            "SessionEnd" => Self::SessionEnd(serde_json::from_value(value)?),
            _ => {
                let session_id = value
                    .get("session_id")
                    .and_then(serde_json::Value::as_str)
                    .map(ToString::to_string);
                Self::Unknown { hook_event_name: name, session_id }
            }
        })
    }

    #[must_use]
    pub fn event_name(&self) -> &str {
        match self {
            Self::SessionStart(_) => "SessionStart",
            Self::UserPromptSubmit(_) => "UserPromptSubmit",
            Self::PreToolUse(_) => "PreToolUse",
            Self::PostToolUse(_) => "PostToolUse",
            Self::Notification(_) => "Notification",
            Self::Stop(_) => "Stop",
            Self::SessionEnd(_) => "SessionEnd",
            Self::Unknown { hook_event_name, .. } => hook_event_name,
        }
    }

    /// Which session this event belongs to.
    #[must_use]
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Self::SessionStart(e) => Some(&e.common.session_id),
            Self::UserPromptSubmit(e) => Some(&e.common.session_id),
            Self::PreToolUse(e) => Some(&e.common.session_id),
            Self::PostToolUse(e) => Some(&e.common.session_id),
            Self::Notification(e) => Some(&e.common.session_id),
            Self::Stop(e) => Some(&e.common.session_id),
            Self::SessionEnd(e) => Some(&e.common.session_id),
            Self::Unknown { session_id, .. } => session_id.as_deref(),
        }
    }

    /// Path to Claude Code's own transcript, when the event carries one.
    #[must_use]
    pub fn transcript_path(&self) -> Option<&str> {
        let common = match self {
            Self::SessionStart(e) => &e.common,
            Self::UserPromptSubmit(e) => &e.common,
            Self::PreToolUse(e) => &e.common,
            Self::PostToolUse(e) => &e.common,
            Self::Notification(e) => &e.common,
            Self::Stop(e) => &e.common,
            Self::SessionEnd(e) => &e.common,
            Self::Unknown { .. } => return None,
        };
        common.transcript_path.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_real_pre_tool_use_payload() {
        // Verbatim shape from a recorded session.
        let json = r#"{
            "hook_event_name": "PreToolUse",
            "session_id": "aede8214-e214-5a68-aac2-50caf19902b4",
            "transcript_path": "/root/.claude/projects/x/y.jsonl",
            "cwd": "/tmp/work",
            "permission_mode": "acceptEdits",
            "tool_name": "Write",
            "tool_use_id": "toolu_011JKWRB",
            "tool_input": {"file_path": "note.txt", "content": "hello"},
            "prompt_id": "p1",
            "effort": "medium"
        }"#;
        let HookEvent::PreToolUse(e) = HookEvent::parse(json).unwrap() else {
            panic!("wrong variant");
        };
        assert_eq!(e.tool_name, "Write");
        assert_eq!(e.tool_use_id.as_deref(), Some("toolu_011JKWRB"));
        assert_eq!(e.common.permission_mode.as_deref(), Some("acceptEdits"));
        assert_eq!(e.common.session_id, "aede8214-e214-5a68-aac2-50caf19902b4");
    }

    #[test]
    fn parses_a_real_post_tool_use_payload_with_duration() {
        let json = r#"{
            "hook_event_name": "PostToolUse",
            "session_id": "s1",
            "tool_name": "Bash",
            "tool_use_id": "toolu_x",
            "duration_ms": 9155,
            "tool_response": {"type": "text", "content": "ok"}
        }"#;
        let HookEvent::PostToolUse(e) = HookEvent::parse(json).unwrap() else {
            panic!("wrong variant");
        };
        assert_eq!(e.duration_ms, Some(9155));
        assert!(e.tool_response.is_some());
    }

    /// Unknown events must survive, or a Claude Code upgrade that adds an event
    /// would start failing the whole receiver.
    #[test]
    fn an_unmodelled_event_parses_instead_of_failing() {
        let json = r#"{"hook_event_name": "SomeFutureHook", "session_id": "s1", "extra": 1}"#;
        let event = HookEvent::parse(json).unwrap();
        assert_eq!(event.event_name(), "SomeFutureHook");
        assert_eq!(event.session_id(), Some("s1"));
    }

    /// Unknown *fields* must also survive: real payloads carry `effort`,
    /// `prompt_id`, `background_tasks`, and more that we do not model.
    #[test]
    fn unmodelled_fields_are_ignored() {
        let json = r#"{
            "hook_event_name": "Stop",
            "session_id": "s1",
            "stop_hook_active": false,
            "background_tasks": [],
            "session_crons": [],
            "effort": "high",
            "something_new": {"nested": true}
        }"#;
        let HookEvent::Stop(e) = HookEvent::parse(json).unwrap() else { panic!("wrong variant") };
        assert!(!e.stop_hook_active);
    }

    #[test]
    fn a_payload_without_an_event_name_is_rejected() {
        let err = HookEvent::parse(r#"{"session_id": "s1"}"#).unwrap_err();
        assert!(matches!(err, crate::Error::MissingEventName));
    }

    #[test]
    fn malformed_json_is_rejected() {
        assert!(HookEvent::parse("{not json").is_err());
    }

    #[test]
    fn transcript_path_is_exposed_for_the_events_that_carry_it() {
        let json = r#"{"hook_event_name":"SessionStart","session_id":"s1",
                       "transcript_path":"/p/t.jsonl","source":"startup"}"#;
        let event = HookEvent::parse(json).unwrap();
        assert_eq!(event.transcript_path(), Some("/p/t.jsonl"));
    }
}
