//! `ansible-herd` — team presence for coding agents, hosted by Herdr.
//!
//! One binary with several entrypoints, because that is the shape a Herdr plugin
//! takes: the manifest declares startup hooks, actions, and panes, and each is an
//! argv command. See `plugins/herdr-presence/herdr-plugin.toml` for which
//! subcommand each one runs, and `docs/plan/herdr-plugin.md` for why the design
//! looks like this.
//!
//! The subcommands are listed in [`USAGE`].

mod clock;
mod config;
mod daemon;
mod herdr;
mod hub;
mod model;
mod roster;
mod state;
mod teleport;

use std::io::Write;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};

use crate::clock::now_ms;
use crate::config::{Config, METADATA_SOURCE, PLUGIN_ID, Paths};
use crate::hub::Hub;
use crate::model::{HelpWanted, MessageKind, Share, split_key};
use crate::roster::Row;
use crate::state::Store;

/// How often a viewer refreshes its watch lease. Comfortably inside
/// [`state::WATCH_LEASE_MS`] so a slow poll never looks like a closed pane.
const LEASE_REFRESH_MS: u64 = 3_000;

// If these ever cross, an open viewer pane would look closed to the owner, who
// would stop publishing to a watcher who is still there.
const _: () = assert!(LEASE_REFRESH_MS * 2 < state::WATCH_LEASE_MS);

const USAGE: &str = "\
ansible-herd — team presence for coding agents, hosted by Herdr

  init                     write a starter config
  doctor                   explain what is and is not working
  startup                  ensure the daemon is running (idempotent)
  daemon [--once]          the reconcile loop
  roster                   the herd, interactive
  status [options]         set what you are working on, or raise a hand
    --headline TEXT        what this machine is working on
    --clear-headline       go back to the terminal title
    --help-wanted TEXT     raise a hand, with a note
    --no-help              lower it
    --share off|title|live what this pane publishes (needs HERDR_PANE_ID)
  watch <n|key>            teleport into a session
  comment <n|key> TEXT     send a comment, optionally --line N
  nudge <n|key>            'look at this', with no body
  inbox [n [--submit|--dismiss]]
                           read what teammates sent, or act on one
  ask                      prompt for a note and raise a hand (a popup pane)
  open roster|teleport|ask open one of this plugin's panes in Herdr
  focus | unfocus          float attention to the top of Herdr's Agents view
  demo [login]             publish a synthetic teammate into the hub
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("ansible-herd: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<()> {
    let (command, rest) = args.split_first().map_or(("help", &[][..]), |(c, r)| (c.as_str(), r));
    let paths = Paths::resolve();
    paths.ensure()?;

    match command {
        "init" => cmd_init(&paths),
        "doctor" => cmd_doctor(&paths),
        "startup" => cmd_startup(&paths),
        "daemon" => cmd_daemon(&paths, rest.iter().any(|a| a == "--once")),
        "status" => cmd_status(&paths, rest),
        "roster" => cmd_roster(&paths),
        "watch" => cmd_watch(&paths, rest),
        "comment" => cmd_message(&paths, rest, MessageKind::Comment),
        "nudge" => cmd_message(&paths, rest, MessageKind::Nudge),
        "inbox" => cmd_inbox(&paths, rest),
        "focus" => cmd_view(true),
        "unfocus" => cmd_view(false),
        "ask" => cmd_ask(&paths),
        "open" => cmd_open(rest.first().context("open <roster|teleport|ask>")?),
        "demo" => cmd_demo(&paths, rest.first().map_or("robin", String::as_str)),
        "help" | "--help" | "-h" => {
            print!("{USAGE}");
            Ok(())
        }
        other => bail!("unknown command {other:?}\n\n{USAGE}"),
    }
}

// ---------------------------------------------------------------- setup

fn cmd_init(paths: &Paths) -> Result<()> {
    let path = paths.config_dir.join("config.toml");
    if path.exists() {
        println!("config already exists: {}", path.display());
        return Ok(());
    }
    let login = std::env::var("USER").unwrap_or_default();
    std::fs::write(&path, config::template(&login))
        .with_context(|| format!("writing {}", path.display()))?;
    println!("wrote {}", path.display());
    println!("edit `login` and the `[hub]` section, then run `ansible-herd doctor`");
    Ok(())
}

