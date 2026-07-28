//! Streaming redaction applied before bytes leave the machine.
//!
//! # Why streaming, and why that is hard
//!
//! PTY output arrives in arbitrary chunks, so a secret can straddle two writes:
//!
//! ```text
//! push(b"...key=sk-ant-api03-AAA")
//! push(b"BBBCCC and then more output")
//! ```
//!
//! A redactor that scanned each write independently would emit the first half
//! verbatim and durably store a live credential. [`Redactor`] therefore retains
//! exactly the tail that could still grow into a match, and releases it as soon
//! as the next write proves it safe — or at end of stream, where a partial token
//! can no longer become one.
//!
//! The hold-back is the forming match, never a fixed window, and it is bounded
//! by [`MAX_LOOKAHEAD`]. Output that cannot become a secret is emitted
//! immediately, so live viewers pay no latency for redaction.
//!
//! # Which shapes, and why these
//!
//! Not guesswork. A session was recorded with `ansible-terminal`'s `vt-record`
//! example and scanned with this crate's `redact-report` example: vendor-prefix
//! rules alone caught 4 of 12 planted credentials. Every miss was a named value
//! (`*_SECRET=`, `PASSWORD=`), a URL-embedded credential, a JWT, or a PEM block,
//! so those became rules too. `docs/spikes/capture-round-trip.md` records the
//! measurement and the gaps that remain.

/// Longest possible PEM end delimiter, `-----END <label>-----`, with a bounded
/// label. While swallowing a key, this much of the tail is retained so a
/// delimiter split across writes can still be recognised.
const PEM_END_MAX: usize = 9 + 64 + 5;

/// Largest window the redactor will hold while a match is forming.
///
/// Bounds live-tail lag. A candidate needing more context than this is abandoned
/// and emitted — a deliberate, documented gap rather than an unbounded stall.
pub const MAX_LOOKAHEAD: usize = 512;

