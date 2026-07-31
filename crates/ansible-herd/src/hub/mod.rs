//! The team hub: where presence, mail, and live frames are exchanged.
//!
//! # Why there is a trait here at all
//!
//! Presence for a team needs three very different latency budgets, and no single
//! transport is good at all of them:
//!
//! | | infrastructure to stand up | presence latency | live frames |
//! |---|---|---|---|
//! | [`dir`] | none, if the team already shares a filesystem | sub-second | yes |
//! | [`git`] | none — a repo the team already has | fetch interval, ~5s | no |
//! | relay | the Worker and Durable Object from Spike B | ~3 ms | yes |
//!
//! The prototype ships the first two, because both work today on an unmodified
//! machine. `git` is the one that answers "connect with a GitHub team" literally:
//! the presence documents live in Git refs, so push access to the repo *is* the
//! authorization, and there is nothing to deploy or pay for. `dir` is what makes
//! the whole thing demonstrable on one machine in a minute, and it is the only
//! backend that carries a live byte stream.
//!
//! The relay is deliberately not implemented here. It is the existing
//! `crates/ansible-transport` path — `docs/spikes/deployed-round-trip.md` measured
//! it at 3 ms against cursor-follow's 1.3–1.6 s — and slotting it in behind this
//! trait is the obvious next step once the shape of the presence data stops
//! moving.
//!
//! # The invariant every backend keeps
//!
//! **One writer per path.** A member writes only paths keyed by their own login.
//! Nothing in the hub is ever read-modify-written by two machines, so there is no
//! lock, no transaction, and no merge — on a filesystem, in Git, or on a relay.
//! Every backend below is a way of moving one-writer documents around; that is the
//! whole design.

pub mod dir;
pub mod git;

use ansible_capture::Chunk;
use anyhow::{Result, bail};

use crate::config::Config;
use crate::model::{MemberDoc, Message};

/// A team hub.
///
/// Methods take `&mut self` because a backend may hold a connection or a fetch
/// cursor; none of them are required to be cheap, and the daemon calls them on a
/// timer rather than per event.
pub trait Hub {
    /// Replace this member's presence document.
    ///
    /// # Errors
    /// Transport failure. The daemon logs and retries on the next heartbeat, so a
    /// hub that is briefly unreachable shows up as staleness rather than a crash.
    fn publish(&mut self, doc: &MemberDoc) -> Result<()>;

    /// Read every member's presence document, including our own.
    ///
    /// Our own is included so the roster can show what teammates are seeing about
    /// us, which is the only honest way to display "you are sharing live".
    ///
    /// # Errors
    /// Transport failure.
    fn members(&mut self) -> Result<Vec<MemberDoc>>;

    /// Append a message to our own outbox.
    ///
    /// # Errors
    /// Transport failure.
    fn send(&mut self, message: &Message) -> Result<()>;

    /// Read messages addressed to `login` from every other member's outbox.
    ///
    /// Delivery is at-least-once and duplicates are expected: the caller
    /// de-duplicates by [`Message::id`] through [`crate::state::Store::deliver`].
    ///
    /// # Errors
    /// Transport failure.
    fn messages_for(&mut self, login: &str) -> Result<Vec<Message>>;

    /// Whether this backend can carry a live terminal stream.
    fn supports_live(&self) -> bool;

    /// Publish one chunk of a live session.
    ///
    /// # Errors
    /// Transport failure, or [`Error::Unsupported`] on a backend without live
    /// support.
    ///
    /// [`Error::Unsupported`]: anyhow::Error
    fn put_chunk(&mut self, key: &str, chunk: &Chunk) -> Result<()>;

    /// Read live chunks for `key` with sequence numbers at or above `from_seq`.
    ///
    /// # Errors
    /// Transport failure.
    fn chunks(&mut self, key: &str, from_seq: u64) -> Result<Vec<Chunk>>;

