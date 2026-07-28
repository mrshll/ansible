//! The terminal state machine.

use std::ffi::c_void;
use std::sync::mpsc::Sender;

use crate::event::TerminalSize;
use crate::sys;
use crate::{Error, Result};

use super::check;

/// Things the terminal asks the host to do while parsing bytes.
///
/// libghostty invokes these synchronously from inside
/// `ghostty_terminal_vt_write`, and explicitly forbids re-entering `vt_write`
/// from a callback. So they only ever enqueue; the caller drains afterwards.
pub struct TerminalCallbacks {
    /// Replies the terminal wants written back to the PTY (DSR, DA, XTVERSION…).
    pub write_pty: Sender<Vec<u8>>,
    /// OSC 0/2 title changes.
    pub title: Sender<String>,
    /// BEL.
    pub bell: Sender<()>,
}

/// Owns a `GhosttyTerminal` and the boxed callback state it points at.
pub struct Terminal {
    raw: sys::GhosttyTerminal,
    size: TerminalSize,
    // Kept alive because the C side holds a raw pointer to it as userdata.
    callbacks: Box<TerminalCallbacks>,
}

// SAFETY: libghostty-vt has no global mutable state; a terminal handle is
// owned exclusively by this struct and every entry point takes `&mut self`.
unsafe impl Send for Terminal {}

impl Terminal {
    pub fn new(
        size: TerminalSize,
        scrollback_rows: u32,
        callbacks: TerminalCallbacks,
    ) -> Result<Self> {
        if !size.is_valid() {
            return Err(Error::InvalidSize { cols: size.cols, rows: size.rows });
        }

        let mut raw: sys::GhosttyTerminal = std::ptr::null_mut();
        // SAFETY: `raw` is a valid out-pointer; a null allocator selects the
        // library default.
        check("ghostty_terminal_new", unsafe {
            sys::ghostty_terminal_new(std::ptr::null(), &mut raw, size.cols, size.rows)
        })?;

        let mut term = Self { raw, size, callbacks: Box::new(callbacks) };
        term.install_callbacks()?;
        term.set_scrollback(scrollback_rows)?;
        term.apply_size(size)?;
        Ok(term)
    }

    fn install_callbacks(&mut self) -> Result<()> {
        let userdata = &mut *self.callbacks as *mut TerminalCallbacks as *mut c_void;
        // SAFETY: userdata outlives the terminal (both are owned by `self` and
        // the terminal is freed first in `Drop`).
        unsafe {
            check(
                "ghostty_terminal_set(USERDATA)",
                sys::ghostty_terminal_set(self.raw, sys::GHOSTTY_TERMINAL_OPT_USERDATA, userdata),
            )?;

            check(
                "ghostty_terminal_set(WRITE_PTY)",
                sys::ghostty_terminal_set(
                    self.raw,
                    sys::GHOSTTY_TERMINAL_OPT_WRITE_PTY,
                    on_write_pty as *const c_void,
                ),
            )?;

            check(
                "ghostty_terminal_set(TITLE_CHANGED)",
                sys::ghostty_terminal_set(
                    self.raw,
                    sys::GHOSTTY_TERMINAL_OPT_TITLE_CHANGED,
                    on_title_changed as *const c_void,
                ),
            )?;

            check(
                "ghostty_terminal_set(BELL)",
                sys::ghostty_terminal_set(
                    self.raw,
                    sys::GHOSTTY_TERMINAL_OPT_BELL,
                    on_bell as *const c_void,
                ),
            )?;
        }
        Ok(())
    }

    fn set_scrollback(&mut self, rows: u32) -> Result<()> {
        let rows = rows as usize;
        // SAFETY: the option takes a pointer to a size_t.
        unsafe {
            check(
                "ghostty_terminal_set(SCROLLBACK_MAX_LINES)",
                sys::ghostty_terminal_set(
                    self.raw,
                    sys::GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES,
                    &rows as *const usize as *const c_void,
                ),
            )
        }
    }

    fn apply_size(&mut self, size: TerminalSize) -> Result<()> {
        // SAFETY: the handle is valid and the dimensions were validated.
        check("ghostty_terminal_resize", unsafe {
            sys::ghostty_terminal_resize(
                self.raw,
                size.cols,
                size.rows,
                size.cell_width_px,
                size.cell_height_px,
            )
        })
    }

    /// Feed PTY bytes through the VT parser.
    ///
    /// Documented never to fail: malformed input is logged internally rather
    /// than surfaced, because the byte stream is untrusted by definition.
    pub fn write_vt(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        // SAFETY: `data` is a valid slice for the duration of the call.
        unsafe { sys::ghostty_terminal_vt_write(self.raw, data.as_ptr(), data.len()) };
    }

    pub fn resize(&mut self, size: TerminalSize) -> Result<()> {
        if !size.is_valid() {
            return Err(Error::InvalidSize { cols: size.cols, rows: size.rows });
        }
        self.apply_size(size)?;
        self.size = size;
        Ok(())
    }

    pub fn size(&self) -> TerminalSize {
        self.size
    }

    /// Whether a DEC private mode is set. Used for bracketed paste (2004).
    pub fn mode_enabled(&mut self, mode: u16) -> bool {
        let mut enabled = false;
        // SAFETY: `enabled` is a valid out-pointer for a bool.
        let result = unsafe {
            sys::ghostty_terminal_mode_get(self.raw, mode as sys::GhosttyMode, &mut enabled)
        };
        result == sys::GHOSTTY_SUCCESS && enabled
    }

