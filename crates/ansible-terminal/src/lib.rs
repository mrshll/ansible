//! A terminal backed by libghostty-vt.
//!
//! Ghostty ships two very different C libraries. The one in `include/ghostty.h`
//! is the GUI embedding API, and its surface config accepts only an AppKit
//! `NSView` or a UIKit `UIView` — there is no Linux variant, so it cannot host a
//! terminal inside a GTK application. The one this crate binds,
//! `include/ghostty/vt.h` (libghostty-vt), is cross-platform and provides VT
//! parsing, terminal state, incremental render state, and input encoders.
//!
//! So this crate owns terminal *state and correctness*; drawing is the host's
//! job, fed by [`Snapshot`]. See `docs/spikes/terminal-embedding.md`.

pub mod backend;
pub mod config;
pub mod event;
pub mod snapshot;

#[cfg(have_libghostty_vt)]
mod sys;
#[cfg(have_libghostty_vt)]
pub mod vt;

#[cfg(have_libghostty_vt)]
mod ghostty;
#[cfg(have_libghostty_vt)]
mod pty;

pub use backend::{TerminalBackend, TerminalEvents};
pub use config::TerminalConfig;
pub use event::{
    ExitReason, Key, KeyAction, KeyEvent, Modifiers, TerminalEvent, TerminalInput, TerminalSize,
};
pub use snapshot::{Cell, CellStyle, CellWidth, Cursor, CursorShape, Rgb, Snapshot};

#[cfg(have_libghostty_vt)]
pub use ghostty::GhosttyTerminal;

/// Whether this build linked the native libghostty-vt library.
///
/// Without it only the contract and snapshot types are available; the harness
/// checks this so it can fail with a useful message instead of a link error.
pub const HAS_NATIVE_BACKEND: bool = cfg!(have_libghostty_vt);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("libghostty-vt call {call} failed: {code}")]
    Vt { call: &'static str, code: i32 },

    #[error("terminal size {cols}x{rows} is invalid")]
    InvalidSize { cols: u16, rows: u16 },

    #[error("the terminal has already exited")]
    Exited,

    #[error("pty error: {0}")]
    Pty(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_describe_themselves() {
        let e = Error::InvalidSize { cols: 0, rows: 24 };
        assert_eq!(e.to_string(), "terminal size 0x24 is invalid");
        assert_eq!(Error::Exited.to_string(), "the terminal has already exited");
    }
}