    /// Drop live chunks below `before_seq`, which the publisher calls to keep a
    /// long session from growing without bound.
    ///
    /// # Errors
    /// Transport failure.
    fn prune_chunks(&mut self, key: &str, before_seq: u64) -> Result<()>;

    /// One line for `doctor` and for the roster's footer, naming where presence is
    /// actually going. Worth showing: "which hub am I on" is the first question
    /// when two people cannot see each other.
    fn describe(&self) -> String;
}

/// Build the hub named in config.
///
/// `state_dir` is where a backend may keep files of its own; the `git` backend
/// mirrors what it publishes there.
///
/// # Errors
/// An unknown `kind`, or a backend whose required settings are missing. Both are
/// reported with the key to fix, because this is the error a new installer hits.
pub fn open(config: &Config, state_dir: &std::path::Path) -> Result<Box<dyn Hub>> {
    match config.hub.kind.as_str() {
        "dir" => {
            let Some(path) = config.hub.path.clone() else {
                bail!(
                    "hub.kind = \"dir\" needs hub.path — a directory every member can read and write"
                );
            };
            Ok(Box::new(dir::DirHub::new(path)))
        }
        "git" => {
            let Some(remote) = config.hub.remote.clone() else {
                bail!(
                    "hub.kind = \"git\" needs hub.remote — the remote carrying the presence refs"
                );
            };
            let repo = config.hub.repo.clone().unwrap_or_else(|| ".".into());
            Ok(Box::new(git::GitHub::new(repo, remote).with_work_dir(state_dir.join("git-hub"))))
        }
        other => bail!("unknown hub.kind {other:?}: expected \"dir\" or \"git\""),
    }
}

/// Reject a document from a schema version we do not understand.
///
/// A team installs a plugin at its own pace, so a teammate on a newer version is
/// the normal case rather than an exception. Skipping their row and saying so
/// beats deserializing fields that have moved.
#[must_use]
pub fn version_ok(doc_version: u32) -> bool {
    doc_version == crate::model::SCHEMA_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Hub as HubConfig;
    use std::path::Path;

    fn with_hub(hub: HubConfig) -> Config {
        Config { hub, ..Config::default() }
    }

    #[test]
    fn a_dir_hub_without_a_path_names_the_key_to_fix() {
        let config = with_hub(HubConfig { kind: "dir".into(), ..HubConfig::default() });
        let err = open(&config, Path::new("/tmp")).err().expect("no path");
        assert!(format!("{err}").contains("hub.path"), "got {err}");
    }

    #[test]
    fn a_git_hub_without_a_remote_names_the_key_to_fix() {
        let config = with_hub(HubConfig { kind: "git".into(), ..HubConfig::default() });
        let err = open(&config, Path::new("/tmp")).err().expect("no remote");
        assert!(format!("{err}").contains("hub.remote"), "got {err}");
    }

    #[test]
    fn an_unknown_kind_lists_the_ones_that_exist() {
        let config = with_hub(HubConfig { kind: "spacetime".into(), ..HubConfig::default() });
        let err = open(&config, Path::new("/tmp")).err().expect("unknown kind");
        let text = format!("{err}");
        assert!(text.contains("dir") && text.contains("git"), "got {text}");
    }

    #[test]
    fn only_the_dir_hub_claims_live_support() {
        let config = with_hub(HubConfig {
            kind: "dir".into(),
            path: Some("/tmp/herd".into()),
            ..HubConfig::default()
        });
        assert!(open(&config, Path::new("/tmp")).expect("dir hub").supports_live());

        let config = with_hub(HubConfig {
            kind: "git".into(),
            remote: Some("origin".into()),
            ..HubConfig::default()
        });
        assert!(!open(&config, Path::new("/tmp")).expect("git hub").supports_live());
    }

    #[test]
    fn only_the_current_schema_version_is_accepted() {
        assert!(version_ok(crate::model::SCHEMA_VERSION));
        assert!(!version_ok(crate::model::SCHEMA_VERSION + 1), "a newer teammate");
        assert!(!version_ok(0));
    }
}
