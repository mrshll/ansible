//! Host-facing terminal contract for the Spike A libghostty integration.
//!
//! This crate intentionally has no Tauri or coordination-layer dependencies.
//! A libghostty adapter owns rendering and PTY details; the desktop host owns
//! lifecycle and forwards typed input without depending on adapter internals.

use std::fmt;

/// Dimensions shared by the renderer and its PTY.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalSize {
    pub columns: u16,
    pub rows: u16,
    pub pixel_width: u32,
    pub pixel_height: u32,
}

impl TerminalSize {
    /// Constructs a usable terminal size.
    ///
    /// Pixel dimensions may be zero when the host cannot report them, but a
    /// terminal must contain at least one row and column.
    ///
    /// # Errors
    ///
    /// Returns [`TerminalSizeError`] when `columns` or `rows` is zero.
    pub fn new(
        columns: u16,
        rows: u16,
        pixel_width: u32,
        pixel_height: u32,
    ) -> Result<Self, TerminalSizeError> {
        if columns == 0 || rows == 0 {
            return Err(TerminalSizeError);
        }

        Ok(Self {
            columns,
            rows,
            pixel_width,
            pixel_height,
        })
    }
}

/// Returned when a host reports a terminal without cells.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalSizeError;

impl fmt::Display for TerminalSizeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("terminal rows and columns must be non-zero")
    }
}

impl std::error::Error for TerminalSizeError {}

/// Input that the desktop host forwards to the active terminal adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminalInput<'a> {
    /// Already-encoded terminal input, including control sequences.
    Bytes(&'a [u8]),
    /// Pasted UTF-8 text. The adapter decides whether to use bracketed paste.
    Paste(&'a str),
    /// A host resize, which must resize rendering and signal the PTY.
    Resize(TerminalSize),
}

/// State changes surfaced to the desktop without exposing adapter internals.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalState {
    Starting,
    Running,
    Exited(i32),
    Failed,
}

/// Notifications emitted by an adapter for host UI and capture wiring.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminalEvent<'a> {
    StateChanged(TerminalState),
    /// PTY bytes are exposed for capture, not for webview rendering.
    Output(&'a [u8]),
    TitleChanged(&'a str),
    Bell,
}

/// Callback target implemented by the desktop host.
pub trait TerminalEventSink {
    fn handle_event(&mut self, event: TerminalEvent<'_>);
}

/// Rendering and PTY boundary implemented by libghostty.
pub trait TerminalBackend {
    type Error: std::error::Error + Send + Sync + 'static;

    /// Starts a command and binds rendering to the adapter's native surface.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when the surface or child process cannot start.
    fn start(
        &mut self,
        command: &TerminalCommand,
        initial_size: TerminalSize,
        event_sink: &mut dyn TerminalEventSink,
    ) -> Result<(), Self::Error>;

    /// Forwards input to the running terminal.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when input cannot be delivered.
    fn send_input(&mut self, input: TerminalInput<'_>) -> Result<(), Self::Error>;

    /// Stops the child process and releases renderer resources.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when shutdown cannot complete cleanly.
    fn shutdown(&mut self) -> Result<(), Self::Error>;
}

/// Command launched inside the adapter-owned PTY.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalCommand {
    pub program: String,
    pub arguments: Vec<String>,
    pub working_directory: Option<String>,
}

impl TerminalCommand {
    #[must_use]
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            arguments: Vec::new(),
            working_directory: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{TerminalCommand, TerminalSize};

    #[test]
    fn terminal_size_rejects_empty_cell_dimensions() {
        assert!(TerminalSize::new(0, 24, 800, 600).is_err());
        assert!(TerminalSize::new(80, 0, 800, 600).is_err());
    }

    #[test]
    fn terminal_size_allows_unknown_pixel_dimensions() {
        let terminal_size = TerminalSize::new(80, 24, 0, 0).unwrap();

        assert_eq!(terminal_size.columns, 80);
        assert_eq!(terminal_size.rows, 24);
    }

    #[test]
    fn terminal_command_starts_without_optional_configuration() {
        let command = TerminalCommand::new("claude");

        assert_eq!(command.program, "claude");
        assert!(command.arguments.is_empty());
        assert!(command.working_directory.is_none());
    }
}