/// A secret shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rule {
    /// A vendor-branded token: literal prefix followed by a body run. Redacts
    /// prefix and body together, since the prefix alone identifies the vendor.
    Token { name: &'static str, prefix: &'static str, min_body: usize },

    /// `NAME=value` or `NAME: value` where `NAME` contains a secret-bearing
    /// word. Redacts only the value: keeping the name is what lets a transcript
    /// still show *which* variable was set, at no cost to safety.
    NamedValue { name: &'static str, needles: &'static [&'static str] },

    /// `scheme://user:password@host`. Redacts only the password.
    UrlPassword { name: &'static str },

    /// A JSON Web Token: dot-separated base64url segments.
    Jwt { name: &'static str },

    /// A PEM private key block, handled as a state machine rather than a
    /// lookahead so a multi-kilobyte key cannot stall the stream.
    PemPrivateKey { name: &'static str },
}

impl Rule {
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Token { name, .. }
            | Self::NamedValue { name, .. }
            | Self::UrlPassword { name }
            | Self::Jwt { name }
            | Self::PemPrivateKey { name } => name,
        }
    }
}

fn is_token_body(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn is_jwt_body(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')
}

/// Outcome of trying one rule at one position.
#[derive(Clone, Copy)]
enum Hit {
    None,
    /// Could still match if more input arrives.
    Partial,
    /// Replace `input[..consumed]` with a marker.
    Redact {
        consumed: usize,
        name: &'static str,
    },
    /// Emit `keep` bytes verbatim, then replace the next `consumed` with a
    /// marker. Used where the label is worth keeping (`NAME=`, `scheme://user:`).
    RedactAfter {
        keep: usize,
        consumed: usize,
        name: &'static str,
    },
    /// Emit a marker and start swallowing until the PEM end delimiter.
    EnterPem {
        consumed: usize,
        name: &'static str,
    },
}

impl Hit {
    /// Total input span, used to prefer the longest match.
    fn span(self) -> usize {
        match self {
            Hit::Redact { consumed, .. } | Hit::EnterPem { consumed, .. } => consumed,
            Hit::RedactAfter { keep, consumed, .. } => keep + consumed,
            Hit::None | Hit::Partial => 0,
        }
    }
}

/// Identifier fragments that mark a value as secret-bearing.
const SECRET_NEEDLES: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "apikey",
    "api_key",
    "credential",
    "private_key",
    "session_key",
    "access_key",
];

/// The rules applied to a session's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ruleset {
    pub version: u32,
    pub rules: Vec<Rule>,
}

impl Ruleset {
    /// Longest literal prefix in the set. Bounds the hold-back for token rules.
    #[must_use]
    pub fn max_prefix_len(&self) -> usize {
        self.rules
            .iter()
            .filter_map(|r| match r {
                Rule::Token { prefix, .. } => Some(prefix.len()),
                _ => None,
            })
            .max()
            .unwrap_or(0)
    }
}

impl Default for Ruleset {
    fn default() -> Self {
        Self {
            version: 2,
            rules: vec![
                Rule::PemPrivateKey { name: "private-key" },
                Rule::Jwt { name: "jwt" },
                Rule::Token { name: "anthropic-key", prefix: "sk-ant-", min_body: 16 },
                Rule::Token { name: "openai-key", prefix: "sk-proj-", min_body: 16 },
                Rule::Token { name: "stripe-live", prefix: "sk_live_", min_body: 16 },
                Rule::Token { name: "github-fine-grained", prefix: "github_pat_", min_body: 20 },
                Rule::Token { name: "github-pat", prefix: "ghp_", min_body: 20 },
                Rule::Token { name: "github-oauth", prefix: "gho_", min_body: 20 },
                Rule::Token { name: "github-server", prefix: "ghs_", min_body: 20 },
                Rule::Token { name: "aws-access-key", prefix: "AKIA", min_body: 16 },
                Rule::Token { name: "aws-session-key", prefix: "ASIA", min_body: 16 },
                Rule::Token { name: "slack-token", prefix: "xoxb-", min_body: 16 },
                Rule::Token { name: "slack-app-token", prefix: "xapp-", min_body: 16 },
                Rule::Token { name: "google-api-key", prefix: "AIza", min_body: 20 },
                Rule::Token { name: "npm-token", prefix: "npm_", min_body: 20 },
                Rule::Token { name: "spacetime-token", prefix: "stdb_", min_body: 20 },
                Rule::UrlPassword { name: "url-password" },
                Rule::NamedValue { name: "named-value", needles: SECRET_NEEDLES },
            ],
        }
    }
}

/// Applies a [`Ruleset`] across write boundaries.
///
/// Feed with [`push`](Redactor::push) and finish with
/// [`finish`](Redactor::finish). The held-back tail is only released by
/// `finish`, so callers must call it before treating the stream as complete.
pub struct Redactor {
    ruleset: Ruleset,
    /// Bytes seen but not yet emitted, because a match may still form.
    pending: Vec<u8>,
    /// Byte before the scan position, for word-boundary decisions. Starts as a
    /// newline so the very first byte counts as a word start.
    prev: u8,
    /// Inside a PEM block: discard until the end delimiter.
    in_pem: bool,
    redactions: u64,
}

impl Redactor {
    #[must_use]
    pub fn new(ruleset: Ruleset) -> Self {
        Self { ruleset, pending: Vec::new(), prev: b'\n', in_pem: false, redactions: 0 }
    }

    #[must_use]
    pub fn redactions(&self) -> u64 {
        self.redactions
    }

    #[must_use]
    pub fn ruleset_version(&self) -> u32 {
        self.ruleset.version
    }

    /// Feed raw bytes, returning the bytes that are safe to emit now.
    pub fn push(&mut self, input: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(input);
        self.drain(false)
    }

    /// Release everything, including the held-back tail.
    ///
    /// A partial match at end of stream can never complete, so the tail is
    /// emitted verbatim — an incomplete token is not a credential.
    pub fn finish(&mut self) -> Vec<u8> {
        self.drain(true)
    }

    fn drain(&mut self, at_end: bool) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.pending.len());
        let mut cursor = 0usize;

        while cursor < self.pending.len() {
            // Swallowing a private key: discard until the end delimiter. No
            // hold-back, so an arbitrarily long key cannot stall the stream.
            if self.in_pem {
                if let Hit::Redact { consumed, .. } = pem_end(&self.pending[cursor..], at_end) {
                    self.in_pem = false;
                    cursor += consumed;
                    continue;
                }
                // Discard key material, but keep enough of the tail that an end
                // delimiter split across writes can still match. Dropping it
                // would leave us swallowing the rest of the session.
                let available = self.pending.len() - cursor;
                cursor = self.pending.len() - PEM_END_MAX.min(available);
                break;
            }

            let rest = &self.pending[cursor..];
            let word_start = !is_ident(self.prev);

            let mut best = Hit::None;
            let mut partial = false;
            for rule in &self.ruleset.rules {
                let hit = try_rule(rule, rest, word_start, at_end);
                match hit {
                    Hit::None => {}
                    Hit::Partial => partial = true,
                    // Longest span wins, so overlapping shapes cannot leave part
                    // of one secret exposed.
                    _ if hit.span() > best.span() => best = hit,
                    _ => {}
                }
            }

            match best {
                Hit::Redact { consumed, name } => {
                    out.extend_from_slice(marker(name).as_bytes());
                    self.redactions += 1;
                    self.prev = b']';
                    cursor += consumed;
                    continue;
                }
                Hit::RedactAfter { keep, consumed, name } => {
                    out.extend_from_slice(&rest[..keep]);
                    out.extend_from_slice(marker(name).as_bytes());
                    self.redactions += 1;
                    self.prev = b']';
                    cursor += keep + consumed;
                    continue;
                }
                Hit::EnterPem { consumed, name } => {
                    out.extend_from_slice(marker(name).as_bytes());
                    self.redactions += 1;
                    self.in_pem = true;
                    self.prev = b']';
                    cursor += consumed;
                    continue;
                }
                Hit::None | Hit::Partial => {}
            }

            // Mid-stream a possible match must wait for more bytes. At end of
            // stream it can never complete, so fall through and emit.
            if partial && !at_end {
                break;
            }

            out.push(rest[0]);
            self.prev = rest[0];
            cursor += 1;
        }

        // Whatever remains is exactly the tail that could still grow into a
        // match; the `Partial` break above is what retains it.
        self.pending.drain(..cursor);
        out
    }
}

