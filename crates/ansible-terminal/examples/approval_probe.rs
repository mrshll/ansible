//! Drive a real Claude Code session to a real permission prompt, and prove the
//! grid can see it.
//!
//!   cargo run -p ansible-terminal --example approval-probe -- --out DIR [--cwd DIR]
//!
//! This is the producer for `AwaitingApproval`. Everything else on the grid
//! comes from hooks; this status cannot, because a denied tool and a slow tool
//! emit identical hook sequences (`docs/spikes/hook-coverage.md` §3). So the
//! probe runs both halves at once — a real hook overlay writing real payloads,
//! and `ansible_hooks::approval` reading the rendered screen — into **one**
//! `StatusMachine`, which is exactly the wiring the app will do.
//!
//! It asserts the three things the claim rests on:
//!
//! 1. a real prompt drives `AwaitingApproval` promptly,
//! 2. answering it returns the session to `Working`,
//! 3. a long legitimate tool call never trips it.
//!
//! Exit status is the result. It also writes the screens it saw to `--out` as
//! fixtures, which is what makes a Claude Code TUI change a reviewable diff
//! instead of a silently dead grid.
//!
//! Needs interactive `claude` credentials and a machine where a permission
//! prompt actually appears. The container this project was developed in forces a
//! permissive mode, which is why this could not be written until now.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ansible_hooks::{HookEvent, SessionStatus, StatusMachine, approval};
use ansible_terminal::{
    GhosttyTerminal, Key, KeyEvent, Modifiers, TerminalBackend, TerminalConfig, TerminalEvent,
    TerminalEvents, TerminalInput, TerminalSize,
};

/// Long enough for a model turn plus a tool round trip on a slow link.
const STEP_TIMEOUT: Duration = Duration::from_secs(120);
/// The negative case: a genuinely slow foreground tool.
///
/// CPU-bound rather than a `sleep`, for two measured reasons. Claude Code
/// dispatches a long `sleep` as a *background* task, which closes the bracket in
/// ~40 ms and measures nothing; and this environment blocks foreground sleeps
/// outright, so the model declines rather than running one. A compute loop is
/// neither special-cased nor backgroundable.
///
/// The live probe therefore asserts "a real pending tool never trips it" for
/// however long the environment allows. The stronger claim — that *no* elapsed
/// time trips it — is asserted deterministically against this run's recorded
/// screen in `ansible-hooks/tests/screen_replay.rs`, where it needs no
/// credentials and cannot be flaky.
const SLOW_TOOL_COMMAND: &str = r#"python3 -c "print(sum(range(1000000000)))""#;
/// Every event name present in the Claude Code binary, so the recording shows
/// which ones fire rather than only the ones we expect. Two of these —
/// `Notification` and `PermissionRequest` — are open questions from
/// hook-coverage §5 that a real prompt can finally answer.
const HOOK_EVENTS: &[&str] = &[
    "PreToolUse",
    "PostToolUse",
    "UserPromptSubmit",
    "Notification",
    "Stop",
    "SubagentStop",
    "SessionStart",
    "SessionEnd",
    "PreCompact",
    "PostToolUseFailure",
    "PermissionRequest",
];

type Failure = Box<dyn std::error::Error>;

fn main() -> Result<(), Failure> {
    let mut args = Args::parse()?;
    std::fs::create_dir_all(&args.out)?;
    std::fs::create_dir_all(&args.cwd)?;
    // The child runs in `--cwd`, not ours, so every path handed to it — the
    // settings overlay, the receiver it names, `HOOK_LOG` — has to be absolute.
    // A relative `--out` would otherwise resolve *below* the work directory
    // (`out/settings.json` becoming `out/work/out/settings.json`), the overlay
    // would not load, and the probe would fail with no hooks rather than
    // recording anything. Once, here, now that the directories exist.
    args.out = args.out.canonicalize()?;
    args.cwd = args.cwd.canonicalize()?;

    let overlay = write_hook_overlay(&args.out)?;
    let mut probe = Probe::spawn(&args, &overlay)?;

    let result = probe.run();
    probe.write_report()?;
    // Shut the child down whatever happened, then report.
    probe.finish();

    let checks = result?;
    println!("\n{}", checks.summary());
    if checks.failed == 0 {
        print_recording_instructions(&probe.out);
        Ok(())
    } else {
        Err(format!("{} check(s) failed", checks.failed).into())
    }
}

