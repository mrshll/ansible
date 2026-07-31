//! A hub that is a set of Git refs on a repository the team already has.
//!
//! This is the backend that answers "connect with a GitHub team" without standing
//! anything up. There is no server, no database, and no API token: presence
//! documents are Git objects, each member publishes to their own ref
//! `refs/herd/<login>`, and **push access to the repository is the
//! authorization**. If GitHub lets you push, you are in the herd; if your access
//! is revoked, your presence stops being publishable the same minute.
//!
//! ```text
//! refs/herd/<login>  ->  presence.json      # that member's whole presence doc
//!                        mail/<id>.json     # messages that member has sent
//! ```
//!
//! Two properties make this work without any coordination:
//!
//! * **Disjoint refs.** A member only ever pushes their own ref, so two people
//!   publishing at the same instant cannot conflict. There is no merge, no
//!   rebase, and no lost update — the failure mode Git usually forces you to think
//!   about simply does not arise.
//! * **No history.** Each publish is a parentless commit that replaces the ref, so
//!   the repository does not grow a commit per heartbeat. The refs are outside
//!   `refs/heads`, so none of this appears in branch listings, `git log`, or a
//!   pull request.
//!
//! What it costs: latency is a fetch interval rather than a socket, so presence is
//! seconds-fresh, not milliseconds-fresh. And it carries no live frames — a commit
//! per terminal chunk would be absurd. Teleport needs the `dir` backend or the
//! relay. That is a real limit, stated rather than papered over.
//!
//! No worktree is touched. Everything below is plumbing against a temporary index,
//! so running this in the repo you are working in cannot disturb your checkout,
//! your index, or your branch.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use ansible_capture::Chunk;
use anyhow::{Context, Result, bail};

use crate::hub::{Hub, version_ok};
use crate::model::{MemberDoc, Message};
use crate::state::{safe_component, write_json};

/// Floor on how often we talk to the remote.
///
/// The daemon's poll interval is tuned for a local hub. Pushing and fetching on
/// that cadence would spend the whole time in Git, so the throttle lives here,
/// where the cost is, rather than asking every user to retune `timing` for their
/// backend choice.
const MIN_REMOTE_INTERVAL_MS: u64 = 3_000;

pub struct GitHub {
    repo: PathBuf,
    remote: String,
    /// Files this member publishes, mirrored on local disk. The pushed tree is a
    /// projection of this directory, which keeps one authoritative copy of our own
    /// outbox instead of reading our own state back out of Git.
    work: PathBuf,
    last_push_ms: u64,
    /// The last successful read, kept so a failed fetch can serve the previous
    /// answer instead of emptying the roster.
    cached: Vec<MemberDoc>,
}

impl GitHub {
    #[must_use]
    pub fn new(repo: impl Into<PathBuf>, remote: impl Into<String>) -> Self {
        let repo = repo.into();
        Self {
            repo,
            remote: remote.into(),
            work: PathBuf::new(),
            last_push_ms: 0,
            cached: Vec::new(),
        }
    }

    /// Point the backend at the directory it may use for its own files.
    ///
    /// Separate from [`GitHub::new`] because the state directory is resolved by
    /// the process, not by config, and the hub factory does not have it.
    #[must_use]
    pub fn with_work_dir(mut self, work: impl Into<PathBuf>) -> Self {
        self.work = work.into();
        self
    }

    fn work_dir(&self) -> PathBuf {
        if self.work.as_os_str().is_empty() {
            self.repo.join(".git/herd-work")
        } else {
            self.work.clone()
        }
    }

    /// Run a Git command in the repo, returning stdout.
    ///
    /// The committer identity is set per invocation rather than read from the
    /// user's config, because `commit-tree` fails outright without one and a
    /// presence heartbeat is a bad place to discover that a machine has no
    /// `user.email`.
    fn git(&self, args: &[&str]) -> Result<String> {
        self.run_git(args, None, None)
    }

    fn git_with_stdin(&self, args: &[&str], input: &[u8]) -> Result<String> {
        self.run_git(args, Some(input), None)
    }

