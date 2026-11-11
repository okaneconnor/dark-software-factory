---
name: root-cause
description: Find out why something fails before changing code. Use when a test, build, command or behaviour fails, errors out or surprises you, and before trying any fix.
---

# Root cause

Guessing at fixes wastes runs and hides the real problem. Work in this order.

1. **Reproduce it.** Find the exact command that shows the failure and run it.
   If you can't reproduce it, say so and gather more information; don't fix
   what you can't see. Prefer a small, fast reproduction, ideally a failing
   test (see `practices:test-first`).
2. **Read the whole error.** All of it, from the first line to the last, not
   just the final line. Note the file, line, message and any "caused by"
   chain. The first error is usually the cause; later ones are often fallout.
3. **Read the code path.** Open the code the error points to and follow the
   data back to where the bad value or state came from. Check recent changes
   in the area, including your own.
4. **Form one hypothesis.** Write down in a sentence what you think is wrong
   and what you would see if you were right.
5. **Test it by changing one thing.** Add a log line, run a narrower case,
   or make a single change. One variable at a time, so the result means
   something. If the hypothesis is wrong, undo the change and form another.
6. **Fix the cause, not the symptom.** Then add or keep a test that fails
   without the fix, and run the original reproduction and the wider suite.

## Two fixes that didn't work: stop

If two fixes in a row haven't worked, don't try a third. Step back and
question the approach:

- Is the hypothesis wrong? What did the failed fixes actually teach you?
- Are you fixing the wrong layer, or assuming something you haven't checked?
- Is the design itself making this hard, so a different approach is simpler?

Re-read the error and code from the start with those answers in mind, then
choose a new approach deliberately.

## If you are still stuck

Nobody is available to ask, so don't wait. Stop and report plainly in your
final message: the command that fails, the error, what you tried, what each
attempt showed, and what you think to check next. Don't leave half-applied
experiments behind, and don't claim the work is done.

## Don't

- Don't silence the error: no skipping tests, catching and ignoring, or
  loosening assertions to get green.
- Don't pile up several changes and see whether the failure goes away.
- Don't edit `.factory/`, `.github/`, `.claude/` or `.mcp.json` to work
  around a failure, and don't use git to bisect or stash; the host owns git.
  If the cause is in one of those paths, report it.