/// Both recordings, in one place.
///
/// The screens and the hook log are a matched pair — two of the tests assert
/// across them — so instructions that name only one produce a half-updated
/// fixture set whose failure is far harder to read than the change that caused it.
fn print_recording_instructions(out: &Path) {
    println!("recordings in {}", out.display());
    println!("to update the checked-in fixtures:");
    println!("  cp {}/*.screen crates/ansible-hooks/tests/fixtures/screens/", out.display());
    println!(
        "  cp {}/session.jsonl crates/ansible-hooks/tests/fixtures/approval-session.jsonl",
        out.display()
    );
    println!("then: cargo test -p ansible-hooks");
}

// ---------------------------------------------------------------- arguments

struct Args {
    out: PathBuf,
    cwd: PathBuf,
    command: String,
}

impl Args {
    fn parse() -> Result<Self, Failure> {
        let mut out = None;
        let mut cwd = None;
        let mut command = "claude".to_string();

        let mut argv = std::env::args().skip(1);
        while let Some(flag) = argv.next() {
            let mut value = || argv.next().ok_or_else(|| format!("{flag} needs a value"));
            match flag.as_str() {
                "--out" => out = Some(PathBuf::from(value()?)),
                "--cwd" => cwd = Some(PathBuf::from(value()?)),
                "--claude" => command = value()?,
                other => return Err(format!("unknown flag `{other}`").into()),
            }
        }

        let out = out.ok_or("usage: approval-probe --out DIR [--cwd DIR] [--claude PATH]")?;
        let cwd = cwd.unwrap_or_else(|| out.join("work"));
        Ok(Self { out, cwd, command })
    }
}

// ------------------------------------------------------------------- checks

/// Assertions, collected rather than fatal.
///
/// One failure should not hide the state of the others: knowing that detection
/// worked but clearing did not is a different bug from the reverse.
#[derive(Default)]
struct Checks {
    lines: Vec<String>,
    failed: usize,
}

impl Checks {
    fn require(&mut self, ok: bool, claim: &str) {
        if ok {
            self.lines.push(format!("  PASS  {claim}"));
        } else {
            self.lines.push(format!("  FAIL  {claim}"));
            self.failed += 1;
        }
        println!("{}", self.lines.last().expect("just pushed"));
    }

    fn note(&mut self, text: &str) {
        self.lines.push(format!("  ....  {text}"));
        println!("{}", self.lines.last().expect("just pushed"));
    }

    fn summary(&self) -> String {
        let passed = self.lines.iter().filter(|l| l.contains("PASS")).count();
        format!("{passed} passed, {} failed", self.failed)
    }
}

// -------------------------------------------------------------------- probe

struct Probe {
    term: GhosttyTerminal,
    events: TerminalEvents,
    machine: StatusMachine,
    out: PathBuf,
    hook_log: PathBuf,
    hook_lines_seen: usize,
    /// Event names observed, in first-seen order.
    hooks_fired: Vec<String>,
    /// How many times each event fired. `PermissionRequest` versus `PreToolUse`
    /// is the ratio that shows it tracks prompts rather than tools.
    hook_counts: BTreeMap<String, usize>,
    started: Instant,
    /// When the child last wrote anything. The prompt's own bytes are the last
    /// thing to arrive before it becomes visible, so this is the baseline for
    /// "how long after it was drawn did we notice".
    last_output: Option<Instant>,
    transitions: Vec<String>,
    detect_calls: u64,
    detect_total: Duration,
    detect_max: Duration,
}

