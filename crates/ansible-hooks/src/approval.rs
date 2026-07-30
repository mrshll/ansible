//! Recognising a permission prompt on the rendered screen.
//!
//! This is the producer for the one status hooks cannot supply. `PreToolUse`
//! fires whether or not a human will be asked, and a denied tool is
//! byte-identical to a slow one ([`crate::status`] documents the measurements),
//! so `AwaitingApproval` has to come from the only place the question is
//! actually visible: the terminal the app already owns.
//!
//! Pure, like the rest of this crate: screen text in, [`TerminalHint`] out. The
//! caller renders a [`Snapshot`] to text and hands it over, which is what lets
//! recorded screens replay as tests.
//!
//! [`Snapshot`]: ../../ansible_terminal/snapshot/struct.Snapshot.html
//!
//! # What a real prompt looks like
//!
//! Recorded from `claude` v2.1.220 (`scripts/probe-approval.sh`, fixtures in
//! `tests/fixtures/screens/`). A `Bash` call:
//!
//! ```text
//!  Bash command
//!
//!    python3 -c "print(sum(range(1000000000)))"
//!    Sum integers 0 to 999999999 in Python
//!
//!  This command requires approval
//!
//!  Do you want to proceed?
//!  ❯ 1. Yes
//!    2. Yes, and don’t ask again for: python3 *
//!    3. No
//!
//!  Esc to cancel · Tab to amend · ctrl+e to explain
//! ```
//!
//! A `Write` call differs in the question and the middle option, and carries a
//! diff above the question, but the shape below it is the same.
//!
//! # Why six signals, one of which is positional
//!
//! Detection requires **all** of: a question line beginning `Do you want` and
//! ending `?`; at least two numbered options; a `❯` marker on one of them; an
//! option that refuses; an `Esc to cancel` footer; and — the one that actually
//! does the work — **that footer being the last thing on screen**.
//!
//! The first five co-occur in ordinary text, which is easy to get wrong and was:
//! an earlier version of this module fired on its own doc comment and on
//! `docs/spikes/approval-producer.md`, because every line is trimmed before
//! matching and a fenced example contains the whole block. Content is not enough
//! to tell a live modal from a faithful description of one. Position is: the
//! modal *replaces* the input box, so nothing is drawn beneath it, whereas a
//! session merely displaying a prompt still has its own input box and status line
//! underneath. `is_the_repositorys_own_documentation_a_prompt` is the regression
//! test.
//!
//! The asymmetry is deliberate, and it is the same one
//! `docs/spikes/hook-coverage.md` §3 argues for. A missed prompt degrades to
//! `Working` plus a visible pending age, which is honest. A false prompt trains
//! people to ignore the single status meant to summon them, and that is
//! unrecoverable. **When in doubt this module reports nothing** — or, where the
//! screen looks half-drawn rather than empty, [`TerminalHint::Indeterminate`],
//! which changes nothing at all.
//!
//! # Known gap
//!
//! Prompts whose question does not begin `Do you want` are not detected. The
//! plan-mode confirmation is the likely case: it has every structural signal but
//! reportedly asks `Ready to code?`. Adding it means recording it first — the
//! fixtures exist precisely so these strings are observed rather than guessed —
//! so it is listed in `docs/spikes/approval-producer.md` §8 as follow-up rather
//! than pattern-matched on a hunch.
//!
//! The strings are a TUI's, so they will move. That is why they are named
//! constants here, why the fixtures are recordings rather than hand-written, and
//! why re-recording is one script: a Claude Code upgrade should produce a
//! reviewable diff, not a silently dead grid.

use crate::status::TerminalHint;

/// The question every observed tool prompt opens with.
///
/// Deliberately a prefix and not the whole line: the observed questions were
/// `Do you want to proceed?` and `Do you want to create probe.txt?`, so the tail
/// carries a tool-specific object.
const QUESTION_PREFIX: &str = "Do you want";

/// The modal's footer. Present on every observed prompt.
const FOOTER: &str = "Esc to cancel";

/// How far below the question to look for options and the footer.
///
/// Bounded so a blank region cannot let unrelated content further down the
/// screen complete the pattern. The largest observed block — question, three
/// options, blank, footer — is six lines.
const BLOCK_SCAN_LINES: usize = 12;

