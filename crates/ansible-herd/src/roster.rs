//! The herd view: many members' presence documents collapsed into one ordered
//! list, and that list rendered as text.
//!
//! Both halves are pure functions of `(documents, me, now)`. That is not
//! incidental tidiness — ordering *is* the product here. A presence view that
//! buries the one blocked session under nine idle ones has failed at the only
//! thing it exists to do, so the ordering rule is a tested function rather than a
//! property of whatever loop happens to draw the screen.
//!
//! # Why this is not a full-screen TUI
//!
//! The workspace forbids `unsafe`, and putting a terminal into raw mode means
//! `termios` — so a hand-written full-screen view is not available without either
//! relaxing that lint or taking a dependency. A prototype does not need one: the
//! roster prints, waits for a line, acts, and reprints. Herdr already gives the
//! overlay pane, the focus restore, and the keybinding; what is missing is only
//! cursor movement. `crossterm` or `ratatui` is the upgrade, and it is a
//! self-contained one.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::model::{HelpWanted, MemberDoc, Share, Status};

/// One line of the herd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// 1-based selector the user types.
    pub index: usize,
    pub key: String,
    pub who: String,
    pub is_me: bool,
    pub agent: String,
    pub status: Status,
    pub headline: String,
    /// Workspace and tab, or repo and branch when the worktree knows them.
    pub location: String,
    pub help: Option<HelpWanted>,
    /// Whether this is the row that should print the note. Exactly one row per
    /// member gets it — the most urgent one — so a hand raised once is said once.
    pub show_help: bool,
    pub share: Share,
    /// Logins watching this session right now, including possibly ourselves.
    pub watchers: Vec<String>,
    /// The member's heartbeat is late. The row is still shown, because "Alice was
    /// blocked and her laptop went to sleep" is information.
    pub stale: bool,
    /// Milliseconds in the current status.
    pub age_ms: u64,
    /// Highest live chunk the owner has published, so a viewer joins at the
    /// current moment instead of replaying the backlog.
    pub live_seq: Option<u64>,
}

impl Row {
    /// Whether this row is asking for a person.
    #[must_use]
    pub fn wants_a_human(&self) -> bool {
        self.help.is_some() || self.status.wants_a_human()
    }

    /// Whether a teammate can open this session and see it live.
    #[must_use]
    pub fn is_watchable(&self) -> bool {
        self.share == Share::Live
    }
}

/// Collapse presence documents into the ordered herd.
///
/// Members quiet for longer than `forget_after_ms` are dropped entirely; members
/// quiet for longer than `stale_after_ms` are kept but marked, and sort below
/// everything fresh regardless of status. A stale `blocked` row is a claim about
/// the past, and it must not outrank a live one.
#[must_use]
pub fn rows(
    members: &[MemberDoc],
    me: &str,
    now_ms: u64,
    stale_after_ms: u64,
    forget_after_ms: u64,
) -> Vec<Row> {
    let watchers = watcher_index(members);

    let mut rows: Vec<Row> = Vec::new();
    for member in members {
        if member.is_stale(now_ms, forget_after_ms) {
            continue;
        }
        let stale = member.is_stale(now_ms, stale_after_ms);
        let who = member.display_name.clone().unwrap_or_else(|| member.login.clone());
        for card in &member.agents {
            // `share = "off"` should never have been published at all, but a row
            // is cheap to drop and a leak is not.
            if card.share == Share::Off {
                continue;
            }
            rows.push(Row {
                index: 0,
                key: card.key.clone(),
                who: who.clone(),
                is_me: member.login == me,
                agent: card.agent.clone(),
                status: card.status,
                headline: card.headline.clone(),
                location: location(card),
                help: card.help.clone().or_else(|| member.help.clone()),
                show_help: false,
                share: card.share,
                watchers: watchers.get(&card.key).cloned().unwrap_or_default(),
                stale,
                age_ms: now_ms.saturating_sub(card.since_ms),
                live_seq: card.live_seq,
            });
        }
    }

    rows.sort_by(|a, b| {
        a.stale
            .cmp(&b.stale)
            .then(b.wants_a_human().cmp(&a.wants_a_human()))
            .then(a.status.cmp(&b.status))
            .then(b.age_ms.cmp(&a.age_ms))
            .then(a.who.cmp(&b.who))
            .then(a.key.cmp(&b.key))
    });
    let mut announced: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (i, row) in rows.iter_mut().enumerate() {
        row.index = i + 1;
        if row.help.is_some() && announced.insert(row.who.clone()) {
            row.show_help = true;
        }
    }
    rows
}