/// Explain the state of the world.
///
/// This is the first thing to run when two people cannot see each other, so it
/// reports every layer rather than stopping at the first problem: identity, hub,
/// Herdr socket, daemon liveness, and what is currently in flight.
fn cmd_doctor(paths: &Paths) -> Result<()> {
    let config = Config::load(&paths.config_dir)?;
    let store = Store::new(&paths.state_dir);
    println!("config     {}", paths.config_dir.join("config.toml").display());
    println!("state      {}", paths.state_dir.display());

    match config.identity() {
        Ok((login, host)) => println!("identity   {login}@{host}"),
        Err(err) => println!("identity   NOT SET — {err}"),
    }

    match hub::open(&config, &paths.state_dir) {
        Ok(mut hub) => {
            println!("hub        {}", hub.describe());
            match hub.members() {
                Ok(members) => {
                    let cards: usize = members.iter().map(|m| m.agents.len()).sum();
                    println!("           {} member(s), {cards} session(s)", members.len());
                }
                Err(err) => println!("           unreadable: {err:#}"),
            }
            if !hub.supports_live() {
                println!("           live teleport: unsupported on this backend");
            }
        }
        Err(err) => println!("hub        NOT USABLE — {err}"),
    }

    let socket = herdr::socket_path();
    print!("herdr      {} — ", socket.display());
    match herdr::Client::connect(&socket).and_then(|mut c| c.ping()) {
        Ok(version) => {
            println!("ok{}", version.map(|v| format!(" (protocol {v})")).unwrap_or_default());
        }
        Err(err) => println!("unreachable: {err:#}"),
    }

    let alive = daemon_alive(&store, now_ms());
    println!(
        "daemon     {}",
        if alive { "running" } else { "not running (`ansible-herd startup`)" }
    );
    println!("watching   {:?}", store.watching(now_ms()));
    println!("inbox      {} unread", store.pending().len());
    Ok(())
}

/// Whether the daemon has heartbeated recently.
///
/// A heartbeat file rather than a pidfile check: sending a signal needs `unsafe`,
/// and `/proc` does not exist on macOS. A file with a timestamp works everywhere
/// and cannot be fooled by pid reuse.
fn daemon_alive(store: &Store, now: u64) -> bool {
    std::fs::read_to_string(store.root().join("daemon.alive"))
        .ok()
        .and_then(|text| text.trim().parse::<u64>().ok())
        .is_some_and(|at| now.saturating_sub(at) < 5_000)
}

/// Start the daemon unless it is already up. Safe to run repeatedly, which is
/// what Herdr's `[[startup]]` contract asks for.
fn cmd_startup(paths: &Paths) -> Result<()> {
    let store = Store::new(&paths.state_dir);
    if daemon_alive(&store, now_ms()) {
        println!("herd daemon already running");
        return Ok(());
    }
    let exe = std::env::current_exe().context("locating our own binary")?;
    let log = std::fs::File::options()
        .create(true)
        .append(true)
        .open(store.log_file())
        .with_context(|| format!("opening {}", store.log_file().display()))?;
    let child = std::process::Command::new(exe)
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()
        .context("starting the herd daemon")?;
    // The startup hook is a one-shot initializer, so it records the pid and
    // exits; the daemon outlives it.
    std::fs::write(store.pid_file(), child.id().to_string())?;
    println!("herd daemon started (pid {}), logging to {}", child.id(), store.log_file().display());
    Ok(())
}

fn cmd_daemon(paths: &Paths, once: bool) -> Result<()> {
    let config = Config::load(&paths.config_dir)?;
    daemon::Daemon::new(config, paths)?.run(once)
}

// ---------------------------------------------------------------- my status