impl Probe {
    fn spawn(args: &Args, overlay: &Path) -> Result<Self, Failure> {
        let hook_log = args.out.join("session.jsonl");
        let _ = std::fs::remove_file(&hook_log);

        let config = TerminalConfig::command(&args.command, TerminalSize::new(120, 40, 8, 16))
            .args(["--settings".to_string(), overlay.display().to_string()])
            // portable-pty does not inherit the parent's directory: with no cwd
            // the child lands in $HOME, which is not somewhere to let a probe
            // create files.
            .cwd(&args.cwd)
            .env("HOOK_LOG", hook_log.display().to_string())
            .env("LC_ALL", "C.UTF-8");

        let term = GhosttyTerminal::spawn(&config)?;
        let events = term.events();
        Ok(Self {
            term,
            events,
            machine: StatusMachine::new(),
            out: args.out.clone(),
            hook_log,
            hook_lines_seen: 0,
            hooks_fired: Vec::new(),
            hook_counts: BTreeMap::new(),
            started: Instant::now(),
            last_output: None,
            transitions: Vec::new(),
            detect_calls: 0,
            detect_total: Duration::ZERO,
            detect_max: Duration::ZERO,
        })
    }

    fn elapsed_ms(&self) -> u128 {
        self.started.elapsed().as_millis()
    }

    /// One host frame: pump the PTY, take the hooks, then read the screen.
    ///
    /// The order matters. `PreToolUse` supplies the tool name and the screen
    /// supplies the fact that someone is waiting, so applying hooks first means
    /// the detail string is already correct when the prompt is recognised.
    fn tick(&mut self) -> Result<(), Failure> {
        self.term.pump()?;

        while let Ok(event) = self.events.try_recv() {
            match event {
                TerminalEvent::Output(_) => self.last_output = Some(Instant::now()),
                TerminalEvent::Exited(reason) => {
                    self.record(&format!("child exited: {reason:?}"));
                }
                _ => {}
            }
        }

        self.drain_hooks();

        let screen = self.term.snapshot()?.screen_text();
        let at = Instant::now();
        let hint = approval::hint(&screen);
        let cost = at.elapsed();
        self.detect_calls += 1;
        self.detect_total += cost;
        self.detect_max = self.detect_max.max(cost);

        if let Some(t) = self.machine.observe_terminal(&hint) {
            self.record(&format!("terminal  {:?} -> {:?}  {}", t.from, t.to, t.detail));
        }
        Ok(())
    }

    /// Apply hook payloads the receiver has appended since the last tick.
    ///
    /// Only *terminated* lines are consumed. The receiver is a separate
    /// short-lived process per hook invocation and this polls the same file every
    /// 5 ms, so a read can land mid-append; counting a torn line as seen would
    /// drop that event permanently. Dropping a `PostToolUse` in particular leaves
    /// the bracket open for the rest of the run, which makes the slow-tool check
    /// pass for entirely the wrong reason.
    fn drain_hooks(&mut self) {
        let Ok(text) = std::fs::read_to_string(&self.hook_log) else {
            return;
        };
        // A trailing fragment with no newline is still being written. Leave it.
        let complete = match text.rfind('\n') {
            Some(end) => &text[..=end],
            None => return,
        };
        let lines: Vec<&str> = complete.lines().filter(|l| !l.trim().is_empty()).collect();
        if lines.len() <= self.hook_lines_seen {
            return;
        }

        let fresh: Vec<String> =
            lines[self.hook_lines_seen..].iter().map(|s| (*s).to_string()).collect();
        self.hook_lines_seen = lines.len();

        for line in fresh {
            // A terminated line that still will not parse is a real problem, not
            // a race, so say so rather than silently continuing.
            let Ok(row) = serde_json::from_str::<serde_json::Value>(&line) else {
                self.record(&format!("WARNING: unparseable hook line: {line:.120}"));
                continue;
            };
            let at_ms = row["at_ms"].as_u64().unwrap_or(0);
            let Ok(event) = HookEvent::from_value(row["payload"].clone()) else {
                self.record("WARNING: hook payload has no hook_event_name");
                continue;
            };

            let name = event.event_name().to_string();
            if !self.hooks_fired.contains(&name) {
                self.hooks_fired.push(name.clone());
            }
            *self.hook_counts.entry(name).or_default() += 1;
            if let Some(t) = self.machine.apply(&event, at_ms) {
                self.record(&format!(
                    "hook      {:?} -> {:?}  {}  ({})",
                    t.from,
                    t.to,
                    t.detail,
                    event.event_name()
                ));
            }
        }
    }

