# Testing

## What to test

- Every change comes with a test that fails without it. For a bug, write the
  failing test first.
- Test behaviour through the public surface of a module, not its private
  helpers. If a helper needs its own tests, it may want to be its own module.
- Cover the edges the change is about: empty input, the boundary values, the
  error paths, not just the happy path.

## Where tests live

- Unit tests sit beside the code in `#[cfg(test)] mod tests`.
- Integration tests in `tests/` exercise a crate as its users do: the
  library API, or the binary through a command runner such as `assert_cmd`.
- Shared fixtures go in `tests/support/` (a module, not a test file).

## Kinds of test

- **Snapshot tests** (`insta`) for anything rendered: prompts, reports,
  generated files, HTML, JSON payloads. Review every snapshot change as
  carefully as code.
- **Property tests** (`proptest`) for invariants that must hold for all
  inputs: parsing round trips, limits that are never exceeded.
- **Fakes at the boundaries.** A fake HTTP server (`wiremock`), a temporary
  directory (`tempfile`), a scripted stand-in for an external program. Fake
  the outside world, not your own code.

## Keep tests deterministic

- No real network, no real time, no sleeping for things to happen. Inject a
  clock, and wait on an event or a condition rather than a duration.
- Tests can run in parallel: give each its own temporary directory, port and
  names.
- Tests that need something heavy (Docker, a database) run only when an
  environment variable enables them, and say so in their module docs.

## Style

- Name a test after the behaviour it proves:
  `a_plan_that_needs_a_decision_asks_its_questions`, not `test_plan_2`.
- One behaviour per test; several asserts are fine when they describe it.
- `assert_eq!` over `assert!(a == b)`, and add a message with the actual
  value when the default output wouldn't explain a failure.
- `unwrap()` and `expect()` are fine in tests.
