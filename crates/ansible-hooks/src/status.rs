//! Deriving a session's grid status from hook events.
//!
//! # What the hooks can and cannot tell you
//!
//! Measured against real sessions (`docs/spikes/hook-coverage.md`):
//!
//! | Status | Source | Reliable? |
//! |---|---|---|
//! | `Starting` | `SessionStart` | yes |
//! | `Working` | `UserPromptSubmit`, `PreToolUse` | yes |
//! | `AwaitingInput` | `Stop` | yes |
//! | `Done` | `SessionEnd` | yes |
//! | `AwaitingApproval` | **nothing** | **no — see below** |
//!
//! `AwaitingApproval` is the status the plan calls the highest-value thing the
//! grid can surface, because it is the interruption a teammate can resolve. It
//! is also the one hooks cannot supply:
//!
//! * When a tool is allowed, `PreToolUse` is followed by `PostToolUse`.
//! * When a tool is **denied**, `PreToolUse` fires and `PostToolUse` never
//!   does. The bracket is left dangling; no event reports the denial.
//! * A dangling bracket is therefore indistinguishable from a tool that is
//!   simply slow. A legitimate `Bash` command was measured holding the bracket
//!   open for 9,155 ms, so no timeout separates the two.
//!
//! So this machine does **not** guess. It tracks brackets, reports what is
//! pending, and takes `AwaitingApproval` only from
//! [`observe_terminal`](StatusMachine::observe_terminal) — a signal the host can
//! obtain because it owns the PTY and can see the prompt on screen. Encoding it
//! as a separate input keeps the dependency honest rather than hiding a guess
//! behind a timer.

use std::collections::BTreeMap;

use crate::event::HookEvent;

/// Coarse status shown on the grid.
///
/// Mirrors the `SessionStatus` enum in the hub schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Starting,
    Working,
    AwaitingInput,
    AwaitingApproval,
    Idle,
    Done,
    Failed,
    Detached,
}

impl SessionStatus {
    /// Whether the session has finished for good.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed)
    }
}

/// A tool that has started but not reported completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingTool {
    pub tool_name: String,
    pub started_at_ms: u64,
}

/// What the host can see on the terminal that hooks do not report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalHint {
    /// The rendered screen shows a permission prompt. The only observable
    /// source for `AwaitingApproval`.
    ApprovalPrompt { tool_name: Option<String> },
    /// The screen shows no prompt.
    NoPrompt,
}

/// A status change worth writing to the hub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub from: SessionStatus,
    pub to: SessionStatus,
    /// Short human string the grid renders verbatim, e.g. `running: Bash`.
    pub detail: String,
}

/// Derives [`SessionStatus`] from hook events plus terminal hints.
///
/// Pure: no I/O, no clock. The caller supplies timestamps, which is what makes
/// recorded sessions replayable as tests.
#[derive(Debug)]
pub struct StatusMachine {
    status: SessionStatus,
    detail: String,
    /// Open `PreToolUse` brackets, keyed by `tool_use_id`. Keyed rather than
    /// counted because several tools can run concurrently and pairing by name
    /// would mismatch them.
    pending: BTreeMap<String, PendingTool>,
    /// Set while the terminal shows a prompt, so a later hook event does not
    /// silently clear an approval the human has not answered.
    approval_on_screen: bool,
}

impl Default for StatusMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl StatusMachine {
    #[must_use]
    pub fn new() -> Self {
        Self {
            status: SessionStatus::Starting,
            detail: String::new(),
            pending: BTreeMap::new(),
            approval_on_screen: false,
        }
    }

    #[must_use]
    pub fn status(&self) -> SessionStatus {
        self.status
    }

    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// Tools whose `PreToolUse` has not been matched by a `PostToolUse`.
    pub fn pending_tools(&self) -> impl Iterator<Item = &PendingTool> {
        self.pending.values()
    }