fn location(card: &crate::model::AgentCard) -> String {
    match (&card.repo, &card.branch) {
        (Some(repo), Some(branch)) => format!("{repo}@{branch}"),
        (Some(repo), None) => repo.clone(),
        (None, Some(branch)) => branch.clone(),
        (None, None) => match (&card.workspace, &card.tab) {
            (Some(ws), Some(tab)) => format!("{ws}/{tab}"),
            (Some(ws), None) => ws.clone(),
            _ => card.pane_id.clone(),
        },
    }
}

/// Who is watching what, from every member's published intent.
fn watcher_index(members: &[MemberDoc]) -> BTreeMap<String, Vec<String>> {
    let mut index: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for member in members {
        for key in &member.watching {
            let entry = index.entry(key.clone()).or_default();
            if !entry.contains(&member.login) {
                entry.push(member.login.clone());
            }
        }
    }
    index
}

/// Render the herd as text.
///
/// One row per session, with the selector the `roster` pane reads back. Kept
/// deliberately narrow — 80 columns of useful text — because this is displayed in
/// an overlay pane over whatever the user was doing.
#[must_use]
pub fn render(rows: &[Row], footer: &str) -> String {
    let mut out = String::new();
    out.push_str("  #  who          agent   state     for   what\n");
    out.push_str("  ─  ───          ─────   ─────     ───   ────\n");
    if rows.is_empty() {
        out.push_str("     (nobody is publishing yet)\n");
    }
    for row in rows {
        let marks = marks(row);
        let state = format!("{}{}", row.status.glyph(), row.status);
        // Writing into a String is infallible, so the Result carries no
        // information worth handling.
        let _ = writeln!(
            out,
            "{:>3}  {:<12} {:<7} {:<9} {:>4}  {}{}",
            row.index,
            truncate(&row.who, 12),
            truncate(&row.agent, 7),
            state,
            fmt_age(row.age_ms),
            truncate(&headline_of(row), 40),
            marks,
        );
        if let (true, Some(help)) = (row.show_help, &row.help) {
            let _ = writeln!(out, "       ↳ help: {}", truncate(&help.note, 60));
        }
    }
    out.push('\n');
    out.push_str(footer);
    out
}

fn headline_of(row: &Row) -> String {
    if row.headline.is_empty() { row.location.clone() } else { row.headline.clone() }
}

/// Trailing markers: sharing, watchers, staleness, and "this one is mine".
fn marks(row: &Row) -> String {
    let mut marks = String::new();
    if row.is_me {
        marks.push_str(" (you)");
    }
    if row.share == Share::Live {
        marks.push_str(" [live]");
    }
    // The hand is up, but the note is printed against a different row of theirs.
    if row.help.is_some() && !row.show_help {
        marks.push_str(" [asked]");
    }
    if !row.watchers.is_empty() {
        let _ = write!(marks, " 👀{}", row.watchers.len());
    }
    if row.stale {
        marks.push_str(" (stale)");
    }
    marks
}

/// Compact duration: `12s`, `4m`, `2h`, `3d`.
#[must_use]
pub fn fmt_age(ms: u64) -> String {
    let secs = ms / 1_000;
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h");
    }
    format!("{}d", hours / 24)
}