fn cmd_status(paths: &Paths, args: &[String]) -> Result<()> {
    let config = Config::load(&paths.config_dir)?;
    let store = Store::new(&paths.state_dir);
    let mut overrides = store.overrides();
    let mut touched = false;

    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        // The value, when this flag takes one. Read before the match so the
        // borrow of `args` is finished by the time `i` moves.
        let value = args.get(i + 1).cloned();
        let required =
            || -> Result<String> { value.clone().with_context(|| format!("{arg} needs a value")) };
        match arg {
            "--headline" | "-m" => {
                overrides.headline = Some(required()?);
                i += 1;
            }
            "--clear-headline" => overrides.headline = None,
            "--help-wanted" | "--stuck" => {
                let note = value.clone().unwrap_or_default();
                if value.is_some() {
                    i += 1;
                }
                overrides.help = Some(HelpWanted { note, since_ms: now_ms() });
            }
            "--no-help" | "--unstuck" => overrides.help = None,
            "--share" => {
                let mode = Share::parse(&required()?).map_err(|got| {
                    anyhow::anyhow!("--share expects off|title|live, got {got:?}")
                })?;
                let pane = current_pane().context(
                    "--share needs a pane: run it from inside the pane, or set HERDR_PANE_ID",
                )?;
                overrides.share.insert(pane, mode);
                i += 1;
            }
            other => bail!("status: unexpected argument {other:?}\n\n{USAGE}"),
        }
        touched = true;
        i += 1;
    }

    if touched {
        overrides.seq += 1;
        store.put_overrides(&overrides)?;
    }

    println!("headline   {}", overrides.headline.as_deref().unwrap_or("(from the terminal title)"));
    match &overrides.help {
        Some(help) if help.note.is_empty() => println!("help       wanted"),
        Some(help) => println!("help       wanted — {}", help.note),
        None => println!("help       no"),
    }
    println!("share      default {}", config.default_share());
    for (pane, mode) in &overrides.share {
        println!("           {pane}: {mode}");
    }
    Ok(())
}

/// The pane this process is running in, as Herdr injects it.
fn current_pane() -> Option<String> {
    std::env::var("HERDR_PANE_ID").ok().filter(|v| !v.is_empty())
}

/// Ask Herdr to open one of our own manifest panes.
///
/// This is what makes the panes keybindable: Herdr's `plugin_action` key type
/// runs an action, and an action is a command — so the command asks Herdr to open
/// the pane. One indirection, and `prefix+h` can open the herd.
///
/// # Errors
/// Whatever Herdr reports, including `plugin_disabled` when the plugin is linked
/// but switched off, which is worth seeing verbatim.
fn cmd_open(entrypoint: &str) -> Result<()> {
    let mut client = herdr::Client::connect(&herdr::socket_path())?;
    client.open_plugin_pane(PLUGIN_ID, entrypoint, None, &[])?;
    println!("opened {entrypoint}");
    Ok(())
}

/// Raise a hand, with the note that makes it worth answering.
///
/// A pane entrypoint rather than an action, because an action is a command with no
/// way to ask a question. Herdr's `popup` placement is exactly right for this: a
/// session-modal terminal that receives all input and closes when the command
/// exits, without disturbing the layout.
///
/// The note is the point. "Blocked" is something Herdr already knows; "I cannot
/// get RLS to deny and I have been at it for twenty minutes" is the thing that
/// makes a teammate walk over.
fn cmd_ask(paths: &Paths) -> Result<()> {
    let store = Store::new(&paths.state_dir);
    let mut overrides = store.overrides();

    if let Some(help) = &overrides.help {
        println!("your hand is already up: {}", help.note);
        println!("enter a new note, or an empty line to lower it");
    } else {
        println!("what do you need? (empty line cancels)");
    }
    print!("> ");
    std::io::stdout().flush()?;

    let mut note = String::new();
    std::io::stdin().read_line(&mut note)?;
    let note = note.trim();

    overrides.help = if note.is_empty() {
        if overrides.help.is_some() {
            println!("hand lowered");
        } else {
            println!("nothing sent");
        }
        None
    } else {
        println!("hand raised — the herd will see it within a poll");
        Some(HelpWanted { note: note.to_string(), since_ms: now_ms() })
    };
    overrides.seq += 1;
    store.put_overrides(&overrides)
}

fn cmd_view(on: bool) -> Result<()> {
    let mut client = herdr::Client::connect(&herdr::socket_path())?;
    if on {
        client.set_attention_view(METADATA_SOURCE)?;
        println!("Herdr's Agents view now sorts attention first");
    } else {
        client.clear_attention_view(METADATA_SOURCE)?;
        println!("restored Herdr's configured Agents sort");
    }
    Ok(())
}

