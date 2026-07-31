//! Local, machine-owned state: what I have chosen to publish, who I am watching,
//! and mail that has arrived but not been dealt with.
//!
//! Herdr has no plugin storage API, so the plugin owns its files. Two rules keep
//! that honest without a lock:
//!
//! 1. **Every file has exactly one writer.** `overrides.json` and
//!    `watching.json` are written by short-lived action processes and only read by
//!    the daemon. The inbox is written only by the daemon. Acknowledgements are
//!    written only by the `inbox` command. No file is ever read-modify-written by
//!    two processes.
//! 2. **Writes are atomic.** Temp file in the same directory, then rename, so a
//!    reader either sees the whole previous version or the whole next one.
//!
//! The inbox is a directory of one file per message rather than one array, which
//! is what removes the last read-modify-write: delivering a message and
//! acknowledging one touch different paths and can race freely.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::model::{HelpWanted, Message, Share};

/// Everything a human has explicitly asked to publish.
///
/// Written by `ansible-herd status`, read by the daemon on every tick. Absent
/// fields mean "no override": the daemon then derives a headline from Herdr's own
/// signals rather than showing nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Overrides {
    /// What I say I am working on. Wins over the terminal title.
    pub headline: Option<String>,
    /// A raised hand. Applies to the whole member, not one pane, because "I am
    /// stuck" is a fact about a person.
    pub help: Option<HelpWanted>,
    /// Per-pane share mode, keyed by Herdr pane id.
    pub share: BTreeMap<String, Share>,
    /// Bumped on every write so a reader can tell a change from a rewrite.
    pub seq: u64,
}

/// How long a watch lease survives without a refresh.
///
/// Watching is a *lease*, not a flag. A viewer pane is stopped by closing it,
/// which kills the process with no chance to clean up, so an entry that is never
/// refreshed has to expire on its own. Otherwise the owner would see "somebody is
/// watching" forever after everybody left — and since a watcher is what causes a
/// `live` pane to be observed and published, a leaked flag would keep a terminal
/// stream running for an audience of nobody.
pub const WATCH_LEASE_MS: u64 = 15_000;

/// One watch lease, as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Lease {
    key: String,
    refreshed_ms: u64,
}