/// Truncate to `max` characters, with an ellipsis when something was lost.
#[must_use]
pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let keep = max.saturating_sub(1);
    let mut out: String = text.chars().take(keep).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentCard, agent_key};

    fn card(pane: &str, status: Status, since_ms: u64) -> AgentCard {
        AgentCard {
            key: agent_key("someone", "box", pane),
            pane_id: pane.into(),
            workspace: Some("ansible".into()),
            tab: Some("main".into()),
            agent: "claude".into(),
            status,
            headline: format!("work on {pane}"),
            repo: None,
            branch: None,
            share: Share::Title,
            help: None,
            since_ms,
            live_seq: None,
        }
    }

    fn member(login: &str, published_ms: u64, cards: Vec<AgentCard>) -> MemberDoc {
        let mut doc = MemberDoc::new(login, "box");
        doc.published_ms = published_ms;
        doc.agents = cards
            .into_iter()
            .map(|mut c| {
                c.key = agent_key(login, "box", &c.pane_id);
                c
            })
            .collect();
        doc
    }

    #[test]
    fn attention_sorts_to_the_top() {
        let members = vec![
            member("alice", 10_000, vec![card("w1:p1", Status::Idle, 9_000)]),
            member("bob", 10_000, vec![card("w1:p1", Status::Blocked, 9_000)]),
            member("carol", 10_000, vec![card("w1:p1", Status::Working, 9_000)]),
            member("dave", 10_000, vec![card("w1:p1", Status::Done, 9_000)]),
        ];
        let who: Vec<String> =
            rows(&members, "me", 10_000, 20_000, 300_000).into_iter().map(|r| r.who).collect();
        assert_eq!(who, vec!["bob", "dave", "carol", "alice"]);
    }

    /// The single most important ordering property: a session that has been
    /// blocked longer is more urgent than one that just became blocked.
    #[test]
    fn among_equals_the_oldest_wait_comes_first() {
        let members = vec![
            member("alice", 20_000, vec![card("w1:p1", Status::Blocked, 19_000)]),
            member("bob", 20_000, vec![card("w1:p1", Status::Blocked, 5_000)]),
        ];
        let who: Vec<String> =
            rows(&members, "me", 20_000, 60_000, 300_000).into_iter().map(|r| r.who).collect();
        assert_eq!(who, vec!["bob", "alice"], "bob has been waiting 15s, alice 1s");
    }

    /// A stale row is a claim about the past. It must never outrank a live one,
    /// even when it is the more alarming status.
    #[test]
    fn stale_rows_sort_below_everything_fresh() {
        let members = vec![
            member("alice", 0, vec![card("w1:p1", Status::Blocked, 0)]),
            member("bob", 100_000, vec![card("w1:p1", Status::Idle, 100_000)]),
        ];
        let rows = rows(&members, "me", 100_000, 20_000, 300_000);
        assert_eq!(rows[0].who, "bob");
        assert!(!rows[0].stale);
        assert_eq!(rows[1].who, "alice");
        assert!(rows[1].stale);
    }

    #[test]
    fn a_forgotten_member_disappears_entirely() {
        let members = vec![
            member("alice", 0, vec![card("w1:p1", Status::Blocked, 0)]),
            member("bob", 400_000, vec![card("w1:p1", Status::Idle, 400_000)]),
        ];
        let rows = rows(&members, "me", 400_000, 20_000, 300_000);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].who, "bob");
    }

    #[test]
    fn a_raised_hand_outranks_a_bare_status() {
        let mut alice = member("alice", 10_000, vec![card("w1:p1", Status::Working, 9_000)]);
        alice.agents[0].help =
            Some(HelpWanted { note: "cannot get RLS to deny".into(), since_ms: 9_500 });
        let members =
            vec![alice, member("bob", 10_000, vec![card("w1:p1", Status::Working, 1_000)])];
        let rows = rows(&members, "me", 10_000, 20_000, 300_000);
        assert_eq!(rows[0].who, "alice");
        assert!(rows[0].wants_a_human());
    }

    /// A hand is raised by a person, so it applies to all of their sessions — but
    /// the note is printed once. The first roster to get this wrong printed the
    /// same sentence under three rows.
    #[test]
    fn a_raised_hand_applies_to_every_session_and_prints_once() {
        let mut alice = member(
            "alice",
            10_000,
            vec![card("w1:p1", Status::Working, 9_000), card("w2:p1", Status::Idle, 9_000)],
        );
        alice.help = Some(HelpWanted { note: "stuck".into(), since_ms: 9_500 });
        let rows = rows(&[alice], "me", 10_000, 20_000, 300_000);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.help.is_some()), "the person is stuck, not one pane");
        assert_eq!(rows.iter().filter(|r| r.show_help).count(), 1, "said once");
        assert!(rows[0].show_help, "on the most urgent row");

        let text = render(&rows, "");
        assert_eq!(text.matches("↳ help: stuck").count(), 1);
        // The other row still says the hand is up, without repeating the note.
        assert!(text.contains("[asked]"));
    }

    /// One person's hand must not silence another's.
    #[test]
    fn each_member_gets_their_own_note_printed() {
        let mut alice = member("alice", 10_000, vec![card("w1:p1", Status::Working, 9_000)]);
        alice.help = Some(HelpWanted { note: "alice is stuck".into(), since_ms: 9_500 });
        let mut bob = member("bob", 10_000, vec![card("w1:p1", Status::Working, 9_000)]);
        bob.help = Some(HelpWanted { note: "bob is stuck".into(), since_ms: 9_500 });

        let text = render(&rows(&[alice, bob], "me", 10_000, 20_000, 300_000), "");
        assert!(text.contains("alice is stuck"), "{text}");
        assert!(text.contains("bob is stuck"), "{text}");
    }

    #[test]
    fn my_own_rows_are_marked_so_i_can_see_what_others_see() {
        let members = vec![member("me", 10_000, vec![card("w1:p1", Status::Working, 9_000)])];
        let rows = rows(&members, "me", 10_000, 20_000, 300_000);
        assert!(rows[0].is_me);
        assert!(render(&rows, "").contains("(you)"));
    }

    #[test]
    fn a_pane_shared_off_never_appears() {
        let mut alice = member("alice", 10_000, vec![card("w1:p1", Status::Blocked, 9_000)]);
        alice.agents[0].share = Share::Off;
        assert!(rows(&[alice], "me", 10_000, 20_000, 300_000).is_empty());
    }

    #[test]
    fn watchers_are_collected_from_published_intent() {
        let key = agent_key("alice", "box", "w1:p1");
        let mut alice = member("alice", 10_000, vec![card("w1:p1", Status::Working, 9_000)]);
        alice.agents[0].share = Share::Live;
        let mut me = member("me", 10_000, vec![]);
        me.watching.push(key.clone());
        let mut bob = member("bob", 10_000, vec![]);
        bob.watching.push(key);

        let rows = rows(&[alice, bob, me], "me", 10_000, 20_000, 300_000);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].watchers, vec!["bob", "me"]);
        assert!(rows[0].is_watchable());
        assert!(render(&rows, "").contains("👀2"));
    }

    #[test]
    fn a_location_prefers_repo_and_branch_over_workspace_labels() {
        let mut located = card("w1:p1", Status::Working, 0);
        located.repo = Some("mrshll/ansible".into());
        located.branch = Some("claude/herd".into());
        assert_eq!(location(&located), "mrshll/ansible@claude/herd");

        let mut bare = card("w1:p1", Status::Working, 0);
        bare.workspace = None;
        bare.tab = None;
        assert_eq!(location(&bare), "w1:p1", "a pane id is the last resort, never nothing");
    }

    #[test]
    fn an_empty_headline_falls_back_to_the_location() {
        let mut alice = member("alice", 10_000, vec![card("w1:p1", Status::Working, 9_000)]);
        alice.agents[0].headline = String::new();
        let rows = rows(&[alice], "me", 10_000, 20_000, 300_000);
        assert!(render(&rows, "").contains("ansible/main"));
    }

    #[test]
    fn an_empty_herd_says_so_rather_than_rendering_a_bare_header() {
        let text = render(&[], "hub: dir at /tmp/herd");
        assert!(text.contains("nobody is publishing yet"));
        assert!(text.contains("hub: dir"));
    }

    #[test]
    fn ages_read_as_durations() {
        assert_eq!(fmt_age(0), "0s");
        assert_eq!(fmt_age(12_500), "12s");
        assert_eq!(fmt_age(4 * 60_000), "4m");
        assert_eq!(fmt_age(2 * 3_600_000), "2h");
        assert_eq!(fmt_age(3 * 24 * 3_600_000), "3d");
    }

    #[test]
    fn truncation_counts_characters_and_marks_the_cut() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("exactlyten", 10), "exactlyten");
        assert_eq!(truncate("elevenchars", 10), "elevencha…");
        assert_eq!(truncate("ααααα", 3), "αα…");
    }

    /// Every row must be selectable, and the numbers must match what was printed.
    #[test]
    fn indices_are_dense_and_one_based() {
        let members = vec![
            member("alice", 10_000, vec![card("w1:p1", Status::Blocked, 9_000)]),
            member("bob", 10_000, vec![card("w1:p1", Status::Idle, 9_000)]),
        ];
        let rows = rows(&members, "me", 10_000, 20_000, 300_000);
        assert_eq!(rows.iter().map(|r| r.index).collect::<Vec<_>>(), vec![1, 2]);
        let text = render(&rows, "");
        assert!(text.contains("  1  alice"));
        assert!(text.contains("  2  bob"));
    }
}