// ---------------------------------------------------------------- the herd

/// Everything the roster and the addressing commands need.
struct Session {
    config: Config,
    paths: Paths,
    store: Store,
    hub: Box<dyn Hub>,
    login: String,
}

impl Session {
    fn open(paths: &Paths) -> Result<Self> {
        let config = Config::load(&paths.config_dir)?;
        let (login, _) = config.identity()?;
        let hub = hub::open(&config, &paths.state_dir)?;
        Ok(Self { config, paths: paths.clone(), store: Store::new(&paths.state_dir), hub, login })
    }

    fn rows(&mut self) -> Result<Vec<Row>> {
        let members = self.hub.members()?;
        Ok(roster::rows(
            &members,
            &self.login,
            now_ms(),
            self.config.hub.stale_after_ms,
            self.config.hub.forget_after_ms,
        ))
    }

    /// Accept either a roster index or a full key.
    ///
    /// Indices are what a human types; keys are what survives a refresh. Both
    /// resolve here so no caller has to care which it got.
    fn resolve(&mut self, selector: &str) -> Result<Row> {
        let rows = self.rows()?;
        if let Ok(index) = selector.parse::<usize>() {
            return rows.into_iter().find(|r| r.index == index).with_context(|| {
                format!("no session numbered {index} — the herd may have changed")
            });
        }
        if split_key(selector).is_none() {
            bail!("{selector:?} is neither a roster number nor a key like login@host/w1:p1");
        }
        rows.into_iter()
            .find(|r| r.key == selector)
            .with_context(|| format!("{selector} is not in the herd right now"))
    }
}

fn cmd_roster(paths: &Paths) -> Result<()> {
    let mut session = Session::open(paths)?;
    let stdin = std::io::stdin();

    loop {
        let rows = session.rows()?;
        let footer = format!(
            "{}\n\n  <n> watch   c <n> <text> comment   ! <text> need help   h <text> headline\n  s <n> live|title|off share   i inbox   a/d <n> accept/dismiss   r refresh   q quit\n> ",
            session.hub.describe()
        );
        print!("\n{}", roster::render(&rows, &footer));
        std::io::stdout().flush()?;

        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let line = line.trim();
        match dispatch(&mut session, line) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            // A mistyped command in an interactive pane should print and carry on,
            // not close the pane the user is working in.
            Err(err) => println!("  ! {err:#}"),
        }
    }
}

