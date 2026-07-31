import { describe, expect, it } from "vitest";

import type { Card, Status, World } from "./model.js";
import { normalize, statusFromHerdr, truncate } from "./model.js";
import { asks, render, rows } from "./roster.js";

const NOW = 1_800_000_000_000;

function card(over: Partial<Card> & { login: string; status: Status }): Card {
  return {
    sessionId: `${over.login}-w1p1`,
    host: "box",
    paneId: "w1:p1",
    agent: "claude",
    statusDetail: "",
    headline: `work by ${over.login}`,
    share: "title",
    statusSince: NOW - 9_000,
    heartbeatAt: NOW,
    watchers: [],
    ...over,
  };
}

function world(cards: Card[], comments: World["comments"] = []): World {
  return { cards, comments, at: NOW, source: "test hub" };
}

const OPTIONS = { me: "me", now: NOW, staleAfterMs: 20_000, forgetAfterMs: 300_000 };

describe("statusFromHerdr", () => {
  it("maps blocked to the status a teammate can resolve", () => {
    // The load-bearing line of the whole pivot.
    expect(statusFromHerdr("blocked")).toBe("AwaitingApproval");
  });

  it("keeps unknown as unknown rather than guessing a lifecycle position", () => {
    expect(statusFromHerdr("unknown")).toBe("Unknown");
    expect(statusFromHerdr("idle")).toBe("AwaitingInput");
    expect(statusFromHerdr("working")).toBe("Working");
    expect(statusFromHerdr("done")).toBe("Done");
  });
});

describe("rows", () => {
  it("sorts attention to the top", () => {
    const list = rows(
      world([
        card({ login: "alice", status: "AwaitingInput" }),
        card({ login: "bob", status: "AwaitingApproval" }),
        card({ login: "carol", status: "Working" }),
        card({ login: "dave", status: "Done" }),
      ]),
      OPTIONS,
    );
    expect(list.map((r) => r.who)).toEqual(["bob", "dave", "carol", "alice"]);
  });

  /** The property people notice: a queue that reorders under you is unreadable. */
  it("puts the oldest wait first among equals", () => {
    const list = rows(
      world([
        card({ login: "alice", status: "AwaitingApproval", statusSince: NOW - 1_000 }),
        card({ login: "bob", status: "AwaitingApproval", statusSince: NOW - 15_000 }),
      ]),
      OPTIONS,
    );
    expect(list.map((r) => r.who)).toEqual(["bob", "alice"]);
  });

  it("sorts stale rows below everything fresh, however alarming they are", () => {
    const list = rows(
      world([
        card({ login: "alice", status: "AwaitingApproval", heartbeatAt: NOW - 60_000 }),
        card({ login: "bob", status: "AwaitingInput" }),
      ]),
      OPTIONS,
    );
    expect(list[0]?.who).toBe("bob");
    expect(list[0]?.stale).toBe(false);
    expect(list[1]?.who).toBe("alice");
    expect(list[1]?.stale).toBe(true);
  });

  it("forgets a card whose machine has been gone for long enough", () => {
    const list = rows(
      world([
        card({ login: "alice", status: "AwaitingApproval", heartbeatAt: NOW - 400_000 }),
        card({ login: "bob", status: "AwaitingInput" }),
      ]),
      OPTIONS,
    );
    expect(list.map((r) => r.who)).toEqual(["bob"]);
  });
});

