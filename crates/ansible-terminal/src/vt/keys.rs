//! Key events encoded into terminal byte sequences.
//!
//! Encoding is libghostty's job, not ours: the correct bytes for a keypress
//! depend on the Kitty keyboard protocol flags, DECCKM, and modify-other-keys
//! state that the *application* has negotiated. `setopt_from_terminal` copies
//! that live state into the encoder before each encode.

use crate::event::{Key, KeyAction, KeyEvent, Modifiers};
use crate::sys;
use crate::Result;

use super::{check, Terminal};

pub struct KeyEncoder {
    raw: sys::GhosttyKeyEncoder,
    event: sys::GhosttyKeyEvent,
}

// SAFETY: both handles are owned exclusively by this struct.
unsafe impl Send for KeyEncoder {}

impl KeyEncoder {
    pub fn new() -> Result<Self> {
        let mut raw: sys::GhosttyKeyEncoder = std::ptr::null_mut();
        // SAFETY: valid out-pointer; null allocator selects the default.
        check("ghostty_key_encoder_new", unsafe {
            sys::ghostty_key_encoder_new(std::ptr::null(), &mut raw)
        })?;

        let mut event: sys::GhosttyKeyEvent = std::ptr::null_mut();
        check("ghostty_key_event_new", unsafe {
            sys::ghostty_key_event_new(std::ptr::null(), &mut event)
        })?;

        Ok(Self { raw, event })
    }

    /// Encode one key press into the bytes to write to the PTY.
    ///
    /// Returns an empty vector for keys the terminal maps to nothing, which is
    /// normal (e.g. a bare modifier, or a release when the app has not asked
    /// for release events).
    pub fn encode(&mut self, terminal: &mut Terminal, key: &KeyEvent) -> Result<Vec<u8>> {
        // SAFETY: both handles are valid; this only copies mode flags.
        unsafe { sys::ghostty_key_encoder_setopt_from_terminal(self.raw, terminal.raw()) };

        let action = match key.action {
            KeyAction::Press => sys::GHOSTTY_KEY_ACTION_PRESS,
            KeyAction::Repeat => sys::GHOSTTY_KEY_ACTION_REPEAT,
            KeyAction::Release => sys::GHOSTTY_KEY_ACTION_RELEASE,
        };

        // SAFETY: `self.event` is a valid key event handle.
        unsafe {
            sys::ghostty_key_event_set_action(self.event, action);
            sys::ghostty_key_event_set_key(self.event, ghostty_key(key.key));
            sys::ghostty_key_event_set_mods(self.event, mods_bits(key.mods));
            sys::ghostty_key_event_set_consumed_mods(self.event, 0);
            sys::ghostty_key_event_set_composing(self.event, false);

            if let Key::Char(c) = key.key {
                sys::ghostty_key_event_set_unshifted_codepoint(self.event, c as u32);
            } else {
                sys::ghostty_key_event_set_unshifted_codepoint(self.event, 0);
            }

            match key.text.as_deref() {
                Some(text) if !text.is_empty() => sys::ghostty_key_event_set_utf8(
                    self.event,
                    text.as_ptr() as *const _,
                    text.len(),
                ),
                _ => sys::ghostty_key_event_set_utf8(self.event, std::ptr::null(), 0),
            }
        }

        let mut buf = [0u8; 128];
        let mut len: usize = 0;
        // SAFETY: `buf` is a valid writable region of `buf.len()` bytes.
        check("ghostty_key_encoder_encode", unsafe {
            sys::ghostty_key_encoder_encode(
                self.raw,
                self.event,
                buf.as_mut_ptr() as *mut _,
                buf.len(),
                &mut len,
            )
        })?;

        Ok(buf[..len.min(buf.len())].to_vec())
    }
}

impl Drop for KeyEncoder {
    fn drop(&mut self) {
        // SAFETY: each handle was created by its matching `_new` and freed once.
        unsafe {
            sys::ghostty_key_event_free(self.event);
            sys::ghostty_key_encoder_free(self.raw);
        }
    }
}

fn mods_bits(mods: Modifiers) -> sys::GhosttyMods {
    let mut bits = 0u32;
    if mods.shift {
        bits |= sys::GHOSTTY_MODS_SHIFT;
    }
    if mods.ctrl {
        bits |= sys::GHOSTTY_MODS_CTRL;
    }
    if mods.alt {
        bits |= sys::GHOSTTY_MODS_ALT;
    }
    if mods.super_ {
        bits |= sys::GHOSTTY_MODS_SUPER;
    }
    bits as sys::GhosttyMods
}