/// Handle one roster line. Returns whether to quit.
fn dispatch(session: &mut Session, line: &str) -> Result<bool> {
    let (head, tail) = line.split_once(' ').map_or((line, ""), |(h, t)| (h, t.trim()));
    match head {
        "" | "r" => Ok(false),
        "q" => Ok(true),
        "i" => {
            let pending = session.store.pending();
            if pending.is_empty() {
                println!("  inbox empty");
            }
            for (i, message) in pending.iter().enumerate() {
                println!("  {:>2}  {} → {}", i + 1, message.from, message.to_key);
                println!("      {}", message.body);
            }
            if !pending.is_empty() {
                println!("  a <n> type it into the pane   a <n> ! submit it   d <n> dismiss");
            }
            Ok(false)
        }
        // Accepting is the moment a teammate's words reach your machine, so it is a
        // separate deliberate keystroke rather than something the inbox does on
        // display.
        "a" | "d" => {
            let (selector, flag) = tail.split_once(' ').map_or((tail, ""), |(s, f)| (s, f.trim()));
            let index: usize = selector.parse().context("a <n> | d <n>")?;
            let pending = session.store.pending();
            let message = pending
                .get(index.wrapping_sub(1))
                .with_context(|| format!("no message numbered {index}"))?;
            if head == "d" {
                session.store.ack(&message.id)?;
                println!("  dismissed");
                return Ok(false);
            }
            let mut args = vec![
                index.to_string(),
                if flag == "!" { "--submit".into() } else { String::new() },
            ];
            args.retain(|a| !a.is_empty());
            cmd_inbox(&session.paths, &args)?;
            Ok(false)
        }
        "!" => {
            let mut overrides = session.store.overrides();
            overrides.help = if tail.is_empty() {
                None
            } else {
                Some(HelpWanted { note: tail.to_string(), since_ms: now_ms() })
            };
            overrides.seq += 1;
            session.store.put_overrides(&overrides)?;
            println!("  {}", if tail.is_empty() { "hand lowered" } else { "hand raised" });
            Ok(false)
        }
        "h" => {
            let mut overrides = session.store.overrides();
            overrides.headline = if tail.is_empty() { None } else { Some(tail.to_string()) };
            overrides.seq += 1;
            session.store.put_overrides(&overrides)?;
            println!("  headline set");
            Ok(false)
        }
        "c" => {
            let (selector, body) = tail.split_once(' ').context("c <n> <text>")?;
            let row = session.resolve(selector)?;
            send(session, &row, MessageKind::Comment, body.trim())?;
            Ok(false)
        }
        "s" => {
            let (selector, mode) = tail.split_once(' ').context("s <n> live|title|off")?;
            let row = session.resolve(selector)?;
            if !row.is_me {
                bail!("{} is not your session — you can only change what you share", row.who);
            }
            let mode = Share::parse(mode.trim())
                .map_err(|got| anyhow::anyhow!("expected live|title|off, got {got:?}"))?;
            let (_, _, pane) = split_key(&row.key).context("own key is malformed")?;
            let mut overrides = session.store.overrides();
            overrides.share.insert(pane.to_string(), mode);
            overrides.seq += 1;
            session.store.put_overrides(&overrides)?;
            println!("  {pane} now sharing {mode}");
            Ok(false)
        }
        selector => {
            let row = session.resolve(selector)?;
            open_teleport(session, &row)?;
            Ok(false)
        }
    }
}

/// Open a teleport view for `row`, preferring a Herdr pane beside the roster.
fn open_teleport(session: &mut Session, row: &Row) -> Result<()> {
    // Take the lease before opening anything: the owner should see the request
    // even if we never get to see a byte, because that request is what prompts
    // them to share.
    session.store.watch_touch(&row.key, now_ms())?;

    if !row.is_watchable() {
        println!(
            "  {} is publishing headline only. You are now listed as wanting in — they can share with `s <n> live`.",
            row.who
        );
        return Ok(());
    }

    match herdr::Client::connect(&herdr::socket_path()).and_then(|mut client| {
        client.open_plugin_pane(PLUGIN_ID, "teleport", None, &[("HERD_KEY", &row.key)])
    }) {
        Ok(()) => println!("  opened a live view of {}", row.key),
        // Outside Herdr, or with the plugin not installed, fall back to streaming
        // into this terminal. The prototype has to be usable before it is
        // installed.
        Err(err) => {
            println!("  {err:#}\n  streaming here instead; ctrl-c to stop");
            stream_here(session, &row.key, row.live_seq.unwrap_or(0))?;
        }
    }
    Ok(())
}

fn cmd_watch(paths: &Paths, args: &[String]) -> Result<()> {
    let mut session = Session::open(paths)?;
    // As a pane entrypoint the key arrives in the environment; on the command line
    // it is an argument.
    let selector = args
        .first()
        .cloned()
        .or_else(|| std::env::var("HERD_KEY").ok().filter(|v| !v.is_empty()))
        .context("watch <n|key> (or set HERD_KEY)")?;
    let (key, from_seq) = match session.resolve(&selector) {
        // Join at the owner's current chunk rather than at zero: a watcher wants
        // what is happening now, and the backlog below this point may already have
        // been pruned.
        Ok(row) => (row.key, row.live_seq.unwrap_or(0)),
        // A key that is momentarily absent from the herd is still watchable: the
        // owner may be mid-heartbeat.
        Err(err) if split_key(&selector).is_some() => {
            println!("{err:#}; watching anyway");
            (selector, 0)
        }
        Err(err) => return Err(err),
    };
    stream_here(&mut session, &key, from_seq)
}

