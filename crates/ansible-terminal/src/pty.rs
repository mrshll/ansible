//! PTY and child process lifecycle.
//!
//! libghostty-vt is deliberately not a process manager: it parses bytes and
//! keeps state. Spawning the shell, owning the master fd, and delivering
//! SIGWINCH are the host's job, so they live here.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use portable_pty::{Child, CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};

use crate::config::TerminalConfig;
use crate::event::{ExitReason, TerminalSize};
use crate::{Error, Result};

pub struct Pty {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    exited: Arc<AtomicBool>,
}

impl Pty {
    pub fn spawn(config: &TerminalConfig) -> Result<(Self, Box<dyn Read + Send>)> {
        if !config.size.is_valid() {
            return Err(Error::InvalidSize { cols: config.size.cols, rows: config.size.rows });
        }

        let pty_system = NativePtySystem::default();
        let pair = pty_system
            .openpty(pty_size(config.size))
            .map_err(|e| Error::Pty(format!("openpty: {e}")))?;

        let mut cmd = CommandBuilder::new(&config.command);
        cmd.args(&config.args);
        if let Some(cwd) = &config.cwd {
            cmd.cwd(cwd);
        }
        // CommandBuilder starts with an empty environment, so seed it from the
        // parent first. A terminal that dropped PATH, HOME, and the user's
        // config would not be a useful terminal.
        for (key, value) in std::env::vars() {
            cmd.env(key, value);
        }
        for (key, value) in &config.env {
            cmd.env(key, value);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| Error::Pty(format!("spawn {}: {e}", config.command)))?;
        // Closing the slave in the parent is what makes the reader see EOF when
        // the child exits; without it the read side never completes.
        drop(pair.slave);

        let reader =
            pair.master.try_clone_reader().map_err(|e| Error::Pty(format!("clone reader: {e}")))?;
        let writer =
            pair.master.take_writer().map_err(|e| Error::Pty(format!("take writer: {e}")))?;

        Ok((
            Self { master: pair.master, writer, child, exited: Arc::new(AtomicBool::new(false)) },
            reader,
        ))
    }

    pub fn write(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        self.writer.write_all(data)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Resize the PTY, which raises SIGWINCH on the foreground process group.
    pub fn resize(&mut self, size: TerminalSize) -> Result<()> {
        if !size.is_valid() {
            return Err(Error::InvalidSize { cols: size.cols, rows: size.rows });
        }
        self.master.resize(pty_size(size)).map_err(|e| Error::Pty(format!("resize: {e}")))
    }

    /// Non-blocking exit check.
    pub fn try_wait(&mut self) -> Option<ExitReason> {
        match self.child.try_wait() {
            Ok(Some(status)) => {
                self.exited.store(true, Ordering::SeqCst);
                Some(exit_reason(status.exit_code()))
            }
            _ => None,
        }
    }

    pub fn has_exited(&self) -> bool {
        self.exited.load(Ordering::SeqCst)
    }

    pub fn kill(&mut self) -> Result<()> {
        if self.has_exited() {
            return Ok(());
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.exited.store(true, Ordering::SeqCst);
        Ok(())
    }
}

fn pty_size(size: TerminalSize) -> PtySize {
    PtySize {
        rows: size.rows,
        cols: size.cols,
        pixel_width: size.pixel_width() as u16,
        pixel_height: size.pixel_height() as u16,
    }
}

/// portable-pty flattens wait status into a single code. On Unix a
/// signal-terminated child is reported as `128 + signal`, matching the shell
/// convention, so recover the signal number from that range.
fn exit_reason(code: u32) -> ExitReason {
    if (129..=192).contains(&code) {
        ExitReason::Signal((code - 128) as i32)
    } else {
        ExitReason::Code(code as i32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_codes_are_recovered() {
        assert_eq!(exit_reason(0), ExitReason::Code(0));
        assert_eq!(exit_reason(1), ExitReason::Code(1));
        assert_eq!(exit_reason(130), ExitReason::Signal(2)); // SIGINT
        assert_eq!(exit_reason(137), ExitReason::Signal(9)); // SIGKILL
    }

    #[test]
    fn pty_size_carries_pixels() {
        let s = pty_size(TerminalSize::new(80, 24, 8, 16));
        assert_eq!((s.cols, s.rows), (80, 24));
        assert_eq!((s.pixel_width, s.pixel_height), (640, 384));
    }
}
