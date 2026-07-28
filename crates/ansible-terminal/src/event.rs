//! Typed input, resize, state, and raw-output events crossing the host boundary.

use std::fmt;

/// Grid geometry plus the pixel size of one cell.
///
/// libghostty-vt needs the pixel dimensions as well as the cell counts: they
/// feed in-band size reports (mode 2048) and the image protocols.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSize {
    pub cols: u16,
    pub rows: u16,
    pub cell_width_px: u32,
    pub cell_height_px: u32,
}

impl TerminalSize {
    pub fn new(cols: u16, rows: u16, cell_width_px: u32, cell_height_px: u32) -> Self {
        Self { cols, rows, cell_width_px, cell_height_px }
    }

    /// Grid dimensions are 1-based in the VT model; zero would be rejected by
    /// `ghostty_terminal_resize` and makes no sense for a PTY winsize either.
    pub fn is_valid(&self) -> bool {
        self.cols > 0 && self.rows > 0
    }

    pub fn pixel_width(&self) -> u32 {
        u32::from(self.cols) * self.cell_width_px
    }

    pub fn pixel_height(&self) -> u32 {
        u32::from(self.rows) * self.cell_height_px
    }
}

/// Keyboard modifiers, mirroring `GhosttyMods`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub super_: bool,
}

impl Modifiers {
    pub const NONE: Self = Self { shift: false, ctrl: false, alt: false, super_: false };

    pub fn ctrl() -> Self {
        Self { ctrl: true, ..Self::NONE }
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::NONE
    }
}

/// A physical key press or release.
///
/// `key` is a logical key identifier from [`Key`]; `text` carries the
/// already-composed UTF-8 for text-producing keys, which is what the IME
/// hands us and what libghostty's encoder expects alongside the keycode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyEvent {
    pub key: Key,
    pub mods: Modifiers,
    pub action: KeyAction,
    pub text: Option<String>,
}

impl KeyEvent {
    pub fn press(key: Key, mods: Modifiers) -> Self {
        Self { key, mods, action: KeyAction::Press, text: None }
    }

    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.text = Some(text.into());
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    Press,
    Repeat,
    Release,
}

/// Logical keys the harness can name.
///
/// Deliberately a small set: the spike only needs the keys the verification
/// matrix exercises, and every variant maps onto a concrete `GHOSTTY_KEY_*`
/// constant so no mapping has to be invented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// A key identified by its unshifted Unicode codepoint (`a`, `1`, `[`).
    Char(char),
    Enter,
    Tab,
    Backspace,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Delete,
    Insert,
    F(u8),
}

/// Everything the host can push into a terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalInput {
    Key(KeyEvent),
    /// Pre-composed text that bypasses key encoding (IME commit, synthetic input).
    Text(String),
    /// Clipboard paste. Bracketed-paste framing is applied by the backend when
    /// the terminal has mode 2004 enabled, so callers never encode it themselves.
    Paste(String),
    Focus(bool),
    /// Bytes written straight to the PTY. Used by tests and by the harness for
    /// signals that have no key representation.
    Raw(Vec<u8>),
}

/// Why the child process is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    Code(i32),
    Signal(i32),
    /// The PTY read side closed without a wait status being available.
    Eof,
}

impl fmt::Display for ExitReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExitReason::Code(c) => write!(f, "exit code {c}"),
            ExitReason::Signal(s) => write!(f, "signal {s}"),
            ExitReason::Eof => write!(f, "pty eof"),
        }
    }
}

/// Events emitted by a running terminal.
///
/// [`TerminalEvent::Output`] carries the raw, unmodified PTY bytes. It exists
/// for transcript capture (Spike B) and is intentionally *not* the rendering
/// path: rendering reads terminal state from the backend instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEvent {
    Output(Vec<u8>),
    /// Terminal state changed; a renderer should take a fresh snapshot.
    Damage,
    Title(String),
    Bell,
    Exited(ExitReason),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_validity_rejects_zero_dimensions() {
        assert!(TerminalSize::new(80, 24, 8, 16).is_valid());
        assert!(!TerminalSize::new(0, 24, 8, 16).is_valid());
        assert!(!TerminalSize::new(80, 0, 8, 16).is_valid());
    }

    #[test]
    fn size_reports_pixel_extent() {
        let size = TerminalSize::new(80, 24, 8, 16);
        assert_eq!(size.pixel_width(), 640);
        assert_eq!(size.pixel_height(), 384);
    }

    #[test]
    fn modifier_helpers_agree() {
        assert!(Modifiers::NONE.is_empty());
        assert!(!Modifiers::ctrl().is_empty());
        assert!(Modifiers::ctrl().ctrl);
    }

    #[test]
    fn key_event_builder_attaches_text() {
        let ev = KeyEvent::press(Key::Char('a'), Modifiers::NONE).with_text("a");
        assert_eq!(ev.text.as_deref(), Some("a"));
        assert_eq!(ev.action, KeyAction::Press);
    }

    #[test]
    fn exit_reason_renders_readably() {
        assert_eq!(ExitReason::Code(0).to_string(), "exit code 0");
        assert_eq!(ExitReason::Signal(2).to_string(), "signal 2");
        assert_eq!(ExitReason::Eof.to_string(), "pty eof");
    }
}