/// How many rows above the `?` a soft-wrapped question may be spread over.
///
/// Bounded for the same reason as [`BLOCK_SCAN_LINES`]: without a limit a stray
/// `?` anywhere on screen could walk up to an unrelated `Do you want`. Four rows
/// is a question whose object is a path long enough to fill three of them at 80
/// columns, which is past anything observed.
const QUESTION_WRAP_LINES: usize = 4;

/// One selectable answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalOption {
    /// The digit the user would press.
    pub number: u8,
    /// Label with the marker and numbering stripped, e.g. `Yes`.
    pub text: String,
    /// Whether the `❯` cursor is on this option.
    pub selected: bool,
}

impl ApprovalOption {
    /// Whether this option declines.
    ///
    /// `No` and `No, exit` were both observed, so this matches on the prefix.
    #[must_use]
    pub fn is_refusal(&self) -> bool {
        self.text.starts_with("No")
    }
}

/// A permission prompt found on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalPrompt {
    /// The question line, verbatim and trimmed.
    pub question: String,
    /// The options, in screen order.
    pub options: Vec<ApprovalOption>,
}

impl ApprovalPrompt {
    /// The option the `❯` cursor is on.
    #[must_use]
    pub fn selected(&self) -> Option<&ApprovalOption> {
        self.options.iter().find(|o| o.selected)
    }
}

/// Find a permission prompt in the visible screen text.
///
/// Returns `None` for a screen with no prompt, and for anything that only
/// partly matches — see the module docs on why that direction is the safe one.
#[must_use]
pub fn detect(screen: &str) -> Option<ApprovalPrompt> {
    let lines: Vec<&str> = screen.lines().collect();

    // The footer must be the last thing on screen. The modal *replaces* the
    // input box, so in every recording nothing is drawn below it — whereas a
    // session merely *displaying* a prompt (a `cat` of these fixtures, a doc
    // describing one, an assistant quoting one back) still has its input box and
    // status line underneath.
    //
    // This is load-bearing rather than belt-and-braces. Without it the other
    // signals co-occur in ordinary text: this module's own doc comment and
    // `docs/spikes/approval-producer.md` both contain a complete block, and
    // since every line is trimmed before matching, indentation does not save it.
    let last = lines.iter().rposition(|l| !l.trim().is_empty())?;
    if !lines[last].contains(FOOTER) {
        return None;
    }

    // Then upward from the footer: the live modal is the lowest one, and an
    // earlier prompt may still be in the viewport above it.
    for i in (0..last).rev() {
        let Some(question) = question_ending_at(&lines, i) else {
            continue;
        };
        if let Some(prompt) = parse_block(&question, &lines[i + 1..=last]) {
            return Some(prompt);
        }
    }
    None
}

/// The question line, rejoined across however many rows it soft-wrapped onto.
///
/// A long object wraps — `Do you want to create <long path>?` — which leaves the
/// prefix on one line and the `?` on another, and no single line satisfies both
/// halves of the test on its own. A deep path in a narrow or resized pane takes
/// more than one continuation row, so this walks up rather than looking back
/// exactly one line.
fn question_ending_at(lines: &[&str], i: usize) -> Option<String> {
    let line = lines[i].trim();
    if !line.ends_with('?') {
        return None;
    }
    if line.starts_with(QUESTION_PREFIX) {
        return Some(line.to_string());
    }

    // Upward through the continuation rows to the one holding the prefix. Every
    // stop condition here keeps an unrelated `?` from reaching back across the
    // screen to borrow a `Do you want` that belongs to something else: the walk
    // is bounded, and a blank line, a second question, an option or the footer
    // all end it. Rows are joined with a single space, which is what the
    // unwrapped question was.
    let mut parts: Vec<&str> = vec![line];
    let mut j = i;
    for _ in 0..QUESTION_WRAP_LINES {
        j = j.checked_sub(1)?;
        let previous = lines[j].trim();
        if previous.is_empty()
            || previous.ends_with('?')
            || previous.contains(FOOTER)
            || parse_option(previous).is_some()
        {
            return None;
        }
        parts.push(previous);
        if previous.starts_with(QUESTION_PREFIX) {
            parts.reverse();
            return Some(parts.join(" "));
        }
    }
    None
}

