//! A hub that is a shared directory.
//!
//! No server, no account, no protocol: a directory every member can read and
//! write. A shared box, an NFS or SMB mount, a Syncthing folder, a Tailscale
//! drive. It is the fastest way to see this whole thing work — two Herdr sessions
//! on one machine pointed at `/tmp/herd` is a complete two-person team — and it is
//! the only backend that carries live teleport frames today.
//!
//! Layout, with one writer per path:
//!
//! ```text
//! <root>/members/<login>.json          # presence, replaced whole
//! <root>/outbox/<login>/<id>.json      # mail, append-only, sender-owned
//! <root>/live/<key>/<seq>.jsonl        # redacted terminal chunks, owner-owned
//! ```
//!
//! `<seq>.jsonl` is exactly the stored form `ansible-capture` already defines and
//! golden-tests, so the teleport stream inherits byte-exactness rather than
//! inventing a second encoding for the same bytes.

use std::path::{Path, PathBuf};

use ansible_capture::Chunk;
use anyhow::{Context, Result};

use crate::hub::{Hub, version_ok};
use crate::model::{MemberDoc, Message};
use crate::state::{read_json, safe_component, write_atomic, write_json};

/// How many live chunks one poll will return. Bounds the viewer's memory and its
/// catch-up burst when it joins a session that has been running for a while.
const MAX_CHUNKS_PER_POLL: usize = 64;

/// How long a sent message stays in the sender's outbox.
///
/// The sender prunes, because the sender is the only writer of that directory. A
/// recipient that was offline for longer than this misses the message — the right
/// trade for a prototype, and the reason the window is generous.
const OUTBOX_RETENTION_MS: u64 = 3_600_000;

pub struct DirHub {
    root: PathBuf,
}

impl DirHub {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn members_dir(&self) -> PathBuf {
        self.root.join("members")
    }

    fn outbox_dir(&self, login: &str) -> PathBuf {
        self.root.join("outbox").join(safe_component(login))
    }

    fn live_dir(&self, key: &str) -> PathBuf {
        self.root.join("live").join(safe_component(key))
    }