fn try_rule(rule: &Rule, rest: &[u8], word_start: bool, at_end: bool) -> Hit {
    match rule {
        Rule::Token { name, prefix, min_body } => {
            // Cheap rejection before the full matcher. With sixteen token rules
            // this is what keeps the common case — a byte that starts nothing —
            // to one comparison instead of sixteen scans.
            let bytes = prefix.as_bytes();
            if rest.first().is_some_and(|b| *b != bytes[0]) {
                return Hit::None;
            }
            match_token(rest, bytes, *min_body, at_end, name)
        }
        Rule::Jwt { name } => match_jwt(rest, at_end, name),
        Rule::PemPrivateKey { name } => match_pem_begin(rest, at_end, name),
        Rule::UrlPassword { name } if word_start => match_url_password(rest, at_end, name),
        Rule::NamedValue { name, needles } if word_start => {
            match_named_value(rest, needles, at_end, name)
        }
        _ => Hit::None,
    }
}

fn match_token(
    input: &[u8],
    prefix: &[u8],
    min_body: usize,
    at_end: bool,
    name: &'static str,
) -> Hit {
    if input.len() < prefix.len() {
        return if prefix.starts_with(input) && !at_end { Hit::Partial } else { Hit::None };
    }
    if &input[..prefix.len()] != prefix {
        return Hit::None;
    }
    let mut end = prefix.len();
    while end < input.len() && is_token_body(input[end]) {
        end += 1;
    }
    if end == input.len() && !at_end && end < MAX_LOOKAHEAD {
        return Hit::Partial;
    }
    if end - prefix.len() >= min_body { Hit::Redact { consumed: end, name } } else { Hit::None }
}

