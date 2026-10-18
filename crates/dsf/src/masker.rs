//! Hides secrets in everything that leaves the host: logs, board pages,
//! GitHub comments, PR bodies and digests. It also scans the exact diff
//! before a push.
//!
//! Two layers: the literal values dsf holds (and the encoded forms it
//! sends), and anything shaped like an Anthropic or GitHub token, which
//! catches secrets the agent found or invented.

use std::sync::{PoisonError, RwLock};

use secrecy::{ExposeSecret, SecretString};

pub const REDACTED: &str = "[REDACTED]";

/// Shorter values would match ordinary text.
const MIN_LITERAL_LEN: usize = 8;

/// Token prefixes, and the fewest characters after the prefix for a match
/// to count as a token rather than prose about one.
const TOKEN_SHAPES: [(&str, usize); 7] = [
    ("sk-ant-", 20),
    ("github_pat_", 20),
    ("ghp_", 30),
    ("gho_", 30),
    ("ghu_", 30),
    ("ghs_", 30),
    ("ghr_", 30),
];

/// Shared across a run's tasks; secrets minted later, such as hourly
/// GitHub tokens, are added as they appear.
#[derive(Default)]
pub struct Masker {
    literals: RwLock<Vec<SecretString>>,
}

/// A secret found by [`Masker::find`]. It says where, never what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// 1-based line number.
    pub line: usize,
    pub kind: FindingKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingKind {
    /// One of the secrets dsf holds.
    Known,
    /// Something shaped like a token with this prefix.
    TokenShaped(&'static str),
}

impl Masker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a secret value to hide. Values too short to mask safely
    /// are ignored.
    pub fn add(&self, secret: &SecretString) {
        let value = secret.expose_secret();
        if value.len() < MIN_LITERAL_LEN {
            return;
        }
        let mut literals = self
            .literals
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if literals.iter().any(|known| known.expose_secret() == value) {
            return;
        }
        literals.push(secret.clone());
        // Longest first, so a secret containing another is masked whole.
        literals.sort_by_key(|literal| std::cmp::Reverse(literal.expose_secret().len()));
    }

    pub fn mask(&self, text: &str) -> String {
        let literals = self.literals.read().unwrap_or_else(PoisonError::into_inner);
        let mut out = text.to_owned();
        for literal in literals.iter() {
            let value = literal.expose_secret();
            if out.contains(value) {
                out = out.replace(value, REDACTED);
            }
        }
        mask_token_shapes(&out)
    }

    /// Where `text` contains a secret, line by line. Used on the exact diff
    /// before a push; any finding stops the push.
    pub fn find(&self, text: &str) -> Vec<Finding> {
        let literals = self.literals.read().unwrap_or_else(PoisonError::into_inner);
        let mut findings = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let line_number = index + 1;
            if literals
                .iter()
                .any(|literal| line.contains(literal.expose_secret()))
            {
                findings.push(Finding {
                    line: line_number,
                    kind: FindingKind::Known,
                });
            }
            for (prefix, _) in token_spans(line) {
                findings.push(Finding {
                    line: line_number,
                    kind: FindingKind::TokenShaped(prefix),
                });
            }
        }
        findings
    }
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Byte ranges of token-shaped substrings, with the prefix that matched.
fn token_spans(text: &str) -> Vec<(&'static str, std::ops::Range<usize>)> {
    let mut spans = Vec::new();
    for (prefix, min_tail) in TOKEN_SHAPES {
        let mut from = 0;
        while let Some(offset) = text[from..].find(prefix) {
            let start = from + offset;
            // A prefix glued to a preceding word character is not a token start.
            let glued = text[..start].chars().next_back().is_some_and(is_token_char);
            let tail_start = start + prefix.len();
            let tail_len = text[tail_start..]
                .find(|c: char| !is_token_char(c))
                .unwrap_or(text.len() - tail_start);
            let end = tail_start + tail_len;
            if !glued && tail_len >= min_tail {
                spans.push((prefix, start..end));
            }
            from = end.max(start + 1);
        }
    }
    spans.sort_by_key(|(_, range)| range.start);
    spans
}

fn mask_token_shapes(text: &str) -> String {
    let spans = token_spans(text);
    if spans.is_empty() {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for (_, range) in spans {
        if range.start < cursor {
            continue; // overlapped by an earlier, longer match
        }
        out.push_str(&text[cursor..range.start]);
        out.push_str(REDACTED);
        cursor = range.end;
    }
    out.push_str(&text[cursor..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn masker_with(secrets: &[&str]) -> Masker {
        let masker = Masker::new();
        for secret in secrets {
            masker.add(&SecretString::from(*secret));
        }
        masker
    }

    #[test]
    fn masks_literal_secrets_wherever_they_appear() {
        let masker = masker_with(&["hunter2-but-longer", "hunter2-but-longer-still"]);
        assert_eq!(
            masker.mask("a=hunter2-but-longer-still b=hunter2-but-longer."),
            "a=[REDACTED] b=[REDACTED]."
        );
    }

    #[test]
    fn ignores_literals_too_short_to_mask_safely() {
        let masker = masker_with(&["abc"]);
        assert_eq!(masker.mask("abcdef"), "abcdef");
    }

    #[test]
    fn masks_token_shaped_strings_it_was_never_told_about() {
        let masker = Masker::new();
        let text = format!(
            "claude: sk-ant-oat01-{} gh: ghp_{} pat=github_pat_{}",
            "a".repeat(40),
            "B".repeat(36),
            "c_".repeat(20)
        );
        assert_eq!(
            masker.mask(&text),
            "claude: [REDACTED] gh: [REDACTED] pat=[REDACTED]"
        );
    }

    #[test]
    fn leaves_prose_about_tokens_alone() {
        let masker = Masker::new();
        for text in [
            "tokens start with sk-ant- or ghp_",
            "see ghp_short",
            "notghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert_eq!(masker.mask(text), text);
        }
    }

    #[test]
    fn finds_secrets_by_line_without_revealing_them() {
        let masker = masker_with(&["s3cr3t-value-123"]);
        let diff = format!(
            "+ok\n+key = \"s3cr3t-value-123\"\n+gh = \"ghs_{}\"\n",
            "x".repeat(36)
        );
        assert_eq!(
            masker.find(&diff),
            [
                Finding {
                    line: 2,
                    kind: FindingKind::Known
                },
                Finding {
                    line: 3,
                    kind: FindingKind::TokenShaped("ghs_")
                },
            ]
        );
        assert_eq!(masker.find("+clean line\n"), Vec::<Finding>::new());
    }

    #[test]
    fn handles_multibyte_text_around_tokens() {
        let masker = Masker::new();
        let text = format!("é→sk-ant-{}é", "z".repeat(25));
        assert_eq!(masker.mask(&text), "é→[REDACTED]é");
    }
}
