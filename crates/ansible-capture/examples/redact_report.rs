//! Report what the ruleset catches in a real capture, and what it leaves behind.
//!
//!   cargo run -p ansible-capture --example redact-report -- capture.raw
//!
//! Used to derive and tune the ruleset against recorded sessions rather than
//! against imagined ones. The "surviving candidates" section is the interesting
//! half: it lists secret-shaped material the rules did *not* remove.

use ansible_capture::{Redactor, Ruleset};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("usage: redact-report <capture-file>")?;
    let raw = std::fs::read(&path)?;

    // Feed in small irregular writes, the way a PTY delivers, so boundary
    // handling is exercised rather than bypassed.
    // Write size matters: it is the single biggest factor in redaction cost, so
    // make it explicit rather than hardcoding a pathological value.
    let write_size: usize =
        std::env::var("WRITE_SIZE").ok().and_then(|v| v.parse().ok()).unwrap_or(64 * 1024);

    let mut redactor = Redactor::new(Ruleset::default());
    let mut out = Vec::new();
    for piece in raw.chunks(write_size.max(1)) {
        out.extend(redactor.push(piece));
    }
    out.extend(redactor.finish());

    // The candidate scan below is diagnostic only and does work the production
    // path never does, so it must not be timed as if it were redaction cost.
    if std::env::var("REDACT_ONLY").is_ok() {
        println!("raw bytes    : {}", raw.len());
        println!("stored bytes : {}", out.len());
        println!("redactions   : {}", redactor.redactions());
        return Ok(());
    }

    let text = strip_ansi(&out);
    println!("capture      : {path}");
    println!("raw bytes    : {}", raw.len());
    println!("stored bytes : {}", out.len());
    println!("redactions   : {}", redactor.redactions());

    println!("\nsurviving candidates (secret-shaped material still present):");
    let mut any = false;
    for line in text.lines() {
        for candidate in candidates(line) {
            println!("  {candidate}");
            any = true;
        }
    }
    if !any {
        println!("  none");
    }
    Ok(())
}

/// Remove escape sequences so the scan sees content, not escape parameters.
fn strip_ansi(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            i += 1;
            if i < bytes.len() && bytes[i] == b'[' {
                i += 1;
                while i < bytes.len() && !bytes[i].is_ascii_alphabetic() {
                    i += 1;
                }
                i += 1;
            } else if i < bytes.len() && bytes[i] == b']' {
                while i < bytes.len() && bytes[i] != 0x07 {
                    i += 1;
                }
                i += 1;
            } else {
                i += 1;
            }
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Heuristics for "this looks like it should have been redacted".
///
/// Deliberately broader than the ruleset: the point is to surface gaps.
fn candidates(line: &str) -> Vec<String> {
    let mut found = Vec::new();
    let lower = line.to_ascii_lowercase();

    for needle in ["token", "secret", "password", "passwd", "api_key", "apikey", "credential"] {
        if lower.contains(needle) && line.contains(['=', ':']) && !line.contains("[redacted:") {
            found.push(format!("named value : {}", line.trim()));
            break;
        }
    }
    if line.contains("://") && line.contains('@') && !line.contains("[redacted:") {
        found.push(format!("url creds   : {}", line.trim()));
    }
    if line.contains("eyJ") && !line.contains("[redacted:") {
        found.push(format!("jwt         : {}", line.trim()));
    }
    if line.contains("PRIVATE KEY") {
        found.push(format!("pem block   : {}", line.trim()));
    }
    found
}