    /// Run against a private index file rather than the repository's own.
    ///
    /// `GIT_INDEX_FILE` is the only way to stage anything without touching the
    /// index the user is working in, which is a hard requirement here: this code
    /// runs inside a repo somebody is mid-commit in.
    fn git_indexed(&self, index: &std::path::Path, args: &[&str]) -> Result<String> {
        self.run_git(args, None, Some(index))
    }

    fn run_git(
        &self,
        args: &[&str],
        input: Option<&[u8]>,
        index: Option<&std::path::Path>,
    ) -> Result<String> {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&self.repo)
            .args(["-c", "user.name=ansible-herd", "-c", "user.email=herd@invalid"])
            .args(args)
            .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(path) = index {
            command.env("GIT_INDEX_FILE", path);
        }

        let mut child = command
            .spawn()
            .with_context(|| format!("spawning git {}", args.first().unwrap_or(&"")))?;
        if let Some(bytes) = input {
            use std::io::Write;
            child.stdin.take().context("git stdin")?.write_all(bytes)?;
        }
        let output = child.wait_with_output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            bail!("git {} failed: {stderr}", args.join(" "));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim_end().to_string())
    }

    /// Build a tree from every file under the work directory and push it as this
    /// member's ref.
    fn push_work_dir(&self, login: &str) -> Result<()> {
        let work = self.work_dir();
        let index = work.join(".herd-index");
        let _ = std::fs::remove_file(&index);

        let mut files = Vec::new();
        collect_files(&work, &work, &mut files)?;
        if files.is_empty() {
            bail!("nothing to publish from {}", work.display());
        }

        // `git hash-object -w` writes the blob into the object database; the
        // temporary index then names it at the right path, and `write-tree` turns
        // that into a tree. No worktree, no checkout, nothing staged in the user's
        // own index.
        self.git_indexed(&index, &["read-tree", "--empty"])?;
        for relative in &files {
            let bytes = std::fs::read(work.join(relative))?;
            let blob =
                self.git_with_stdin(&["hash-object", "-w", "--path", relative, "--stdin"], &bytes)?;
            let cacheinfo = format!("100644,{blob},{relative}");
            self.git_indexed(&index, &["update-index", "--add", "--cacheinfo", &cacheinfo])?;
        }
        let tree = self.git_indexed(&index, &["write-tree"])?;
        let _ = std::fs::remove_file(&index);

        // Parentless: the ref is a pointer to the current state, not a log of
        // every heartbeat this machine has ever sent.
        let commit = self.git(&["commit-tree", &tree, "-m", "herd presence"])?;
        let ref_name = herd_ref(login);
        self.git(&["update-ref", &ref_name, &commit])?;
        let refspec = format!("+{ref_name}:{ref_name}");
        self.git(&["push", "--quiet", &self.remote, &refspec])?;
        Ok(())
    }

    /// Read a member's presence document out of their ref.
    fn read_presence(&self, ref_name: &str) -> Option<MemberDoc> {
        let spec = format!("{ref_name}:presence.json");
        let text = self.git(&["cat-file", "blob", &spec]).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Read every message in a member's ref.
    fn read_mail(&self, ref_name: &str) -> Vec<Message> {
        let spec = format!("{ref_name}:mail");
        let Ok(listing) = self.git(&["ls-tree", "-z", "--name-only", &spec]) else {
            return Vec::new();
        };
        listing
            .split('\0')
            .filter(|name| std::path::Path::new(name).extension().is_some_and(|e| e == "json"))
            .filter_map(|name| {
                let blob = format!("{ref_name}:mail/{name}");
                let text = self.git(&["cat-file", "blob", &blob]).ok()?;
                serde_json::from_str::<Message>(&text).ok()
            })
            .collect()
    }

    fn herd_refs(&self) -> Vec<String> {
        self.git(&["for-each-ref", "--format=%(refname)", "refs/herd/"])
            .map(|out| out.lines().map(str::to_string).filter(|l| !l.is_empty()).collect())
            .unwrap_or_default()
    }
}

