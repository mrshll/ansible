//! Claude Code hook payloads and the session status they imply.
//!
//! Pure and I/O-free, like [`ansible-capture`](../ansible_capture/index.html):
//! the caller supplies the JSON and the timestamps. That is what lets recorded
//! sessions be replayed as tests, and it is why `tests/fixtures/` holds real
//! captures rather than hand-written payloads.
//!
//! The headline finding, measured rather than assumed: hooks reliably yield
//! `Starting`, `Working`, `AwaitingInput`, and `Done`, but **cannot** yield
//! `AwaitingApproval` — the one status the plan calls the highest-value signal
//! on the grid. A denied tool fires `PreToolUse` and never `PostToolUse`, which
//! is indistinguishable from a slow tool. [`StatusMachine`] therefore takes that
//! status from a terminal hint instead of inferring it from a timer. See
//! `docs/spikes/hook-coverage.md`.

pub mod event;
pub mod status;

pub use event::{
    Common, HookEvent, Notification, PostToolUse, PreToolUse, SessionEnd, SessionStart, Stop,
    UserPromptSubmit,
};
pub use status::{PendingTool, SessionStatus, StatusMachine, TerminalHint, Transition};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The payload had no `hook_event_name`, so it cannot be dispatched.
    ///
    /// Every real payload carries one; a payload without it is not a hook
    /// payload and guessing its type from structure would be unsound.
    #[error("hook payload has no hook_event_name")]
    MissingEventName,

    #[error("hook payload is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_describe_themselves() {
        assert_eq!(Error::MissingEventName.to_string(), "hook payload has no hook_event_name");
    }
}
