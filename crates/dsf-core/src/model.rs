//! Claude model names, as the operator allows them and workflows ask for
//! them.

/// Whether `name` is plausible as a `claude --model` value: letters, digits
/// and `. _ - : [ ]`, at most 64 characters, and not starting with `-` so it
/// can never be read as another flag.
pub fn is_model_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('-')
        && name.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':' | b'[' | b']')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_model_ids_and_rejects_flags() {
        for good in [
            "claude-sonnet-5-5",
            "opus[1m]",
            "claude-haiku-4-5-20251001",
            "fast",
        ] {
            assert!(is_model_name(good), "{good}");
        }
        for bad in ["", "--x", "-m", "a b", "a/b", &"x".repeat(65)] {
            assert!(!is_model_name(bad), "{bad}");
        }
    }
}
