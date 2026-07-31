/**
 * Where the plugin's files live, and the config it reads.
 *
 * Herdr injects three directories and is explicit about what belongs in each:
 * `HERDR_PLUGIN_ROOT` is a managed source checkout and must hold nothing durable,
 * `HERDR_PLUGIN_CONFIG_DIR` is user-editable config, and `HERDR_PLUGIN_STATE_DIR`
 * is runtime state. This is the only module that reads those variables, so no
 * subcommand has to guess — and every path has a fallback for the case that
 * matters during development: running from a shell with no Herdr environment at
 * all.
 */

import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { homedir, hostname } from "node:os";
import { join } from "node:path";

import { parse as parseToml } from "smol-toml";

/**
 * The plugin id declared in `herdr-plugin.toml`.
 *
 * Herdr requires a plugin-owned Agents view to be sourced as
 * `plugin:<HERDR_PLUGIN_ID>` and rejects the set when the id does not match an
 * enabled plugin, so this constant and the manifest must agree.
 */
export const PLUGIN_ID = "ansible.herd";

/** Source identifier for every metadata report this plugin makes. */
export const METADATA_SOURCE = `plugin:${PLUGIN_ID}`;

export interface Config {
  /** GitHub login. The identity the whole hub is keyed by. */
  login: string;
  displayName: string | undefined;
  /** Machine name, so a laptop and a workbox are two rows rather than one. */
  host: string;
  hub: {
    /**
     * `spacetime` for the real hub, `local` for a file-backed one.
     *
     * `local` exists so the whole thing is demonstrable in one terminal with
     * nothing deployed. It is not a second architecture — it implements the same
     * interface and carries the same records.
     */
    kind: "spacetime" | "local";
    /** `spacetime`: the database name or address to publish presence to. */
    database: string | undefined;
    /** `spacetime`: passed to the CLI as `--server` when set. */
    server: string | undefined;
    /** `local`: a directory every member can read and write. */
    path: string | undefined;
    /** Base URL of the transcript Worker, for live frames. */
    workerBaseUrl: string | undefined;
    staleAfterMs: number;
    forgetAfterMs: number;
  };
  share: {
    /** What a pane publishes before anyone changes it. */
    default: "off" | "title" | "live";
    /**
     * Whether a teammate's comment may be *submitted* to the agent rather than
     * only typed into its composer. Off unless a human edits this file.
     */
    allowSubmit: boolean;
  };
  timing: { heartbeatMs: number; pollMs: number; reconcileMs: number };
}

export const DEFAULTS: Config = {
  login: "",
  host: "",
  displayName: undefined,
  hub: {
    kind: "local",
    database: undefined,
    server: undefined,
    path: undefined,
    workerBaseUrl: undefined,
    staleAfterMs: 20_000,
    forgetAfterMs: 300_000,
  },
  share: { default: "title", allowSubmit: false },
  timing: { heartbeatMs: 5_000, pollMs: 2_000, reconcileMs: 1_000 },
};

export interface Paths {
  configDir: string;
  stateDir: string;
}

function env(key: string): string | undefined {
  const value = process.env[key];
  return value !== undefined && value.length > 0 ? value : undefined;
}

export function resolvePaths(): Paths {
  const fallback = join(homedir(), ".local/share/ansible-herd");
  return {
    configDir: env("HERDR_PLUGIN_CONFIG_DIR") ?? join(fallback, "config"),
    stateDir: env("HERDR_PLUGIN_STATE_DIR") ?? join(fallback, "state"),
  };
}

export function ensurePaths(paths: Paths): void {
  mkdirSync(paths.configDir, { recursive: true });
  mkdirSync(paths.stateDir, { recursive: true });
}

/**
 * Read `config.toml`, filling in defaults.
 *
 * A missing file is not an error — it yields defaults, so `doctor` can explain
 * what to write instead of failing to start. A malformed one *is*, because a
 * silently ignored config is how a pane ends up sharing more than its owner
 * thinks.
 */
export function loadConfig(paths: Paths): Config {
  const file = join(paths.configDir, "config.toml");
  let raw: Record<string, unknown> = {};
  try {
    raw = parseToml(readFileSync(file, "utf8")) as Record<string, unknown>;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== "ENOENT") {
      throw new Error(`reading ${file}`, { cause: error });
    }
  }

  const hub = (raw["hub"] ?? {}) as Record<string, unknown>;
  const share = (raw["share"] ?? {}) as Record<string, unknown>;
  const timing = (raw["timing"] ?? {}) as Record<string, unknown>;
  const shareDefault = str(share["default"]);

  return {
    login: str(raw["login"]) ?? DEFAULTS.login,
    displayName: str(raw["display_name"]),
    host: str(raw["host"]) ?? hostname(),
    hub: {
      kind: hub["kind"] === "spacetime" ? "spacetime" : "local",
      database: str(hub["database"]),
      server: str(hub["server"]),
      path: str(hub["path"]),
      workerBaseUrl: str(hub["worker_base_url"]),
      staleAfterMs: num(hub["stale_after_ms"]) ?? DEFAULTS.hub.staleAfterMs,
      forgetAfterMs: num(hub["forget_after_ms"]) ?? DEFAULTS.hub.forgetAfterMs,
    },
    share: {
      // An unrecognised value falls back to `title`, never up to `live`: a config
      // typo must not escalate what leaves the machine.
      default:
        shareDefault === "off" || shareDefault === "live" || shareDefault === "title"
          ? shareDefault
          : DEFAULTS.share.default,
      allowSubmit: share["allow_submit"] === true,
    },
    timing: {
      heartbeatMs: num(timing["heartbeat_ms"]) ?? DEFAULTS.timing.heartbeatMs,
      pollMs: num(timing["poll_ms"]) ?? DEFAULTS.timing.pollMs,
      reconcileMs: num(timing["reconcile_ms"]) ?? DEFAULTS.timing.reconcileMs,
    },
  };
}

function str(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

function num(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

/** The starter config `init` writes: every knob, with the two real decisions blank. */
export function template(login: string): string {
  return `# ansible-herd — team presence for coding agents, hosted by Herdr.
# Docs: docs/plan/herdr-plugin.md in the ansible repo.

# Your GitHub login. The identity the whole hub is keyed by.
login = "${login}"
# display_name = "Sam"
# host = "sams-box"          # defaults to this machine's hostname

[hub]
# "spacetime" — the real hub: presence in SpacetimeDB, live frames through the
#               transcript Worker. Needs the module published and the CLI logged in.
# "local"     — a directory every member can read and write. No infrastructure,
#               and enough to see the whole thing work in one terminal.
kind = "local"
path = "/tmp/herd"

# For kind = "spacetime":
# database = "ansible-hub"
# server = "maincloud"
# worker_base_url = "https://transcripts.example.workers.dev"

stale_after_ms = 20000
forget_after_ms = 300000

[share]
# What a pane publishes before anyone changes it: "off", "title", or "live".
# "title" is headline and status only — no terminal contents leave this machine
# until a pane is explicitly set to "live".
default = "title"
# Whether a teammate's comment may be submitted to your agent as a prompt rather
# than only typed into its composer for you to send. Leave this false unless you
# have thought about it.
allow_submit = false

[timing]
heartbeat_ms = 5000
poll_ms = 2000
reconcile_ms = 1000
`;
}

export function writeConfigIfAbsent(paths: Paths, login: string): string | undefined {
  const file = join(paths.configDir, "config.toml");
  try {
    readFileSync(file);
    return undefined;
  } catch {
    writeFileSync(file, template(login));
    return file;
  }
}
