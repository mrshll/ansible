//! [`TerminalBackend`] implemented over libghostty-vt and a real PTY.

use std::io::Read;
use std::sync::mpsc::{Receiver as StdReceiver, channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, bounded};

use crate::backend::{TerminalBackend, TerminalEvents};
use crate::config::TerminalConfig;
use crate::event::{ExitReason, TerminalEvent, TerminalInput, TerminalSize};
use crate::pty::Pty;
use crate::snapshot::Snapshot;
use crate::vt::{KeyEncoder, RenderState, Terminal, TerminalCallbacks, keys};
use crate::{Error, Result};

/// How many read chunks may queue before the reader thread blocks.
///
/// Bounded on purpose: an unbounded queue turns a slow consumer into unbounded
/// memory growth under high-volume output, which is exactly the case the spike
/// has to survive.
const OUTPUT_QUEUE_DEPTH: usize = 1024;

/// Read buffer size. 64 KiB matches the chunk size the capture path will use.
const READ_CHUNK: usize = 64 * 1024;

/// Poll gap used by the blocking helpers. Short enough not to distort latency
/// measurement, long enough not to spin a core.
const POLL_INTERVAL: Duration = Duration::from_millis(1);

pub struct GhosttyTerminal {
    terminal: Terminal,
    render: RenderState,
    encoder: KeyEncoder,
    pty: Pty,

    /// Raw PTY bytes, tee'd to the host for transcript capture.
    events_tx: Sender<TerminalEvent>,
    events_rx: Receiver<TerminalEvent>,

    /// Bytes arriving from the reader thread, still unparsed.
    incoming: Receiver<ReadResult>,
    reader: Option<JoinHandle<()>>,

    /// Raw bytes the tee could not hand over in time. Non-zero means the
    /// transcript has a gap, which callers must be able to detect.
    dropped_output_bytes: u64,

    /// Replies libghostty wants written back (DSR, DA, XTVERSION…).
    write_pty: StdReceiver<Vec<u8>>,
    titles: StdReceiver<String>,
    bells: StdReceiver<()>,

    exited: bool,
}

enum ReadResult {
    Data(Vec<u8>),
    Eof,
}