fn match_jwt(input: &[u8], at_end: bool, name: &'static str) -> Hit {
    const PREFIX: &[u8] = b"eyJ";
    if input.len() < PREFIX.len() {
        return if PREFIX.starts_with(input) && !at_end { Hit::Partial } else { Hit::None };
    }
    if &input[..PREFIX.len()] != PREFIX {
        return Hit::None;
    }
    let mut end = PREFIX.len();
    let mut dots = 0;
    while end < input.len() && is_jwt_body(input[end]) {
        if input[end] == b'.' {
            dots += 1;
        }
        end += 1;
    }
    if end == input.len() && !at_end && end < MAX_LOOKAHEAD {
        return Hit::Partial;
    }
    // Two dots plus real length, so ordinary base64 in output is not mistaken
    // for a token.
    if dots >= 2 && end >= 40 { Hit::Redact { consumed: end, name } } else { Hit::None }
}

fn match_pem_begin(input: &[u8], at_end: bool, name: &'static str) -> Hit {
    const BEGIN: &[u8] = b"-----BEGIN ";
    if input.len() < BEGIN.len() {
        return if BEGIN.starts_with(input) && !at_end { Hit::Partial } else { Hit::None };
    }
    if &input[..BEGIN.len()] != BEGIN {
        return Hit::None;
    }
    let tail = &input[BEGIN.len()..];
    let Some(close) = find(tail, b"-----") else {
        return if at_end || tail.len() > 64 { Hit::None } else { Hit::Partial };
    };
    // Certificates are public and useful in a transcript; only keys are secret.
    if find(&tail[..close], b"PRIVATE KEY").is_none() {
        return Hit::None;
    }
    Hit::EnterPem { consumed: BEGIN.len() + close + 5, name }
}

/// Consume through the end delimiter that closes a PEM block.
fn pem_end(input: &[u8], at_end: bool) -> Hit {
    const END: &[u8] = b"-----END ";
    const NAME: &str = "private-key";
    match find(input, END) {
        Some(start) => match find(&input[start + END.len()..], b"-----") {
            Some(close) => Hit::Redact { consumed: start + END.len() + close + 5, name: NAME },
            // Truncated: swallow what we have rather than release key bytes.
            None if at_end => Hit::Redact { consumed: input.len(), name: NAME },
            None => Hit::Partial,
        },
        None if at_end => Hit::Redact { consumed: input.len(), name: NAME },
        None => Hit::Partial,
    }
}

fn match_url_password(input: &[u8], at_end: bool, name: &'static str) -> Hit {
    const SEP: &[u8] = b"://";

    if !input.first().is_some_and(u8::is_ascii_alphabetic) {
        return Hit::None;
    }
    let mut scheme_end = 0;
    while scheme_end < input.len()
        && (input[scheme_end].is_ascii_alphanumeric()
            || matches!(input[scheme_end], b'+' | b'.' | b'-'))
    {
        scheme_end += 1;
    }
    if input.len() < scheme_end + SEP.len() {
        // Only wait if the bytes we do have are still consistent with "://".
        // Without this check every trailing word looks like a possible scheme
        // and the last word of each write would be needlessly delayed.
        let have = &input[scheme_end..];
        return if !at_end && SEP.starts_with(have) { Hit::Partial } else { Hit::None };
    }
    if &input[scheme_end..scheme_end + SEP.len()] != SEP {
        return Hit::None;
    }

    // The userinfo runs to '@'; a ':' inside it separates user from password.
    let authority = &input[scheme_end + SEP.len()..];
    let mut i = 0;
    let mut colon = None;
    while i < authority.len() {
        match authority[i] {
            b'@' => break,
            b' ' | b'\t' | b'\r' | b'\n' | b'/' => return Hit::None,
            b':' if colon.is_none() => colon = Some(i),
            _ => {}
        }
        i += 1;
    }
    if i == authority.len() {
        return if !at_end && i < MAX_LOOKAHEAD { Hit::Partial } else { Hit::None };
    }
    let Some(colon) = colon else { return Hit::None };
    if i <= colon + 1 {
        return Hit::None;
    }

    // Keep `scheme://user:`, redact the password up to but not including '@'.
    Hit::RedactAfter { keep: scheme_end + SEP.len() + colon + 1, consumed: i - colon - 1, name }
}

