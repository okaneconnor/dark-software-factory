# practices

A dsf standards pack of working habits for any codebase: test first, find the
root cause before fixing, and show evidence before claiming success. It's an
ordinary Claude Code plugin, so it also works interactively:
`claude --plugin-dir examples/packs/practices`.

It is language-neutral and has no hooks. It's written for unattended agents:
nothing asks a person a question, and nothing commits, pushes, creates
branches or touches `.factory/`, `.github/`, `.claude/` or `.mcp.json`. An
agent that is stuck says so in its final message.

- `rules.md` is added to every agent's system prompt: the rules that always
  apply.
- `skills/test-first/` is loaded when behaviour is added or changed: write the
  test, see it fail for the right reason, make it pass, then tidy up.
- `skills/root-cause/` is loaded when something fails: reproduce, read the
  whole error, test one hypothesis at a time, and stop and rethink after two
  fixes that didn't work.
- `skills/evidence/` is loaded before reporting results: no claim of done,
  fixed or passing without a command run in this session, quoted in the final
  message.

## Turning it on

Add the pack to a repo's entry in the factory's config:

```toml
packs = ["practices"]
```

It combines with language packs, for example `packs = ["rust", "practices"]`.
