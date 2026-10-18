# dsf — a dark software factory

`dsf` turns GitHub issues you have approved into verified **draft** pull requests, unattended, overnight. It drives [Claude Code](https://claude.com/claude-code) in headless mode inside locked-down containers that never hold a GitHub token, and it treats a script in the target repo as the only definition of "done".

> **Status: pre-alpha, under construction.** Nothing here runs yet. The design is in [`docs/design.md`](docs/design.md) and progress is tracked in the [roadmap](#roadmap).

## How it works

```
issue labelled factory:ready (by an allowlisted user)
   │  claim → factory:running
   ▼
 plan ──▶ [approval gate] ──▶ implement ──▶ verify ──▶ review ──▶ publish
                                             ▲    │       │
                                             └ fix ◀──────┘
   │
   ├─ pass → draft PR, factory:review
   └─ fail → comment with the reason, factory:blocked
```

- **plan:** reads the repo and writes a plan, without editing anything. If the spec needs a decision, it stops and asks on the issue rather than guessing.
- **approval gate:** optional. The plan is posted on the issue, and the run waits for `factory:plan-approved`. Silence never counts as approval.
- **implement / fix:** Claude Code edits the code. After every stage the host commits a checkpoint on the run branch.
- **verify:** runs the target repo's `.factory/verify.sh` in a separate container that holds no credentials. A failure feeds its output back to a bounded fix loop.
- **review:** a fresh-context, read-only pass that checks the change against the issue.
- **publish:** the host pushes the branch and opens a **draft** PR. The agent takes no part in this step.

Each target repo can tune this pipeline in `.factory/workflow.toml`, within limits set by the operator.

## Principles

- **Draft PRs only.** Nothing is ever merged; branch protection decides what lands.
- **The agent never holds a GitHub token.** The host clones, commits and pushes; containers get no GitHub credentials at all.
- **`.factory/verify.sh` defines done.** It runs in the target repo, and the agent cannot change it mid-run.
- **Fail closed.** Silence, timeouts and errors are never mistaken for approval or success.
- **Bounded.** Every loop has a visit cap, and usage limits pause the factory instead of burning retries.
- **Small and single-user.** One static binary, one config file, self-hosted. Claude Code is authenticated with `claude setup-token`; no API keys are involved.

## Non-goals

- Auto-merge, or any route to `main` that bypasses review.
- Multiple users or tenants, or a hosted service.
- Routing work across several LLM providers.
- Cloud sandboxes.
- A general-purpose workflow engine.

## Roadmap

| Phase | Scope | Status |
|---|---|---|
| S | Spikes: Claude Code in a hardened container, read-only `.git`, egress proxy, usage-limit errors | Next |
| P0 | Repo foundations: workspace, CI, gate, docs | **In progress** |
| P1 | Core model: workflows, routing, labels, scheduling rules | Planned |
| P2 | Local engine: host git, sandbox, agent adapter, verify loop, store | Planned |
| P3 | GitHub: claiming, labels, publishing draft PRs | Planned |
| P4 | Daemon: scheduler, night window, approval gate, digest | Planned |
| P5 | Packaging: release binary and worker image | Planned |
| P6 | First deployment and dogfooding | Planned |
| P7 | Read-only web runs board | Planned |

## Development

```
.factory/verify.sh    # fmt, clippy -D warnings, tests: the same gate CI runs
```

The toolchain is pinned in `rust-toolchain.toml`. Contributor and agent conventions are in [`CLAUDE.md`](CLAUDE.md).

## Acknowledgements

- **[Fabro](https://github.com/fabro-sh/fabro)** (MIT) is the reference design. dsf adopts its ideas of outcome-routed stages, bounded verify-and-fix loops, per-stage git checkpoints and publishing as a step separate from the agent. dsf is a fresh, much smaller implementation and contains no Fabro code.
- **StrongDM's Attractor** is the graph and outcome model that Fabro builds on.

## License

[MIT](LICENSE)
