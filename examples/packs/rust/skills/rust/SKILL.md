---
name: rust
description: Rust conventions for this repository. Covers error design and context, panics, newtypes and enums, API naming and visibility, ownership and borrowing, async with Tokio, testing (unit, integration, snapshot, property), Clippy and rustfmt, dependencies, unsafe, and documentation, plus a review checklist. Use before writing, changing or reviewing Rust code.
---

# Rust

The reference pages are in `references/`, next to this file. Read the ones
the task needs before writing code:

| Task | Read |
| --- | --- |
| Returning, wrapping or reporting errors; deciding whether to panic | `references/errors.md` |
| Designing a type, a function signature or a public API | `references/types-and-apis.md` |
| Lifetimes, cloning, collections, shared state | `references/ownership.md` |
| Anything `async`, tasks, timeouts, shutdown | `references/async.md` |
| Writing or fixing tests | `references/testing.md` |
| Lints, formatting, dependencies, features, `unsafe` | `references/lints-and-tooling.md` |
| Doc comments, module docs, comments | `references/documentation.md` |
| Reviewing a change | all of them, then the checklist below |

## Review checklist

A change is ready when:

- [ ] `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` pass, and any new `#[allow]` says why.
- [ ] No new `unwrap`, `expect`, `panic!` or indexing that can fail on input; errors carry context.
- [ ] New types make invalid values hard to build; public items have the narrowest visibility that works.
- [ ] No needless `clone`, `String` or `Vec` where a borrow would do.
- [ ] Async code doesn't block, holds no sync lock across `.await`, and bounds every wait.
- [ ] Tests cover the change, fail without it, and are deterministic.
- [ ] Public items are documented, and comments explain why.
- [ ] No new dependency without a reason the standard library or an existing one can't meet.
