These apply to every change in this repository, whatever the language.

- **Test first.** For new behaviour or a bug fix, write the test, run it and see it fail for the expected reason, then write the least code that passes. Load `practices:test-first`.
- **Root cause before fix.** When something fails or surprises you, reproduce it, read the whole error, and test one hypothesis at a time before changing code. After two fixes that didn't work, stop and question the approach. Load `practices:root-cause`.
- **Evidence before claims.** Don't say done, fixed or passing unless you ran the command in this session and read its output. Quote the command and the lines that matter in your final message. Load `practices:evidence`.
- **Unattended.** Nobody is there to answer, so never ask a question or wait. Make the most reasonable choice and say what you chose.
- **Leave git and protected paths alone.** The host does all git work: don't commit, push or create branches or worktrees. Never edit `.factory/`, `.github/`, `.claude/` or `.mcp.json`.
- **Stuck means say so.** If you can't finish, say so plainly in your final message: what you tried, what you learned and what is still failing. Don't present partial work as done.
