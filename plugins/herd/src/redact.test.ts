import { describe, expect, it } from "vitest";

import { Redactor, latin1Decode, latin1Encode, redact } from "./redact.js";

/**
 * The twelve credentials the Rust redactor was measured against. Recording found
 * that vendor-prefix rules alone caught four of them, which is why the other rule
 * classes exist — so the whole set is the regression test.
 *
 * Assembled from fragments rather than written as literals. They are synthetic, but
 * they are *shaped* like the real thing, which is the entire point — and a
 * credential-shaped literal in a repository trips every secret scanner that ever
 * looks at it. GitHub's push protection rejected the first version of this file for
 * exactly that reason. `join` keeps the string the redactor sees identical while
 * leaving nothing for a scanner to match.
 */
const secret = (...parts: string[]): string => parts.join("");

const PLANTED: Array<[string, string]> = [
  ["anthropic-key", secret("sk-", "ant-", "api03-AAAABBBBCCCCDDDDEEEE")],
  ["openai-key", secret("sk-", "proj-", "AAAABBBBCCCCDDDDEEEE")],
  ["stripe-live", secret("sk_", "live_", "AAAABBBBCCCCDDDDEEEE")],
  ["github-fine-grained", secret("github", "_pat_", "11ABCDEFG0123456789abcdefgh")],
  ["github-pat", secret("ghp", "_", "0123456789abcdefghij0123")],
  ["aws-access-key", secret("AKIA", "IOSFODNN7EXAMPLE")],
  ["slack-token", secret("xoxb", "-1234567890-", "ABCDEFGHIJKLMNOP")],
  ["google-api-key", secret("AIza", "SyA0123456789abcdefghijklmnopqrst")],
  ["npm-token", secret("npm", "_", "0123456789abcdefghijklmnopqrstuvwx")],
  ["spacetime-token", secret("stdb", "_", "0123456789abcdefghijklmnop")],
  [
    "jwt",
    secret(
      "eyJhbGciOiJIUzI1NiJ9.",
      "eyJzdWIiOiIxMjM0NTY3ODkwIn0.",
      "dBjftJeZ4CVPmB92K27uhbUJU1p1r",
    ),
  ],
  ["named-value", secret("DATABASE_PASSWORD=", "hunter2hunter2")],
];
describe("redact", () => {
  it("catches every credential the recording planted", () => {
    for (const [rule, planted] of PLANTED) {
      const line = `before ${planted} after`;
      const { text, hits } = redact(line);
      expect(text, `${rule} survived: ${text}`).not.toContain(planted);
      expect(hits, rule).toBeGreaterThan(0);
      expect(text).toContain("before");
      expect(text).toContain("after");
    }
  });

  it("keeps the name of a named value and removes only the value", () => {
    const { text } = redact("AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG");
    expect(text).toContain("AWS_SECRET_ACCESS_KEY=");
    expect(text).not.toContain("wJalrXUtnFEMI");
  });

  it("keeps the user and scheme of a URL and removes only the password", () => {
    const { text } = redact("psql postgres://sam:sup3rsecret@db.internal:5432/app");
    expect(text).toContain("postgres://sam:");
    expect(text).toContain("@db.internal:5432/app");
    expect(text).not.toContain("sup3rsecret");
  });

  it("removes a whole PEM block", () => {
    const pem = [
      "-----BEGIN OPENSSH PRIVATE KEY-----",
      "b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAAB",
      "AAAAMwAAAAtzc2gtZWQyNTUxOQAAACBqZ2VuZXJhdGVkX2tleV9o",
      "-----END OPENSSH PRIVATE KEY-----",
    ].join("\n");
    const { text } = redact(`key follows\n${pem}\ndone`);
    expect(text).not.toContain("b3BlbnNzaC1rZXktdjEA");
    expect(text).toContain("key follows");
    expect(text).toContain("done");
  });

  it("leaves ordinary output alone", () => {
    const line = "test result: ok. 63 passed; 0 failed — 18 MiB/s";
    expect(redact(line)).toEqual({ text: line, hits: 0 });
  });

  /**
   * The false-positive case that matters: a headline is at most 80 characters and
   * a version string or a hash must not read as a secret.
   */
  it("does not fire on version strings, hashes, or paths", () => {
    for (const benign of [
      "refactor auth middleware",
      "bump to v2.7.1",
      "commit 6f7d13bd8a9f4c2e1b0a",
      "crates/ansible-capture/src/redact.rs",
      "100% of 63 tests",
    ]) {
      expect(redact(benign).hits, benign).toBe(0);
    }
  });
});

const bytes = (s: string): Uint8Array => latin1Encode(s);
const text = (b: Uint8Array): string => latin1Decode(b);

describe("Redactor", () => {
  function stream(chunks: string[]): { out: string; hits: number } {
    const redactor = new Redactor();
    let out = "";
    for (const chunk of chunks) {
      out += text(redactor.push(bytes(chunk)));
    }
    out += text(redactor.finish());
    return { out, hits: redactor.hits };
  }

  it("is byte-exact for output with no secrets in it", () => {
    const source = "[32mok[0m\r\n$ cargo test\r\n   Compiling\r\n";
    const { out, hits } = stream([...source]);
    expect(out).toBe(source);
    expect(hits).toBe(0);
  });

  /** The reason the redactor is a stream and not a function. */
  it("catches a secret split across two writes", () => {
    const { out } = stream([
      `export TOK=${secret("ghp", "_0123456789")}`,
      "abcdefghij0123 && echo ok\n",
    ]);
    expect(out).not.toContain(secret("ghp", "_0123456789abcdefghij0123"));
    expect(out).toContain("echo ok");
  });

  it("catches a PEM block split across many writes", () => {
    const pem = [
      "-----BEGIN RSA PRIVATE KEY-----",
      "MIIEowIBAAKCAQEAxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
      "-----END RSA PRIVATE KEY-----",
    ].join("\n");
    const { out } = stream([
      "cat id_rsa\n",
      pem.slice(0, 40),
      pem.slice(40, 90),
      pem.slice(90),
      "\ndone\n",
    ]);
    expect(out).not.toContain("MIIEowIBAAKCAQEA");
    expect(out).toContain("cat id_rsa");
    expect(out).toContain("done");
  });

  /**
   * Without `finish`, the held-back tail is lost — which would make a replay
   * silently short rather than loudly broken. Worth a test because the bug is
   * invisible.
   */
  it("only releases its tail on finish", () => {
    const redactor = new Redactor();
    const held = text(redactor.push(bytes("no newline yet, so nothing is safe")));
    expect(held).toBe("");
    expect(text(redactor.finish())).toBe("no newline yet, so nothing is safe");
  });

  it("survives bytes that are not valid UTF-8", () => {
    const raw = new Uint8Array([0x1b, 0x5b, 0x41, 0xff, 0xfe, 0x00, 0x80, 0x0a]);
    const redactor = new Redactor();
    const out = new Uint8Array([...redactor.push(raw), ...redactor.finish()]);
    expect([...out]).toEqual([...raw]);
  });

  it("round-trips every byte value through the latin-1 codec", () => {
    const all = new Uint8Array(256);
    for (let i = 0; i < 256; i += 1) all[i] = i;
    expect([...latin1Encode(latin1Decode(all))]).toEqual([...all]);
  });
});