    /// How long the longest-open bracket has been open.
    ///
    /// Deliberately *not* used to infer `AwaitingApproval`: a legitimate slow
    /// tool produces the same reading. Exposed so a host can surface "running
    /// for 2m" as detail, or reconcile against the transcript.
    #[must_use]
    pub fn longest_pending_ms(&self, now_ms: u64) -> Option<u64> {
        self.pending.values().map(|p| now_ms.saturating_sub(p.started_at_ms)).max()
    }

    /// Apply a hook event. Returns `Some` only when the status actually changed.
    ///
    /// Collapsing no-op calls here is what lets the caller fire this on every
    /// event without writing a history row per event — the hub's
    /// `update_session_status` is documented as the hottest reducer in the
    /// system and must tolerate exactly that.
    pub fn apply(&mut self, event: &HookEvent, at_ms: u64) -> Option<Transition> {
        match event {
            HookEvent::SessionStart(_) => self.goto(SessionStatus::Starting, String::new()),

            HookEvent::UserPromptSubmit(_) => {
                // A new prompt supersedes any prompt that was on screen.
                self.approval_on_screen = false;
                self.goto(SessionStatus::Working, String::new())
            }

            HookEvent::PreToolUse(e) => {
                let key =
                    e.tool_use_id.clone().unwrap_or_else(|| format!("{}@{at_ms}", e.tool_name));
                self.pending.insert(
                    key,
                    PendingTool { tool_name: e.tool_name.clone(), started_at_ms: at_ms },
                );
                // Not `AwaitingApproval`: at this point it is unknown whether a
                // human will be asked. Only the terminal can say.
                if self.approval_on_screen {
                    return None;
                }
                self.goto(SessionStatus::Working, format!("running: {}", e.tool_name))
            }

            HookEvent::PostToolUse(e) => {
                if let Some(id) = &e.tool_use_id {
                    self.pending.remove(id);
                } else {
                    // No id to correlate: drop the oldest bracket for this tool
                    // rather than leak it forever.
                    if let Some(key) = self
                        .pending
                        .iter()
                        .find(|(_, p)| p.tool_name == e.tool_name)
                        .map(|(k, _)| k.clone())
                    {
                        self.pending.remove(&key);
                    }
                }
                self.approval_on_screen = false;
                let detail = self
                    .pending
                    .values()
                    .next()
                    .map_or_else(String::new, |p| format!("running: {}", p.tool_name));
                self.goto(SessionStatus::Working, detail)
            }

            HookEvent::Notification(e) => {
                // Never observed firing in the recorded sessions, so treated as
                // advisory: it raises attention but does not assert a cause.
                let detail = e.message.clone().unwrap_or_else(|| "needs attention".into());
                self.goto(SessionStatus::AwaitingInput, detail)
            }

            HookEvent::Stop(_) => {
                // The assistant finished its turn, so the human is next. Any
                // brackets still open at this point were never closed — a
                // denial or an abandoned tool — and must not leak into the next
                // turn's detail.
                self.pending.clear();
                self.approval_on_screen = false;
                self.goto(SessionStatus::AwaitingInput, String::new())
            }

            HookEvent::SessionEnd(e) => {
                self.pending.clear();
                self.approval_on_screen = false;
                // `reason` was observed as `"other"` even on a clean exit, so it
                // is not treated as an error signal. The supervisor's exit
                // status is the authority on `Failed`.
                let detail = e.reason.clone().unwrap_or_default();
                self.goto(SessionStatus::Done, detail)
            }

            // An unmodelled event tells us the session is alive and nothing
            // more. Asserting a status from it would be a guess.
            HookEvent::Unknown { .. } => None,
        }
    }

