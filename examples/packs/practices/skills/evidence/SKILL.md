---
name: evidence
description: Back every claim of success with command output. Use before saying work is done, fixed, passing or working, and when writing the final message.
---

# Evidence

A claim is only as good as the output behind it. Before you say anything is
done, fixed, passing or working:

1. **Run the command in this session.** The one that checks the claim: the
   test, the build, the linter, the project's verify script, or the program
   itself. An earlier run, a run before your last edit, or "it should work"
   doesn't count. If you edited anything after the run, run it again.
2. **Read the output.** All of it, not just the exit status or the last line.
   Look for failures, warnings, skipped or zero tests, and errors that
   don't change the exit code.
3. **Check it covers the claim.** "The tests pass" says nothing about a
   behaviour no test exercises. For a bug fix, the check is the reproduction
   that failed before. For a feature, it is the new test or a real run.
4. **Quote it.** In the final message, give the command and the lines that
   matter, for example:

   ```
   $ <the command you ran>
   test result: ok. 42 passed; 0 failed
   ```

## Report faithfully

- If something failed, say so with the output, even when it looks unrelated.
- If you skipped a step, couldn't run a command (no network, missing tool) or
  only checked part of the change, say what and why.
- Never use "should", "probably" or "ought to" in place of a result. Either
  you ran it or you didn't.
- When it is done and verified, say so plainly without hedging.
- If you are stuck, say that plainly too: what fails, what you tried and what
  is left. Nobody is waiting to answer, so don't end with a question.

## Before you finish

- Run the project's full gate if it has one, and quote its result.
- Keep the message short: what changed, the evidence, anything not done.
- Don't include secrets or tokens that appear in output.
