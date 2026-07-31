//! The one place this crate reads a clock.
//!
//! Kept to a single function so the rest of the code takes `now_ms` as an
//! argument and stays testable, which is the same boundary `ansible-capture` and
//! `ansible-hooks` draw. Anything that reaches for the time instead of accepting
//! it is a thing that cannot be replayed.

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch.
///
/// A clock before 1970, or one so far in the future that milliseconds overflow
/// `u64`, both saturate rather than panic. Presence is not worth aborting for, and
/// a saturated timestamp shows up as a stale row, which is the honest reading of a
/// machine whose clock is wrong.
#[must_use]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}