impl Hub for GitHub {
    fn publish(&mut self, doc: &MemberDoc) -> Result<()> {
        write_json(&self.work_dir().join("presence.json"), doc)?;
        if doc.published_ms.saturating_sub(self.last_push_ms) < MIN_REMOTE_INTERVAL_MS
            && self.last_push_ms != 0
        {
            // The document is on disk and will go out with the next push. Skipping
            // the network is the whole point of the throttle.
            return Ok(());
        }
        self.push_work_dir(&doc.login)?;
        self.last_push_ms = doc.published_ms;
        Ok(())
    }

    fn members(&mut self) -> Result<Vec<MemberDoc>> {
        // `--prune` so a member whose ref was deleted leaves the roster rather
        // than lingering forever as a stale row.
        if let Err(err) =
            self.git(&["fetch", "--quiet", "--prune", &self.remote, "+refs/herd/*:refs/herd/*"])
        {
            // A network blip must not empty the roster. Serving the last good
            // answer lets staleness do its job — every row carries a publish time,
            // so an old answer reads as old rather than as "everyone left".
            if self.cached.is_empty() {
                return Err(err).context("fetching herd refs");
            }
            eprintln!("herd: fetch failed, showing the last known herd: {err}");
            return Ok(self.cached.clone());
        }
        let mut out: Vec<MemberDoc> = self
            .herd_refs()
            .iter()
            .filter_map(|r| self.read_presence(r))
            .filter(|doc| version_ok(doc.v))
            .collect();
        out.sort_by(|a, b| a.login.cmp(&b.login).then(a.host.cmp(&b.host)));
        self.cached.clone_from(&out);
        Ok(out)
    }

    fn send(&mut self, message: &Message) -> Result<()> {
        let path =
            self.work_dir().join("mail").join(format!("{}.json", safe_component(&message.id)));
        write_json(&path, message)?;
        // Send is user-initiated and rare, so it pushes immediately rather than
        // waiting for the next heartbeat: a comment that arrives five seconds late
        // is fine, one that waits for a timer is confusing.
        self.push_work_dir(&message.from)
    }

    fn messages_for(&mut self, login: &str) -> Result<Vec<Message>> {
        let mut out: Vec<Message> = self
            .herd_refs()
            .iter()
            .flat_map(|r| self.read_mail(r))
            .filter(|m| version_ok(m.v) && m.to == login)
            .collect();
        out.sort_by_key(|m| (m.created_ms, m.id.clone()));
        Ok(out)
    }

    fn supports_live(&self) -> bool {
        false
    }

    fn put_chunk(&mut self, _key: &str, _chunk: &Chunk) -> Result<()> {
        bail!(
            "the git hub cannot carry live frames — a commit per terminal chunk is not a stream. Use hub.kind = \"dir\" for teleport, or wait for the relay backend"
        )
    }

    fn chunks(&mut self, _key: &str, _from_seq: u64) -> Result<Vec<Chunk>> {
        Ok(Vec::new())
    }

    fn prune_chunks(&mut self, _key: &str, _before_seq: u64) -> Result<()> {
        Ok(())
    }

    fn describe(&self) -> String {
        format!("git hub on {} (refs/herd/*, presence only — no live frames)", self.remote)
    }
}

/// The ref one member publishes to.
#[must_use]
fn herd_ref(login: &str) -> String {
    format!("refs/herd/{}", safe_component(login))
}

