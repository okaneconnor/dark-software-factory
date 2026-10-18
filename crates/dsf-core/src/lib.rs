//! The pure domain model behind `dsf`: configuration, workflow definitions,
//! stage routing, the GitHub label lifecycle and scheduling rules.
//!
//! This crate performs no IO — no filesystem, network, process or clock
//! access. Everything here is deterministic and unit-testable; the `dsf`
//! binary crate owns every side effect. `clippy.toml` in this crate's
//! directory turns the usual IO entry points into lint errors.

pub mod clamp;
pub mod config;
pub mod duration;
pub mod issue;
pub mod labels;
pub mod protect;
pub mod routing;
pub mod schedule;
pub mod signature;
pub mod workflow;

/// Formats problems as an indented bullet list under an error's headline.
pub(crate) fn bullets<T: std::fmt::Display>(items: &[T]) -> String {
    use std::fmt::Write;
    items.iter().fold(String::new(), |mut out, item| {
        // Writing to a String can't fail.
        let _ = write!(out, "\n  - {item}");
        out
    })
}