    /// Supply what only the terminal can see.
    ///
    /// This is the sole path to [`SessionStatus::AwaitingApproval`], because no
    /// hook reports a pending or refused approval.
    pub fn observe_terminal(&mut self, hint: &TerminalHint) -> Option<Transition> {
        match hint {
            TerminalHint::ApprovalPrompt { tool_name } => {
                self.approval_on_screen = true;
                // Prefer the tool named by the caller, then the open bracket:
                // `PreToolUse` gives a reliable tool name even though it cannot
                // tell us an approval is pending.
                let tool = tool_name
                    .clone()
                    .or_else(|| self.pending.values().next().map(|p| p.tool_name.clone()));
                let detail = match tool {
                    Some(name) => format!("awaiting approval: {name}"),
                    None => "awaiting approval".to_string(),
                };
                self.goto(SessionStatus::AwaitingApproval, detail)
            }
            TerminalHint::NoPrompt => {
                if !self.approval_on_screen {
                    return None;
                }
                self.approval_on_screen = false;
                if self.status != SessionStatus::AwaitingApproval {
                    return None;
                }
                // The prompt is gone, so the tool was answered and is running.
                let detail = self
                    .pending
                    .values()
                    .next()
                    .map_or_else(String::new, |p| format!("running: {}", p.tool_name));
                self.goto(SessionStatus::Working, detail)
            }
        }
    }

    /// Mark the session lost, e.g. when the heartbeat stops.
    pub fn detach(&mut self) -> Option<Transition> {
        if self.status.is_terminal() {
            return None;
        }
        self.pending.clear();
        self.goto(SessionStatus::Detached, String::new())
    }

    /// Mark the session failed, from the supervisor's exit status.
    pub fn fail(&mut self, detail: impl Into<String>) -> Option<Transition> {
        self.pending.clear();
        self.goto(SessionStatus::Failed, detail.into())
    }

