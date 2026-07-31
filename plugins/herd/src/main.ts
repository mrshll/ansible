#!/usr/bin/env node
/**
 * `ansible-herd` — team presence for coding agents, hosted by Herdr.
 *
 * One binary with a subcommand per manifest entrypoint, because that is the shape
 * a Herdr plugin takes.
 *
 * **Status: the pure core is ported and tested; the I/O layer is not finished.**
 * `model`, `redact`, and `roster` are complete, and the commands below exercise
 * them end to end against a file-backed world. The Herdr socket client, the
 * SpacetimeDB hub adapter, the reconcile daemon, and teleport are the next step —
 * `crates/ansible-herd` (Rust) remains in the tree and working in the meantime, by
 * the decision recorded in `docs/adr/0005-typescript-and-the-herdr-host.md`.
 */

import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

import type { Card, World } from "./model.js";
import { normalize, statusFromHerdr } from "./model.js";
import { PLUGIN_ID, ensurePaths, loadConfig, resolvePaths, writeConfigIfAbsent } from "./paths.js";
import { Redactor, redact } from "./redact.js";
import { render, rows } from "./roster.js";

const USAGE = `\
ansible-herd — team presence for coding agents, hosted by Herdr

  init                     write a starter config
  doctor                   explain what is and is not working
  roster                   show the herd
  demo [login]             write a synthetic teammate into the local world
  redact                   filter stdin through the redactor, for scrubbing captures

Not yet ported from crates/ansible-herd: daemon, watch, comment, inbox, ask.
See docs/adr/0005-typescript-and-the-herdr-host.md.
`;

function main(argv: string[]): number {
  const [command = "help", ...rest] = argv;
  const paths = resolvePaths();
  ensurePaths(paths);

  switch (command) {
    case "init": {
      const written = writeConfigIfAbsent(paths, process.env["USER"] ?? "");
      console.log(
        written === undefined
          ? `config already exists: ${join(paths.configDir, "config.toml")}`
          : `wrote ${written}\nedit \`login\` and the \`[hub]\` section, then run \`ansible-herd doctor\``,
      );
      return 0;
    }
    case "doctor":
      return doctor(paths);
    case "roster":
      return roster(paths);
    case "demo":
      return demo(paths, rest[0] ?? "robin");
    case "redact":
      return redactStdin();
    case "help":
    case "--help":
    case "-h":
      process.stdout.write(USAGE);
      return 0;
    default:
      process.stderr.write(`ansible-herd: unknown command ${JSON.stringify(command)}\n\n${USAGE}`);
      return 1;
  }
}

/**
 * Explain the state of the world, layer by layer.
 *
 * Reports every layer rather than stopping at the first problem, because "why
 * can't we see each other" is the question it exists to answer.
 */
function doctor(paths: ReturnType<typeof resolvePaths>): number {
  const config = loadConfig(paths);
  console.log(`config     ${join(paths.configDir, "config.toml")}`);
  console.log(`state      ${paths.stateDir}`);
  console.log(
    `identity   ${config.login.length > 0 ? `${config.login}@${config.host}` : "NOT SET — run `ansible-herd init`"}`,
  );
  console.log(`hub        ${describeHub(config)}`);
  console.log(
    `share      default ${config.share.default}, allowSubmit ${config.share.allowSubmit}`,
  );
  console.log(`plugin     ${PLUGIN_ID}`);

  const herdrSocket = process.env["HERDR_SOCKET_PATH"];
  console.log(`herdr      ${herdrSocket ?? "(no HERDR_SOCKET_PATH — not running inside Herdr)"}`);

  const world = readWorld(paths);
  console.log(`world      ${world.cards.length} session(s) from ${world.source}`);
  console.log(
    "\nnot yet ported: daemon, watch, comment, inbox, ask — use the Rust plugin at\nplugins/herdr-presence for those until the TypeScript I/O layer lands.",
  );
  return 0;
}

function describeHub(config: ReturnType<typeof loadConfig>): string {
  if (config.hub.kind === "spacetime") {
    return `spacetime ${config.hub.database ?? "(hub.database unset)"} on ${config.hub.server ?? "default server"}`;
  }
  return `local ${config.hub.path ?? "(hub.path unset)"}`;
}

function worldPath(paths: ReturnType<typeof resolvePaths>): string {
  const config = loadConfig(paths);
  return join(config.hub.path ?? paths.stateDir, "world.json");
}

/**
 * Read the snapshot the daemon maintains.
 *
 * Reading a file rather than the hub is deliberate and survives into the finished
 * design: the daemon holds the only hub connection and writes what it sees here,
 * so short-lived commands are fast, work offline, and never open a connection of
 * their own.
 */