    /// Drop our own sent messages once they are older than the retention window.
    fn prune_outbox(&self, login: &str, now_ms: u64) {
        let dir = self.outbox_dir(login);
        let Ok(entries) = std::fs::read_dir(&dir) else { return };
        for entry in entries.filter_map(std::result::Result::ok) {
            let path = entry.path();
            let expired = read_json::<Message>(&path)
                .is_some_and(|m| now_ms.saturating_sub(m.created_ms) > OUTBOX_RETENTION_MS);
            if expired {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

impl Hub for DirHub {
    fn publish(&mut self, doc: &MemberDoc) -> Result<()> {
        let path = self.members_dir().join(format!("{}.json", safe_component(&doc.login)));
        write_json(&path, doc)?;
        self.prune_outbox(&doc.login, doc.published_ms);
        Ok(())
    }

    fn members(&mut self) -> Result<Vec<MemberDoc>> {
        let dir = self.members_dir();
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            // An empty hub is not an error: it is what a team of one looks like
            // before anyone else has published, and what every hub looks like on
            // the first run.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err).context(format!("reading {}", dir.display())),
        };
        let mut out: Vec<MemberDoc> = entries
            .filter_map(std::result::Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
            .filter_map(|p| read_json::<MemberDoc>(&p))
            .filter(|doc| version_ok(doc.v))
            .collect();
        out.sort_by(|a, b| a.login.cmp(&b.login).then(a.host.cmp(&b.host)));
        Ok(out)
    }

    fn send(&mut self, message: &Message) -> Result<()> {
        let path =
            self.outbox_dir(&message.from).join(format!("{}.json", safe_component(&message.id)));
        write_json(&path, message)
    }

    fn messages_for(&mut self, login: &str) -> Result<Vec<Message>> {
        let root = self.root.join("outbox");
        let Ok(senders) = std::fs::read_dir(&root) else { return Ok(Vec::new()) };
        let mut out = Vec::new();
        for sender in senders.filter_map(std::result::Result::ok) {
            let Ok(files) = std::fs::read_dir(sender.path()) else { continue };
            for file in files.filter_map(std::result::Result::ok) {
                let Some(message) = read_json::<Message>(&file.path()) else { continue };
                if version_ok(message.v) && message.to == login {
                    out.push(message);
                }
            }
        }
        out.sort_by_key(|m| (m.created_ms, m.id.clone()));
        Ok(out)
    }

    fn supports_live(&self) -> bool {
        true
    }

    fn put_chunk(&mut self, key: &str, chunk: &Chunk) -> Result<()> {
        let path = self.live_dir(key).join(format!("{:020}.jsonl", chunk.seq));
        let jsonl = chunk.to_jsonl().context("encoding a live chunk")?;
        write_atomic(&path, jsonl.as_bytes())
    }

    fn chunks(&mut self, key: &str, from_seq: u64) -> Result<Vec<Chunk>> {
        let dir = self.live_dir(key);
        let Ok(entries) = std::fs::read_dir(&dir) else { return Ok(Vec::new()) };
        let mut paths: Vec<(u64, PathBuf)> = entries
            .filter_map(std::result::Result::ok)
            .filter_map(|e| seq_of(&e.path()).map(|seq| (seq, e.path())))
            .filter(|(seq, _)| *seq >= from_seq)
            .collect();
        paths.sort_by_key(|(seq, _)| *seq);
        paths.truncate(MAX_CHUNKS_PER_POLL);

        let mut out = Vec::with_capacity(paths.len());
        for (_, path) in paths {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            // A chunk still being written is not corruption, it is a race with the
            // publisher's rename losing by a hair. Stop at the first unreadable
            // one so the stream stays contiguous and resume from it next poll.
            match Chunk::from_jsonl(&text) {
                Ok(chunk) => out.push(chunk),
                Err(_) => break,
            }
        }
        Ok(out)
    }

    fn prune_chunks(&mut self, key: &str, before_seq: u64) -> Result<()> {
        let dir = self.live_dir(key);
        let Ok(entries) = std::fs::read_dir(&dir) else { return Ok(()) };
        for entry in entries.filter_map(std::result::Result::ok) {
            let path = entry.path();
            if seq_of(&path).is_some_and(|seq| seq < before_seq) {
                let _ = std::fs::remove_file(&path);
            }
        }
        Ok(())
    }

    fn describe(&self) -> String {
        format!("dir hub at {}", self.root.display())
    }
}

/// Recover a chunk's sequence number from its file name.
///
/// Names are zero-padded so a lexical directory listing is already in sequence
/// order, but the number is parsed rather than trusted from position, because
/// anything at all can end up in a shared directory.
fn seq_of(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_suffix(".jsonl")?;
    stem.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentCard, MessageKind, SCHEMA_VERSION, Share, Status, agent_key};
    use ansible_capture::{Chunker, ChunkerConfig, Ruleset};

    fn hub(tag: &str) -> DirHub {
        let dir =
            std::env::temp_dir().join(format!("ansible-herd-dir-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        DirHub::new(dir)
    }

    fn doc(login: &str, published_ms: u64) -> MemberDoc {
        let mut doc = MemberDoc::new(login, "box");
        doc.published_ms = published_ms;
        doc.seq = 1;
        doc.agents.push(AgentCard {
            key: agent_key(login, "box", "w1:p1"),
            pane_id: "w1:p1".into(),
            workspace: Some("ansible".into()),
            tab: None,
            agent: "claude".into(),
            status: Status::Working,
            headline: "refactor auth".into(),
            repo: None,
            branch: None,
            share: Share::Title,
            help: None,
            since_ms: published_ms,
            live_seq: None,
        });
        doc
    }

    /// Chunks for a byte string. `finish` matters: the redactor holds a tail back
    /// for lookahead, so `push` alone can produce no chunks at all.
    fn chunks_of(bytes: &[u8], max_bytes: usize) -> Vec<Chunk> {
        let mut chunker =
            Chunker::new("s1", ChunkerConfig { max_bytes, max_age_ms: 1_000 }, Ruleset::default());
        let mut out = chunker.push(bytes, 0);
        out.extend(chunker.finish(1));
        out
    }

    fn message(id: &str, from: &str, to: &str, created_ms: u64) -> Message {
        Message {
            v: SCHEMA_VERSION,
            id: id.into(),
            from: from.into(),
            to: to.into(),
            to_key: agent_key(to, "box", "w1:p1"),
            kind: MessageKind::Comment,
            body: "look at line 42".into(),
            anchor_line: Some(42),
            created_ms,
        }
    }

    #[test]
    fn an_empty_hub_is_not_an_error() {
        let mut hub = hub("empty");
        assert!(hub.members().expect("read").is_empty());
        assert!(hub.messages_for("mrshll").expect("read").is_empty());
    }

    #[test]
    fn presence_round_trips_and_is_replaced_whole() {
        let mut hub = hub("presence");
        hub.publish(&doc("mrshll", 1_000)).expect("publish");
        assert_eq!(hub.members().expect("read").len(), 1);

        let mut second = doc("mrshll", 2_000);
        second.seq = 2;
        second.agents.clear();
        hub.publish(&second).expect("republish");
        let members = hub.members().expect("read");
        assert_eq!(members.len(), 1, "replaced, not appended");
        assert!(members[0].agents.is_empty());
        assert_eq!(members[0].seq, 2);
    }

    #[test]
    fn two_members_do_not_collide() {
        let mut hub = hub("two");
        hub.publish(&doc("mrshll", 1_000)).expect("publish");
        hub.publish(&doc("alice", 1_000)).expect("publish");
        let logins: Vec<String> =
            hub.members().expect("read").into_iter().map(|d| d.login).collect();
        assert_eq!(logins, vec!["alice", "mrshll"]);
    }

    #[test]
    fn a_document_from_a_newer_schema_is_skipped_rather_than_misread() {
        let mut hub = hub("version");
        hub.publish(&doc("mrshll", 1_000)).expect("publish");
        let path = hub.members_dir().join("future.json");
        write_atomic(&path, br#"{"v":99,"login":"future","host":"box","seq":1,"published_ms":1}"#)
            .expect("write");
        let logins: Vec<String> =
            hub.members().expect("read").into_iter().map(|d| d.login).collect();
        assert_eq!(logins, vec!["mrshll"]);
    }

    #[test]
    fn unrelated_files_in_the_hub_are_ignored() {
        let mut hub = hub("junk");
        hub.publish(&doc("mrshll", 1_000)).expect("publish");
        write_atomic(&hub.members_dir().join("README.txt"), b"hello").expect("write");
        write_atomic(&hub.members_dir().join("broken.json"), b"{").expect("write");
        assert_eq!(hub.members().expect("read").len(), 1);
    }

    #[test]
    fn mail_is_readable_by_its_recipient_and_invisible_to_others() {
        let mut hub = hub("mail");
        hub.send(&message("alice-1", "alice", "mrshll", 100)).expect("send");
        hub.send(&message("alice-2", "alice", "bob", 200)).expect("send");
        let mine = hub.messages_for("mrshll").expect("read");
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].id, "alice-1");
        assert_eq!(mine[0].anchor_line, Some(42));
    }

    #[test]
    fn mail_is_ordered_oldest_first_across_senders() {
        let mut hub = hub("mail-order");
        hub.send(&message("alice-1", "alice", "mrshll", 300)).expect("send");
        hub.send(&message("bob-1", "bob", "mrshll", 100)).expect("send");
        let ids: Vec<String> =
            hub.messages_for("mrshll").expect("read").into_iter().map(|m| m.id).collect();
        assert_eq!(ids, vec!["bob-1", "alice-1"]);
    }

    /// The sender is the only writer of its own outbox, so the sender prunes.
    #[test]
    fn publishing_prunes_our_own_expired_mail_and_leaves_recent_mail_alone() {
        let mut hub = hub("prune-mail");
        hub.send(&message("mrshll-1", "mrshll", "alice", 0)).expect("send");
        hub.send(&message("mrshll-2", "mrshll", "alice", OUTBOX_RETENTION_MS)).expect("send");

        let mut doc = doc("mrshll", OUTBOX_RETENTION_MS + 1);
        doc.seq = 5;
        hub.publish(&doc).expect("publish");

        let ids: Vec<String> =
            hub.messages_for("alice").expect("read").into_iter().map(|m| m.id).collect();
        assert_eq!(ids, vec!["mrshll-2"], "only the expired one goes");
    }

    /// Chunks go through `ansible-capture`'s stored form, so this test is also a
    /// check that a live stream survives the trip byte for byte.
    #[test]
    fn live_chunks_round_trip_in_order_and_byte_exactly() {
        let mut hub = hub("live");
        let key = agent_key("mrshll", "box", "w1:p1");

        let mut chunker = Chunker::new(
            "s1",
            ChunkerConfig { max_bytes: 8, max_age_ms: 1_000 },
            Ruleset::default(),
        );
        let source = b"hello world, this is terminal output";
        let mut published = Vec::new();
        for (i, byte) in source.iter().enumerate() {
            let at = u64::try_from(i).expect("small");
            published.extend(chunker.push(&[*byte], at));
        }
        published.extend(chunker.finish(1_000));
        assert!(published.len() > 1, "expected several chunks");
        for chunk in &published {
            hub.put_chunk(&key, chunk).expect("put");
        }

        let read = hub.chunks(&key, 0).expect("read");
        assert_eq!(read.len(), published.len());
        let seqs: Vec<u64> = read.iter().map(|c| c.seq).collect();
        let expected: Vec<u64> = (0..u64::try_from(published.len()).expect("small")).collect();
        assert_eq!(seqs, expected, "ordered by sequence, not by directory order");

        let rebuilt: Vec<u8> = read.iter().flat_map(Chunk::payload).collect();
        assert_eq!(rebuilt, source, "byte-exact through the hub");
    }

    #[test]
    fn a_watcher_resumes_from_its_cursor() {
        let mut hub = hub("cursor");
        let key = agent_key("mrshll", "box", "w1:p1");
        for chunk in &chunks_of(b"abcdefghijkl", 4) {
            hub.put_chunk(&key, chunk).expect("put");
        }
        let all = hub.chunks(&key, 0).expect("read");
        assert!(all.len() >= 3, "got {}", all.len());

        let tail = hub.chunks(&key, 2).expect("read from cursor");
        assert_eq!(tail.first().map(|c| c.seq), Some(2));
        assert_eq!(tail.len(), all.len() - 2);
    }

    #[test]
    fn pruning_drops_only_chunks_below_the_cursor() {
        let mut hub = hub("prune-live");
        let key = agent_key("mrshll", "box", "w1:p1");
        for chunk in &chunks_of(b"abcdefghijkl", 4) {
            hub.put_chunk(&key, chunk).expect("put");
        }
        hub.prune_chunks(&key, 2).expect("prune");
        let left: Vec<u64> = hub.chunks(&key, 0).expect("read").iter().map(|c| c.seq).collect();
        assert!(left.iter().all(|seq| *seq >= 2), "got {left:?}");
        assert!(!left.is_empty());
    }

    /// A half-written chunk must stop the read rather than leave a gap in the
    /// middle of a stream a viewer is splicing by byte offset.
    #[test]
    fn a_truncated_chunk_ends_the_poll_instead_of_creating_a_gap() {
        let mut hub = hub("truncated");
        let key = agent_key("mrshll", "box", "w1:p1");
        let produced = chunks_of(b"abcdefghijkl", 4);
        assert!(produced.len() > 2);
        for chunk in &produced {
            hub.put_chunk(&key, chunk).expect("put");
        }
        // Corrupt the second chunk the way a losing rename race would.
        write_atomic(&hub.live_dir(&key).join(format!("{:020}.jsonl", 1)), b"{\"session_id\"")
            .expect("write");

        let read = hub.chunks(&key, 0).expect("read");
        assert_eq!(read.len(), 1, "stops at the bad chunk: {read:?}");
        assert_eq!(read[0].seq, 0);
    }

    #[test]
    fn a_poll_is_bounded_so_a_late_joiner_does_not_read_a_whole_session() {
        let mut hub = hub("bounded");
        let key = agent_key("mrshll", "box", "w1:p1");
        let produced = chunks_of(&[b'x'; MAX_CHUNKS_PER_POLL + 10], 1);
        assert!(produced.len() > MAX_CHUNKS_PER_POLL, "got {}", produced.len());
        for chunk in &produced {
            hub.put_chunk(&key, chunk).expect("put");
        }
        assert_eq!(hub.chunks(&key, 0).expect("read").len(), MAX_CHUNKS_PER_POLL);
    }

    /// Keys come from other people's presence documents, and they name a
    /// directory.
    #[test]
    fn a_key_with_path_separators_cannot_escape_the_live_directory() {
        let mut hub = hub("escape");
        let chunks = chunks_of(b"x", 64);
        let chunk = chunks.first().expect("finish always closes the open chunk");

        hub.put_chunk("../../escaped", chunk).expect("put");
        let live = hub.root.join("live");
        let entries: Vec<String> = std::fs::read_dir(&live)
            .expect("live dir exists")
            .filter_map(std::result::Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["_.._.._escaped".to_string()], "no path separators survive");
        // And the chunk is readable back under the same sanitized name, so
        // sanitization is stable rather than lossy in one direction only.
        assert_eq!(hub.chunks("../../escaped", 0).expect("read").len(), 1);
    }

    #[test]
    fn a_sequence_number_is_parsed_from_the_file_name_not_assumed() {
        assert_eq!(seq_of(Path::new("/x/00000000000000000007.jsonl")), Some(7));
        assert_eq!(seq_of(Path::new("/x/notanumber.jsonl")), None);
        assert_eq!(seq_of(Path::new("/x/7.json")), None);
    }
}
