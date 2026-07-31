/**
 * Tunables, in one place, in the unit the host actually uses.
 *
 * SpacetimeDB timestamps and durations are microseconds, so these are too —
 * converting at the call site is how a factor-of-1000 bug gets in.
 */

/** How long a session may go without a heartbeat before the reaper detaches it. */
export const HEARTBEAT_TIMEOUT_MICROS = 90_000_000n;

/** How often the reaper runs. */
export const REAP_INTERVAL_MICROS = 60_000_000n;

/** How often history pruning runs. */
export const PRUNE_INTERVAL_MICROS = 3_600_000_000n;

/** Transitions retained per session. Bounds the one table that grows. */
export const HISTORY_PER_SESSION = 200;
