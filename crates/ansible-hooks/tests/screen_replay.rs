//! Replay recorded screens through the approval detector.
//!
//! The fixtures in `tests/fixtures/screens/` are verbatim
//! [`Snapshot::screen_text`] captures from a real interactive `claude` v2.1.220
//! session, written by `scripts/probe-approval.sh`. Recordings rather than
//! hand-written screens, for the same reason the hook payloads are: the strings
//! belong to a TUI, so a Claude Code upgrade should show up here as a reviewable
//! diff instead of a silently dead grid.
//!
//! These tests are also where the third of the three claims lives. The live
//! probe can only assert that a pending tool did not trip the detector for as
//! long as the environment let one run; asserting that *no* elapsed time trips
//! it belongs somewhere deterministic, with no credentials and nothing to be
//! flaky about. See `no_elapsed_time_turns_a_working_screen_into_an_approval`.
//!
//! [`Snapshot::screen_text`]: ../../ansible_terminal/snapshot/struct.Snapshot.html

use std::path::PathBuf;

use ansible_hooks::{HookEvent, SessionStatus, StatusMachine, TerminalHint, approval};

/// Every recorded screen, and whether a permission prompt is on it.
///
/// The `false` rows carry the weight. `folder-trust` is the TUI's closest
/// look-alike — a numbered modal with a `Yes`, a `No`, and the same footer — and
/// it appears before any tool runs, so reading it as an approval would put every
/// fresh session on the grid as `AwaitingApproval`.
const SCREENS: &[(&str, bool)] = &[
    ("write-approval", true),
    ("bash-approval", true),
    ("write-answered", false),
    ("long-tool-working", false),
    ("folder-trust", false),
];

fn screen(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/screens")
        .join(format!("{name}.screen"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

fn recorded_session() -> Vec<(u64, HookEvent)> {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/approval-session.jsonl");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let row: serde_json::Value = serde_json::from_str(line).expect("fixture line is JSON");
            let at_ms = row["at_ms"].as_u64().expect("at_ms");
            let event = HookEvent::from_value(row["payload"].clone()).expect("payload parses");
            (at_ms, event)
        })
        .collect()
}

/// Raw event names as the receiver logged them, including ones the crate does
/// not model as a typed variant.
fn recorded_event_names() -> Vec<String> {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/approval-session.jsonl");
    let text = std::fs::read_to_string(&path).expect("fixture");
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let row: serde_json::Value = serde_json::from_str(line).expect("JSON");
            row["event"].as_str().unwrap_or_default().to_string()
        })
        .collect()
}

#[test]
fn every_recorded_screen_classifies_as_recorded() {
    for (name, expects_prompt) in SCREENS {
        let text = screen(name);
        assert!(!text.trim().is_empty(), "{name} is empty");
        let found = approval::detect(&text).is_some();
        assert_eq!(
            found, *expects_prompt,
            "{name}: expected prompt={expects_prompt}, got {found}. \
             If Claude Code's prompt changed, re-record with scripts/probe-approval.sh \
             and review the diff."
        );
    }
}

/// The false positive that matters most, called out on its own.
#[test]
fn the_folder_trust_gate_is_not_a_tool_approval() {
    let text = screen("folder-trust");
    // It really does look like one: same marker, same numbering, same footer.
    assert!(text.contains("❯ 1. Yes"), "the look-alike must still look alike");
    assert!(text.contains("2. No, exit"));
    assert!(text.contains("Esc to cancel"));
    // And it still must not read as a tool approval.
    assert!(approval::detect(&text).is_none());
    assert_eq!(approval::hint(&text), TerminalHint::NoPrompt);
}

#[test]
fn recorded_prompts_expose_a_question_and_a_refusal() {
    let write = approval::detect(&screen("write-approval")).expect("prompt");
    assert_eq!(write.question, "Do you want to create probe.txt?");

    let bash = approval::detect(&screen("bash-approval")).expect("prompt");
    assert_eq!(bash.question, "Do you want to proceed?");

    for prompt in [&write, &bash] {
        assert_eq!(prompt.options.len(), 3, "three options in both recorded shapes");
        assert_eq!(prompt.selected().map(|o| o.number), Some(1));
        assert!(
            prompt.options.iter().any(ansible_hooks::ApprovalOption::is_refusal),
            "a prompt with no way to decline is not a prompt a human is answering"
        );
    }
}

