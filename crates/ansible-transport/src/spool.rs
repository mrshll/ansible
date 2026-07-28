//! The local spool: chunks that exist but are not yet in R2.
//!
//! The plan requires that uploads never drop-and-continue. On Worker failure the
//! publisher retries with backoff and keeps buffering here; the cursor stops
//! advancing and viewers show a stall. That is the whole point — a stalled tail is
//! recoverable, a dropped chunk is not.
//!
//! It doubles as the crash path. A killed app sends no `SessionEnd` and closes no
//! PTY, so `reap_stale_sessions` marks the session `Detached`; on next launch the
//! spool is what lets the tail be re-uploaded and the session finalized late.
//!
//! Chunks are stored one file per sequence number, named so that lexical order is
//! not relied upon anywhere — the sequence number in the filename is parsed back
//! out and sorted numerically, because `10.jsonl` sorts before `9.jsonl`.

use std::fs;
use std::path::{Path, PathBuf};

use ansible_capture::Chunk;

use crate::Result;

pub struct Spool {
    dir: PathBuf,
}

impl Spool {
    /// Open (creating if needed) the spool directory for one session.
    ///
    /// # Errors
    /// Errors if the directory cannot be created.
    pub fn open(root: &Path, session_id: &str) -> Result<Self> {
        let dir = root.join(session_id);
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    fn path_for(&self, seq: u64) -> PathBuf {
        self.dir.join(format!("{seq}.jsonl"))
    }

    /// Persist a chunk before attempting to upload it.
    ///
    /// Written before the network call, not after: a chunk that only exists in
    /// flight is a chunk that a crash loses, and losing one is unrecoverable
    /// because byte offsets are contiguous and nothing downstream can synthesize
    /// the missing range.
    ///
    /// # Errors
    /// Errors if the chunk cannot be serialized or written.
    pub fn put(&self, chunk: &Chunk) -> Result<()> {
        let text = chunk.to_jsonl()?;
        fs::write(self.path_for(chunk.seq), text)?;
        Ok(())
    }

    /// Forget a chunk the Worker has confirmed is durable.
    ///
    /// # Errors
    /// Errors on an I/O failure other than the file already being gone.
    pub fn remove(&self, seq: u64) -> Result<()> {
        match fs::remove_file(self.path_for(seq)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Every spooled sequence number, ascending.
    ///
    /// Sorted numerically rather than by filename, so a resume after 10 chunks
    /// replays them in stream order instead of `1, 10, 2`.
    ///
    /// # Errors
    /// Errors if the directory cannot be read.
    pub fn pending(&self) -> Result<Vec<u64>> {
        let mut seqs = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "jsonl")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                && let Ok(seq) = stem.parse::<u64>()
            {
                seqs.push(seq);
            }
        }
        seqs.sort_unstable();
        Ok(seqs)
    }

    /// Read a spooled chunk back.
    ///
    /// # Errors
    /// Errors if the file is missing or does not parse as a valid chunk.
    pub fn get(&self, seq: u64) -> Result<Chunk> {
        let text = fs::read_to_string(self.path_for(seq))?;
        Ok(Chunk::from_jsonl(&text)?)
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ansible_capture::Record;

    fn chunk(seq: u64, start: u64, payload: &[u8]) -> Chunk {
        Chunk {
            session_id: "s-1".into(),
            seq,
            byte_start: start,
            byte_end: start + payload.len() as u64,
            started_at_ms: 1_700_000_000_000,
            ended_at_ms: 1_700_000_000_010,
            redaction_version: 2,
            records: vec![Record { at_delta_ms: 0, bytes: payload.to_vec() }],
        }
    }

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "ansible-spool-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn round_trips_a_chunk_through_the_spool() {
        let root = tempdir();
        let spool = Spool::open(&root, "sess").unwrap();
        let original = chunk(0, 0, b"hello");
        spool.put(&original).unwrap();
        assert_eq!(spool.get(0).unwrap(), original);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn pending_is_ordered_numerically_not_lexically() {
        let root = tempdir().join("numeric");
        let spool = Spool::open(&root, "sess").unwrap();
        // `10` sorts before `9` as text. If the spool ever replayed in filename
        // order it would upload chunk 10 before chunk 9, and the Worker would
        // reject the whole tail as a gap.
        for seq in [1_u64, 2, 9, 10, 11] {
            spool.put(&chunk(seq, seq * 5, b"12345")).unwrap();
        }
        assert_eq!(spool.pending().unwrap(), vec![1, 2, 9, 10, 11]);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn removing_a_confirmed_chunk_is_idempotent() {
        let root = tempdir().join("idem");
        let spool = Spool::open(&root, "sess").unwrap();
        spool.put(&chunk(0, 0, b"x")).unwrap();
        spool.remove(0).unwrap();
        // A retry that already succeeded must not turn into an error.
        spool.remove(0).unwrap();
        assert!(spool.pending().unwrap().is_empty());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ignores_unrelated_files() {
        let root = tempdir().join("junk");
        let spool = Spool::open(&root, "sess").unwrap();
        spool.put(&chunk(3, 0, b"abc")).unwrap();
        fs::write(spool.dir().join("notes.txt"), "ignore me").unwrap();
        fs::write(spool.dir().join("nope.jsonl"), "not a number").unwrap();
        assert_eq!(spool.pending().unwrap(), vec![3]);
        fs::remove_dir_all(&root).ok();
    }
}
