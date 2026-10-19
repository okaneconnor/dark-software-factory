These apply to every Rust change in this repository.

- **Tooling.** Code is formatted by `cargo fmt` with the repository's settings, and `cargo clippy --all-targets -- -D warnings` passes with the lints the workspace sets. Never silence a lint without a local `#[allow(...)]` and a comment saying why.
- **No `unsafe`** unless the repository already uses it for the same purpose; then every block has a `// SAFETY:` comment proving it sound.
- **Errors, not panics.** Return errors; no `unwrap()` or `expect()` outside tests. Libraries define error types with `thiserror`; applications use `anyhow` and add context where an error crosses a boundary, saying what was being attempted.
- **Types carry meaning.** Use a newtype or an enum instead of a bare `String`, `bool` or integer when the value has rules, and check those rules once, when the value is made.
- **Borrow by default.** Take `&str`, `&[T]` and `&Path`; clone only when the code really needs its own copy.
- **Async never blocks.** Blocking IO and heavy computation go to `spawn_blocking`; never hold a `std::sync::Mutex` guard across `.await`; every network call has a timeout.
- **Tests with every change**, written to fail without it. Keep them deterministic: no real network, no wall clock, no sleeps; pass time and fakes in.
- **Docs.** Public items have doc comments stating what they promise; comments explain why, not what.
- **Fit in.** Follow the surrounding code's patterns and names before adding new ones. Add a dependency only when the standard library and existing dependencies can't do the job.

Load the `rust:rust` skill for the reference pages and the review checklist.