    pub(crate) fn raw(&mut self) -> sys::GhosttyTerminal {
        self.raw
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // SAFETY: the handle was created by ghostty_terminal_new and is freed
        // exactly once. The callback userdata is dropped after this returns.
        unsafe { sys::ghostty_terminal_free(self.raw) };
    }
}

/// Recover the callbacks from the userdata pointer libghostty hands back.
///
/// # Safety
/// `userdata` must be the pointer installed by `install_callbacks`.
unsafe fn callbacks<'a>(userdata: *mut c_void) -> Option<&'a TerminalCallbacks> {
    (!userdata.is_null()).then(|| unsafe { &*(userdata as *const TerminalCallbacks) })
}

unsafe extern "C" fn on_write_pty(
    _terminal: sys::GhosttyTerminal,
    userdata: *mut c_void,
    data: *const u8,
    len: usize,
) {
    let Some(cb) = (unsafe { callbacks(userdata) }) else { return };
    if data.is_null() || len == 0 {
        return;
    }
    let bytes = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
    let _ = cb.write_pty.send(bytes);
}

unsafe extern "C" fn on_title_changed(terminal: sys::GhosttyTerminal, userdata: *mut c_void) {
    let Some(cb) = (unsafe { callbacks(userdata) }) else { return };
    let mut s: sys::GhosttyString = unsafe { std::mem::zeroed() };
    let result = unsafe {
        sys::ghostty_terminal_get(
            terminal,
            sys::GHOSTTY_TERMINAL_DATA_TITLE,
            &mut s as *mut sys::GhosttyString as *mut c_void,
        )
    };
    if result != sys::GHOSTTY_SUCCESS || s.ptr.is_null() || s.len == 0 {
        return;
    }
    let bytes = unsafe { std::slice::from_raw_parts(s.ptr, s.len) };
    if let Ok(title) = std::str::from_utf8(bytes) {
        let _ = cb.title.send(title.to_string());
    }
}

unsafe extern "C" fn on_bell(_terminal: sys::GhosttyTerminal, userdata: *mut c_void) {
    if let Some(cb) = unsafe { callbacks(userdata) } {
        let _ = cb.bell.send(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{channel, Receiver};

    fn terminal(
        cols: u16,
        rows: u16,
    ) -> (Terminal, Receiver<Vec<u8>>, Receiver<String>, Receiver<()>) {
        let (wtx, wrx) = channel();
        let (ttx, trx) = channel();
        let (btx, brx) = channel();
        let term = Terminal::new(
            TerminalSize::new(cols, rows, 8, 16),
            1000,
            TerminalCallbacks { write_pty: wtx, title: ttx, bell: btx },
        )
        .expect("terminal");
        (term, wrx, trx, brx)
    }

    #[test]
    fn rejects_zero_sized_grid() {
        let (wtx, _wrx) = channel();
        let (ttx, _trx) = channel();
        let (btx, _brx) = channel();
        let result = Terminal::new(
            TerminalSize::new(0, 24, 8, 16),
            1000,
            TerminalCallbacks { write_pty: wtx, title: ttx, bell: btx },
        );
        assert!(matches!(result.err(), Some(Error::InvalidSize { .. })));
    }

    #[test]
    fn resize_updates_reported_size() {
        let (mut term, ..) = terminal(80, 24);
        assert_eq!(term.size().cols, 80);
        term.resize(TerminalSize::new(120, 40, 8, 16)).unwrap();
        assert_eq!(term.size(), TerminalSize::new(120, 40, 8, 16));
    }

    #[test]
    fn resize_rejects_zero() {
        let (mut term, ..) = terminal(80, 24);
        assert!(term.resize(TerminalSize::new(80, 0, 8, 16)).is_err());
    }

    #[test]
    fn osc_title_reaches_the_callback() {
        let (mut term, _w, titles, _b) = terminal(80, 24);
        term.write_vt(b"\x1b]0;spike-a\x07");
        assert_eq!(titles.try_recv().unwrap(), "spike-a");
    }

    #[test]
    fn bel_reaches_the_callback() {
        let (mut term, _w, _t, bells) = terminal(80, 24);
        term.write_vt(b"\x07");
        assert!(bells.try_recv().is_ok());
    }

    #[test]
    fn device_status_report_is_answered_through_write_pty() {
        let (mut term, writes, ..) = terminal(80, 24);
        // DSR cursor position request; the terminal must reply with CPR.
        term.write_vt(b"\x1b[6n");
        let reply = writes.try_recv().expect("terminal should answer DSR");
        assert!(reply.starts_with(b"\x1b["), "unexpected reply: {reply:?}");
        assert!(reply.ends_with(b"R"), "unexpected reply: {reply:?}");
    }

    #[test]
    fn bracketed_paste_mode_tracks_the_escape_sequence() {
        let (mut term, ..) = terminal(80, 24);
        assert!(!term.mode_enabled(2004));
        term.write_vt(b"\x1b[?2004h");
        assert!(term.mode_enabled(2004));
        term.write_vt(b"\x1b[?2004l");
        assert!(!term.mode_enabled(2004));
    }

    #[test]
    fn empty_write_is_a_noop() {
        let (mut term, ..) = terminal(80, 24);
        term.write_vt(b"");
    }
}
