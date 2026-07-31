//! The plugin's own configuration, and where its files live.
//!
//! Herdr injects three directories and is explicit about what belongs in each:
//! `HERDR_PLUGIN_ROOT` is a managed source checkout and must not hold anything
//! durable, `HERDR_PLUGIN_CONFIG_DIR` is for user-editable config, and
//! `HERDR_PLUGIN_STATE_DIR` is for runtime state. This module is the only place
//! that reads those variables, so a subcommand never has to guess.
//!
//! Every path also has a fallback for the case that matters during development:
//! running `ansible-herd` from a shell, outside a plugin invocation, with no
//! Herdr-injected environment at all.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::model::Share;

/// The plugin id declared in `herdr-plugin.toml`.
///
/// Herdr requires plugin-owned Agents views to be sourced as
/// `plugin:<HERDR_PLUGIN_ID>` and rejects the set when the id does not match an
/// enabled plugin, so this constant and the manifest must agree.
pub const PLUGIN_ID: &str = "ansible.herd";

/// Source identifier used for every metadata report this plugin makes.
///
/// Herdr scopes presentation expiry and sequence numbers per source, so a single
/// stable string means our tokens can never be confused with a user hook's.
pub const METADATA_SOURCE: &str = "plugin:ansible.herd";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// GitHub login. This is the identity the whole hub is keyed by.
    pub login: String,
    /// Optional friendlier name for roster rows.
    pub display_name: Option<String>,
    /// Machine name, so one person running agents on a laptop and a workbox shows
    /// up as two rows rather than one flickering between them.
    pub host: Option<String>,
    pub hub: Hub,
    pub share: ShareConfig,
    pub timing: Timing,
}

/// Where the team's presence lives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Hub {
    /// `dir` or `git`. See `hub/mod.rs` for what each can and cannot carry.
    pub kind: String,
    /// `dir` hub: a directory every member can read and write.
    pub path: Option<PathBuf>,
    /// `git` hub: the repository that carries the presence refs. Anything Git
    /// accepts — an `origin`-style remote name resolved in `repo`, or a URL.
    pub remote: Option<String>,
    /// `git` hub: a local clone to run plumbing in. Defaults to the current
    /// directory, which is the repo the pane is already sitting in.
    pub repo: Option<PathBuf>,
    /// How long without a heartbeat before a member is shown as stale.
    pub stale_after_ms: u64,
    /// How long without a heartbeat before a member is dropped from the roster.
    pub forget_after_ms: u64,
}

impl Default for Hub {
    fn default() -> Self {
        Self {
            kind: "dir".into(),
            path: None,
            remote: None,
            repo: None,
            stale_after_ms: 20_000,
            forget_after_ms: 300_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShareConfig {
    /// What a pane shares before anyone touches it. `title` by default; see
    /// [`Share`].
    pub default: String,
    /// Whether a teammate's comment may be *submitted* to the agent rather than
    /// only typed into its composer.
    ///
    /// Off by default, and it stays off unless a human edits this file. A comment
    /// that submits itself is a remote human writing a prompt to your agent; that
    /// should never become true because a default changed.
    pub allow_submit: bool,
}

impl Default for ShareConfig {
    fn default() -> Self {
        Self { default: "title".into(), allow_submit: false }
    }
}

// Every field is a duration, and the `_ms` suffix is the unit — dropping it to
// satisfy the lint would make `poll = 2000` ambiguous in a hand-edited file.
#[expect(clippy::struct_field_names, reason = "the shared suffix is the unit")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Timing {
    /// Publish at least this often even when nothing changed, so a reader can
    /// tell "quiet" from "gone".
    pub heartbeat_ms: u64,
    /// How often to read the hub.
    pub poll_ms: u64,
    /// How often to reconcile against Herdr when no event nudges us.
    pub reconcile_ms: u64,
}

impl Default for Timing {
    fn default() -> Self {
        Self { heartbeat_ms: 5_000, poll_ms: 2_000, reconcile_ms: 1_000 }
    }
}

impl Config {
    /// Read `config.toml` from the plugin config directory.
    ///
    /// A missing file is not an error — it yields defaults, so `doctor` can
    /// explain what to write instead of failing to start.
    ///
    /// # Errors
    /// When the file exists but is unreadable or not valid TOML for this schema.
    /// `deny_unknown_fields` makes a typo'd key an error rather than a setting
    /// that silently does nothing.
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join("config.toml");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => return Err(err).context(format!("reading {}", path.display())),
        };
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// The identity to publish under, and the checks that make it usable.
    ///
    /// # Errors
    /// When `login` is unset. Everything else has a defensible default; an
    /// identity does not, and guessing one would put a wrong name on a teammate's
    /// roster.
    pub fn identity(&self) -> Result<(String, String)> {
        if self.login.trim().is_empty() {
            bail!(
                "set `login` in config.toml (your GitHub login) — run `ansible-herd init` to write a starter file"
            );
        }
        Ok((self.login.trim().to_string(), self.host.clone().unwrap_or_else(hostname)))
    }

    /// The share mode a pane starts in.
    #[must_use]
    pub fn default_share(&self) -> Share {
        Share::parse(&self.share.default).unwrap_or_default()
    }
}

/// Best-effort machine name.
///
/// Read from the kernel rather than shelled out to, then from the environment,
/// and finally a constant. A wrong hostname splits one person into two roster
/// rows, which is untidy but harmless — worth a fallback chain rather than an
/// error.
#[must_use]
pub fn hostname() -> String {
    for path in ["/proc/sys/kernel/hostname", "/etc/hostname"] {
        if let Ok(text) = std::fs::read_to_string(path) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    std::env::var("HOSTNAME").ok().filter(|h| !h.is_empty()).unwrap_or_else(|| "unknown".into())
}

/// Directory layout, resolved once per process.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
}

impl Paths {
    /// Resolve from the Herdr-injected environment, with development fallbacks.
    #[must_use]
    pub fn resolve() -> Self {
        let env = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty()).map(PathBuf::from);
        let fallback = fallback_base();
        Self {
            config_dir: env("HERDR_PLUGIN_CONFIG_DIR").unwrap_or_else(|| fallback.join("config")),
            state_dir: env("HERDR_PLUGIN_STATE_DIR").unwrap_or_else(|| fallback.join("state")),
        }
    }