/// Whether a question line is present at all, wrapped or not.
///
/// Separates "no prompt" from "cannot tell yet" — see [`hint`].
fn has_question_line(screen: &str) -> bool {
    screen.lines().any(|l| l.trim().starts_with(QUESTION_PREFIX))
}

/// The hint to hand [`crate::StatusMachine::observe_terminal`].
///
/// The tool name is deliberately `None`. `PreToolUse` fires before the human is
/// asked, so the status machine already holds a reliable name in its open
/// bracket; the screen supplies the fact that someone is *waiting*, which is the
/// half no hook has. Hooks give the noun, the terminal gives the verb.
#[must_use]
pub fn hint(screen: &str) -> TerminalHint {
    match detect(screen) {
        Some(_) => TerminalHint::ApprovalPrompt { tool_name: None },
        // A question with no readable block under it is a frame caught
        // mid-redraw far more often than it is a screen with no prompt, and
        // guessing `NoPrompt` here is what would blink the grid off
        // `AwaitingApproval` and straight back. An answered prompt leaves no
        // question line behind — checked against the recorded screens — so the
        // falling edge still reaches `NoPrompt`.
        None if has_question_line(screen) => TerminalHint::Indeterminate,
        None => TerminalHint::NoPrompt,
    }
}

/// Parse the option list and footer that must follow a question line.
fn parse_block(question: &str, below: &[&str]) -> Option<ApprovalPrompt> {
    let mut options: Vec<ApprovalOption> = Vec::new();
    let mut footer = false;

    for raw in below.iter().take(BLOCK_SCAN_LINES) {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(option) = parse_option(line) {
            options.push(option);
            continue;
        }
        if line.contains(FOOTER) {
            footer = true;
            break;
        }
        // Not an option and not the footer. Most often an option that soft-wrapped
        // onto a second screen line — `2. Yes, and don't ask again for: <a long
        // command> *` at 80 columns — so keep scanning. Breaking here used to make
        // a whole ordinary class of prompt invisible, and every fixture was
        // recorded at 120 columns where nothing wrapped, so no test caught it.
    }

    // Each of these is individually reachable by accident; together they are
    // not. See the module docs.
    if !footer || options.len() < 2 {
        return None;
    }
    if !options.iter().any(|o| o.selected) {
        return None;
    }
    if !options.iter().any(ApprovalOption::is_refusal) {
        return None;
    }

    Some(ApprovalPrompt { question: question.to_string(), options })
}

