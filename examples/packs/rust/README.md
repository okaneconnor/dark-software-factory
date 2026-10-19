# rust

A dsf standards pack for Rust codebases, strict in the way dsf itself is.
It's an ordinary Claude Code plugin, so it also works interactively:
`claude --plugin-dir examples/packs/rust`.

- `rules.md` is added to every agent's system prompt: the rules that always
  apply.
- `skills/rust/` is loaded when relevant. It has reference pages on errors,
  types and APIs, ownership, async, testing, lints and tooling, and
  documentation, plus a review checklist.
- `hooks/hooks.json` runs `cargo fmt` after an agent edits a `.rs` file, when
  `cargo` is installed in the worker image.

dsf's own `CLAUDE.md` imports `rules.md`, so these rules also apply when
working on dsf with Claude Code. Copy the pack and adapt it to your own
conventions.