/// Every file under `root`, as paths relative to `root`, sorted.
///
/// Sorted so the tree — and therefore the commit — is a function of the content
/// and not of directory iteration order. Without that, an unchanged presence
/// document could produce a different commit on every push.
fn collect_files(
    root: &std::path::Path,
    dir: &std::path::Path,
    out: &mut Vec<String>,
) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Ok(()) };
    let mut names: Vec<PathBuf> =
        entries.filter_map(std::result::Result::ok).map(|e| e.path()).collect();
    names.sort();
    for path in names {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        // Our temporary index and any half-written file are not content.
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            collect_files(root, &path, out)?;
        } else if let Ok(relative) = path.strip_prefix(root) {
            if let Some(text) = relative.to_str() {
                out.push(text.to_string());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MessageKind, SCHEMA_VERSION, agent_key};
    use crate::state::read_json;

    /// A bare repo standing in for GitHub, plus one clone per member. Everything
    /// below is the real Git plumbing against a real remote; only the network is
    /// missing.
    struct Fixture {
        root: PathBuf,
        remote: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Option<Self> {
            if Command::new("git").arg("--version").stdout(Stdio::null()).status().is_err() {
                return None;
            }
            let root =
                std::env::temp_dir().join(format!("ansible-herd-git-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let remote = root.join("remote.git");
            std::fs::create_dir_all(&remote).expect("mkdir");
            run(&["init", "--bare", "--quiet"], &remote);
            Some(Self { root, remote })
        }

        fn member(&self, login: &str) -> GitHub {
            let repo = self.root.join(login);
            std::fs::create_dir_all(&repo).expect("mkdir");
            run(&["init", "--quiet"], &repo);
            GitHub::new(&repo, self.remote.display().to_string())
                .with_work_dir(self.root.join(format!("{login}-work")))
        }
    }

    fn run(args: &[&str], cwd: &std::path::Path) {
        let status = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?} failed in {}", cwd.display());
    }

    fn doc(login: &str, published_ms: u64) -> MemberDoc {
        let mut doc = MemberDoc::new(login, "box");
        doc.published_ms = published_ms;
        doc.seq = 1;
        doc.watching.push(agent_key("someone", "else", "w1:p1"));
        doc
    }

    #[test]
    fn presence_crosses_a_real_remote_between_two_members() {
        let Some(fixture) = Fixture::new("two-members") else { return };
        let mut me = fixture.member("mrshll");
        let mut them = fixture.member("alice");

        me.publish(&doc("mrshll", 1_000)).expect("publish");
        them.publish(&doc("alice", 1_000)).expect("publish");

        let seen: Vec<String> = me.members().expect("fetch").into_iter().map(|d| d.login).collect();
        assert_eq!(seen, vec!["alice", "mrshll"], "each member sees the whole herd");

        // Watching intent is part of the document, and it is the whole teleport
        // handshake, so it has to survive the trip.
        let alice =
            me.members().expect("fetch").into_iter().find(|d| d.login == "alice").expect("alice");
        assert_eq!(alice.watching, vec![agent_key("someone", "else", "w1:p1")]);
    }

    #[test]
    fn republishing_replaces_the_ref_without_growing_history() {
        let Some(fixture) = Fixture::new("no-history") else { return };
        let mut me = fixture.member("mrshll");
        me.publish(&doc("mrshll", 1_000)).expect("publish");
        let mut second = doc("mrshll", 1_000 + MIN_REMOTE_INTERVAL_MS);
        second.seq = 2;
        me.publish(&second).expect("republish");

        assert_eq!(me.members().expect("fetch")[0].seq, 2);
        // Parentless commits: the ref is state, not a log. Without this the
        // repository would gain a commit every heartbeat, forever.
        let count = me.git(&["rev-list", "--count", &herd_ref("mrshll")]).expect("rev-list");
        assert_eq!(count, "1");
    }

    #[test]
    fn the_remote_is_only_touched_once_per_interval() {
        let Some(fixture) = Fixture::new("throttle") else { return };
        let mut me = fixture.member("mrshll");
        me.publish(&doc("mrshll", 10_000)).expect("first publish");

        let mut quick = doc("mrshll", 10_500);
        quick.seq = 2;
        me.publish(&quick).expect("throttled publish");
        // Written locally, not pushed: a teammate still sees seq 1.
        assert_eq!(me.members().expect("fetch")[0].seq, 1);

        let mut later = doc("mrshll", 10_000 + MIN_REMOTE_INTERVAL_MS);
        later.seq = 3;
        me.publish(&later).expect("publish after the interval");
        assert_eq!(me.members().expect("fetch")[0].seq, 3);
    }

    #[test]
    fn mail_crosses_the_remote_and_only_reaches_its_recipient() {
        let Some(fixture) = Fixture::new("mail") else { return };
        let mut alice = fixture.member("alice");
        let mut me = fixture.member("mrshll");
        me.publish(&doc("mrshll", 1_000)).expect("publish");

        alice
            .send(&Message {
                v: SCHEMA_VERSION,
                id: "alice-1".into(),
                from: "alice".into(),
                to: "mrshll".into(),
                to_key: agent_key("mrshll", "box", "w1:p1"),
                kind: MessageKind::Comment,
                body: "the RLS comment is stale".into(),
                anchor_line: Some(42),
                created_ms: 1_100,
            })
            .expect("send");

        me.members().expect("fetch");
        let mine = me.messages_for("mrshll").expect("read mail");
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].body, "the RLS comment is stale");
        assert!(me.messages_for("bob").expect("read mail").is_empty());
    }

    #[test]
    fn a_deleted_ref_leaves_the_roster() {
        let Some(fixture) = Fixture::new("prune") else { return };
        let mut me = fixture.member("mrshll");
        let mut alice = fixture.member("alice");
        me.publish(&doc("mrshll", 1_000)).expect("publish");
        alice.publish(&doc("alice", 1_000)).expect("publish");
        assert_eq!(me.members().expect("fetch").len(), 2);

        alice
            .git(&["push", "--quiet", &alice.remote, &format!(":{}", herd_ref("alice"))])
            .expect("delete ref");
        let seen: Vec<String> = me.members().expect("fetch").into_iter().map(|d| d.login).collect();
        assert_eq!(seen, vec!["mrshll"], "--prune must drop the local mirror too");
    }

    #[test]
    fn nothing_is_written_to_the_users_branches_index_or_worktree() {
        let Some(fixture) = Fixture::new("clean") else { return };
        let mut me = fixture.member("mrshll");
        me.publish(&doc("mrshll", 1_000)).expect("publish");

        let branches =
            me.git(&["for-each-ref", "--format=%(refname)", "refs/heads/"]).expect("refs");
        assert!(branches.is_empty(), "presence must not create branches: {branches}");
        let staged = me.git(&["diff", "--cached", "--name-only"]).expect("diff");
        assert!(staged.is_empty(), "the user's index must be untouched: {staged}");
        let status = me.git(&["status", "--porcelain"]).expect("status");
        assert!(status.is_empty(), "the worktree must be untouched: {status}");
    }

    #[test]
    fn live_frames_are_refused_with_a_reason_and_a_way_forward() {
        let Some(fixture) = Fixture::new("no-live") else { return };
        let mut me = fixture.member("mrshll");
        assert!(!me.supports_live());
        let mut chunker = ansible_capture::Chunker::new(
            "s1",
            ansible_capture::ChunkerConfig::default(),
            ansible_capture::Ruleset::default(),
        );
        let mut chunks = chunker.push(b"x", 0);
        chunks.extend(chunker.finish(1));
        let err = me.put_chunk("k", &chunks[0]).expect_err("refused");
        let text = format!("{err}");
        assert!(text.contains("dir"), "the error should name the backend that works: {text}");
        // And reading is empty rather than an error, so a viewer degrades to
        // "nothing to show" instead of crashing.
        assert!(me.chunks("k", 0).expect("read").is_empty());
    }

    #[test]
    fn a_login_cannot_smuggle_a_ref_path() {
        assert_eq!(herd_ref("mrshll"), "refs/herd/mrshll");
        assert_eq!(herd_ref("../heads/main"), "refs/herd/_.._heads_main");
    }

    #[test]
    fn collected_files_are_relative_and_sorted() {
        let root =
            std::env::temp_dir().join(format!("ansible-herd-collect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("mail")).expect("mkdir");
        std::fs::write(root.join("presence.json"), b"{}").expect("write");
        std::fs::write(root.join("mail/b.json"), b"{}").expect("write");
        std::fs::write(root.join("mail/a.json"), b"{}").expect("write");
        std::fs::write(root.join(".herd-index"), b"junk").expect("write");

        let mut files = Vec::new();
        collect_files(&root, &root, &mut files).expect("collect");
        assert_eq!(files, vec!["mail/a.json", "mail/b.json", "presence.json"]);
    }

    #[test]
    fn reading_an_unreadable_ref_is_none_rather_than_a_panic() {
        let Some(fixture) = Fixture::new("missing") else { return };
        let me = fixture.member("mrshll");
        assert!(me.read_presence("refs/herd/nobody").is_none());
        assert!(me.read_mail("refs/herd/nobody").is_empty());
        assert!(read_json::<MemberDoc>(&fixture.root.join("nope.json")).is_none());
    }
}
