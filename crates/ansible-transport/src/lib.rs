//! Getting chunks to the archive, and getting them back out.
//!
//! [`ansible_capture`] turns PTY bytes into ordered chunks; this crate moves them
//! and reassembles them. The split matters: capture is a pure function with no I/O
//! and no clock, which is what makes it golden-testable, so every network call,
//! retry, and spool write lives here instead.
//!
//! # Two streams, one order
//!
//! A session publishes twice over, and the difference is the whole design:
//!
//! - [`Publisher::publish_frame`] sends bytes the instant they leave the PTY.
//!   Ephemeral, addressed by byte range, and the only reason the relay feels live.
//! - [`Publisher::publish_chunk`] sends a closed chunk (~64 KiB or ~1s). Durable,
//!   addressed by sequence, lands in R2, and is what advances the cursor.
//!
//! [`LiveViewer`] consumes both and cannot double-apply the overlap, because it
//! tracks a single `received_through` byte offset and splices by absolute offset
//! rather than trusting arrival order. That is what makes "backfill through the
//! cursor, then tail the relay, and fall back to cursor-follow after a disconnect"
//! safe rather than hopeful.
//!
//! # The invariant
//!
//! Order. A gap is never spliced over, never inferred, and never silently
//! tolerated: [`LiveViewer::apply`] rejects anything that would leave one, because
//! a visible stall is strictly better than a transcript that is quietly wrong.

pub mod publisher;
pub mod spool;
pub mod viewer;

pub use publisher::{Publisher, PublisherConfig};
pub use spool::Spool;
pub use viewer::{Applied, LiveViewer, RelayFrame};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The stream would have gained a gap or gone backwards.
    #[error("transport protocol violation: {0}")]
    Protocol(String),

    /// The Worker rejected a publish, or was unreachable.
    #[error("worker rejected {what}: {status} {body}")]
    Worker { what: String, status: u16, body: String },

    #[error("transport I/O failed: {0}")]
    Io(#[from] std::io::Error),

    #[error("chunk serialization failed: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("capture: {0}")]
    Capture(#[from] ansible_capture::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Milliseconds since the Unix epoch.
///
/// The capture crate takes timestamps as arguments rather than reading a clock, so
/// somebody has to be the clock. It is this crate, at the edge, where the reading
/// is also the thing being measured for latency.
#[must_use]
pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}