fn match_named_value(input: &[u8], needles: &[&str], at_end: bool, name: &'static str) -> Hit {
    const MAX_IDENT: usize = 64;

    let mut ident_end = 0;
    while ident_end < input.len() && is_ident(input[ident_end]) && ident_end < MAX_IDENT {
        ident_end += 1;
    }
    if ident_end == 0 {
        return Hit::None;
    }
    if ident_end == input.len() && !at_end {
        return Hit::Partial;
    }

    // Compared in place: allocating a lowercased copy at every word start
    // dominated the profile and made redaction slower than the terminal it has
    // to keep up with.
    if !needles.iter().any(|n| contains_ignore_ascii_case(&input[..ident_end], n.as_bytes())) {
        return Hit::None;
    }

    let mut i = ident_end;
    while i < input.len() && matches!(input[i], b' ' | b'\t') {
        i += 1;
    }
    if i == input.len() {
        return if at_end { Hit::None } else { Hit::Partial };
    }
    if !matches!(input[i], b'=' | b':') {
        return Hit::None;
    }
    i += 1;
    while i < input.len() && matches!(input[i], b' ' | b'\t') {
        i += 1;
    }
    if i == input.len() {
        return if at_end { Hit::None } else { Hit::Partial };
    }

    // The value is the rest of the line.
    let value_start = i;
    while i < input.len() && !matches!(input[i], b'\r' | b'\n') {
        i += 1;
    }
    if i == input.len() && !at_end {
        return if i < MAX_LOOKAHEAD { Hit::Partial } else { Hit::None };
    }
    if i == value_start {
        return Hit::None;
    }

    Hit::RedactAfter { keep: value_start, consumed: i - value_start, name }
}