/// Criterion 3: elapsed time is not an input to the status.
///
/// A legitimate `Bash` call was measured holding its bracket open for 9.2 s, and
/// the recorded session here held one for 15.1 s. A real build or test run holds
/// it for minutes. Any timeout would have to choose a number, and mislabelling
/// `Working` as `AwaitingApproval` is the worse error — it trains people to
/// ignore the one status meant to summon them.
///
/// The machine's only clock is the `at_ms` its caller passes to `apply`, so that
/// is what this advances — out to an hour, with real hook events, re-reading the
/// recorded mid-tool screen at every step. An earlier version of this test looped
/// over an `age_ms` it never handed to the machine, which made six identical
/// iterations and could not have failed for the reason it advertised.
#[test]
fn no_elapsed_time_turns_a_working_screen_into_an_approval() {
    let working = screen("long-tool-working");
    let mut machine = StatusMachine::new();

    // Open a real bracket from the recording.
    let session = recorded_session();
    let pre = session
        .iter()
        .find(|(_, e)| e.event_name() == "PreToolUse")
        .expect("the recording contains a PreToolUse");
    let start_ms = 1_000_u64;
    machine.apply(
        &HookEvent::parse(
            r#"{"hook_event_name":"UserPromptSubmit","session_id":"s","prompt":"go"}"#,
        )
        .unwrap(),
        0,
    );
    machine.apply(&pre.1, start_ms);
    assert_eq!(machine.status(), SessionStatus::Working);
    assert_eq!(machine.pending_tools().count(), 1);
    let detail_at_start = machine.detail().to_string();

    // A second tool whose bracket opens and closes repeatedly, at ever-later
    // timestamps: this is the clock actually reaching the machine.
    let heartbeat_pre = HookEvent::parse(
        r#"{"hook_event_name":"PreToolUse","session_id":"s","tool_name":"Read","tool_use_id":"h"}"#,
    )
    .unwrap();
    let heartbeat_post = HookEvent::parse(
        r#"{"hook_event_name":"PostToolUse","session_id":"s","tool_name":"Read","tool_use_id":"h"}"#,
    )
    .unwrap();

    for age_ms in [1_000_u64, 10_000, 30_000, 60_000, 300_000, 3_600_000] {
        let now = start_ms + age_ms;
        machine.apply(&heartbeat_pre, now);
        machine.apply(&heartbeat_post, now);

        let transition = machine.observe_terminal(&approval::hint(&working));
        assert!(transition.is_none(), "a working screen changes nothing at {age_ms} ms");
        assert_eq!(
            machine.status(),
            SessionStatus::Working,
            "a bracket open for {age_ms} ms must still read as Working"
        );
        assert_eq!(
            machine.detail(),
            detail_at_start,
            "the detail must not drift with age either, at {age_ms} ms"
        );
        // The age *is* visible to the host, and deliberately unused by the status.
        assert_eq!(machine.longest_pending_ms(now), Some(age_ms));
    }

    // The screen is still the only thing that can move it.
    let t = machine
        .observe_terminal(&approval::hint(&screen("bash-approval")))
        .expect("a real prompt still transitions after an hour of Working");
    assert_eq!(t.to, SessionStatus::AwaitingApproval);
}