/// Map a logical key onto a `GHOSTTY_KEY_*` constant.
///
/// Only physical keys that exist upstream are mapped. Anything else resolves to
/// `UNIDENTIFIED`, where libghostty falls back to the event's UTF-8 text — the
/// correct behavior for layouts we do not enumerate.
fn ghostty_key(key: Key) -> sys::GhosttyKey {
    match key {
        Key::Enter => sys::GHOSTTY_KEY_ENTER,
        Key::Tab => sys::GHOSTTY_KEY_TAB,
        Key::Backspace => sys::GHOSTTY_KEY_BACKSPACE,
        Key::Escape => sys::GHOSTTY_KEY_ESCAPE,
        Key::Up => sys::GHOSTTY_KEY_ARROW_UP,
        Key::Down => sys::GHOSTTY_KEY_ARROW_DOWN,
        Key::Left => sys::GHOSTTY_KEY_ARROW_LEFT,
        Key::Right => sys::GHOSTTY_KEY_ARROW_RIGHT,
        Key::Home => sys::GHOSTTY_KEY_HOME,
        Key::End => sys::GHOSTTY_KEY_END,
        Key::PageUp => sys::GHOSTTY_KEY_PAGE_UP,
        Key::PageDown => sys::GHOSTTY_KEY_PAGE_DOWN,
        Key::Delete => sys::GHOSTTY_KEY_DELETE,
        Key::Insert => sys::GHOSTTY_KEY_INSERT,
        Key::F(n) => function_key(n),
        Key::Char(c) => char_key(c),
    }
}

fn function_key(n: u8) -> sys::GhosttyKey {
    match n {
        1 => sys::GHOSTTY_KEY_F1,
        2 => sys::GHOSTTY_KEY_F2,
        3 => sys::GHOSTTY_KEY_F3,
        4 => sys::GHOSTTY_KEY_F4,
        5 => sys::GHOSTTY_KEY_F5,
        6 => sys::GHOSTTY_KEY_F6,
        7 => sys::GHOSTTY_KEY_F7,
        8 => sys::GHOSTTY_KEY_F8,
        9 => sys::GHOSTTY_KEY_F9,
        10 => sys::GHOSTTY_KEY_F10,
        11 => sys::GHOSTTY_KEY_F11,
        12 => sys::GHOSTTY_KEY_F12,
        _ => sys::GHOSTTY_KEY_UNIDENTIFIED,
    }
}

fn char_key(c: char) -> sys::GhosttyKey {
    match c.to_ascii_lowercase() {
        'a' => sys::GHOSTTY_KEY_A,
        'b' => sys::GHOSTTY_KEY_B,
        'c' => sys::GHOSTTY_KEY_C,
        'd' => sys::GHOSTTY_KEY_D,
        'e' => sys::GHOSTTY_KEY_E,
        'f' => sys::GHOSTTY_KEY_F,
        'g' => sys::GHOSTTY_KEY_G,
        'h' => sys::GHOSTTY_KEY_H,
        'i' => sys::GHOSTTY_KEY_I,
        'j' => sys::GHOSTTY_KEY_J,
        'k' => sys::GHOSTTY_KEY_K,
        'l' => sys::GHOSTTY_KEY_L,
        'm' => sys::GHOSTTY_KEY_M,
        'n' => sys::GHOSTTY_KEY_N,
        'o' => sys::GHOSTTY_KEY_O,
        'p' => sys::GHOSTTY_KEY_P,
        'q' => sys::GHOSTTY_KEY_Q,
        'r' => sys::GHOSTTY_KEY_R,
        's' => sys::GHOSTTY_KEY_S,
        't' => sys::GHOSTTY_KEY_T,
        'u' => sys::GHOSTTY_KEY_U,
        'v' => sys::GHOSTTY_KEY_V,
        'w' => sys::GHOSTTY_KEY_W,
        'x' => sys::GHOSTTY_KEY_X,
        'y' => sys::GHOSTTY_KEY_Y,
        'z' => sys::GHOSTTY_KEY_Z,
        '0' => sys::GHOSTTY_KEY_DIGIT_0,
        '1' => sys::GHOSTTY_KEY_DIGIT_1,
        '2' => sys::GHOSTTY_KEY_DIGIT_2,
        '3' => sys::GHOSTTY_KEY_DIGIT_3,
        '4' => sys::GHOSTTY_KEY_DIGIT_4,
        '5' => sys::GHOSTTY_KEY_DIGIT_5,
        '6' => sys::GHOSTTY_KEY_DIGIT_6,
        '7' => sys::GHOSTTY_KEY_DIGIT_7,
        '8' => sys::GHOSTTY_KEY_DIGIT_8,
        '9' => sys::GHOSTTY_KEY_DIGIT_9,
        ' ' => sys::GHOSTTY_KEY_SPACE,
        '-' => sys::GHOSTTY_KEY_MINUS,
        '=' => sys::GHOSTTY_KEY_EQUAL,
        '[' => sys::GHOSTTY_KEY_BRACKET_LEFT,
        ']' => sys::GHOSTTY_KEY_BRACKET_RIGHT,
        '\\' => sys::GHOSTTY_KEY_BACKSLASH,
        ';' => sys::GHOSTTY_KEY_SEMICOLON,
        '\'' => sys::GHOSTTY_KEY_QUOTE,
        ',' => sys::GHOSTTY_KEY_COMMA,
        '.' => sys::GHOSTTY_KEY_PERIOD,
        '/' => sys::GHOSTTY_KEY_SLASH,
        '`' => sys::GHOSTTY_KEY_BACKQUOTE,
        _ => sys::GHOSTTY_KEY_UNIDENTIFIED,
    }
}