impl GhosttyTerminal {
    /// Spawn the child under a new PTY and start the reader thread.
    ///
    /// # Errors
    /// [`Error::InvalidSize`] if `config.size` has a zero dimension,
    /// [`Error::Pty`] if the PTY cannot be opened or the command cannot be
    /// spawned, [`Error::Vt`] if libghostty fails to allocate the terminal,
    /// render, or encoder state, or [`Error::Io`] if the reader thread cannot
    /// be started.
    pub fn spawn(config: &TerminalConfig) -> Result<Self> {
        let (write_pty_tx, write_pty) = channel();
        let (title_tx, titles) = channel();
        let (bell_tx, bells) = channel();

        let terminal = Terminal::new(
            config.size,
            config.scrollback_rows,
            TerminalCallbacks { write_pty: write_pty_tx, title: title_tx, bell: bell_tx },
        )?;

        let (pty, mut reader) = Pty::spawn(config)?;
        let (incoming_tx, incoming) = bounded(OUTPUT_QUEUE_DEPTH);

        let handle = thread::Builder::new().name("ansible-pty-read".into()).spawn(move || {
            let mut buf = vec![0u8; READ_CHUNK];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        let _ = incoming_tx.send(ReadResult::Eof);
                        return;
                    }
                    Ok(n) => {
                        if incoming_tx.send(ReadResult::Data(buf[..n].to_vec())).is_err() {
                            return;
                        }
                    }
                }
            }
        })?;

        let (events_tx, events_rx) = bounded(OUTPUT_QUEUE_DEPTH);

        Ok(Self {
            terminal,
            render: RenderState::new()?,
            encoder: KeyEncoder::new()?,
            pty,
            events_tx,
            events_rx,
            dropped_output_bytes: 0,
            incoming,
            reader: Some(handle),
            write_pty,
            titles,
            bells,
            exited: false,
        })
    }

    /// Drain pending PTY output into the terminal and emit host events.
    ///
    /// Returns the number of byte chunks consumed. Callers drive this from
    /// their own loop: a GUI from a frame tick, a test from a poll loop.
    ///
    /// # Errors
    /// [`Error::Pty`] or [`Error::Io`] if a reply to a terminal query cannot be
    /// written back to the PTY. Malformed output never errors: the VT parser
    /// treats the byte stream as untrusted by definition.
    pub fn pump(&mut self) -> Result<usize> {
        let mut chunks = 0;
        let mut damaged = false;

        while let Ok(msg) = self.incoming.try_recv() {
            match msg {
                ReadResult::Data(bytes) => {
                    // Tee the raw bytes before parsing, so capture sees exactly
                    // what arrived.
                    //
                    // The send is non-blocking on purpose. Waiting for a slow
                    // transcript consumer would stall rendering and input,
                    // which is a worse failure than a transcript gap — but the
                    // gap must never be silent, so undelivered bytes are
                    // counted and exposed via `dropped_output_bytes`.
                    let len = bytes.len() as u64;
                    if self.events_tx.try_send(TerminalEvent::Output(bytes.clone())).is_err() {
                        self.dropped_output_bytes += len;
                    }
                    self.terminal.write_vt(&bytes);
                    damaged = true;
                    chunks += 1;
                }
                ReadResult::Eof => {
                    self.finish();
                    return Ok(chunks);
                }
            }
        }

        // Answer terminal queries. Collected first so the borrow on the
        // receiver ends before writing to the PTY.
        let replies: Vec<Vec<u8>> = self.write_pty.try_iter().collect();
        for reply in replies {
            self.pty.write(&reply)?;
        }

        while let Ok(title) = self.titles.try_recv() {
            let _ = self.events_tx.try_send(TerminalEvent::Title(title));
        }
        while self.bells.try_recv().is_ok() {
            let _ = self.events_tx.try_send(TerminalEvent::Bell);
        }

        if damaged {
            let _ = self.events_tx.try_send(TerminalEvent::Damage);
        }

        // A child can exit without closing the PTY if it forked; check too.
        if !self.exited {
            if let Some(reason) = self.pty.try_wait() {
                self.exited = true;
                let _ = self.events_tx.try_send(TerminalEvent::Exited(reason));
            }
        }

        Ok(chunks)
    }

    /// Pump until `ready` holds or `timeout` elapses. Returns whether it held.
    ///
    /// A PTY gives no completion signal, so anything that waits on child output
    /// has to poll. Hosts with an event loop call [`pump`](Self::pump) from a
    /// frame tick instead; this exists for headless drivers and tests.
    ///
    /// # Errors
    /// Whatever [`pump`](Self::pump) returns; the loop stops at the first error
    /// rather than spinning to the deadline.
    pub fn pump_until(
        &mut self,
        timeout: Duration,
        mut ready: impl FnMut(&mut Self) -> bool,
    ) -> Result<bool> {
        let deadline = Instant::now() + timeout;
        loop {
            self.pump()?;
            if ready(self) {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    /// Pump until the visible screen satisfies `matches`.
    ///
    /// # Errors
    /// Whatever [`pump`](Self::pump) returns. A snapshot that fails to render is
    /// treated as "not matching yet" rather than as an error, so a transient
    /// render failure does not abort the wait.
    pub fn wait_for_screen(
        &mut self,
        timeout: Duration,
        matches: impl Fn(&Snapshot) -> bool,
    ) -> Result<bool> {
        self.pump_until(timeout, |term| term.snapshot().is_ok_and(|s| matches(&s)))
    }

    fn finish(&mut self) {
        if self.exited {
            return;
        }
        self.exited = true;
        let reason = self.pty.try_wait().unwrap_or(ExitReason::Eof);
        let _ = self.events_tx.try_send(TerminalEvent::Exited(reason));
    }

    /// Raw PTY bytes that never reached [`TerminalEvent::Output`].
    ///
    /// Always zero while a consumer drains [`events`](Self::events) promptly.
    /// Any non-zero value means the transcript for this session has a gap and
    /// cannot be treated as byte-exact. Callers that need byte-exactness must
    /// check this rather than assume delivery.
    #[must_use]
    pub fn dropped_output_bytes(&self) -> u64 {
        self.dropped_output_bytes
    }

    /// Bytes a given input would put on the wire, without sending them.
    /// Exposed for tests and for latency measurement.
    ///
    /// # Errors
    /// [`Error::Vt`] if libghostty fails to encode a key event. The text, raw,
    /// paste, and focus forms cannot fail.
    pub fn encode(&mut self, input: &TerminalInput) -> Result<Vec<u8>> {
        Ok(match input {
            TerminalInput::Key(key) => self.encoder.encode(&mut self.terminal, key)?,
            TerminalInput::Text(text) => text.as_bytes().to_vec(),
            TerminalInput::Raw(bytes) => bytes.clone(),
            TerminalInput::Paste(text) => {
                if self.terminal.mode_enabled(keys::BRACKETED_PASTE_MODE) {
                    keys::bracket_paste(text)
                } else {
                    text.as_bytes().to_vec()
                }
            }
            TerminalInput::Focus(focused) => {
                keys::encode_focus(&mut self.terminal, *focused).unwrap_or_default()
            }
        })
    }
}

impl TerminalBackend for GhosttyTerminal {
    fn send(&mut self, input: TerminalInput) -> Result<()> {
        if self.exited {
            return Err(Error::Exited);
        }
        let bytes = self.encode(&input)?;
        self.pty.write(&bytes)
    }

    fn resize(&mut self, size: TerminalSize) -> Result<()> {
        // Terminal first so its reflow is done before the child is told to
        // redraw; the other order makes applications paint at the old size.
        self.terminal.resize(size)?;
        self.pty.resize(size)
    }

    fn size(&self) -> TerminalSize {
        self.terminal.size()
    }

    fn events(&self) -> TerminalEvents {
        self.events_rx.clone()
    }

    fn snapshot(&mut self) -> Result<Snapshot> {
        self.render.snapshot(&mut self.terminal)
    }

    fn has_exited(&self) -> bool {
        self.exited
    }

    fn shutdown(&mut self) -> Result<()> {
        self.pty.kill();
        self.exited = true;

        // Detach rather than join. The reader thread can be parked in read() on
        // the master fd — which killing the child does not wake — or blocked
        // sending into a full queue, so joining here would hang. Replacing the
        // receiver disconnects the channel so a blocked send returns at once,
        // and the read side unblocks when the Pty drops the master fd.
        let (_tx, disconnected) = bounded(0);
        self.incoming = disconnected;
        self.reader.take();
        Ok(())
    }
}

impl Drop for GhosttyTerminal {
    fn drop(&mut self) {
        self.pty.kill();
    }
}
