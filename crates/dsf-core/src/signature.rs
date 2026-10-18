//! Fingerprints of failing command output, so a run can stop when a fix
//! attempt changed nothing.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Only the end of the output is compared: that is where test runners and
/// compilers summarise what failed.
const TAIL_LINES: usize = 40;
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// A stable hash of a command's output tail with run-to-run noise removed.
/// Two failures with the same signature failed the same way.
///
/// Serialised as 16 hex digits: TOML and SQLite integers are signed, and
/// half of all signatures don't fit in an `i64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct FailureSignature(u64);

impl TryFrom<String> for FailureSignature {
    type Error = String;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        if text.len() == 16 {
            u64::from_str_radix(&text, 16)
                .map(Self)
                .map_err(|error| format!("bad failure signature `{text}`: {error}"))
        } else {
            Err(format!(
                "bad failure signature `{text}`: expected 16 hex digits"
            ))
        }
    }
}

impl From<FailureSignature> for String {
    fn from(signature: FailureSignature) -> Self {
        signature.to_string()
    }
}

impl FailureSignature {
    /// Removes what differs between identical failures (colour codes,
    /// timings, timestamps, addresses and temporary paths), then hashes the
    /// last lines with FNV-1a, which unlike `std`'s hasher is stable across
    /// Rust releases, so stored signatures stay comparable.
    pub fn of(output: &str) -> Self {
        let cleaned = strip_ansi(output);
        let lines: Vec<String> = cleaned
            .lines()
            .map(normalise_line)
            .filter(|line| !line.is_empty())
            .collect();
        let tail = &lines[lines.len().saturating_sub(TAIL_LINES)..];
        let mut hash = FNV_OFFSET;
        for line in tail {
            for byte in line.bytes().chain(std::iter::once(b'\n')) {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(FNV_PRIME);
            }
        }
        Self(hash)
    }
}

impl fmt::Display for FailureSignature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            // A CSI sequence ends at its first byte in '@'..='~'.
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
    }
    out
}

fn normalise_line(line: &str) -> String {
    line.split_whitespace()
        .map(normalise_token)
        .collect::<Vec<_>>()
        .join(" ")
}

fn normalise_token(token: &str) -> &str {
    let core = token
        .trim_start_matches(['(', '[', '{', '"', '\''])
        .trim_end_matches([')', ']', '}', ',', ';', ':', '.', '"', '\'']);
    if is_hex_address(core) {
        "<hex>"
    } else if is_duration(core) {
        "<duration>"
    } else if is_timestamp(core) {
        "<time>"
    } else if core.contains("/tmp/") || core.contains("/.tmp") {
        "<tmp>"
    } else {
        token
    }
}

fn is_hex_address(token: &str) -> bool {
    token
        .strip_prefix("0x")
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// `0.52s`, `12ms`, `3.1µs`: a number followed directly by a time unit.
fn is_duration(token: &str) -> bool {
    let number_len = token
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(token.len());
    let (number, unit) = token.split_at(number_len);
    number.bytes().next().is_some_and(|b| b.is_ascii_digit())
        && matches!(
            unit,
            "ns" | "us" | "µs" | "ms" | "s" | "m" | "h" | "sec" | "secs"
        )
}

/// Clock times, dates and ISO 8601 timestamps.
fn is_timestamp(token: &str) -> bool {
    let digits = token.bytes().filter(u8::is_ascii_digit).count();
    digits >= 4
        && token
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'-' | b':' | b'.' | b'T' | b'Z' | b'+'))
        && (token.contains(':') || token.matches('-').count() == 2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const CARGO_FAILURE: &str = "\
running 3 tests
test parser::handles_empty ... ok
test parser::handles_unicode ... FAILED
failures:
---- parser::handles_unicode stdout ----
thread 'parser::handles_unicode' panicked at src/parser.rs:42:9:
assertion failed: left == right
test result: FAILED. 2 passed; 1 failed; 0 ignored; finished in 0.52s";

    #[test]
    fn identical_failures_with_different_timings_match() {
        let again = CARGO_FAILURE.replace("0.52s", "1.07s");
        assert_eq!(
            FailureSignature::of(CARGO_FAILURE),
            FailureSignature::of(&again)
        );
    }

    #[test]
    fn noise_is_ignored() {
        let noisy = "\u{1b}[31merror\u{1b}[0m at 2026-10-18T23:04:11Z in 12ms: ptr 0x7ffd3a2c, file /tmp/.tmpAb12/x.rs";
        let calm = "error at 2026-10-19T01:00:00Z in 3ms: ptr 0x10, file /tmp/.tmpZz99/x.rs";
        assert_eq!(FailureSignature::of(noisy), FailureSignature::of(calm));
    }

    #[test]
    fn different_failures_differ() {
        let other = CARGO_FAILURE.replace("handles_unicode", "handles_tabs");
        assert_ne!(
            FailureSignature::of(CARGO_FAILURE),
            FailureSignature::of(&other)
        );
        let progress = CARGO_FAILURE.replace("2 passed; 1 failed", "1 passed; 2 failed");
        assert_ne!(
            FailureSignature::of(CARGO_FAILURE),
            FailureSignature::of(&progress)
        );
    }

    #[test]
    fn only_the_tail_counts() {
        let same = "warning: unused import\n".repeat(TAIL_LINES);
        let head_a = format!("compiling a\n{same}{CARGO_FAILURE}");
        let head_b = format!("compiling b\n{same}{CARGO_FAILURE}");
        assert_eq!(FailureSignature::of(&head_a), FailureSignature::of(&head_b));
    }

    #[test]
    fn signature_is_stable_across_releases() {
        // Signatures are persisted, so the hash must never change silently.
        assert_eq!(FailureSignature::of("").to_string(), "cbf29ce484222325");
        assert_eq!(FailureSignature::of("boom").to_string(), "585791b29d304900");
    }

    proptest! {
        #[test]
        fn never_panics(output in "\\PC{0,200}") {
            let _ = FailureSignature::of(&output);
        }
    }
}