/// ASCII-case-insensitive substring test that allocates nothing.
fn contains_ignore_ascii_case(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|w| w.iter().zip(needle).all(|(a, b)| a.eq_ignore_ascii_case(b)))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn marker(name: &str) -> String {
    format!("[redacted:{name}]")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redact_all(chunks: &[&[u8]]) -> (String, u64) {
        let mut r = Redactor::new(Ruleset::default());
        let mut out = Vec::new();
        for c in chunks {
            out.extend(r.push(c));
        }
        out.extend(r.finish());
        (String::from_utf8_lossy(&out).into_owned(), r.redactions())
    }

    fn redact(input: &[u8]) -> String {
        redact_all(&[input]).0
    }

    #[test]
    fn passes_ordinary_output_through_unchanged() {
        let input = b"$ cargo test\n   Compiling ansible-capture v0.1.0\nok\n";
        let (out, n) = redact_all(&[input]);
        assert_eq!(out.as_bytes(), input);
        assert_eq!(n, 0);
    }

    #[test]
    fn redacts_vendor_tokens() {
        assert_eq!(
            redact(b"export KEY=sk-ant-api03-AAAABBBBCCCCDDDD rest"),
            "export KEY=[redacted:anthropic-key] rest"
        );
        assert_eq!(redact(b"a AKIAIOSFODNN7EXAMPLE b"), "a [redacted:aws-access-key] b");
    }

    /// The boundary case this module exists for.
    #[test]
    fn redacts_a_secret_split_across_two_writes() {
        let (out, n) = redact_all(&[b"key=sk-ant-api0", b"3-AAAABBBBCCCCDDDD done"]);
        assert_eq!(out, "key=[redacted:anthropic-key] done");
        assert_eq!(n, 1);
    }

    #[test]
    fn redacts_a_secret_fed_one_byte_at_a_time() {
        let secret = b"prefix ghp_ABCDEFGHIJKLMNOPQRSTUV suffix";
        let singles: Vec<&[u8]> = secret.iter().map(std::slice::from_ref).collect();
        let (out, n) = redact_all(&singles);
        assert_eq!(out, "prefix [redacted:github-pat] suffix");
        assert_eq!(n, 1);
    }

    #[test]
    fn redacts_a_secret_at_the_very_end_of_the_stream() {
        let (out, n) = redact_all(&[b"tok ghp_ABCDEFGHIJKLMNOPQRSTUV"]);
        assert_eq!(out, "tok [redacted:github-pat]");
        assert_eq!(n, 1);
    }

    #[test]
    fn leaves_bare_and_short_prefixes_alone() {
        assert_eq!(redact(b"ids start with X and are long"), "ids start with X and are long");
        assert_eq!(redact(b"AKIA end"), "AKIA end");
    }

    // --- named values: the largest share of the real leak surface ---

    #[test]
    fn redacts_a_named_value_but_keeps_the_name() {
        // Keeping the name is the point: the transcript still shows what was
        // configured without showing the value.
        assert_eq!(
            redact(b"SESSION_PASSWORD=correcthorsebatterystaple\n"),
            "SESSION_PASSWORD=[redacted:named-value]\n"
        );
        assert_eq!(
            redact(b"MY_SERVICE_SECRET=aB3xY9zQ7wE1rT5yU8iO2p\n"),
            "MY_SERVICE_SECRET=[redacted:named-value]\n"
        );
        assert_eq!(redact(b"API_TOKEN: abc123def456\n"), "API_TOKEN: [redacted:named-value]\n");
    }

    #[test]
    fn leaves_unrelated_assignments_alone() {
        assert_eq!(redact(b"RUST_LOG=debug\nPATH=/usr/bin\n"), "RUST_LOG=debug\nPATH=/usr/bin\n");
        assert_eq!(redact(b"HOST=example.com\n"), "HOST=example.com\n");
    }

    #[test]
    fn redacts_a_named_value_split_across_writes() {
        let (out, n) = redact_all(&[b"PASSWORD=hunt", b"er2secret\n"]);
        assert_eq!(out, "PASSWORD=[redacted:named-value]\n");
        assert_eq!(n, 1);
    }

    // --- url credentials ---

    #[test]
    fn redacts_a_url_password_but_keeps_scheme_user_and_host() {
        assert_eq!(
            redact(b"remote: https://oauth2:tok123@github.com/a/b.git\n"),
            "remote: https://oauth2:[redacted:url-password]@github.com/a/b.git\n"
        );
    }

    #[test]
    fn leaves_a_url_without_credentials_alone() {
        let input = b"fetching https://api.anthropic.com/v1/messages\n";
        assert_eq!(redact(input).as_bytes(), input);
    }

    // --- jwt ---

    #[test]
    fn redacts_a_jwt() {
        let input = b"Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abc123signature\n";
        let out = redact(input);
        assert!(!out.contains("eyJhbGci"), "jwt survived: {out}");
        assert!(out.contains("[redacted:jwt]"));
    }

    #[test]
    fn leaves_short_base64_alone() {
        let input = b"data: eyJhIjoxfQ==\n";
        assert_eq!(redact(input).as_bytes(), input);
    }

    // --- pem blocks ---

    #[test]
    fn redacts_a_whole_pem_private_key_block() {
        let input = b"before\n-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAA\nAAAAB\n-----END OPENSSH PRIVATE KEY-----\nafter\n";
        let out = redact(input);
        assert!(!out.contains("b3BlbnNzaC"), "key body survived: {out}");
        assert!(out.contains("before"), "context before the key was lost");
        assert!(out.contains("after"), "context after the key was lost");
        assert!(out.contains("[redacted:private-key]"));
    }

    #[test]
    fn redacts_a_pem_block_arriving_in_many_writes() {
        let input: &[u8] = b"-----BEGIN RSA PRIVATE KEY-----\nSECRETKEYMATERIAL\n-----END RSA PRIVATE KEY-----\ntail\n";
        let pieces: Vec<&[u8]> = input.chunks(7).collect();
        let (out, _) = redact_all(&pieces);
        assert!(!out.contains("SECRETKEYMATERIAL"), "key body survived: {out}");
        assert!(out.contains("tail"));
    }

    #[test]
    fn leaves_a_certificate_block_alone() {
        // Public certificates are not secrets and are useful in a transcript.
        let input = b"-----BEGIN CERTIFICATE-----\nMIIBoDCCAUoCAQ\n-----END CERTIFICATE-----\n";
        assert_eq!(redact(input).as_bytes(), input);
    }

    #[test]
    fn an_unterminated_pem_block_is_still_swallowed() {
        let out = redact(b"-----BEGIN RSA PRIVATE KEY-----\nDANGLINGKEYBYTES");
        assert!(!out.contains("DANGLINGKEYBYTES"), "truncated key survived: {out}");
    }

    // --- invariants ---

    #[test]
    fn preserves_binary_and_escape_sequences() {
        let input: &[u8] = b"\x1b[38;2;255;0;0mred\x1b[0m\x00\xff\xfe";
        let mut r = Redactor::new(Ruleset::default());
        let mut out = r.push(input);
        out.extend(r.finish());
        assert_eq!(out, input);
    }

    /// Output that cannot become a secret must not be delayed, or every live
    /// viewer pays latency for nothing.
    ///
    /// A write ending on a word boundary is released in full. A write ending
    /// *mid-identifier* must hold that identifier, because the next bytes could
    /// turn it into `SOME_SECRET=` — that hold is correctness, not sloppiness,
    /// and it is bounded by the identifier cap.
    #[test]
    fn releases_a_write_that_ends_on_a_word_boundary() {
        let mut r = Redactor::new(Ruleset::default());
        let input = b"just some plain output text here\n";
        assert_eq!(r.push(input).len(), input.len());
    }

    #[test]
    fn holds_only_the_trailing_identifier_when_a_write_ends_mid_word() {
        let mut r = Redactor::new(Ruleset::default());
        let emitted = r.push(b"plain output then MY_SEC");
        assert_eq!(emitted, b"plain output then ");
        // ...and it is released once the next write resolves it.
        let more = r.push(b"TION=visible\n");
        assert_eq!(String::from_utf8_lossy(&more), "MY_SECTION=visible\n");
    }

    #[test]
    fn the_result_does_not_depend_on_where_the_stream_is_split() {
        let input: &[u8] =
            b"log ghp_ABCDEFGHIJKLMNOPQRSTUV then PASSWORD=zzz\nand https://u:p2@h/ end\n";
        let whole = redact(input);
        for split in 1..input.len() {
            let (a, b) = input.split_at(split);
            let (parts, _) = redact_all(&[a, b]);
            assert_eq!(parts, whole, "mismatch when split at {split}");
        }
    }

    /// The exact shapes captured by `vt-record` and reported by `redact-report`.
    /// See docs/spikes/capture-round-trip.md.
    #[test]
    fn every_planted_credential_from_the_recorded_session_is_removed() {
        let session = concat!(
            "GITHUB_TOKEN=ghp_FAKEFAKEFAKEFAKEFAKEFAKE12\r\n",
            "DATABASE_URL=postgres://admin:hunter2pass@db.internal:5432/prod\r\n",
            "ANTHROPIC_BASE_URL=https://api.anthropic.com\r\n",
            "ANTHROPIC_API_KEY=sk-ant-api03-FAKEFAKEFAKEFAKEFAKE1234\r\n",
            "SESSION_PASSWORD=correcthorsebatterystaple\r\n",
            "AWS_ACCESS_KEY_ID=AKIAFAKEFAKEFAKEFAKE\r\n",
            "MY_SERVICE_SECRET=aB3xY9zQ7wE1rT5yU8iO2p\r\n",
            "API_TOKEN=tok_live_9f8e7d6c5b4a3210\r\n",
            "PASSWORD=s3cr3t-value\r\n",
            "Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abc123signature\r\n",
            "origin\thttps://oauth2:ghp_FAKEFAKEFAKEFAKEFAKEFAKE12@github.com/acme/repo.git (fetch)\r\n",
            "-----BEGIN OPENSSH PRIVATE KEY-----\r\n",
            "b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAAB\r\n",
            "-----END OPENSSH PRIVATE KEY-----\r\n",
        );
        let out = redact(session.as_bytes());

        for secret in [
            "ghp_FAKEFAKEFAKEFAKEFAKEFAKE12",
            "hunter2pass",
            "sk-ant-api03-FAKEFAKEFAKEFAKEFAKE1234",
            "correcthorsebatterystaple",
            "AKIAFAKEFAKEFAKEFAKE",
            "aB3xY9zQ7wE1rT5yU8iO2p",
            "tok_live_9f8e7d6c5b4a3210",
            "s3cr3t-value",
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
            "b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAAB",
        ] {
            assert!(!out.contains(secret), "`{secret}` survived redaction:\n{out}");
        }

        // Non-secret context must survive, or transcripts become unreadable.
        assert!(out.contains("ANTHROPIC_BASE_URL=https://api.anthropic.com"));
        assert!(out.contains("github.com/acme/repo.git"));
    }
}