/// Wrap text in bracketed-paste markers.
///
/// Callers must only do this when the application enabled mode 2004; sending
/// the markers to an application that did not ask for them makes them appear
/// as literal `[200~` text.
pub fn bracket_paste(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + 12);
    out.extend_from_slice(b"\x1b[200~");
    out.extend_from_slice(text.as_bytes());
    out.extend_from_slice(b"\x1b[201~");
    out
}

/// Encode a focus in/out event, but only when the application enabled focus
/// reporting (mode 1004). Unsolicited `CSI I` would show up as stray input.
pub fn encode_focus(terminal: &mut Terminal, focused: bool) -> Option<Vec<u8>> {
    if !terminal.mode_enabled(FOCUS_REPORTING_MODE) {
        return None;
    }
    let event = if focused { sys::GHOSTTY_FOCUS_GAINED } else { sys::GHOSTTY_FOCUS_LOST };
    let mut buf = [0u8; 16];
    let mut len: usize = 0;
    // SAFETY: `buf` is a valid writable region of `buf.len()` bytes.
    let result = unsafe {
        sys::ghostty_focus_encode(event, buf.as_mut_ptr() as *mut _, buf.len(), &mut len)
    };
    (result == sys::GHOSTTY_SUCCESS && len > 0 && len <= buf.len()).then(|| buf[..len].to_vec())
}

/// DEC private mode 1004: focus in/out reporting.
pub const FOCUS_REPORTING_MODE: u16 = 1004;