    /// Create both directories. Herdr creates them for a real plugin invocation;
    /// this covers the development path.
    ///
    /// # Errors
    /// When a directory cannot be created.
    pub fn ensure(&self) -> Result<()> {
        std::fs::create_dir_all(&self.config_dir)
            .with_context(|| format!("creating {}", self.config_dir.display()))?;
        std::fs::create_dir_all(&self.state_dir)
            .with_context(|| format!("creating {}", self.state_dir.display()))?;
        Ok(())
    }
}

fn fallback_base() -> PathBuf {
    let home = std::env::var("HOME").ok().filter(|h| !h.is_empty()).unwrap_or_else(|| ".".into());
    PathBuf::from(home).join(".local/share/ansible-herd")
}

/// The starter config `init` writes: every knob, with the defaults shown and the
/// two decisions a human has to make left blank.
#[must_use]
pub fn template(login: &str) -> String {
    format!(
        r#"# ansible-herd — team presence for coding agents, hosted by Herdr.
# Docs: docs/plan/herdr-plugin.md in the ansible repo.

# Your GitHub login. This is the identity the whole hub is keyed by, and the only
# path in the hub your machine is allowed to write.
login = "{login}"
# display_name = "Sam"
# host = "sams-box"        # defaults to this machine's hostname

[hub]
# "dir" — a directory every member can read and write: a shared box, an NFS or
#         SMB mount, a Syncthing folder, a Tailscale drive. Sub-second, and the
#         only backend that carries live teleport frames today.
# "git"  — presence over Git refs on a repo the team already has. No new
#          infrastructure and GitHub push access is the authorization, at the
#          cost of fetch-interval latency and no live frames.
kind = "dir"
path = "/path/to/shared/herd"

# For kind = "git":
# remote = "origin"
# repo = "/path/to/a/clone"   # defaults to the current directory

stale_after_ms = 20000
forget_after_ms = 300000

[share]
# What a pane publishes before anyone changes it: "off", "title", or "live".
# "title" is headline and status only — no terminal contents ever leave the
# machine until someone opts a pane into "live".
default = "title"
# Whether a teammate's comment may be submitted to your agent as a prompt rather
# than only typed into its composer for you to send. Leave this false unless you
# have thought about it: true means a remote human can write directly to your
# agent's input.
allow_submit = false

[timing]
heartbeat_ms = 5000
poll_ms = 2000
reconcile_ms = 1000
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_config_file_yields_defaults() {
        let dir = tempdir("cfg-missing");
        let config = Config::load(&dir).expect("missing file is not an error");
        assert_eq!(config, Config::default());
        assert_eq!(config.default_share(), Share::Title);
        assert!(!config.share.allow_submit);
    }

    #[test]
    fn the_template_parses_and_keeps_the_safe_defaults() {
        let dir = tempdir("cfg-template");
        std::fs::write(dir.join("config.toml"), template("mrshll")).expect("write");
        let config = Config::load(&dir).expect("template is valid TOML for this schema");
        assert_eq!(config.login, "mrshll");
        // The two decisions that must not drift: what a pane shares by default,
        // and whether a teammate can write to your agent.
        assert_eq!(config.default_share(), Share::Title);
        assert!(!config.share.allow_submit);
    }

    #[test]
    fn an_unknown_key_is_an_error_rather_than_a_setting_that_does_nothing() {
        let dir = tempdir("cfg-typo");
        std::fs::write(dir.join("config.toml"), "login = \"a\"\nallow_sumbit = true\n")
            .expect("write");
        let err = Config::load(&dir).expect_err("typo must not be silently ignored");
        assert!(format!("{err:#}").contains("allow_sumbit"), "got {err:#}");
    }

    #[test]
    fn an_identity_without_a_login_is_refused() {
        let config = Config::default();
        let err = config.identity().expect_err("no login");
        assert!(format!("{err}").contains("login"), "got {err}");
    }

    #[test]
    fn an_explicit_host_wins_over_the_machine_name() {
        let config =
            Config { login: "mrshll".into(), host: Some("workbox".into()), ..Config::default() };
        let (login, host) = config.identity().expect("identity");
        assert_eq!(login, "mrshll");
        assert_eq!(host, "workbox");
    }

    #[test]
    fn an_unparseable_share_mode_falls_back_to_title() {
        // A config typo must not silently escalate to `live`.
        let config = Config {
            share: ShareConfig { default: "yes please".into(), allow_submit: false },
            ..Config::default()
        };
        assert_eq!(config.default_share(), Share::Title);
    }

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ansible-herd-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }
}
