//! Replay real recorded sessions through the status machine.
//!
//! The fixtures in `tests/fixtures/` were produced by installing a hook overlay
//! against a real `claude` run and logging every payload — see
//! `docs/spikes/hook-coverage.md`. Asserting against recordings rather than
//! hand-written payloads is what keeps these tests honest: if Claude Code
//! changes a payload shape, re-recording the fixture is the fix, and the diff
//! shows exactly what moved.

use std::path::PathBuf;

use ansible_hooks::{HookEvent, SessionStatus, StatusMachine, TerminalHint};

/// One logged hook invocation: `{event, at_ms, payload}`.
struct Recorded {
    at_ms: u64,
    event: HookEvent,
}

fn load(name: &str) -> Vec<Recorded> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));

    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let row: serde_json::Value = serde_json::from_str(line).expect("fixture line is JSON");
            let at_ms = row["at_ms"].as_u64().expect("at_ms");
            // The receiver wraps the payload; the payload itself is what the
            // hook actually received, and it self-identifies via
            // `hook_event_name`.
            let event = HookEvent::from_value(row["payload"].clone()).expect("payload parses");
            Recorded { at_ms, event }
        })
        .collect()
}

fn names(rows: &[Recorded]) -> Vec<&str> {
    rows.iter().map(|r| r.event.event_name()).collect()
}

#[test]
fn every_recorded_payload_parses() {
    for fixture in ["tool-allowed.jsonl", "tool-denied.jsonl"] {
        let rows = load(fixture);
        assert!(!rows.is_empty(), "{fixture} is empty");
        for row in &rows {
            assert!(
                !matches!(row.event, HookEvent::Unknown { .. }),
                "{fixture}: {} was not modelled",
                row.event.event_name()
            );
        }
    }
}

#[test]
fn recorded_payloads_carry_the_fields_the_app_routes_on() {
    for fixture in ["tool-allowed.jsonl", "tool-denied.jsonl"] {
        for row in load(fixture) {
            assert!(
                row.event.session_id().is_some(),
                "{fixture}: {} has no session_id, so it could not be routed",
                row.event.event_name()
            );
            assert!(
                row.event.transcript_path().is_some(),
                "{fixture}: {} has no transcript_path",
                row.event.event_name()
            );
        }
    }
}

#[test]
fn the_allowed_run_records_a_closed_bracket() {
    let rows = load("tool-allowed.jsonl");
    assert_eq!(
        names(&rows),
        vec!["SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "Stop", "SessionEnd"],
        "recorded event order changed; re-record the fixture and review"
    );
}

/// The measured deny path. `PostToolUse` is absent — this is the finding the
/// whole status design turns on, so it is asserted directly.
#[test]
fn the_denied_run_records_no_post_tool_use() {
    let rows = load("tool-denied.jsonl");
    let recorded = names(&rows);
    assert!(recorded.contains(&"PreToolUse"), "expected a PreToolUse: {recorded:?}");
    assert!(
        !recorded.contains(&"PostToolUse"),
        "a denied tool must not report PostToolUse; got {recorded:?}"
    );
    assert!(
        !recorded.contains(&"Notification") && !recorded.contains(&"PermissionRequest"),
        "no hook reported the denial, which is why AwaitingApproval needs the terminal: {recorded:?}"
    );
}

#[test]
fn replaying_the_allowed_run_ends_done_with_no_leaked_brackets() {
    let mut machine = StatusMachine::new();
    let mut seen = Vec::new();
    for row in load("tool-allowed.jsonl") {
        if let Some(t) = machine.apply(&row.event, row.at_ms) {
            seen.push(t.to);
        }
    }
    assert_eq!(machine.status(), SessionStatus::Done);
    assert_eq!(machine.pending_tools().count(), 0);
    assert!(seen.contains(&SessionStatus::Working), "never reported Working: {seen:?}");
    assert!(seen.contains(&SessionStatus::AwaitingInput), "never reported AwaitingInput: {seen:?}");
}

#[test]
fn replaying_the_denied_run_also_ends_clean() {
    let mut machine = StatusMachine::new();
    let rows = load("tool-denied.jsonl");

    // Mid-run, after PreToolUse and before Stop, the bracket is open and the
    // status is Working — the same reading a slow tool would give.
    for row in &rows {
        machine.apply(&row.event, row.at_ms);
        if row.event.event_name() == "PreToolUse" {
            assert_eq!(machine.status(), SessionStatus::Working);
            assert_eq!(machine.pending_tools().count(), 1);
        }
    }

    assert_eq!(machine.status(), SessionStatus::Done);
    assert_eq!(
        machine.pending_tools().count(),
        0,
        "a denied tool must not leave a bracket open past the end of the session"
    );
}

/// End to end: the same recorded deny run, but with the terminal hint the host
/// can supply. This is the only way the grid reaches `AwaitingApproval`.
#[test]
fn a_terminal_hint_turns_the_denied_run_into_awaiting_approval() {
    let mut machine = StatusMachine::new();
    let rows = load("tool-denied.jsonl");

    for row in &rows {
        machine.apply(&row.event, row.at_ms);

        if row.event.event_name() == "PreToolUse" {
            // Hooks alone: indistinguishable from a slow tool.
            assert_eq!(machine.status(), SessionStatus::Working);

            // The host sees a prompt on the rendered screen.
            let t = machine
                .observe_terminal(&TerminalHint::ApprovalPrompt { tool_name: None })
                .expect("status should change");
            assert_eq!(t.to, SessionStatus::AwaitingApproval);
            assert_eq!(
                machine.detail(),
                "awaiting approval: Bash",
                "the tool name comes from PreToolUse, which is reliable"
            );
        }
    }
    assert_eq!(machine.status(), SessionStatus::Done);
}

#[test]
fn replay_is_idempotent_under_duplicate_delivery() {
    // A localhost receiver can retry, so the same payload may arrive twice.
    let rows = load("tool-allowed.jsonl");

    let mut once = StatusMachine::new();
    for row in &rows {
        once.apply(&row.event, row.at_ms);
    }

    let mut twice = StatusMachine::new();
    for row in &rows {
        twice.apply(&row.event, row.at_ms);
        twice.apply(&row.event, row.at_ms);
    }

    assert_eq!(once.status(), twice.status());
    assert_eq!(once.detail(), twice.detail());
    assert_eq!(once.pending_tools().count(), twice.pending_tools().count());
}

#[test]
fn transitions_are_far_fewer_than_events() {
    // The hub's update_session_status is documented as the hottest reducer in
    // the system and must tolerate being called far more often than it changes
    // anything. Collapsing here is what makes that cheap.
    let rows = load("tool-allowed.jsonl");
    let mut machine = StatusMachine::new();
    let changes = rows.iter().filter(|r| machine.apply(&r.event, r.at_ms).is_some()).count();
    assert!(
        changes < rows.len(),
        "expected fewer transitions ({changes}) than events ({})",
        rows.len()
    );
}
