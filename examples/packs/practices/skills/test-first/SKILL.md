---
name: test-first
description: Write the test before the code. Use before adding a feature, changing behaviour or fixing a bug, whenever the change can be checked by an automated test.
---

# Test first

1. **Write a test for the behaviour.** Put it next to the existing tests and
   follow their style. Test what the code should do, not how it does it. For a
   bug, the test reproduces the bug.
2. **Run it and watch it fail.** Run only that test. Read the failure: it must
   fail for the reason you expect (the behaviour is missing or wrong), not
   because of a typo, a missing import or a broken setup. Fix the test until
   it fails for the right reason.
3. **Write the least code that makes it pass.** No extra features, no
   speculative generality. Run the test again and see it pass.
4. **Tidy up with the tests green.** Remove duplication and improve names, then
   run the whole suite after each small step. If a test goes red, undo the
   last step.

## A test that passes straight away proves nothing

If the new test passes before you wrote any code, it isn't testing the change.
Either the behaviour already exists, or the test is wrong. Find out which:
tighten the assertion, or temporarily break the code under test and confirm
the test fails, then restore it. Don't keep a test you have never seen fail.

## When a test can't come first

Some changes have no meaningful automated test: documentation, comments, pure
configuration or data files with nothing to execute. For these:

- Say in your final message that no test came first, and why.
- Still check the result another way: build it, lint it, run the config
  through its parser or validator, or run the command it feeds.
- If the project's gate (its verify or CI script) covers the change, run it.

"It's hard to test" is not a reason to skip. Look for a smaller seam, a fake
for the slow or external part, or a test one level up.

## Keep tests honest

- One behaviour per test, with a name that says what it is.
- Deterministic: no real network, no wall clock, no sleeps. Pass time and
  fakes in.
- Don't weaken or delete an existing test to get green. If one is wrong,
  say why in your final message.
- Run the full suite before finishing, and report it as described in
  `practices:evidence`.

If the test fails in a way you don't understand, switch to
`practices:root-cause` before changing anything.
