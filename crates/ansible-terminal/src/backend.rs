//! The host contract every terminal implementation satisfies.

use crate::Result;
use crate::event::{TerminalEvent, TerminalInput, TerminalSize};
use crate::snapshot::Snapshot;

/// Receiving end of a terminal's event stream.
///
/// Aliased so hosts can name the type without taking a direct dependency on
/// the channel crate.
pub type TerminalEvents = crossbeam_channel::Receiver<TerminalEvent>;

/// A live terminal attached to a child process.
///
/// The contract deliberately separates three concerns that Spike A needs to
/// keep apart:
///
/// * [`send`](TerminalBackend::send) and [`resize`](TerminalBackend::resize)
///   push host intent down to the child.
/// * [`events`](TerminalBackend::events) surfaces raw PTY bytes for transcript
///   capture. It is a tee, never the rendering path.
/// * [`snapshot`](TerminalBackend::snapshot) exposes terminal *state* for a
///   renderer. Renderers read this; they never parse the byte stream.
///
/// Implementations must be usable from a plain binary with no GUI toolkit and
/// no Tauri, which is what keeps the composition model swappable.
pub trait TerminalBackend: Send {
    /// Deliver input to the child process.
    ///
    /// # Errors
    /// [`crate::Error::Exited`] if the child is already gone, or a
    /// [`crate::Error::Pty`]/[`crate::Error::Io`] if the write fails.
    fn send(&mut self, input: TerminalInput) -> Result<()>;

    /// Resize the grid and raise SIGWINCH on the child.
    ///
    /// # Errors
    /// [`crate::Error::InvalidSize`] if `size` has a zero dimension, or an
    /// implementation error if the terminal state or the PTY rejects the resize.
    fn resize(&mut self, size: TerminalSize) -> Result<()>;

    /// Current grid geometry.
    fn size(&self) -> TerminalSize;

    /// Stream of output/state events. Cloning the receiver is the caller's job.
    fn events(&self) -> TerminalEvents;

    /// Copy the visible screen into a renderable snapshot.
    ///
    /// # Errors
    /// An implementation error if the terminal state cannot be read.
    fn snapshot(&mut self) -> Result<Snapshot>;

    /// Whether the child has exited.
    fn has_exited(&self) -> bool;

    /// Terminate the child and release resources. Idempotent.
    ///
    /// # Errors
    /// An implementation error if resources cannot be released. Killing the
    /// child itself is infallible: a child that is already gone is success.
    fn shutdown(&mut self) -> Result<()>;
}
