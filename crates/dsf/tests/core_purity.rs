//! `dsf-core` must not depend on anything that does IO (see CLAUDE.md).
//! Its own code is policed by the lint bans in `crates/dsf-core/clippy.toml`;
//! this checks its dependencies.

// Helpers outside #[test] functions aren't covered by clippy's
// allow-unwrap-in-tests, and a failed precondition should panic here.
#![allow(clippy::unwrap_used)]

use std::process::Command;

/// The direct dependencies `dsf-core` may have. Adding one means checking
/// that it performs no IO, then listing it here.
const ALLOWED: &[&str] = &["jiff", "serde", "thiserror", "toml"];

/// Crates that exist to do IO. None may appear anywhere in the tree.
const FORBIDDEN: &[&str] = &[
    "axum",
    "bollard",
    "curl",
    "git2",
    "hyper",
    "libgit2-sys",
    "libsqlite3-sys",
    "mio",
    "openssl",
    "reqwest",
    "rusqlite",
    "rustls",
    "socket2",
    "tokio",
    "ureq",
];

/// Package names in `dsf-core`'s normal (non-dev, non-build) dependency tree.
fn dependencies(extra: &[&str]) -> Vec<String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let output = Command::new(cargo)
        .args(["tree", "--package", "dsf-core", "--edges", "normal"])
        .args([
            "--prefix",
            "none",
            "--format",
            "{p}",
            "--locked",
            "--offline",
        ])
        .args(extra)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut names: Vec<String> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|name| *name != "dsf-core")
        .map(str::to_owned)
        .collect();
    names.sort();
    names.dedup();
    names
}

#[test]
fn direct_dependencies_are_allowlisted() {
    assert_eq!(dependencies(&["--depth", "1"]), ALLOWED);
}

#[test]
fn no_io_crates_anywhere_in_the_tree() {
    let all = dependencies(&[]);
    let found: Vec<&String> = all
        .iter()
        .filter(|name| FORBIDDEN.contains(&name.as_str()))
        .collect();
    assert_eq!(found, Vec::<&String>::new(), "full tree: {all:?}");
}
