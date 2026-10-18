//! Paths an agent must never change. A change to any of them blocks the
//! run, because they control how the work is checked (`.factory/`), what
//! runs with credentials (`.github/`) or how agents behave (`.claude/`,
//! `.mcp.json`).

/// Always protected, whatever the operator configures.
pub const BUILTIN: [&str; 4] = [".factory/", ".github/", ".claude/", ".mcp.json"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtectedPaths {
    /// Entries without a trailing slash; each covers itself and anything
    /// beneath it.
    entries: Vec<String>,
}

impl ProtectedPaths {
    /// The built-in paths plus the operator's. Entries are repo-relative;
    /// a trailing `/` is optional.
    pub fn new<S: AsRef<str>>(extra: &[S]) -> Self {
        let entries = BUILTIN
            .iter()
            .copied()
            .chain(extra.iter().map(AsRef::as_ref))
            .map(|entry| entry.trim_end_matches('/').to_owned())
            .filter(|entry| !entry.is_empty())
            .collect();
        Self { entries }
    }

    /// Whether a repo-relative path, as git reports it, is protected. An
    /// entry covers the path itself and everything beneath it, so turning a
    /// protected file into a directory, or the reverse, is caught too.
    /// Matching ignores ASCII case, so the rule holds on case-insensitive
    /// filesystems as well.
    pub fn is_protected(&self, path: &str) -> bool {
        self.entries.iter().any(|entry| {
            path.len() >= entry.len()
                && path.as_bytes()[..entry.len()].eq_ignore_ascii_case(entry.as_bytes())
                && matches!(path.as_bytes().get(entry.len()), None | Some(b'/'))
        })
    }

    /// The changed paths that touch a protected entry.
    pub fn violations<'a, I>(&self, changed: I) -> Vec<&'a str>
    where
        I: IntoIterator<Item = &'a str>,
    {
        changed
            .into_iter()
            .filter(|path| self.is_protected(path))
            .collect()
    }
}

/// Whether `entry` is acceptable as an operator-configured protected path:
/// repo-relative, with no empty, `.` or `..` components.
pub fn is_valid_entry(entry: &str) -> bool {
    let trimmed = entry.strip_suffix('/').unwrap_or(entry);
    !trimmed.is_empty()
        && !trimmed.starts_with('/')
        && trimmed
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_paths_and_everything_beneath_them_are_protected() {
        let protected = ProtectedPaths::new::<&str>(&[]);
        for path in [
            ".factory/verify.sh",
            ".factory",
            ".github/workflows/ci.yml",
            ".claude/settings.json",
            ".mcp.json",
            ".mcp.json/x",
            ".GitHub/workflows/ci.yml",
        ] {
            assert!(protected.is_protected(path), "{path}");
        }
    }

    #[test]
    fn similar_names_are_not_protected() {
        let protected = ProtectedPaths::new::<&str>(&[]);
        for path in [
            ".factoryx/a",
            "src/.factory/a",
            ".github2",
            ".mcp.json.bak",
            "docs/.claude.md",
            "README.md",
        ] {
            assert!(!protected.is_protected(path), "{path}");
        }
    }

    #[test]
    fn operator_entries_extend_the_list() {
        let protected = ProtectedPaths::new(&["infra/", "Makefile"]);
        assert!(protected.is_protected("infra/main.tf"));
        assert!(protected.is_protected("Makefile"));
        assert!(!protected.is_protected("infrastructure.md"));
        assert_eq!(
            protected.violations(["src/lib.rs", "infra/a.tf", ".github/x.yml"]),
            ["infra/a.tf", ".github/x.yml"]
        );
    }

    #[test]
    fn validates_operator_entries() {
        for good in ["infra/", "Makefile", "deploy/prod/"] {
            assert!(is_valid_entry(good), "{good}");
        }
        for bad in ["", "/", "/etc", "../x", "a//b", "./a", "a/../b"] {
            assert!(!is_valid_entry(bad), "{bad}");
        }
    }
}