/// Handle on the plugin's state directory.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Read the human's overrides, treating a missing or corrupt file as "none".
    ///
    /// Corrupt is deliberately not an error. This runs on the daemon's hot path,
    /// and a half-written file — which atomic renames should prevent but a full
    /// disk can still produce — must degrade to "no override" rather than stop
    /// presence for everyone.
    #[must_use]
    pub fn overrides(&self) -> Overrides {
        read_json(&self.root.join("overrides.json")).unwrap_or_default()
    }

    /// Replace the overrides file.
    ///
    /// # Errors
    /// When the file cannot be written.
    pub fn put_overrides(&self, overrides: &Overrides) -> Result<()> {
        write_json(&self.root.join("overrides.json"), overrides)
    }

    /// Take or refresh a watch lease. Called by the viewer on every poll.
    ///
    /// # Errors
    /// When the lease file cannot be written.
    pub fn watch_touch(&self, key: &str, now_ms: u64) -> Result<()> {
        write_json(&self.watch_path(key), &Lease { key: key.to_string(), refreshed_ms: now_ms })
    }

    /// Give up a lease immediately, for the case where the viewer exits cleanly.
    ///
    /// # Errors
    /// Never in practice; a missing lease is success.
    pub fn watch_release(&self, key: &str) -> Result<()> {
        let path = self.watch_path(key);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err).context(format!("removing {}", path.display())),
        }
    }

    /// Keys with a live lease, expiring the rest as it goes.
    #[must_use]
    pub fn watching(&self, now_ms: u64) -> Vec<String> {
        let dir = self.root.join("watching");
        let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
        let mut keys = Vec::new();
        for entry in entries.filter_map(std::result::Result::ok) {
            let path = entry.path();
            match read_json::<Lease>(&path) {
                Some(lease) if now_ms.saturating_sub(lease.refreshed_ms) <= WATCH_LEASE_MS => {
                    keys.push(lease.key);
                }
                // Expired or unreadable: sweep it. Doing this on read means the
                // daemon's own poll is the garbage collector, with nothing extra to
                // schedule.
                _ => {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        keys.sort();
        keys.dedup();
        keys
    }

    fn watch_path(&self, key: &str) -> PathBuf {
        self.root.join("watching").join(format!("{}.json", safe_component(key)))
    }

    /// Store an inbound message, unless it is already there.
    ///
    /// Returns whether this was the first sighting. Message ids are minted by
    /// their sender and never reused, so this is what makes delivery idempotent
    /// across a hub that hands the same file back on every poll.
    ///
    /// # Errors
    /// When the inbox directory or the message file cannot be written.
    pub fn deliver(&self, message: &Message) -> Result<bool> {
        let name = safe_component(&message.id);
        let dir = self.root.join("inbox");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{name}.json"));
        if path.exists() || self.is_acked(&message.id) {
            return Ok(false);
        }
        write_json(&path, message)?;
        Ok(true)
    }

    /// Messages that have arrived and not been acknowledged, oldest first.
    #[must_use]
    pub fn pending(&self) -> Vec<Message> {
        let dir = self.root.join("inbox");
        let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
        let mut out: Vec<Message> = entries
            .filter_map(std::result::Result::ok)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
            .filter_map(|e| read_json::<Message>(&e.path()))
            .filter(|m| !self.is_acked(&m.id))
            .collect();
        out.sort_by_key(|m| (m.created_ms, m.id.clone()));
        out
    }

    /// Mark a message dealt with. Idempotent.
    ///
    /// # Errors
    /// When the marker cannot be written.
    pub fn ack(&self, id: &str) -> Result<()> {
        let dir = self.root.join("acked");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(safe_component(id));
        if !path.exists() {
            std::fs::write(&path, b"")?;
        }
        // Dropping the delivered copy keeps the inbox proportional to unread mail
        // rather than to all mail ever received.
        let _ = std::fs::remove_file(
            self.root.join("inbox").join(format!("{}.json", safe_component(id))),
        );
        Ok(())
    }

    #[must_use]
    pub fn is_acked(&self, id: &str) -> bool {
        self.root.join("acked").join(safe_component(id)).exists()
    }

    /// Next per-sender message counter.
    ///
    /// Two `comment` commands racing could read the same value and mint the same
    /// id, which would make one comment overwrite the other. Accepted for a
    /// prototype: the window is a few microseconds and both processes are started
    /// by the same pair of hands.
    ///
    /// # Errors
    /// When the counter file cannot be read or written.
    pub fn next_outbox_seq(&self) -> Result<u64> {
        let path = self.root.join("outbox-seq");
        let current: u64 =
            std::fs::read_to_string(&path).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        let next = current + 1;
        write_atomic(&path, next.to_string().as_bytes())?;
        Ok(next)
    }

    /// Record that a member's attention-worthy state has already been announced,
    /// so a notification fires on a rising edge instead of on every poll.
    ///
    /// # Errors
    /// When the marker file cannot be written.
    pub fn put_announced(&self, announced: &BTreeMap<String, String>) -> Result<()> {
        write_json(&self.root.join("announced.json"), announced)
    }

    #[must_use]
    pub fn announced(&self) -> BTreeMap<String, String> {
        read_json(&self.root.join("announced.json")).unwrap_or_default()
    }

    #[must_use]
    pub fn pid_file(&self) -> PathBuf {
        self.root.join("daemon.pid")
    }

    #[must_use]
    pub fn log_file(&self) -> PathBuf {
        self.root.join("daemon.log")
    }
}

/// Reduce an untrusted string to something safe to use as one path component.
///
/// The hub carries other people's data: logins, message ids, and pane ids all
/// arrive from outside and all end up in file names. Anything outside a
/// conservative set becomes `_`, and the result is length-capped, so a crafted id
/// cannot escape the directory it belongs in or exhaust a filesystem limit.
#[must_use]
pub fn safe_component(raw: &str) -> String {
    let mut out: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' })
        .take(96)
        .collect();
    // A leading dot hides the file; `.` and `..` are the traversal cases.
    if out.starts_with('.') {
        out.insert(0, '_');
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// Deserialize a JSON file, treating any failure as absence.
#[must_use]
pub fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Serialize to JSON and write atomically.
///
/// # Errors
/// When the file cannot be written or renamed into place.
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let text = serde_json::to_vec_pretty(value)?;
    write_atomic(path, &text)
}

/// Write via a temp file in the same directory, then rename.
///
/// Same directory because rename is only atomic within a filesystem, and the
/// point of the whole exercise is that a reader never sees a partial file.
///
/// # Errors
/// When the parent directory, the temp file, or the rename fails.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("out"),
        std::process::id()
    ));
    {
        let mut file =
            std::fs::File::create(&temp).with_context(|| format!("creating {}", temp.display()))?;
        file.write_all(bytes)?;
        file.flush()?;
    }
    std::fs::rename(&temp, path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MessageKind, SCHEMA_VERSION};

    fn store(tag: &str) -> Store {
        let dir =
            std::env::temp_dir().join(format!("ansible-herd-state-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        Store::new(dir)
    }

    fn message(id: &str, created_ms: u64) -> Message {
        Message {
            v: SCHEMA_VERSION,
            id: id.into(),
            from: "alice".into(),
            to: "mrshll".into(),
            to_key: "mrshll@box/w1:p1".into(),
            kind: MessageKind::Comment,
            body: "try --no-verify".into(),
            anchor_line: None,
            created_ms,
        }
    }

    #[test]
    fn overrides_round_trip_and_default_to_empty() {
        let store = store("overrides");
        assert_eq!(store.overrides(), Overrides::default());

        let mut overrides =
            Overrides { headline: Some("refactor auth".into()), ..Overrides::default() };
        overrides.share.insert("w1:p1".into(), Share::Live);
        overrides.seq = 3;
        store.put_overrides(&overrides).expect("write");
        assert_eq!(store.overrides(), overrides);
    }

    /// The daemon reads this file on every tick. A truncated or hand-mangled file
    /// must not stop presence for the whole team.
    #[test]
    fn a_corrupt_overrides_file_reads_as_no_override() {
        let store = store("corrupt");
        std::fs::write(store.root().join("overrides.json"), b"{not json").expect("write");
        assert_eq!(store.overrides(), Overrides::default());
    }

    #[test]
    fn delivery_is_idempotent() {
        let store = store("deliver");
        assert!(store.deliver(&message("alice-1", 10)).expect("first"));
        assert!(!store.deliver(&message("alice-1", 10)).expect("second"), "already delivered");
        assert_eq!(store.pending().len(), 1);
    }

    #[test]
    fn an_acked_message_does_not_come_back_on_the_next_poll() {
        let store = store("ack");
        store.deliver(&message("alice-1", 10)).expect("deliver");
        store.ack("alice-1").expect("ack");
        assert!(store.pending().is_empty());
        // The hub still holds the message and will offer it again; the ack is what
        // stops it reappearing in the inbox forever.
        assert!(!store.deliver(&message("alice-1", 10)).expect("redeliver"));
        assert!(store.pending().is_empty());
    }

    #[test]
    fn pending_is_ordered_oldest_first() {
        let store = store("order");
        store.deliver(&message("alice-2", 200)).expect("deliver");
        store.deliver(&message("alice-1", 100)).expect("deliver");
        store.deliver(&message("bob-1", 150)).expect("deliver");
        let ids: Vec<String> = store.pending().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, vec!["alice-1", "bob-1", "alice-2"]);
    }

    #[test]
    fn a_watch_lease_is_visible_while_it_is_refreshed() {
        let store = store("watch");
        assert!(store.watching(1_000).is_empty());
        store.watch_touch("alice@box/w1:p1", 1_000).expect("touch");
        assert_eq!(store.watching(1_000), vec!["alice@box/w1:p1"]);
        assert_eq!(store.watching(1_000 + WATCH_LEASE_MS), vec!["alice@box/w1:p1"]);
    }

    /// A viewer pane is closed by killing it, so this is the only thing that stops
    /// a leaked "somebody is watching" — and with it, a live stream with no
    /// audience.
    #[test]
    fn an_unrefreshed_lease_expires_and_is_swept() {
        let store = store("watch-expire");
        store.watch_touch("alice@box/w1:p1", 1_000).expect("touch");
        assert!(store.watching(1_000 + WATCH_LEASE_MS + 1).is_empty());
        // Swept on read: the daemon's poll is the collector.
        let left = std::fs::read_dir(store.root().join("watching"))
            .expect("dir")
            .filter_map(std::result::Result::ok)
            .count();
        assert_eq!(left, 0);
    }

    #[test]
    fn releasing_a_lease_is_idempotent() {
        let store = store("watch-release");
        store.watch_touch("alice@box/w1:p1", 1_000).expect("touch");
        store.watch_release("alice@box/w1:p1").expect("release");
        store.watch_release("alice@box/w1:p1").expect("release again");
        assert!(store.watching(1_000).is_empty());
    }

    #[test]
    fn two_leases_on_different_keys_coexist() {
        let store = store("watch-two");
        store.watch_touch("alice@box/w1:p1", 1_000).expect("touch");
        store.watch_touch("bob@box/w2:p1", 1_000).expect("touch");
        assert_eq!(store.watching(1_000), vec!["alice@box/w1:p1", "bob@box/w2:p1"]);
    }

    #[test]
    fn the_outbox_counter_increases_across_processes() {
        let store = store("seq");
        assert_eq!(store.next_outbox_seq().expect("first"), 1);
        assert_eq!(store.next_outbox_seq().expect("second"), 2);
        // A fresh handle on the same directory continues rather than restarting,
        // which is what keeps message ids unique across separate `comment` runs.
        let reopened = Store::new(store.root());
        assert_eq!(reopened.next_outbox_seq().expect("third"), 3);
    }

    /// Message ids and logins arrive from other people's machines and end up in
    /// file names.
    #[test]
    fn untrusted_ids_cannot_escape_their_directory() {
        // Dots survive — they are legal in a file name — but no separator does,
        // and the leading dot is defused.
        assert_eq!(safe_component("../../etc/passwd"), "_.._.._etc_passwd");
        assert_eq!(safe_component(".."), "_..");
        assert_eq!(safe_component(".ssh"), "_.ssh");
        assert_eq!(safe_component(""), "_");
        assert_eq!(safe_component("alice-17"), "alice-17");
        assert!(!safe_component(&"x".repeat(500)).contains('/'));
        assert_eq!(safe_component(&"x".repeat(500)).len(), 96);
    }

    #[test]
    fn a_traversing_message_id_lands_inside_the_inbox() {
        let store = store("traversal");
        let mut hostile = message("../../pwned", 1);
        hostile.id = "../../pwned".into();
        store.deliver(&hostile).expect("deliver");
        // The file must be inside the inbox, whatever the sender called it.
        let entries: Vec<_> = std::fs::read_dir(store.root().join("inbox"))
            .expect("inbox exists")
            .filter_map(std::result::Result::ok)
            .collect();
        assert_eq!(entries.len(), 1);
        assert!(!store.root().join("pwned").exists());
        // And it is still readable as the message it is, id intact.
        assert_eq!(store.pending()[0].id, "../../pwned");
    }

    #[test]
    fn atomic_writes_leave_no_temp_files_behind() {
        let store = store("atomic");
        let path = store.root().join("thing.json");
        write_atomic(&path, b"one").expect("write");
        write_atomic(&path, b"two").expect("overwrite");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "two");
        let leftovers: Vec<_> = std::fs::read_dir(store.root())
            .expect("read dir")
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files remained: {leftovers:?}");
    }

    #[test]
    fn writing_creates_missing_parent_directories() {
        let store = store("mkdir");
        let path = store.root().join("a/b/c.json");
        write_atomic(&path, b"x").expect("write");
        assert!(path.exists());
    }
}