/// DEC private mode 2004: bracketed paste.
pub const BRACKETED_PASTE_MODE: u16 = 2004;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::TerminalSize;
    use crate::vt::TerminalCallbacks;
    use std::sync::mpsc::channel;

    fn terminal() -> Terminal {
        let (write_pty, a) = channel();
        let (title, b) = channel();
        let (bell, c) = channel();
        std::mem::forget((a, b, c));
        Terminal::new(
            TerminalSize::new(80, 24, 8, 16),
            1000,
            TerminalCallbacks { write_pty, title, bell },
        )
        .expect("terminal")
    }

    fn encode(term: &mut Terminal, key: Key, mods: Modifiers) -> Vec<u8> {
        KeyEncoder::new()
            .expect("encoder")
            .encode(term, &KeyEvent::press(key, mods))
            .expect("encode")
    }

    fn encode_text(term: &mut Terminal, key: Key, mods: Modifiers, text: &str) -> Vec<u8> {
        KeyEncoder::new()
            .expect("encoder")
            .encode(term, &KeyEvent::press(key, mods).with_text(text))
            .expect("encode")
    }

    #[test]
    fn plain_letter_encodes_to_itself() {
        let mut term = terminal();
        assert_eq!(encode_text(&mut term, Key::Char('a'), Modifiers::NONE, "a"), b"a");
    }

    #[test]
    fn ctrl_c_encodes_to_the_interrupt_byte() {
        let mut term = terminal();
        assert_eq!(encode(&mut term, Key::Char('c'), Modifiers::ctrl()), vec![0x03]);
    }

    #[test]
    fn ctrl_d_encodes_to_eof() {
        let mut term = terminal();
        assert_eq!(encode(&mut term, Key::Char('d'), Modifiers::ctrl()), vec![0x04]);
    }

    #[test]
    fn enter_encodes_to_carriage_return() {
        let mut term = terminal();
        assert_eq!(encode(&mut term, Key::Enter, Modifiers::NONE), b"\r");
    }

    #[test]
    fn tab_and_escape_encode_to_control_bytes() {
        let mut term = terminal();
        assert_eq!(encode(&mut term, Key::Tab, Modifiers::NONE), b"\t");
        assert_eq!(encode(&mut term, Key::Escape, Modifiers::NONE), b"\x1b");
    }

    #[test]
    fn arrows_use_normal_cursor_keys_by_default() {
        let mut term = terminal();
        assert_eq!(encode(&mut term, Key::Up, Modifiers::NONE), b"\x1b[A");
        assert_eq!(encode(&mut term, Key::Down, Modifiers::NONE), b"\x1b[B");
        assert_eq!(encode(&mut term, Key::Right, Modifiers::NONE), b"\x1b[C");
        assert_eq!(encode(&mut term, Key::Left, Modifiers::NONE), b"\x1b[D");
    }

    #[test]
    fn arrows_switch_to_application_mode_when_the_app_asks() {
        let mut term = terminal();
        // DECCKM. This is exactly the state a caller could not track alone,
        // and the reason encoding belongs to libghostty.
        term.write_vt(b"\x1b[?1h");
        assert_eq!(encode(&mut term, Key::Up, Modifiers::NONE), b"\x1bOA");
    }

    #[test]
    fn alt_prefixes_escape() {
        let mut term = terminal();
        let out =
            encode_text(&mut term, Key::Char('b'), Modifiers { alt: true, ..Modifiers::NONE }, "b");
        assert_eq!(out, b"\x1bb");
    }

    /// The encoder must follow the Kitty keyboard protocol once an application
    /// negotiates it. Claude Code does, so getting this wrong would mangle
    /// every keystroke — and it is precisely what a hand-rolled key map misses.
    #[test]
    fn kitty_keyboard_protocol_disambiguates_control_keys() {
        let mut term = terminal();
        assert_eq!(encode(&mut term, Key::Escape, Modifiers::NONE), b"\x1b");
        assert_eq!(encode(&mut term, Key::Char('c'), Modifiers::ctrl()), vec![0x03]);

        term.write_vt(b"\x1b[>1u"); // push flags: disambiguate escape codes

        assert_eq!(
            encode(&mut term, Key::Escape, Modifiers::NONE),
            b"\x1b[27u",
            "escape must become CSI 27 u under the Kitty protocol"
        );
        assert_eq!(
            encode(&mut term, Key::Char('c'), Modifiers::ctrl()),
            b"\x1b[99;5u",
            "ctrl-c must become CSI 99;5 u under the Kitty protocol"
        );
        // Unambiguous keys are deliberately left alone by the protocol.
        assert_eq!(encode_text(&mut term, Key::Char('a'), Modifiers::NONE, "a"), b"a");
    }

    #[test]
    fn function_keys_encode() {
        let mut term = terminal();
        assert!(!encode(&mut term, Key::F(1), Modifiers::NONE).is_empty());
        assert!(!encode(&mut term, Key::F(12), Modifiers::NONE).is_empty());
    }

    #[test]
    fn bracketed_paste_wraps_the_payload() {
        assert_eq!(bracket_paste("hi"), b"\x1b[200~hi\x1b[201~".to_vec());
    }

    #[test]
    fn focus_events_only_encode_when_requested() {
        let mut term = terminal();
        assert!(encode_focus(&mut term, true).is_none(), "focus reporting is off by default");
        term.write_vt(b"\x1b[?1004h");
        assert_eq!(encode_focus(&mut term, true).as_deref(), Some(&b"\x1b[I"[..]));
        assert_eq!(encode_focus(&mut term, false).as_deref(), Some(&b"\x1b[O"[..]));
    }
}
