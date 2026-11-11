# Roles target repo kit

Copy this kit into a repository to run each issue through distinct roles,
each with one job:

- **Planner** (`plan.md.j2`): reads the repo and breaks the issue into small
  tasks. Each names the files it touches, the test to write first and how to
  tell it is done. It returns `needs_decision` with questions when the issue
  is ambiguous.
- **Implementer** (`implement.md.j2`): works through the plan in order, test
  first for each step, running the relevant tests after each. Its final
  message lists each step with the test command and its result.
- **Fixer** (`fix.md.j2`): given the verify output or review findings, finds
  the root cause before changing code, deals with every finding (fixes it or
  explains why it is wrong) and never weakens or deletes a test.
- **Scope reviewer** (`scope-review.md.j2`): compares the diff only with the
  issue, its acceptance criteria and the plan. It reports what is missing and
  what was added that nobody asked for.
- **Quality reviewer** (`quality-review.md.j2`): checks correctness, tests,
  readability and the repo's own rules. Findings are tagged *must fix*,
  *should fix* or *nit*; only the first two request changes, and nits are
  listed in the summary.

## Flow

```
plan -> approve -> implement -> verify -> scope_review -> quality_review -> publish
                                  |            |                |
                                  +----------> fix <------------+
                                               |
                                               +-> verify
```

A failed verify goes to `fix`, as does a `changes_requested` from either
review. `fix` always goes back to `verify`. Visit limits: verify 5, fix 4,
each review 2. A review that runs out of visits publishes, as the built-in
workflow does. Two reviewers need no engine support: `fix` gets the findings
of whichever review last requested changes.

## Using it

From the root of the target repository:

```sh
cp -r /path/to/dsf/examples/target-repos/roles/.factory .
```

Then replace `.factory/verify.sh` with the repo's real checks: format, lint
and tests. The copy shipped here prints a message and fails on purpose, so a
repo can't publish with an empty gate. Edit the prompts to add the repo's
own conventions if you like.

The kit pairs well with the `practices` pack (`examples/packs/practices`),
which gives the agent engineering standards to follow, but it doesn't require
it.
