# Lints and tooling

## Lints

- `cargo clippy --all-targets -- -D warnings` must pass. Workspace lints
  live in the root `Cargo.toml` (`[workspace.lints]`) and every crate opts in
  with `[lints] workspace = true`.
- A strict but practical baseline:

  ```toml
  [workspace.lints.rust]
  unsafe_code = "forbid"

  [workspace.lints.clippy]
  pedantic = { level = "warn", priority = -1 }
  unwrap_used = "deny"
  expect_used = "deny"
  ```

- Silencing a lint is an exception: put `#[allow(clippy::lint_name)]` on the
  smallest item that needs it, with a comment saying why. Never allow lints
  crate-wide to make a change pass.

## Formatting

- `cargo fmt` with the repository's `rustfmt.toml`, or defaults. Don't
  hand-format against it.

## Toolchain and dependencies

- Pin the toolchain in `rust-toolchain.toml` and the edition in
  `Cargo.toml`, and bump them deliberately.
- Add a dependency only when the standard library and the existing
  dependencies can't do the job reasonably. Prefer well-known,
  well-maintained crates, and turn off default features you don't use.
- Keep `Cargo.lock` committed for applications and workspaces, and run
  `cargo deny` (licences, advisories, sources) where the repository has it.
- Features are additive: enabling one must never break code that doesn't use
  it.

## `unsafe` and macros

- No `unsafe` unless the repository already uses it for the same purpose.
  Where it's unavoidable, keep the block minimal, wrap it in a safe API, and
  write a `// SAFETY:` comment saying why each requirement holds.
- Write a macro only when a function, a generic or a trait can't do the job,
  and keep it small. Prefer `derive` macros from established crates to
  hand-written ones.