function readWorld(paths: ReturnType<typeof resolvePaths>): World {
  try {
    const parsed = JSON.parse(readFileSync(worldPath(paths), "utf8")) as World;
    return {
      cards: parsed.cards ?? [],
      comments: parsed.comments ?? [],
      at: parsed.at ?? 0,
      source: parsed.source ?? "world.json",
    };
  } catch {
    return { cards: [], comments: [], at: 0, source: "no snapshot yet" };
  }
}

function roster(paths: ReturnType<typeof resolvePaths>): number {
  const config = loadConfig(paths);
  const world = readWorld(paths);
  const now = Date.now();
  const list = rows(world, {
    me: config.login,
    now,
    staleAfterMs: config.hub.staleAfterMs,
    forgetAfterMs: config.hub.forgetAfterMs,
  });
  console.log(`\n${render(list, describeHub(config), now)}\n`);
  return 0;
}

/**
 * Write a synthetic teammate into the local world.
 *
 * Presence is the one kind of feature you cannot evaluate alone: the first person
 * to install this would otherwise see an empty roster and have nothing to judge.
 * The three states that matter — blocked with a raised hand, working and sharing
 * live, and done — plus an unread comment.
 */
function demo(paths: ReturnType<typeof resolvePaths>, login: string): number {
  const now = Date.now();
  const host = "demo-box";
  const help = { login, note: "RLS refuses to compare an enum to a literal", since: now - 240_000 };

  const card = (pane: string, over: Partial<Card> & Pick<Card, "status" | "headline">): Card => ({
    sessionId: `${login}-${pane.replace(":", "")}`,
    login,
    displayName: `${login} (demo)`,
    host,
    paneId: pane,
    agent: "claude",
    statusDetail: "",
    share: "title",
    statusSince: now,
    heartbeatAt: now,
    watchers: [],
    help,
    repo: "mrshll/ansible",
    branch: "demo/herd",
    ...over,
  });

  const world: World = {
    at: now,
    source: "demo",
    cards: [
      card("w1:p1", {
        status: statusFromHerdr("blocked"),
        headline: normalize(redact("wire up read authorization").text),
        statusSince: now - 240_000,
      }),
      card("w1:p2", {
        status: statusFromHerdr("working"),
        headline: "port the chunker to the relay",
        statusSince: now - 35_000,
        share: "live",
        watchers: ["sam"],
        chunkCursor: 12,
      }),
      card("w2:p1", {
        status: statusFromHerdr("done"),
        headline: "docs: hook coverage table",
        statusSince: now - 900_000,
      }),
    ],
    comments: [
      {
        id: 1,
        sessionId: `${login}-w1p1`,
        from: "sam",
        to: login,
        body: "the RLS enum limitation is in ADR 0003 — carry a bool beside the enum",
        chunkSeq: 0,
        byteOffset: 0,
        createdAt: now - 30_000,
      },
    ],
  };

  const file = worldPath(paths);
  // `hub.path` names a directory the daemon would have created; `demo` runs before
  // there is a daemon, and the starter config points at one that does not exist
  // yet. Creating parents is what the Rust side does on every state write — see
  // `state.rs: writing_creates_missing_parent_directories`.
  mkdirSync(dirname(file), { recursive: true });
  writeFileSync(file, `${JSON.stringify(world, null, 2)}\n`);
  console.log(`wrote ${world.cards.length} synthetic session(s) to ${file}`);
  console.log("run `ansible-herd roster` to see them");
  return 0;
}

/**
 * Scrub a capture on its way past.
 *
 * `scripts/probe-herdr.sh` pipes real terminal output through this before writing
 * it to a telemetry directory, which does two jobs at once: it makes the capture
 * safe to hand to somebody else, and it runs the redactor over real session bytes
 * instead of test fixtures. If a secret ever shows up in a probe capture, that is
 * the most valuable bug report this repo could receive.
 *
 * Byte-exact for input containing no secrets, and streaming, so a long capture does
 * not have to fit in memory.
 */
function redactStdin(): number {
  const redactor = new Redactor();
  process.stdin.on("data", (chunk: Buffer) => {
    process.stdout.write(redactor.push(new Uint8Array(chunk)));
  });
  process.stdin.on("end", () => {
    process.stdout.write(redactor.finish());
    if (redactor.hits > 0) {
      process.stderr.write(`redacted ${redactor.hits} secret(s)\n`);
    }
  });
  return 0;
}

process.exitCode = main(process.argv.slice(2));