    fn goto(&mut self, to: SessionStatus, detail: String) -> Option<Transition> {
        // A terminal status is final: a late hook event must not resurrect a
        // finished session on the grid.
        if self.status.is_terminal() && !to.is_terminal() {
            return None;
        }
        if self.status == to && self.detail == detail {
            return None;
        }
        let from = self.status;
        self.status = to;
        self.detail.clone_from(&detail);
        Some(Transition { from, to, detail })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::HookEvent;

    fn event(json: &str) -> HookEvent {
        HookEvent::parse(json).expect("fixture parses")
    }

    fn session_start() -> HookEvent {
        event(r#"{"hook_event_name":"SessionStart","session_id":"s1","source":"startup"}"#)
    }
    fn prompt() -> HookEvent {
        event(r#"{"hook_event_name":"UserPromptSubmit","session_id":"s1","prompt":"go"}"#)
    }
    fn pre(tool: &str, id: &str) -> HookEvent {
        event(&format!(
            r#"{{"hook_event_name":"PreToolUse","session_id":"s1","tool_name":"{tool}","tool_use_id":"{id}"}}"#
        ))
    }
    fn post(tool: &str, id: &str) -> HookEvent {
        event(&format!(
            r#"{{"hook_event_name":"PostToolUse","session_id":"s1","tool_name":"{tool}","tool_use_id":"{id}","duration_ms":12}}"#
        ))
    }
    fn stop() -> HookEvent {
        event(r#"{"hook_event_name":"Stop","session_id":"s1","stop_hook_active":false}"#)
    }
    fn session_end() -> HookEvent {
        event(r#"{"hook_event_name":"SessionEnd","session_id":"s1","reason":"other"}"#)
    }

    #[test]
    fn a_clean_turn_walks_starting_working_awaiting_input() {
        let mut m = StatusMachine::new();
        m.apply(&session_start(), 0);
        assert_eq!(m.status(), SessionStatus::Starting);

        m.apply(&prompt(), 100);
        assert_eq!(m.status(), SessionStatus::Working);

        m.apply(&pre("Bash", "t1"), 200);
        assert_eq!(m.status(), SessionStatus::Working);
        assert_eq!(m.detail(), "running: Bash");

        m.apply(&post("Bash", "t1"), 300);
        assert_eq!(m.status(), SessionStatus::Working);
        assert_eq!(m.detail(), "");

        m.apply(&stop(), 400);
        assert_eq!(m.status(), SessionStatus::AwaitingInput);

        m.apply(&session_end(), 500);
        assert_eq!(m.status(), SessionStatus::Done);
    }

    #[test]
    fn transitions_are_only_reported_when_something_changed() {
        let mut m = StatusMachine::new();
        assert!(m.apply(&session_start(), 0).is_none(), "already Starting");
        assert!(m.apply(&prompt(), 1).is_some(), "Starting -> Working");
        // Re-submitting a prompt while already Working with no detail is a no-op,
        // which is what lets the caller fire on every event.
        assert!(m.apply(&prompt(), 2).is_none());
    }

    #[test]
    fn brackets_are_tracked_by_tool_use_id() {
        let mut m = StatusMachine::new();
        m.apply(&prompt(), 0);
        m.apply(&pre("Bash", "t1"), 10);
        m.apply(&pre("Read", "t2"), 20);
        assert_eq!(m.pending_tools().count(), 2);

        // Closing the second must not close the first.
        m.apply(&post("Read", "t2"), 30);
        let names: Vec<&str> = m.pending_tools().map(|p| p.tool_name.as_str()).collect();
        assert_eq!(names, vec!["Bash"]);
    }

    /// The measured deny path: `PreToolUse` then `Stop`, with no `PostToolUse`.
    #[test]
    fn a_denied_tool_leaves_a_dangling_bracket_that_stop_clears() {
        let mut m = StatusMachine::new();
        m.apply(&prompt(), 0);
        m.apply(&pre("Bash", "t1"), 10);
        assert_eq!(m.pending_tools().count(), 1, "bracket is open");

        // No PostToolUse ever arrives. Without the clear in `Stop`, the next
        // turn would report `running: Bash` forever.
        m.apply(&stop(), 20);
        assert_eq!(m.pending_tools().count(), 0, "Stop must clear dangling brackets");
        assert_eq!(m.status(), SessionStatus::AwaitingInput);
        assert_eq!(m.detail(), "");
    }

    /// A slow tool and a blocked one are indistinguishable from hooks alone.
    /// This test documents that rather than pretending otherwise.
    #[test]
    fn a_slow_tool_is_indistinguishable_from_a_blocked_one() {
        let mut slow = StatusMachine::new();
        slow.apply(&prompt(), 0);
        slow.apply(&pre("Bash", "t1"), 1_000);

        let mut blocked = StatusMachine::new();
        blocked.apply(&prompt(), 0);
        blocked.apply(&pre("Bash", "t1"), 1_000);

        // Same status, same detail, same pending age. Only the terminal differs.
        assert_eq!(slow.status(), blocked.status());
        assert_eq!(slow.detail(), blocked.detail());
        assert_eq!(slow.longest_pending_ms(11_000), blocked.longest_pending_ms(11_000));
        assert_eq!(slow.longest_pending_ms(11_000), Some(10_000));
    }

    #[test]
    fn awaiting_approval_comes_only_from_the_terminal() {
        let mut m = StatusMachine::new();
        m.apply(&prompt(), 0);
        m.apply(&pre("Bash", "t1"), 10);
        assert_eq!(m.status(), SessionStatus::Working, "hooks alone cannot know");

        let t = m
            .observe_terminal(&TerminalHint::ApprovalPrompt { tool_name: None })
            .expect("transition");
        assert_eq!(t.to, SessionStatus::AwaitingApproval);
        // The tool name comes from the reliable half: PreToolUse.
        assert_eq!(m.detail(), "awaiting approval: Bash");
    }

    #[test]
    fn an_explicit_tool_name_from_the_terminal_wins() {
        let mut m = StatusMachine::new();
        m.apply(&prompt(), 0);
        m.observe_terminal(&TerminalHint::ApprovalPrompt { tool_name: Some("WebFetch".into()) });
        assert_eq!(m.detail(), "awaiting approval: WebFetch");
    }

    #[test]
    fn clearing_the_prompt_returns_to_working() {
        let mut m = StatusMachine::new();
        m.apply(&prompt(), 0);
        m.apply(&pre("Bash", "t1"), 10);
        m.observe_terminal(&TerminalHint::ApprovalPrompt { tool_name: None });
        assert_eq!(m.status(), SessionStatus::AwaitingApproval);

        m.observe_terminal(&TerminalHint::NoPrompt);
        assert_eq!(m.status(), SessionStatus::Working);
        assert_eq!(m.detail(), "running: Bash");
    }

    #[test]
    fn no_prompt_is_a_noop_when_no_prompt_was_showing() {
        let mut m = StatusMachine::new();
        m.apply(&prompt(), 0);
        assert!(m.observe_terminal(&TerminalHint::NoPrompt).is_none());
        assert_eq!(m.status(), SessionStatus::Working);
    }

    /// A `PreToolUse` arriving while the prompt is up must not overwrite
    /// `AwaitingApproval` with `Working` — that would flicker the grid away from
    /// the one status a teammate can act on.
    #[test]
    fn a_tool_event_does_not_clear_an_unanswered_approval() {
        let mut m = StatusMachine::new();
        m.apply(&prompt(), 0);
        m.observe_terminal(&TerminalHint::ApprovalPrompt { tool_name: Some("Bash".into()) });
        assert!(m.apply(&pre("Bash", "t1"), 10).is_none());
        assert_eq!(m.status(), SessionStatus::AwaitingApproval);
    }

    #[test]
    fn answering_the_prompt_via_post_tool_use_resumes_working() {
        let mut m = StatusMachine::new();
        m.apply(&prompt(), 0);
        m.apply(&pre("Bash", "t1"), 10);
        m.observe_terminal(&TerminalHint::ApprovalPrompt { tool_name: None });
        m.apply(&post("Bash", "t1"), 20);
        assert_eq!(m.status(), SessionStatus::Working);
        assert_eq!(m.pending_tools().count(), 0);
    }

    #[test]
    fn a_late_event_cannot_resurrect_a_finished_session() {
        let mut m = StatusMachine::new();
        m.apply(&session_end(), 0);
        assert_eq!(m.status(), SessionStatus::Done);
        assert!(m.apply(&prompt(), 10).is_none());
        assert_eq!(m.status(), SessionStatus::Done);
    }

    #[test]
    fn detach_and_fail_are_available_to_the_supervisor() {
        let mut m = StatusMachine::new();
        m.apply(&prompt(), 0);
        assert_eq!(m.detach().unwrap().to, SessionStatus::Detached);

        let mut n = StatusMachine::new();
        n.apply(&prompt(), 0);
        let t = n.fail("exit code 1").unwrap();
        assert_eq!(t.to, SessionStatus::Failed);
        assert_eq!(n.detail(), "exit code 1");
    }

    #[test]
    fn detach_does_not_override_a_finished_session() {
        let mut m = StatusMachine::new();
        m.apply(&session_end(), 0);
        assert!(m.detach().is_none());
    }

    #[test]
    fn an_unmodelled_event_changes_nothing() {
        let mut m = StatusMachine::new();
        m.apply(&prompt(), 0);
        let before = m.status();
        assert!(
            m.apply(&event(r#"{"hook_event_name":"FutureThing","session_id":"s1"}"#), 1).is_none()
        );
        assert_eq!(m.status(), before);
    }

    #[test]
    fn a_post_without_an_id_still_closes_a_bracket() {
        let mut m = StatusMachine::new();
        m.apply(&prompt(), 0);
        m.apply(&pre("Bash", "t1"), 10);
        let no_id =
            event(r#"{"hook_event_name":"PostToolUse","session_id":"s1","tool_name":"Bash"}"#);
        m.apply(&no_id, 20);
        assert_eq!(m.pending_tools().count(), 0, "brackets must not leak");
    }
}