/// The measured behaviour behind the `Notification` guard in `status.rs`.
///
/// Recorded separately because it needs a prompt left *unanswered* for several
/// seconds, which the main recording never does — it answers within ~50 ms, so a
/// notification six seconds later cannot appear in it. Without this fixture the
/// claim in the README and in approval-producer.md §5 had no regression net at
/// all: the guard could become dead code, or start suppressing a notification
/// that should transition the session, with nothing failing.
#[test]
fn a_late_notification_arrives_about_a_prompt_that_is_still_up() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/late-notification-session.jsonl");
    let text = std::fs::read_to_string(&path).expect("fixture");
    let rows: Vec<serde_json::Value> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    let notification = rows
        .iter()
        .find(|r| r["event"] == "PermissionRequest")
        .and_then(|_| rows.iter().find(|r| r["event"] == "Notification"))
        .expect("the recording contains a Notification after a PermissionRequest");
    assert_eq!(
        notification["payload"]["message"].as_str(),
        Some("Claude needs your permission"),
        "the message the guard keys its reasoning on"
    );

    // And it really is late: measured against the PreToolUse of the tool whose
    // prompt was left on screen.
    let last_pre = rows
        .iter()
        .rfind(|r| {
            r["event"] == "PreToolUse" && r["at_ms"].as_u64() <= notification["at_ms"].as_u64()
        })
        .expect("a PreToolUse precedes it");
    let gap_ms = notification["at_ms"].as_u64().unwrap() - last_pre["at_ms"].as_u64().unwrap();
    assert!(
        gap_ms > 3_000,
        "the notification should lag the prompt by seconds, not milliseconds; got {gap_ms} ms"
    );

    // Replaying it must leave the session on the status the screen established.
    let mut machine = StatusMachine::new();
    for row in &rows {
        let at_ms = row["at_ms"].as_u64().unwrap();
        let event = HookEvent::from_value(row["payload"].clone()).expect("parses");
        machine.apply(&event, at_ms);
        if event.event_name() == "PreToolUse" && machine.pending_tools().count() == 1 {
            machine.observe_terminal(&approval::hint(&screen("bash-approval")));
        }
    }
    assert_eq!(
        machine.status(),
        SessionStatus::AwaitingApproval,
        "the late notification must not demote the status it is reporting"
    );
}

/// The two halves together, on one recording: hooks give the noun, the screen
/// gives the verb.
#[test]
fn the_recorded_session_reaches_awaiting_approval_and_comes_back() {
    let mut machine = StatusMachine::new();
    let mut sequence = Vec::new();

    for (at_ms, event) in recorded_session() {
        if let Some(t) = machine.apply(&event, at_ms) {
            sequence.push(t.to);
        }

        // The prompt is drawn just after `PreToolUse` — measured at about 30 ms,
        // which is why the app cannot use "a bracket opened" as a stand-in for
        // "no approval needed".
        if event.event_name() == "PreToolUse" && machine.pending_tools().count() == 1 {
            let first_tool =
                machine.pending_tools().next().map(|p| p.tool_name.clone()).unwrap_or_default();
            if first_tool == "Write" {
                assert_eq!(machine.status(), SessionStatus::Working, "hooks alone cannot know");

                let t = machine
                    .observe_terminal(&approval::hint(&screen("write-approval")))
                    .expect("the screen is the only thing that knows");
                assert_eq!(t.to, SessionStatus::AwaitingApproval);
                assert_eq!(
                    machine.detail(),
                    "awaiting approval: Write",
                    "the tool name came from PreToolUse, not from the screen"
                );
                sequence.push(t.to);

                // Answered: the prompt is gone from the screen.
                let back = machine
                    .observe_terminal(&approval::hint(&screen("write-answered")))
                    .expect("clearing is a transition");
                assert_eq!(back.to, SessionStatus::Working);
                sequence.push(back.to);
            }
        }
    }

    assert!(
        sequence.contains(&SessionStatus::AwaitingApproval),
        "the whole point of the recording: {sequence:?}"
    );
    assert_eq!(machine.pending_tools().count(), 0, "no bracket may leak past the end");
}

/// `PermissionRequest` was absent from the `--print` recordings and fires here.
///
/// Kept as a checked-in regression net for the finding, and for its shape: it
/// fires once per prompt and not once per tool, so it is a real rising edge. It
/// is deliberately *not* wired to `AwaitingApproval` — nothing reports the prompt
/// being dismissed, so the falling edge is still only on the screen.
#[test]
fn permission_request_fires_once_per_prompt_not_once_per_tool() {
    let names = recorded_event_names();
    let requests = names.iter().filter(|n| *n == "PermissionRequest").count();
    let pre_tools = names.iter().filter(|n| *n == "PreToolUse").count();

    assert!(requests >= 1, "PermissionRequest did not fire: {names:?}");
    assert!(
        requests < pre_tools,
        "PermissionRequest should track prompts ({requests}), not tools ({pre_tools})"
    );
    assert!(
        !names.iter().any(|n| n == "PostToolUseFailure"),
        "unexpected event in the recording; re-check the finding"
    );
}