describe("roster filtering", () => {
  it("never shows a session shared off", () => {
    const list = rows(
      world([card({ login: "alice", status: "AwaitingApproval", share: "off" })]),
      OPTIONS,
    );
    expect(list).toHaveLength(0);
  });

  it("ranks a raised hand above a bare status", () => {
    const list = rows(
      world([
        card({
          login: "alice",
          status: "Working",
          help: { login: "alice", note: "cannot get RLS to deny", since: NOW - 240_000 },
        }),
        card({ login: "bob", status: "Working", statusSince: NOW - 100 }),
      ]),
      OPTIONS,
    );
    expect(list[0]?.who).toBe("alice");
    expect(asks(list[0]!)).toBe(true);
  });

  /** The bug the first roster had: the same note printed under three rows. */
  it("applies a hand to every session of that person but prints the note once", () => {
    const help = { login: "alice", note: "stuck", since: NOW - 1_000 };
    const list = rows(
      world([
        card({ login: "alice", status: "Working", help, sessionId: "a1" }),
        card({ login: "alice", status: "AwaitingInput", help, sessionId: "a2", paneId: "w2:p1" }),
      ]),
      OPTIONS,
    );
    expect(list.filter((r) => r.showHelp)).toHaveLength(1);
    expect(list[0]?.showHelp).toBe(true);

    const text = render(list, "", NOW);
    expect(text.match(/↳ help: stuck/gu)).toHaveLength(1);
    expect(text).toContain("[asked]");
  });

  it("gives each person their own note", () => {
    const text = render(
      rows(
        world([
          card({
            login: "alice",
            status: "Working",
            help: { login: "alice", note: "alice is stuck", since: NOW },
          }),
          card({
            login: "bob",
            status: "Working",
            help: { login: "bob", note: "bob is stuck", since: NOW },
          }),
        ]),
        OPTIONS,
      ),
      "",
      NOW,
    );
    expect(text).toContain("alice is stuck");
    expect(text).toContain("bob is stuck");
  });
});

describe("roster rendering", () => {
  it("marks my own rows so I can see what teammates see", () => {
    const list = rows(world([card({ login: "me", status: "Working" })]), OPTIONS);
    expect(list[0]?.isMe).toBe(true);
    expect(render(list, "", NOW)).toContain("(you)");
  });

  it("counts watchers and unread comments in the margin", () => {
    const list = rows(
      world(
        [card({ login: "me", status: "Working", share: "live", watchers: ["alice", "bob"] })],
        [
          {
            id: 1,
            sessionId: "me-w1p1",
            from: "alice",
            to: "me",
            body: "try --no-verify",
            chunkSeq: 0,
            byteOffset: 0,
            createdAt: NOW,
          },
        ],
      ),
      OPTIONS,
    );
    const text = render(list, "", NOW);
    expect(text).toContain("[live]");
    expect(text).toContain("👀2");
    expect(text).toContain("✉1");
  });

  it("falls back to the location when a headline is empty", () => {
    const list = rows(
      world([card({ login: "alice", status: "Working", headline: "", repo: "mrshll/ansible" })]),
      OPTIONS,
    );
    expect(render(list, "", NOW)).toContain("mrshll/ansible");
  });

  it("says so when the herd is empty rather than printing a bare header", () => {
    const text = render([], "hub: local", NOW);
    expect(text).toContain("nobody is publishing yet");
    expect(text).toContain("hub: local");
  });

  it("numbers rows densely from one, matching what is printed", () => {
    const list = rows(
      world([
        card({ login: "alice", status: "AwaitingApproval" }),
        card({ login: "bob", status: "AwaitingInput" }),
      ]),
      OPTIONS,
    );
    expect(list.map((r) => r.index)).toEqual([1, 2]);
    const text = render(list, "", NOW);
    expect(text).toContain("  1  alice");
    expect(text).toContain("  2  bob");
  });
});

describe("normalize", () => {
  it("collapses whitespace and caps length", () => {
    expect(normalize("  refactor   auth\tmiddleware\n")).toBe("refactor auth middleware");
    expect(normalize("")).toBe("");
    expect(normalize("   ")).toBe("");
    expect(normalize("abcdef", 3)).toBe("abc");
  });

  it("counts code points, so an emoji is never split in half", () => {
    // Two code points, four UTF-16 units. A `.length`-based cap would cut one in
    // half and publish a lone surrogate.
    expect(normalize("🐑🐑", 2)).toBe("🐑🐑");
    expect([...normalize("🐑🐑🐑", 2)]).toHaveLength(2);
  });

  it("never ends on a space", () => {
    expect(normalize("aaa bbb", 4)).toBe("aaa");
  });
});

describe("truncate", () => {
  it("marks the cut and counts code points", () => {
    expect(truncate("short", 10)).toBe("short");
    expect(truncate("exactlyten", 10)).toBe("exactlyten");
    expect(truncate("elevenchars", 10)).toBe("elevencha…");
    expect(truncate("🐑🐑🐑", 2)).toBe("🐑…");
  });
});