    fn record(&mut self, what: &str) {
        let line = format!("[{:>7} ms] {what}", self.elapsed_ms());
        println!("{line}");
        self.transitions.push(line);
    }

    /// Pump until `ready` holds, or the timeout elapses.
    fn until(
        &mut self,
        timeout: Duration,
        mut ready: impl FnMut(&mut Self) -> bool,
    ) -> Result<bool, Failure> {
        let deadline = Instant::now() + timeout;
        loop {
            self.tick()?;
            if ready(self) {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn until_screen(&mut self, timeout: Duration, needle: &str) -> Result<bool, Failure> {
        let needle = needle.to_string();
        self.until(timeout, |p| p.term.snapshot().is_ok_and(|s| s.screen_text().contains(&needle)))
    }

    fn until_prompt(&mut self, timeout: Duration) -> Result<bool, Failure> {
        self.until(timeout, |p| p.machine.status() == SessionStatus::AwaitingApproval)
    }

    fn pump_for(&mut self, duration: Duration) -> Result<(), Failure> {
        self.until(duration, |_| false)?;
        Ok(())
    }

    /// Wait for `Stop`, i.e. for the assistant to hand the turn back.
    ///
    /// Typing the next prompt before this lands gets it queued into the running
    /// turn instead of starting a new one, which silently skips a phase.
    fn wait_for_turn_end(&mut self, timeout: Duration) -> Result<bool, Failure> {
        self.until(timeout, |p| p.machine.status() == SessionStatus::AwaitingInput)
    }

    fn screen(&mut self) -> Result<String, Failure> {
        Ok(self.term.snapshot()?.screen_text())
    }

    fn save_screen(&mut self, label: &str) -> Result<(), Failure> {
        let text = self.screen()?;
        let path = self.out.join(format!("{label}.screen"));
        std::fs::write(&path, format!("{text}\n"))?;
        println!("           recorded {}", path.display());
        Ok(())
    }

    /// Type a prompt and wait for Claude Code to accept it.
    ///
    /// Returning before `UserPromptSubmit` lands would let the caller start
    /// waiting for `Stop` while the *previous* turn's `AwaitingInput` is still
    /// current, and every such wait would return instantly.
    fn submit(&mut self, text: &str) -> Result<(), Failure> {
        let before = self.hook_counts.get("UserPromptSubmit").copied().unwrap_or(0);
        self.term.send(TerminalInput::Text(text.to_string()))?;
        // Claude Code's input box debounces; sending Enter in the same frame as
        // the text can submit an empty prompt.
        self.pump_for(Duration::from_millis(200))?;
        self.press(Key::Enter)?;

        let accepted = self.until(Duration::from_secs(30), |p| {
            p.hook_counts.get("UserPromptSubmit").copied().unwrap_or(0) > before
        })?;
        if !accepted {
            return Err("the prompt was never accepted (no UserPromptSubmit)".into());
        }
        Ok(())
    }

    fn press(&mut self, key: Key) -> Result<(), Failure> {
        self.term.send(TerminalInput::Key(KeyEvent::press(key, Modifiers::NONE)))?;
        Ok(())
    }

    fn finish(&mut self) {
        let _ = self.term.shutdown();
    }

    // ------------------------------------------------------------ the script

    fn run(&mut self) -> Result<Checks, Failure> {
        let mut checks = Checks::default();

        self.trust_gate(&mut checks)?;
        self.approval_round(&mut checks)?;
        self.long_tool(&mut checks)?;
        self.allowed_tool(&mut checks)?;
        self.report_hooks(&mut checks);

        Ok(checks)
    }

    /// The folder-trust gate is a numbered prompt that is *not* a tool approval.
    ///
    /// It is the closest look-alike the TUI has, and it appears before any tool
    /// runs, so mistaking it for an approval would put every fresh session on the
    /// grid as `AwaitingApproval`.
    fn trust_gate(&mut self, checks: &mut Checks) -> Result<(), Failure> {
        println!("\n== the folder-trust gate is not a tool approval ==");
        if !self.until_screen(Duration::from_secs(60), "trust")? {
            checks.note("no trust gate (already-trusted directory); skipping the look-alike check");
            return Ok(());
        }

        self.save_screen("folder-trust")?;
        let screen = self.screen()?;
        checks.require(
            approval::detect(&screen).is_none(),
            "the folder-trust gate is not detected as a tool approval",
        );
        checks.require(
            self.machine.status() != SessionStatus::AwaitingApproval,
            "a fresh session does not reach AwaitingApproval before any tool runs",
        );

        self.press(Key::Enter)?;
        self.pump_for(Duration::from_secs(6))?;
        Ok(())
    }

    /// Criteria 1 and 2: a real prompt is seen, and answering it clears.
    fn approval_round(&mut self, checks: &mut Checks) -> Result<(), Failure> {
        println!("\n== a real approval prompt drives AwaitingApproval ==");
        self.submit("Create a file called probe.txt containing the single word hello")?;

        if !self.until_prompt(STEP_TIMEOUT)? {
            self.save_screen("no-approval-seen")?;
            checks.require(false, "a real permission prompt was detected");
            return Ok(());
        }

        // The prompt's own bytes are the last thing to arrive before it is
        // visible, so this is the delay the grid would inherit.
        let noticed_after = self.last_output.map(|t| t.elapsed());
        self.save_screen("write-approval")?;

        checks.require(true, "a real permission prompt was detected");
        if let Some(delay) = noticed_after {
            let ms = delay.as_secs_f64() * 1000.0;
            checks.require(
                delay < Duration::from_secs(1),
                &format!("detected within a second of the prompt being drawn ({ms:.1} ms)"),
            );
        }

        let detail = self.machine.detail().to_string();
        checks.require(
            detail.starts_with("awaiting approval"),
            &format!("the detail names the interruption (`{detail}`)"),
        );
        checks.require(
            detail.len() > "awaiting approval".len(),
            &format!("the tool name came from PreToolUse, not the screen (`{detail}`)"),
        );

        let prompt = approval::detect(&self.screen()?).expect("prompt is on screen");
        checks.note(&format!("question: {}", prompt.question));
        checks.require(
            prompt.options.iter().any(ansible_hooks::ApprovalOption::is_refusal),
            "the prompt offers a refusal, so a human really is being asked",
        );

        // Criterion 2: answer it. Option 1 is the selected `Yes`.
        println!("\n== answering it returns the session to Working ==");
        let answered_at = Instant::now();
        self.press(Key::Enter)?;

        let cleared = self.until(Duration::from_secs(30), |p| {
            p.machine.status() != SessionStatus::AwaitingApproval
        })?;
        checks.require(cleared, "the prompt clearing was noticed");
        if cleared {
            let ms = answered_at.elapsed().as_secs_f64() * 1000.0;
            checks.note(&format!("cleared {ms:.0} ms after the keystroke"));
            checks.require(
                self.machine.status() == SessionStatus::Working,
                &format!("back to Working (got {:?})", self.machine.status()),
            );
        }
        self.wait_for_turn_end(Duration::from_secs(60))?;
        self.save_screen("write-answered")?;
        checks.require(
            approval::detect(&self.screen()?).is_none(),
            "the answered screen holds no prompt",
        );
        Ok(())
    }

    /// Criterion 3: the case a timer would get wrong.
    ///
    /// A tool that legitimately runs far longer than any plausible timeout must
    /// never read as an approval. This is the whole reason the status is taken
    /// from the screen rather than from `longest_pending_ms`.
    fn long_tool(&mut self, checks: &mut Checks) -> Result<(), Failure> {
        println!("\n== a slow legitimate tool call never trips it ==");
        self.submit(&format!(
            "Use the Bash tool to run exactly this command and wait for it to finish, \
             with no explanation first: {SLOW_TOOL_COMMAND}"
        ))?;

        // One loop for the whole turn, because the interesting states interleave:
        // `PreToolUse` lands about 30 ms *before* the prompt is drawn, so waiting
        // on "a bracket opened" as a proxy for "no approval needed" reads the gap
        // as an answer and then mistakes the prompt for a false positive. Only
        // the screen knows, and only after it has been drawn.
        let deadline = Instant::now() + Duration::from_secs(240);
        let mut answered = false;
        let mut bash_question = None;
        let mut working_screen = None;
        let mut mid_tool_samples = 0_u64;
        let mut longest_open_ms = 0_u64;
        let mut retripped = false;
        let mut cleared = false;

        loop {
            self.tick()?;
            longest_open_ms =
                longest_open_ms.max(self.machine.longest_pending_ms(now_ms()).unwrap_or(0));

            match self.machine.status() {
                SessionStatus::AwaitingApproval => {
                    if answered {
                        // Only a re-trip if it had already gone away: the prompt
                        // legitimately stays on screen for the few tens of
                        // milliseconds between the keystroke and the redraw.
                        if cleared {
                            retripped = true;
                        }
                    } else {
                        self.save_screen("bash-approval")?;
                        bash_question = approval::detect(&self.screen()?).map(|p| p.question);
                        answered = true;
                        self.press(Key::Enter)?;
                    }
                }
                // Mid-tool with nothing on screen: the negative case.
                SessionStatus::Working if self.machine.pending_tools().count() > 0 => {
                    if answered {
                        cleared = true;
                    }
                    mid_tool_samples += 1;
                    if working_screen.is_none() && longest_open_ms > 2_000 {
                        self.save_screen("long-tool-working")?;
                        working_screen = Some(self.screen()?);
                    }
                }
                // `Stop` fired: the turn is over, and it is safe to type again.
                SessionStatus::AwaitingInput => break,
                _ => {}
            }

            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        if let Some(question) = &bash_question {
            checks.note(&format!("bash prompt: {question}"));
        }
        checks.require(answered, "a second prompt shape (Bash) was detected and answered");
        checks.note(&format!(
            "{mid_tool_samples} screens read mid-tool; longest open bracket {:.1} s",
            f64::from(u32::try_from(longest_open_ms).unwrap_or(u32::MAX)) / 1000.0
        ));
        checks.require(!retripped, "answering did not flicker back to AwaitingApproval");
        checks.require(
            longest_open_ms >= 2_000,
            &format!(
                "a bracket stayed open while the tool ran ({longest_open_ms} ms; if this is \
                 tiny, the environment backgrounded or refused the command)"
            ),
        );
        match &working_screen {
            Some(screen) => checks.require(
                approval::detect(screen).is_none(),
                "a real mid-tool screen reads as no prompt",
            ),
            None => checks.require(false, "recorded a mid-tool screen for the any-duration test"),
        }
        Ok(())
    }

    /// A tool that runs without asking.
    ///
    /// Needed to make the `PermissionRequest` claim falsifiable: "it fires when a
    /// human is asked" only means something if there is also a tool call where
    /// nobody was asked and it stayed silent.
    fn allowed_tool(&mut self, checks: &mut Checks) -> Result<(), Failure> {
        println!("\n== a tool that runs without asking ==");
        let before = self.hook_counts.get("PreToolUse").copied().unwrap_or(0);
        // `echo` is allowed without asking, and asking for a command rather than
        // a fact forces a fresh tool call: asked to *read* probe.txt the model
        // answers from earlier context and calls nothing.
        self.submit(
            "Use the Bash tool to run exactly this command, with no explanation: echo probe-allowed",
        )?;

        let ran = self.until(Duration::from_secs(120), |p| {
            p.hook_counts.get("PreToolUse").copied().unwrap_or(0) > before
        })?;
        checks.require(ran, "a further tool call was observed");
        // Let PostToolUse and Stop land before the counts are read.
        self.wait_for_turn_end(Duration::from_secs(90))?;
        Ok(())
    }

    /// Answers the two questions hook-coverage §5 left open.
    ///
    /// Both `Notification` and `PermissionRequest` were absent from the `--print`
    /// recordings, and both fire here. `PermissionRequest` is the interesting one:
    /// it fires when a human is actually asked and not for a tool that was
    /// allowed, which makes it a real rising edge — the falling edge is still
    /// only on the screen, so it corroborates this detector rather than replacing
    /// it. See `docs/spikes/approval-producer.md`.
    fn report_hooks(&mut self, checks: &mut Checks) {
        println!("\n== which hooks fired on a real interactive session ==");
        checks.note(&format!("fired: {}", self.hooks_fired.join(", ")));

        for open_question in ["Notification", "PermissionRequest"] {
            let n = self.hook_counts.get(open_question).copied().unwrap_or(0);
            checks.note(&format!(
                "{open_question}: {}",
                if n == 0 { "did not fire".to_string() } else { format!("fired {n}x") }
            ));
        }

        let requests = self.hook_counts.get("PermissionRequest").copied().unwrap_or(0);
        let pre_tools = self.hook_counts.get("PreToolUse").copied().unwrap_or(0);
        checks.require(requests >= 1, "PermissionRequest fires on a real permission prompt");
        checks.require(
            requests < pre_tools,
            &format!(
                "PermissionRequest fires only when a human is asked, not per tool \
                 ({requests} of {pre_tools} PreToolUse)"
            ),
        );
        checks.require(
            !self.hooks_fired.is_empty(),
            "the hook overlay was installed and delivered payloads",
        );
    }

    fn write_report(&mut self) -> Result<(), Failure> {
        let mut report = String::new();
        let _ = writeln!(report, "# approval-probe");
        let _ = writeln!(report);
        let _ = writeln!(report, "hooks fired: {}", self.hooks_fired.join(", "));
        let mean_us = if self.detect_calls == 0 {
            0.0
        } else {
            self.detect_total.as_secs_f64() * 1e6
                / f64::from(u32::try_from(self.detect_calls).unwrap_or(u32::MAX))
        };
        let _ = writeln!(
            report,
            "detector: {} screens, mean {mean_us:.1} us, max {:.1} us",
            self.detect_calls,
            self.detect_max.as_secs_f64() * 1e6
        );
        let _ = writeln!(report);
        let _ = writeln!(report, "## transitions");
        for line in &self.transitions {
            let _ = writeln!(report, "{line}");
        }
        std::fs::write(self.out.join("report.md"), report)?;
        Ok(())
    }
}

/// Wall clock in milliseconds — the same epoch the receiver stamps its lines
/// with, so pending-bracket ages compare correctly against hook timestamps.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

// ------------------------------------------------------------ hook overlay

/// Write a settings overlay subscribing every hook event to a logging receiver.
///
/// The same mechanism `scripts/capture-hook-payloads.sh` uses, and the same
/// wrapped line shape, so the log this produces is a fixture the existing
/// replay tests can load.
fn write_hook_overlay(out: &Path) -> Result<PathBuf, Failure> {
    let receiver = out.join("receiver.py");
    std::fs::write(
        &receiver,
        r#"#!/usr/bin/env python3
"""Append one JSON line per hook invocation: {event, at_ms, payload}."""
import json, os, sys, time

event = sys.argv[1] if len(sys.argv) > 1 else "unknown"
raw = sys.stdin.read()
try:
    payload = json.loads(raw) if raw.strip() else None
except json.JSONDecodeError:
    payload = {"_unparsed": raw[:4000]}
with open(os.environ["HOOK_LOG"], "a", encoding="utf-8") as f:
    f.write(json.dumps({"event": event, "at_ms": int(time.time() * 1000), "payload": payload}) + "\n")
"#,
    )?;

    let hooks: Vec<String> = HOOK_EVENTS
        .iter()
        .map(|event| {
            format!(
                r#"    "{event}": [{{"hooks": [{{"type": "command", "command": "python3 {} {event}"}}]}}]"#,
                receiver.display()
            )
        })
        .collect();

    let overlay = out.join("settings.json");
    std::fs::write(&overlay, format!("{{\n  \"hooks\": {{\n{}\n  }}\n}}\n", hooks.join(",\n")))?;
    Ok(overlay)
}