/// Stream a session into this process's stdout until it ends.
fn stream_here(session: &mut Session, key: &str, from_seq: u64) -> Result<()> {
    println!("── live: {key} ─────────────────────────────");
    let mut last_lease = 0_u64;
    let store = session.store.clone();
    let target = key.to_string();
    let result = teleport::view(
        session.hub.as_mut(),
        key,
        from_seq,
        &mut std::io::stdout(),
        std::time::Duration::from_millis(150),
        || {
            let now = now_ms();
            if now.saturating_sub(last_lease) >= LEASE_REFRESH_MS {
                last_lease = now;
                // Refreshing from inside the loop is what makes "the pane is open"
                // and "I am watching" the same fact.
                let _ = store.watch_touch(&target, now);
            }
            true
        },
    );
    let _ = session.store.watch_release(key);
    result
}

// ---------------------------------------------------------------- talking

fn cmd_message(paths: &Paths, args: &[String], kind: MessageKind) -> Result<()> {
    let mut session = Session::open(paths)?;
    let (selector, rest) = args.split_first().context("comment <n|key> <text>")?;
    let row = session.resolve(selector)?;

    let mut body = Vec::new();
    let mut anchor = None;
    let mut i = 0;
    while i < rest.len() {
        if rest[i] == "--line" {
            i += 1;
            anchor = rest.get(i).and_then(|v| v.parse::<u32>().ok());
        } else {
            body.push(rest[i].clone());
        }
        i += 1;
    }
    let body = body.join(" ");
    if body.is_empty() && kind == MessageKind::Comment {
        bail!("comment <n|key> <text> — a comment with no text is a nudge, try `nudge`");
    }
    send_anchored(&mut session, &row, kind, &body, anchor)
}

fn send(session: &mut Session, row: &Row, kind: MessageKind, body: &str) -> Result<()> {
    send_anchored(session, row, kind, body, None)
}

fn send_anchored(
    session: &mut Session,
    row: &Row,
    kind: MessageKind,
    body: &str,
    anchor: Option<u32>,
) -> Result<()> {
    let message =
        daemon::compose(&session.store, &session.login, &row.key, kind, body, anchor, now_ms())?;
    session.hub.send(&message)?;
    println!("  sent to {} ({})", row.who, row.key);
    Ok(())
}

/// Read the inbox, and — only when asked — put a comment into the agent.
///
/// The default is to *type* the comment into the pane's composer without
/// submitting it, so the owner reads it and presses Enter. `--submit` skips that,
/// and needs `allow_submit = true` in config as well as the flag: a teammate
/// writing directly to your agent's input is a decision that should take two
/// deliberate steps, not one.
fn cmd_inbox(paths: &Paths, args: &[String]) -> Result<()> {
    let config = Config::load(&paths.config_dir)?;
    let store = Store::new(&paths.state_dir);
    let pending = store.pending();

    let Some(selector) = args.first() else {
        if pending.is_empty() {
            println!("inbox empty");
        }
        for (i, message) in pending.iter().enumerate() {
            println!("{:>3}  {} → {}", i + 1, message.from, message.to_key);
            println!("     {}", message.body);
            if let Some(line) = message.anchor_line {
                println!("     (about line {line})");
            }
        }
        if !pending.is_empty() {
            println!(
                "\n`ansible-herd inbox <n>` types it into the pane; add --submit to send it to the agent; --dismiss to drop it"
            );
        }
        return Ok(());
    };

    let index: usize = selector.parse().context("inbox <n>")?;
    let message = pending
        .get(index.wrapping_sub(1))
        .with_context(|| format!("no message numbered {index}"))?;
    let dismiss = args.iter().any(|a| a == "--dismiss");
    let submit = args.iter().any(|a| a == "--submit");

    if dismiss {
        store.ack(&message.id)?;
        println!("dismissed {}", message.id);
        return Ok(());
    }

    let (_, _, pane) = split_key(&message.to_key).context("message target is not a herd key")?;
    let text = format!("[from @{}] {}", message.from, message.body);
    let mut client = herdr::Client::connect(&herdr::socket_path())?;

    if submit {
        if !config.share.allow_submit {
            bail!(
                "--submit needs allow_submit = true under [share] in config.toml — a teammate's words going straight to your agent is an explicit choice"
            );
        }
        client.submit_prompt(pane, &text)?;
        println!("submitted to {pane}");
    } else {
        client.send_text(pane, &text)?;
        println!("typed into {pane} — press Enter there to send it to the agent");
    }
    store.ack(&message.id)?;
    Ok(())
}

