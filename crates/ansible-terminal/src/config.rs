//! How a terminal session is launched.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::event::TerminalSize;

/// The command and environment for a terminal session.
///
/// The command is configurable rather than hardcoded to `claude` so the spike
/// can be exercised with a plain shell or a deterministic fixture on machines
/// without Claude Code credentials.
#[derive(Debug, Clone)]
pub struct TerminalConfig {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub size: TerminalSize,
    /// Scrollback rows retained by libghostty-vt.
    pub scrollback_rows: u32,
}

impl TerminalConfig {
    /// A login shell, honoring `$SHELL`.
    #[must_use]
    pub fn shell(size: TerminalSize) -> Self {
        let command = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        Self::command(command, size)
    }

    pub fn command(command: impl Into<String>, size: TerminalSize) -> Self {
        let mut env = BTreeMap::new();
        // Ghostty's own terminfo is not installed in this harness, and claiming
        // to be `xterm-ghostty` without it makes ncurses programs fall back to
        // dumb output. `xterm-256color` is present everywhere and exercises the
        // colour and box-drawing paths the spike needs.
        env.insert("TERM".to_string(), "xterm-256color".to_string());
        env.insert("COLORTERM".to_string(), "truecolor".to_string());
        Self {
            command: command.into(),
            args: Vec::new(),
            cwd: None,
            env,
            size,
            scrollback_rows: 10_000,
        }
    }

    #[must_use]
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    #[must_use]
    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size() -> TerminalSize {
        TerminalSize::new(80, 24, 8, 16)
    }

    #[test]
    fn command_config_sets_colour_capable_term() {
        let cfg = TerminalConfig::command("/bin/sh", size());
        assert_eq!(cfg.env.get("TERM").map(String::as_str), Some("xterm-256color"));
        assert_eq!(cfg.env.get("COLORTERM").map(String::as_str), Some("truecolor"));
    }

    #[test]
    fn builders_accumulate() {
        let cfg = TerminalConfig::command("/bin/sh", size())
            .arg("-c")
            .args(["echo hi"])
            .env("FOO", "bar")
            .cwd("/tmp");
        assert_eq!(cfg.args, vec!["-c", "echo hi"]);
        assert_eq!(cfg.env.get("FOO").map(String::as_str), Some("bar"));
        assert_eq!(cfg.cwd.as_deref(), Some(std::path::Path::new("/tmp")));
    }

    #[test]
    fn shell_config_defaults_to_a_real_shell() {
        let cfg = TerminalConfig::shell(size());
        assert!(!cfg.command.is_empty());
    }
}
