# stream-json fixtures

Real output from `claude -p --output-format stream-json --verbose`, captured on 2026-10-18 with Claude Code 2.1.292 during the spikes in [`docs/design.md`](../../../../../docs/design.md#spike-findings). Each ran in the hardened container shape dsf uses: read-only root, tmpfs `HOME`, non-root user, `cap-drop ALL`, OAuth token.

Sanitised before committing: session, message, request and tool-use ids, timestamps, and the account's usage figures and reset times are replaced with fixed placeholders. Tool-use ids still pair up with their results. Everything else is verbatim.

| File | Stage shape | Shows |
|---|---|---|
| `basic.jsonl` | One-turn `-p` through the egress proxy, `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` | The minimal sequence: `system/init`, `assistant`, `rate_limit_event`, `result` |
| `read-stage-structured-output.jsonl` | `--restricted --tools Read,Grep,Glob --strict-mcp-config --json-schema <plan schema>`, worktree read-only | Read-only exploration, then `StructuredOutput`; the plan lands in `result.structured_output` |
| `write-stage-readonly-git.jsonl` | `--restricted --tools Read,Grep,Glob,Edit,Write,Bash --permission-mode acceptEdits --allowedTools Bash`, `.git` read-only | An edit, `git status`/`git diff`, and a `git commit` failing on the read-only `.git` |

There is no usage-limit capture yet. Its shape is documented in the design from the CLI's schema; the first real one should be added here.