// ---------------------------------------------------------------- demo

/// Publish a synthetic teammate into the hub.
///
/// Presence is the one kind of feature you cannot evaluate alone: the first person
/// to install this would otherwise see an empty roster and have nothing to judge.
/// This writes one fake member with the three states that matter — blocked with a
/// raised hand, working and sharing live, and done — plus a few chunks of live
/// output so `watch` has something to render. It is a hub write like any other, so
/// it exercises the real serialization, the real ordering, and the real teleport
/// path; only the agent is imaginary.
///
/// Delete it by removing the member file the command prints.
fn cmd_demo(paths: &Paths, login: &str) -> Result<()> {
    use crate::model::{AgentCard, HelpWanted as Help, MemberDoc, Status, agent_key};

    let config = Config::load(&paths.config_dir)?;
    let mut hub = hub::open(&config, &paths.state_dir)?;
    let now = now_ms();
    let host = "demo-box";

    let card = |pane: &str, status: Status, headline: &str, share: Share, age_ms: u64| AgentCard {
        key: agent_key(login, host, pane),
        pane_id: pane.to_string(),
        workspace: Some("ansible".into()),
        tab: Some("main".into()),
        agent: "claude".into(),
        status,
        headline: headline.to_string(),
        repo: Some("mrshll/ansible".into()),
        branch: Some("demo/herd".into()),
        share,
        help: None,
        since_ms: now.saturating_sub(age_ms),
        live_seq: None,
    };

    let mut doc = MemberDoc::new(login, host);
    doc.display_name = Some(format!("{login} (demo)"));
    doc.seq = now;
    doc.published_ms = now;
    doc.help = Some(Help {
        note: "RLS refuses to compare an enum to a literal".into(),
        since_ms: now.saturating_sub(240_000),
    });
    doc.agents = vec![
        card("w1:p1", Status::Blocked, "wire up read authorization", Share::Title, 240_000),
        card("w1:p2", Status::Working, "port the chunker to the relay", Share::Live, 35_000),
        card("w2:p1", Status::Done, "docs: hook coverage table", Share::Title, 900_000),
    ];

    // A live session with nothing to watch is a broken promise, so the demo
    // publishes real chunks through the real pipeline.
    if hub.supports_live() {
        let key = agent_key(login, host, "w1:p2");
        let mut chunker = ansible_capture::Chunker::new(
            &key,
            ansible_capture::ChunkerConfig { max_bytes: 512, max_age_ms: 50 },
            ansible_capture::Ruleset::default(),
        );
        let script = concat!(
            "\x1b[2J\x1b[H$ cargo test -p ansible-capture\r\n",
            "   Compiling ansible-capture v0.1.0\r\n",
            "\x1b[32mtest result: ok. 63 passed\x1b[0m\r\n",
            "> reading crates/ansible-capture/src/redact.rs\r\n",
            "\x1b[33m✳\x1b[0m porting the chunker to the relay…\r\n",
        );
        let mut chunks = chunker.push(script.as_bytes(), now);
        chunks.extend(chunker.finish(now + 1));
        for chunk in &chunks {
            hub.put_chunk(&key, chunk)?;
        }
        doc.agents[1].live_seq = chunks.last().map(|c| c.seq);
    }

    hub.publish(&doc)?;
    println!("published {} synthetic session(s) for {login}@{host}", doc.agents.len());
    println!("run `ansible-herd roster` to see them; `watch <n>` on the live one");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_lists_every_subcommand_the_dispatcher_accepts() {
        for command in [
            "init", "doctor", "startup", "daemon", "roster", "status", "watch", "comment", "nudge",
            "inbox", "focus",
        ] {
            assert!(USAGE.contains(command), "usage is missing {command}:\n{USAGE}");
        }
    }

    /// The two flags that decide what leaves the machine have to be discoverable
    /// without reading the source.
    #[test]
    fn usage_documents_the_sharing_controls() {
        assert!(USAGE.contains("--share off|title|live"));
        assert!(USAGE.contains("--submit"));
    }
}
