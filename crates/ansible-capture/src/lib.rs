//! Transcript capture: raw PTY bytes in, redacted ordered chunks out.
//!
//! This crate is a pure function of `(bytes, timestamps, config)`. It does no
//! I/O, reads no clock, and depends on neither the terminal crate nor the hub —
//! which is what lets the capture path be golden-tested and fuzzed. Capture
//! correctness is the one thing in this system with no acceptable failure mode,
//! so it gets the strictest boundary.
//!
//! Two invariants hold everywhere below:
//!
//! 1. **Order.** Chunk sequence numbers are dense and increasing, and byte
//!    ranges are contiguous. [`Reassembler`] refuses anything else rather than
//!    splicing over a gap, because a silent gap is worse than a visible stall.
//! 2. **Redaction happens first.** Secrets are replaced before a byte can reach
//!    a chunk, including secrets that straddle two writes. Byte offsets
//!    therefore index the *redacted* stream — the only one that gets stored.

pub mod chunk;
pub mod chunker;
pub mod redact;

pub use chunk::{Chunk, Record};
pub use chunker::{Chunker, ChunkerConfig, Reassembler};
pub use redact::{Redactor, Rule, Ruleset};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The chunk stream violated an ordering or self-description invariant.
    #[error("capture protocol violation: {0}")]
    Protocol(String),

    #[error("chunk serialization failed: {0}")]
    Serde(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_errors_describe_themselves() {
        let e = Error::Protocol("expected chunk 3 but got 5".into());
        assert_eq!(e.to_string(), "capture protocol violation: expected chunk 3 but got 5");
    }
}