/// `❯ 1. Yes` or `3. No` into an [`ApprovalOption`].
fn parse_option(line: &str) -> Option<ApprovalOption> {
    let mut rest = line;
    let selected = match rest.strip_prefix('❯') {
        Some(tail) => {
            rest = tail.trim_start();
            true
        }
        None => false,
    };

    let (digits, tail) = rest.split_once('.')?;
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let number = digits.parse().ok()?;

    let text = tail.trim();
    if text.is_empty() {
        return None;
    }

    Some(ApprovalOption { number, text: text.to_string(), selected })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The recorded `Bash` prompt, trimmed to the modal.
    const BASH_PROMPT: &str = "\
 Bash command

   touch sentinel.txt
   Create empty sentinel file

 Do you want to proceed?
 ❯ 1. Yes
   2. Yes, and always allow access to w4-probe/ from this project
   3. No

 Esc to cancel · Tab to amend · ctrl+e to explain";

    #[test]
    fn a_recorded_bash_prompt_is_detected() {
        let prompt = detect(BASH_PROMPT).expect("prompt");
        assert_eq!(prompt.question, "Do you want to proceed?");
        assert_eq!(prompt.options.len(), 3);
        assert_eq!(prompt.selected().map(|o| o.number), Some(1));
        assert_eq!(prompt.selected().map(|o| o.text.as_str()), Some("Yes"));
    }

    #[test]
    fn options_keep_their_numbers_and_lose_their_markers() {
        let prompt = detect(BASH_PROMPT).expect("prompt");
        let numbers: Vec<u8> = prompt.options.iter().map(|o| o.number).collect();
        assert_eq!(numbers, vec![1, 2, 3]);
        assert!(prompt.options[2].is_refusal());
        assert!(!prompt.options[0].is_refusal());
    }

    #[test]
    fn hint_is_a_named_hint_with_no_tool_name() {
        // The name comes from PreToolUse, which is the reliable half.
        assert_eq!(hint(BASH_PROMPT), TerminalHint::ApprovalPrompt { tool_name: None });
    }

    #[test]
    fn an_ordinary_screen_is_no_prompt() {
        assert_eq!(
            hint("❯ some output\n  ⏸ manual mode on · ? for shortcuts"),
            TerminalHint::NoPrompt
        );
        assert!(detect("").is_none());
    }

    /// The failure this module exists to avoid, in its real form.
    ///
    /// An earlier version of this test used prose with no marker and no footer,
    /// which passed for reasons that had nothing to do with the risk: the actual
    /// hazard is a *verbatim* block, and the repository is full of them. This is
    /// what the screen looks like when a session displays one.
    #[test]
    fn is_the_repositorys_own_documentation_a_prompt() {
        let screen = format!(
            "\
 Here is what a real prompt looks like, from the spike doc:

{BASH_PROMPT}

────────────────────────────────────────────────────────────
❯
────────────────────────────────────────────────────────────
  ⏸ manual mode on · ? for shortcuts"
        );
        assert!(
            detect(&screen).is_none(),
            "a complete block with the input box still under it is a description, not a modal"
        );
        // And it must not be mistaken for an *answered* prompt either, because
        // the question is still on screen.
        assert_eq!(hint(&screen), TerminalHint::Indeterminate);
    }

    /// A frame caught between drawing the question and drawing the options.
    #[test]
    fn a_half_drawn_prompt_is_indeterminate_not_absent() {
        let partial = " Bash command\n\n Do you want to proceed?";
        assert!(detect(partial).is_none());
        assert_eq!(
            hint(partial),
            TerminalHint::Indeterminate,
            "reporting NoPrompt here would blink the grid off AwaitingApproval and back"
        );
    }

    /// The wrap that used to make a whole class of prompt invisible.
    #[test]
    fn an_option_that_soft_wrapped_is_still_a_prompt() {
        let screen = "\
 Bash command

 Do you want to proceed?
 ❯ 1. Yes
   2. Yes, and don’t ask again for: docker compose -f ./deploy/docker-compose.yaml
      up --detach --force-recreate *
   3. No

 Esc to cancel · Tab to amend";
        let prompt = detect(screen).expect("a wrapped option must not hide the prompt");
        assert_eq!(prompt.question, "Do you want to proceed?");
        assert_eq!(prompt.options.len(), 3);
        assert!(prompt.options[2].is_refusal());
    }

    /// The same wrap, on the question itself.
    #[test]
    fn a_question_that_soft_wrapped_is_rejoined() {
        let screen = "\
 Do you want to create
 crates/ansible-hooks/tests/fixtures/screens/a-rather-long-fixture-name.screen?
 ❯ 1. Yes
   3. No

 Esc to cancel";
        let prompt = detect(screen).expect("a wrapped question must not hide the prompt");
        assert!(prompt.question.starts_with("Do you want to create"));
        assert!(prompt.question.ends_with(".screen?"));
    }

    /// The same wrap, over more rows than one.
    ///
    /// A narrow or resized pane spreads a long object over several continuation
    /// rows. Looking back exactly one line found the `?` and the prefix on
    /// non-adjacent rows, reported nothing, and left a real prompt reading
    /// `Indeterminate` for as long as it was on screen.
    #[test]
    fn a_question_that_wrapped_over_three_rows_is_rejoined() {
        let screen = "\
 Do you want to create
 crates/ansible-hooks/tests/fixtures/screens/
 a-rather-long-fixture-name-that-keeps-going.screen?
 ❯ 1. Yes
   3. No

 Esc to cancel";
        let prompt = detect(screen).expect("a question over three rows must not hide the prompt");
        assert_eq!(
            prompt.question,
            "Do you want to create crates/ansible-hooks/tests/fixtures/screens/ \
             a-rather-long-fixture-name-that-keeps-going.screen?"
        );
        assert_eq!(prompt.options.len(), 2);
    }

    /// The bound on that walk.
    #[test]
    fn a_stray_question_mark_cannot_reach_a_distant_prefix() {
        let mut screen = String::from(" Do you want to create\n");
        for _ in 0..=QUESTION_WRAP_LINES {
            screen.push_str(" a continuation row\n");
        }
        screen.push_str(" something entirely else?\n ❯ 1. Yes\n   3. No\n\n Esc to cancel\n");
        assert!(detect(&screen).is_none());
    }

    /// And the stops inside it: the walk must not cross a blank line.
    #[test]
    fn a_blank_line_ends_the_question_walk() {
        let screen = "\
 Do you want to create

 probe.txt?
 ❯ 1. Yes
   3. No

 Esc to cancel";
        assert!(detect(screen).is_none(), "a paragraph break is not a soft wrap");
    }

    /// The positional signal, on its own.
    #[test]
    fn a_prompt_with_anything_drawn_below_it_is_not_live() {
        let screen = format!("{BASH_PROMPT}\n\n❯ \n  ⏸ manual mode on");
        assert!(detect(&screen).is_none());
    }

    #[test]
    fn a_question_with_no_options_is_not_a_prompt() {
        assert!(detect("Do you want to proceed?\n\n Esc to cancel").is_none());
    }

    #[test]
    fn a_prompt_with_no_selection_marker_is_not_a_prompt() {
        let screen = "\
 Do you want to proceed?
   1. Yes
   3. No

 Esc to cancel";
        assert!(detect(screen).is_none());
    }

    #[test]
    fn a_prompt_with_no_refusal_is_not_a_prompt() {
        // A numbered chooser that cannot be declined is some other UI.
        let screen = "\
 Do you want to proceed?
 ❯ 1. Alpha
   2. Beta

 Esc to cancel";
        assert!(detect(screen).is_none());
    }

    #[test]
    fn a_prompt_whose_footer_scrolled_away_is_not_a_prompt() {
        let screen = "\
 Do you want to proceed?
 ❯ 1. Yes
   3. No";
        assert!(detect(screen).is_none(), "reporting nothing is the safe direction");
    }

    #[test]
    fn distant_content_cannot_complete_the_pattern() {
        let mut screen = String::from(" Do you want to proceed?\n");
        for _ in 0..BLOCK_SCAN_LINES + 2 {
            screen.push('\n');
        }
        screen.push_str(" ❯ 1. Yes\n   3. No\n\n Esc to cancel\n");
        assert!(detect(&screen).is_none());
    }

    #[test]
    fn the_lowest_question_on_screen_wins() {
        // An earlier prompt may still be in the viewport above the live one.
        let screen =
            format!("{BASH_PROMPT}\n\n{}", BASH_PROMPT.replace("proceed?", "create x.txt?"));
        let prompt = detect(&screen).expect("prompt");
        assert_eq!(prompt.question, "Do you want to create x.txt?");
    }

    #[test]
    fn a_diff_above_the_question_does_not_confuse_the_options() {
        // Digits in a rendered diff (`  1 hello`) are not option lines: they
        // have no `.` separator.
        let screen = "\
 Create file
 probe.txt
  1 hello
  2 world
 Do you want to create probe.txt?
 ❯ 1. Yes
   2. Yes, allow all edits during this session (shift+tab)
   3. No

 Esc to cancel · Tab to amend";
        let prompt = detect(screen).expect("prompt");
        assert_eq!(prompt.options.len(), 3);
    }

    #[test]
    fn option_parsing_rejects_non_options() {
        assert!(parse_option("● Done — sentinel.txt created").is_none());
        assert!(parse_option("1 hello").is_none());
        assert!(parse_option("1.").is_none());
        assert!(parse_option("x. Yes").is_none());
        assert_eq!(parse_option("❯ 1. Yes").map(|o| o.selected), Some(true));
        assert_eq!(parse_option("3. No").map(|o| o.selected), Some(false));
    }
}
