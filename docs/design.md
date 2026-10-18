# dsf design

> **Status:** agreed design (October 2026). The spikes that tested its assumptions are recorded in [Spike findings](#spike-findings). When reality disagrees with this document, change the document in the same PR.

## 1. Purpose

dsf turns GitHub issues that a trusted human has approved into verified **draft** pull requests, unattended, inside a time window (by default overnight). It replaces a prototype made of a Python orchestrator, a bash worker entrypoint and a digest script. The prototype proved the idea, but it left the GitHub token inside the agent's container, retried from scratch after any interruption, and had no way to stage or gate the work.

dsf is deliberately small and single-user. The reference design is [Fabro](https://github.com/fabro-sh/fabro). dsf takes its patterns, not its code (§9).

## 2. Fixed decisions

| Decision | Detail |
|---|---|
| Output | **Draft PRs only.** Never merges; branch protection on target repos decides what lands. |
| Trigger | Issues labelled `factory:ready` in configured repos. Lifecycle: `factory:draft` → `factory:ready` (human) → `factory:running` → `factory:review` (draft PR open) or `factory:blocked` (reason in a comment). `blocked by #N` in the body makes an issue wait for #N to close. |
| Agent | Claude Code headless (`claude -p`), authenticated with `CLAUDE_CODE_OAUTH_TOKEN` from `claude setup-token` (subscription usage, not API keys). |
| Isolation | Every stage runs in a hardened, throwaway Docker container on a dedicated host. |
| Credentials | **The agent never holds a GitHub credential.** The host clones, commits and pushes. |
| GitHub identity | A private **GitHub App**, installed only on the configured repos. dsf mints installation tokens that last an hour, each narrowed to one repository and the permissions one step needs, so there is no long-lived token to rotate and everything dsf does shows as its `[bot]` account. |
| Done | The target repo's `.factory/verify.sh` exits 0. Failures feed a bounded fix loop. |
| Limits | A usage-limit hit requeues the issue and pauses claiming until the limit resets. |
| Schedule | Claims only inside a configurable window (default 23:00–07:00 local wall-clock time; it may span midnight, and equal start and end mean always open); running work finishes. A systemd timer runs `dsf tick` every few minutes; there is no long-running daemon. One worker by default, maximum two. |
| Reporting | A morning digest (default 07:00) to a Microsoft Teams Workflows webhook, or to stdout. |
| v1 scope | Staged pipeline, per-repo workflows, optional plan-approval gate, read-only web runs board. |

## 3. Architecture

One binary, `dsf`. In production a systemd timer runs `dsf tick` every five minutes as an unprivileged `factory` user in the `docker` group. Membership of that group is root-equivalent on the host, so the host should be a dedicated VM; that VM boundary is what contains it.

A tick is one pass over everything dsf looks after, and it keeps nothing in memory between passes. What it decides from is in the store or on GitHub, so a crash, a reboot or a new binary between ticks loses nothing. systemd never starts a tick while the last one is still running, and a tick that finds `dsf run` holding the state directory does nothing.

- **Scheduler** (`dsf tick`). Recovers from any crash, lists each repo's queue, puts stale labels right and expires unanswered plans; then, inside the window, carries on paused runs and claims issues, up to the worker limit, until nothing is left (§5, Scheduling).
- **Runs.** Each executes its workflow stage by stage, inside the tick's process.
- **Egress proxy.** The agent containers' only route to the internet (§6). It runs as a sidecar container from the dsf binary itself, attached to both Docker's default bridge and the agents' internal network, so agent traffic stays on Docker networks and never meets the host's firewall.
- **Digest.** A second timer sends the morning summary.
- **Board.** A separate, read-only service on `127.0.0.1`, exposed by the operator if wanted (e.g. `tailscale serve`). It never holds the GitHub key or the Claude token.

Fabro runs a long-lived server with its own scheduler loop, because it serves an API to many users. dsf is one person's overnight batch job, which is what timers are for: there is less to go wrong, `systemctl list-timers` and `journalctl` show what happened, and stopping the timer is an off switch. [`examples/systemd/`](../examples/systemd/) has the units.

CLI commands for operators and development:

| Command | Purpose |
|---|---|
| `dsf run owner/repo#N` | Run one issue in the foreground |
| `dsf run --local <repo> --issue-file <file>` | Run against a local repo with no GitHub, for development and tests. The result is pushed to `factory/issue-<n>` in that repo, or `--publish-to`. Exit codes: 0 published, 2 blocked, 3 suspended, 4 publish failed. |
| `dsf run --resume <run> [--approve]` | Resume a suspended or interrupted local run; `--approve` passes a waiting plan gate |
| `dsf tick` | One pass of the factory (§5, Scheduling). The timer runs it. |
| `dsf status` | The queue, runs in progress and the last week's results, read from the store alone |
| `dsf pause [reason]`, `dsf unpause` | Stop and restart claiming. Runs already going carry on. |
| `dsf doctor` | Preflight: Docker, tokens, image, config, labels |
| `dsf workflow validate <file> [--config <file>]` | Validate a workflow file, applying the operator's caps when a config is given |
| `dsf workflow default` | Print the built-in workflow, a starting point for a repo's own |
| `dsf config validate <file>` | Validate a config file, including its time zone |
| `dsf labels init` | Create the label set in configured repos |
| `dsf digest --dry-run` | Render the digest without sending it |

## 4. Pipeline and per-repo workflows

A workflow is a small graph of **stages**. Each stage has a kind and routes on its **outcome label**:

| Kind | Does | Outcome labels |
|---|---|---|
| `agent` | Runs Claude Code (invocation in §6.4). `mode = "read"` gives it only Read, Grep and Glob over a read-only worktree; `mode = "write"` adds Edit, Write and Bash. A `schema` makes Claude return structured output via `--json-schema`, whose `outcome` field becomes the label. | `success` / `failure`, or the schema's labels |
| `command` | Runs a command, e.g. `.factory/verify.sh`, in a container with no credentials | `success` / `failure` |
| `gate` | Waits for a human decision on the issue (§5) | `approved` / `rejected` |

The terminals `@publish` and `@blocked` end the run.

The built-in default workflow:

```toml
version = 1
start = "plan"

[stages.plan]
kind = "agent"
mode = "read"
prompt = "plan.md.j2"
schema = "plan"
on = { ready = "approve", needs_decision = "@blocked" }

# Only active when the repo policy is `always`, or the issue carries
# factory:approve-plan; otherwise it passes straight through as `approved`.
[stages.approve]
kind = "gate"
on = { approved = "implement", rejected = "@blocked" }

[stages.implement]
kind = "agent"
mode = "write"
prompt = "implement.md.j2"
on = { success = "verify", failure = "@blocked" }

# Always runs the .factory/ snapshot taken at the run's base commit.
[stages.verify]
kind = "command"
run = ".factory/verify.sh"
timeout = "30m"
network = "none"
max_visits = 5
on = { success = "review", failure = "fix" }

# Fresh context plus a host-built handoff: plan, diff stat, verify output tail.
[stages.fix]
kind = "agent"
mode = "write"
prompt = "fix.md.j2"
max_visits = 3
on = { success = "verify", failure = "@blocked" }

# Fresh context, read-only. Advisory: a quality check, not a security control.
[stages.review]
kind = "agent"
mode = "read"
prompt = "review.md.j2"
schema = "review"
max_visits = 2
on = { approve = "@publish", changes_requested = "fix" }
exhausted = "@publish"
```

**Rules:**

- **Stages.** Names match `[a-z][a-z0-9_-]*`, at most 32 characters. Unknown keys anywhere are errors, which is also what stops a repo from setting images, mounts or limits.
- **Agent stages** need `mode` and `prompt`; `schema` is optional. There are two schemas: `plan`, with outcomes `ready` and `needs_decision`, and `review`, with `approve` and `changes_requested`.
- **Command stages** run one script under `.factory/`, with no shell and no arguments, from the snapshot of `.factory/` taken at the run's base commit. Anything else, such as `make test`, is refused, because the agent could edit the `Makefile` and so weaken the check that judges its work. `timeout` defaults to 30 minutes. `network` defaults to `none`; `proxy` opts in to the egress proxy.
- **Visit caps.** Each stage has a `max_visits` (default 1). When a stage would be entered beyond its cap, the run goes to the stage's `exhausted` target (default `@blocked`), which must be `@publish` or `@blocked`. Total work is therefore bounded by Σ `max_visits`, so every valid workflow terminates; a property test checks this over arbitrary graphs. The default workflow's budget is 13, and its longest possible run is 12 stage visits.
- **Validation.** Every referenced stage exists, every outcome label a stage can produce has a route and no other label does, every stage is reachable from `start`, and `@publish` is reachable through a route or an `exhausted` target. Validation reports every problem at once.
- **Repeat-failure breaker.** If a stage fails twice in a row with the same failure signature, the run goes to `@blocked` rather than spending the remaining visits. The signature is an FNV-1a hash of the last 40 lines of output after removing colour codes, timings, timestamps, addresses and temporary paths, so identical failures match while a change in what failed doesn't. Any other result from the stage resets it.
- **Per-repo overrides.** A target repo may provide `.factory/workflow.toml` and `.factory/prompts/*.md.j2`. dsf reads them from the **default branch at claim time** and snapshots them into the run, so the agent cannot change the rules it is running under.
- **Clamping.** Repo files may only tune what the operator allows. Visit counts and command timeouts above the operator's caps (by default 5 visits per stage and 60 minutes) are lowered, and the run reports each change. A workflow with more stages than the cap (default 16) is refused. Images, mounts, resource limits, agent-stage networking and protected paths are not settable from a repo at all. Publishing is built in and not configurable.
- **Prompts** are minijinja templates named `[a-z0-9_-]+.md.j2`, looked up in the run's snapshot, so a name can't point anywhere else. The defaults are embedded in the binary.

## 5. Run state, suspension and resume

State lives in SQLite as plain rows, updated in one transaction at each stage boundary. Stage boundaries are the only points a run can stop and resume.

| Table | Purpose |
|---|---|
| `runs` | Status, suspend reason, `resume_after`, next stage, visit counts, `base_sha`, `checkpoint_sha`, the workflow snapshot and hash, an issue snapshot (title, body, author, `updated_at`), the plan, the approval comment id, the last failure signature, the PR number |
| `stage_attempts` | One row per stage attempt: visit number, status, outcome, failure class, container id, before and after SHAs, exit code, usage, structured output |
| `events` | A human-readable timeline for the board and digest. Not the source of truth. |
| `kv` | Factory-wide state: whether claiming is paused and why, `backoff_until`, consecutive usage-limit hits, and the last tick's record (the queue it saw and what it did) for `dsf status` and the board |

A run moves `queued → running → suspended{approval | usage_limit | interrupted} → queued → …`. It ends in one of `published`, `blocked`, `publish_failed` or `canceled`.

Resuming a run always removes any container still labelled with the run, then resets the worktree to `checkpoint_sha`, cleans it, and executes `next_stage`. The order matters: an orphaned agent must not keep writing into a worktree that has just been reset.

- **Crash or restart.** Every tick starts by recovering. Attempts left running are marked `interrupted`, their runs suspended and their containers removed, so an orphaned agent can't carry on unseen; a run that stopped before its first stage has nothing to continue and is canceled. An interrupted run whose issue still reads `factory:running` carries on at the next tick inside the window, re-running the interrupted stage. An interruption does not consume a visit, but two in a row block the run. The interruption count is saved before the stage re-runs, so repeated crashes are counted. Tests kill `dsf run` with SIGKILL mid-stage and resume it, and drop a tick mid-stage and let the next tick carry on.
- **Usage limit.** Detected from the stream (spike S4): a `rate_limit_event` whose status is `rejected`, or an API error of kind `usage_limit_reached`, both carrying the reset time. The run is suspended and the issue relabelled `factory:ready`, with claiming backing off until the limit resets: two minutes after the reported reset time, capped at eight days because weekly limits can be days away. Without a reset time the pause is 30 minutes, doubling with each consecutive hit up to 4 hours. On the next claim the suspended run resumes if the issue is unchanged since its snapshot; otherwise a fresh run supersedes it.
- **Approval gate.** The plan is posted as an issue comment carrying a `dsf:run=<id>` marker, and the issue is labelled `factory:awaiting-approval`. The run continues only when `factory:plan-approved` is added *after* that comment, by an allowlisted user. Rejection is a human relabelling `factory:ready` after editing the spec, which produces a fresh run. An unanswered gate expires to `@blocked` after a configurable time (default 3 days), which every tick checks, inside the window or not. Silence never approves.

A unique index on `(repo, issue)` over non-terminal runs prevents double claims. Resumed runs still respect the window.

**Claiming.** Issue listings can be stale, so every decision starts from a fresh read of the issue and its label history, never from a list. dsf records the run first (the unique index), then removes `factory:ready`. Removing a label that is already gone fails with 404, so exactly one claimant succeeds; a loser cancels its run and touches nothing else. Only then does dsf add `factory:running`. When the run ends, dsf moves the issue to its new lifecycle label and comments: the draft PR's link, the reason it stopped, or when it will continue.

**Scheduling.** A tick works in this order:

1. Recover from any crash (above).
2. For each repo, list the `factory:ready` issues, saving the list for `dsf status`, and put right any issue labelled `factory:running` that no unfinished run is working on: it moves to wherever its last run ended, or to `factory:blocked` with a comment.
3. If claiming is allowed, look in on paused runs first (an interrupted run carries on; an approved plan continues; a closed issue cancels its run), then claim queued issues, oldest first, taking turns between repos. If claiming isn't allowed, only waiting plans are checked, so they expire on time.

Claiming is allowed inside the window, unless the operator has paused it (`dsf pause`) or a usage limit started a backoff. It is checked again before each claim, so the window closing or a limit being hit stops new work while runs already going finish. A run that stops because Claude refused its token pauses claiming until the operator runs `dsf unpause`, rather than failing every issue in the queue the same way.

An issue carries exactly one lifecycle label: `draft`, `ready`, `running`, `awaiting-approval`, `review` or `blocked`, each prefixed `factory:`. An issue with more than one is left alone until a human tidies it. Claiming also removes any leftover `factory:plan-approved`, so it can't approve the new run's plan.

## 6. Security model

**Threat model.** The agent is capable and may be steered by hostile text: a prompt injection in an issue, a comment, a dependency or the repo itself. Assume it will try to:

- obtain credentials;
- execute code on the host;
- weaken the checks that judge its work;
- exfiltrate data.

The controls below are what make dsf safe to leave running unattended. [`CLAUDE.md`](../CLAUDE.md) lists them as invariants.

1. **Host-side git with an isolated git directory.**
   - Each run has `runs/<id>/repo.git` (non-bare) and `runs/<id>/work`. Containers mount the worktree, and `.git` only read-only. The agent can therefore never write hooks, config, refs, objects or the index, so nothing it does can make a host git command run its code (`core.hooksPath`, `core.fsmonitor`, filter drivers, `includeIf`, `insteadOf` and similar).
   - Every host git call goes through one `HostGit` wrapper:
     - fixed argv;
     - a cleared environment (`GIT_CONFIG_NOSYSTEM=1`, `GIT_CONFIG_GLOBAL=/dev/null`, `GIT_ATTR_NOSYSTEM=1`, a nonexistent `HOME`, `GIT_TERMINAL_PROMPT=0`);
     - `-c core.hooksPath=/dev/null -c core.fsmonitor=false -c commit.gpgsign=false`, plus no credential helper, no `ext::` transport and `transfer.fsckObjects=true`;
     - `--no-verify`.
   - Remotes may only be `https://` or local paths, and pushes only go to `factory/` branches.
   - Before `git add`, the worktree is walked without following symlinks, and any `.git` below the top level is refused, because adding a nested repository makes git read its files. The top-level `.git` is only the empty mount point; host git never looks at it.
   - Tests plant a config, filter driver and hooks in a worktree `.git`, check that plain git springs them, and check that `HostGit` never does.
   - Diffs are computed between recorded SHAs (`base_sha`, `checkpoint_sha`), never against refs the agent could influence. Verify runs against a clean export of the checkpoint commit (written through a private index, so the run's own index is untouched), with `.factory/` replaced by the base snapshot. It therefore checks exactly what would be published, not whatever else is lying in the worktree.
   - Pushes use an explicit URL and `<sha>:refs/heads/factory/issue-<n>`. Authentication is an installation token for that one repository (§6.8), sent as a URL-scoped `http.extraheader` passed through `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_*` environment variables, so it is never written to disk.
2. **No symlink following.** The host never follows symlinks in a worktree. Agent outputs are read from Claude's stream-json result or from git objects (`git cat-file`). Trees containing gitlinks, `.gitmodules` or symlinks that escape the repo are refused.
3. **Protected paths.** After **every** agent stage, the change since `base_sha` is checked against `.factory/`, `.github/`, `.claude/`, `.mcp.json` and any operator additions. An entry covers the path itself and everything beneath it, ignoring ASCII case. A violation is deterministic and blocks the run immediately. The `.factory/` snapshot that command stages run is extracted from git objects at `base_sha`, and a snapshot containing a symlink is refused: one could point at a file the agent is free to edit.
4. **Hardened containers.**
   - Settings: the factory user's uid (so bind-mounted files stay its own), `cap-drop ALL`, `no-new-privileges`, a read-only root filesystem with tmpfs `/tmp` and `HOME`, PID, memory and CPU limits with no extra swap, a private IPC namespace, an init process, and no image entrypoint wrapping the command. The runner fixes all of these itself; a stage can't ask for anything else. It also refuses to mount the Docker socket or use the `host` network. Docker tests check each property from inside a container, including that the environment seen by PID 1 is exactly the one passed in.
   - Environment: cleared, with an allowlist. Agent containers get `CLAUDE_CODE_OAUTH_TOKEN`, the proxy settings, `HOME`, `DISABLE_AUTOUPDATER=1` and `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`; verify containers receive no secrets.
   - Labels: `dsf.run` and `dsf.attempt`, used for reconciliation and cleanup.
   - Invocation (spike S1): every agent stage runs `claude -p --restricted --strict-mcp-config --no-session-persistence --output-format stream-json --verbose` with an explicit `--tools` list and `--model`. `--restricted` ignores the repo's and user's settings files, so a repo's `.claude/settings.json` hooks never run; it also refuses `bypassPermissions`, confines file tools to the working directory, and blocks writes to git and settings files. Read stages get `--tools Read,Grep,Glob`. Write stages get `--tools Read,Grep,Glob,Edit,Write,Bash --permission-mode acceptEdits --allowedTools Bash`. Without `--tools`, the CLI's default set includes web, scheduling and remote-trigger tools. The prompt is the last argument, after `--`, so nothing in it can be read as a flag, and it is capped at 120 KiB because Linux limits a single argument to 128 KiB.
   - Reading the stream: a successful `result` completes the stage. Otherwise, an authentication or account error pauses the factory for a human, a usage limit, throttle or server error suspends the run until the reset time (or a backoff), and anything else, including a stream that ends without a result, fails the stage.
   - Claude Code is never run with `--bare`, which ignores OAuth.
5. **Egress.**
   - Agent containers sit on an internal Docker network whose only exit is dsf's allowlisting CONNECT proxy (`HTTPS_PROXY`). The internal network has no DNS of its own, so the proxy also does all name resolution.
   - The proxy is `dsf proxy`, run from the dsf image as a hardened sidecar. It accepts only `CONNECT` to allowlisted host names, never IP literals, on port 443, and passes TLS through untouched. A name is refused if it resolves only to private, loopback, link-local or shared (`100.64.0.0/10`, which Tailscale uses) addresses, so an allowlisted name can't be pointed at the LAN. Each resolved address is tried in turn, because Docker networks often have no IPv6 route. The sidecar carries a `dsf.role` label rather than `dsf.run`, so cleaning up after runs never removes it, and it is replaced whenever its image or allowlist changes. The allowlist is `api.anthropic.com`, which spike S3 showed is all Claude Code needs, plus the operator's package registries.
   - Verify has no network by default; a repo may opt in to the proxy.
   - DNS filtering on the host is a useful second layer, not a substitute.
6. **Secret hygiene.**
   - Tokens are held in `secrecy` types.
   - A masker redacts every known secret value, and the encoded forms dsf sends such as the basic-auth header, from everything that leaves the host: logs, board pages, GitHub comments and PR bodies, digests. It also redacts anything shaped like an Anthropic or GitHub token, which catches secrets the agent found rather than ones dsf holds.
   - Before any push, the exact diff is scanned for literal secret values.
7. **Trusted input only.**
   - dsf acts only on configured repos, and only on issues whose author **and** the user who applied `factory:ready` are on the operator allowlist, verified through the issue events API. Logins are compared case-insensitively, as GitHub does. This matters because repos, including this one, may be public.
   - The labeller is the latest *person* to add `factory:ready`. dsf's own bot re-adds the label when a usage limit requeues an issue; that must neither count as trust nor hide who gave it.
   - Only allowlisted users' comments are included in prompts, newest first up to 16 KiB. Everyone else's, and dsf's own, never reach an agent.
   - Plan approval counts only if an allowlisted user added `factory:plan-approved` at or after the plan comment was posted, and the label is still there.
   - If the issue body changes after claiming, the run re-plans.

8. **GitHub App credentials.**
   - The app's private key is dsf's only long-lived GitHub secret. It stays on the host, as a systemd credential or a file only its owner can read (dsf refuses one that others can), and it only ever signs JWTs that last nine minutes.
   - Every API call and git operation uses an installation token that lasts an hour and is narrowed to one repository and one step's permissions: `issues: write` to read, label and comment on issues; `contents: read` to clone; `contents: write` and `pull_requests: write` to push the branch and open its PR. Claiming an issue can't push code, and publishing can't reach another repository.
   - Tokens are added to the masker as they are minted, together with the header that carries them.
   - The app has no `workflows` permission, so GitHub itself refuses a push that touches `.github/workflows/`, behind dsf's own protected-path check.
   - The API client never follows redirects, and pagination never follows a `Link` outside the API, so a token can't be sent anywhere else.

**Accepted residual risks:**

- The Claude token is necessarily inside the agent container, so a compromised agent can misuse it within the subscription's limits. The egress allowlist limits where it could send it.
- The `factory` user's Docker access is root-equivalent on the host. The dedicated VM is the containment.
- Verify runs the repo's own tests, which the agent may edit, for example by deleting a failing test. Protecting `.factory/` stops it weakening the gate itself, not the tests the gate runs. The review stage and the human reviewing the draft PR are the check on that, which is one reason dsf never merges.

## 7. Board, digest and configuration

- **Board.** A separate read-only service: axum and server-rendered HTML (maud), refreshed by htmx polling every few seconds. It has no mutating routes, and every view is masked. Its overview is a kanban of the issue lifecycle (ready, running, awaiting approval, review and blocked) under the window and any pause or backoff; `dsf status` prints the same columns as text. Run detail shows the stage timeline, logs, diff stat and PR link, and there is a history page. Cards move only when a person relabels the issue on GitHub: the trust checks (§6.7) rely on knowing which human added a label, and a board that relabelled issues would act as the app.
- **Digest.** Built from SQLite: draft PRs opened, runs blocked with reasons, queue depth and any active backoff. It is posted as an Adaptive Card.
- **Configuration.** A single file, conventionally `/etc/dsf/config.toml`. [`examples/config.toml`](../examples/config.toml) documents every key with placeholder values, and `dsf config validate` checks a file. It sets:
  - the time zone, window (default 23:00–07:00), digest time (07:00) and worker count (1 or 2); how often a tick runs is the timer's setting;
  - repos and per-repo approval policy (`never`, `label` or `always`; default `label`);
  - the user allowlist, and the GitHub App's ID;
  - caps;
  - extra egress hosts (the Anthropic API is always allowed);
  - the worker image;
  - extra protected paths;
  - the state directory, and how long a plan waits for approval (default 3 days).

  Unknown keys are errors, so a secret pasted into the wrong place is rejected rather than silently kept. Secrets are files: under systemd, `$CREDENTIALS_DIRECTORY/claude-oauth-token` and `$CREDENTIALS_DIRECTORY/github-app.pem`; otherwise the same names in `~/.config/dsf/`.

- **Setting up the GitHub App.** Once per operator:
  1. Create a GitHub App (Settings → Developer settings → GitHub Apps) with webhooks off, installable only on your account, and repository permissions Contents, Issues and Pull requests set to read and write. Nothing else; in particular, not Workflows.
  2. Install it with *Only select repositories*, choosing exactly the configured repos.
  3. Generate a private key, save it as `github-app.pem` with mode 600, and put the App ID in the config's `github.app_id`.
  4. Run `dsf labels init owner/repo` for each repo, which also checks the app is installed there.

## 8. Repo layout and stack

```
crates/dsf-core/      pure domain model (no IO, enforced in CI)
crates/dsf/           the binary: cli, scheduler, status, factory (GitHub side), github, engine, hostgit,
                      sandbox, agent, proxy, store; digest and web to come
defaults/             default workflow and prompt templates (embedded)
crates/dsf/tests/fake-agent/  test-only: a shell `claude` that acts on `fake-<stage>:` lines in the
                      issue and prints stream-json, in a small Alpine image
worker/Dockerfile     agent and verify image: Node, pinned Claude Code, git, make, Python
docker/dsf.Dockerfile dsf itself, used for the proxy sidecar
examples/             config, systemd units and target-repo .factory/ examples
docs/                 this document and operations notes
.factory/verify.sh    the gate; also what CI runs
```

| Need | Crate |
|---|---|
| Async runtime, HTTP server, HTML | tokio, axum, maud (+ vendored htmx) |
| Docker | bollard |
| GitHub | reqwest (rustls) with about ten hand-written endpoints |
| Storage | rusqlite (bundled) + rusqlite_migration |
| CLI, config, templates | clap, serde, toml, minijinja |
| Errors, logging | thiserror, anyhow, tracing |
| Secrets, time | secrecy, jiff (DST-correct scheduling) |
| Tests | insta, wiremock, proptest, tempfile, assert_cmd |

## 9. What comes from Fabro, and what doesn't

**Adopted:**
- Outcome-routed stages with visit caps.
- Bounded verify→fix loops and a repeat-failure breaker.
- Fail-closed human gates.
- A git checkpoint commit per stage.
- Publishing as a separate, non-agent step with its own `publish_failed` outcome.
- Per-stage artefacts.
- An allowlisted environment for every container.
- A secret masker.
- A `doctor` command.
- Scripted fake agents and snapshot tests.
- A scheduler that decides from the clock it is given, so tests can say when "now" is.

**Deliberately not adopted:**

| Not adopted | Why |
|---|---|
| The DOT/Petri graph engine | A TOML stage graph is enough |
| Multi-provider model routing and failover | One subscription, so pausing on limits is the right behaviour |
| Cloud sandboxes | Out of scope |
| Multi-user auth and a GitHub App wizard | Single user |
| An SPA and OpenAPI codegen | Server-rendered HTML is enough |
| Pushing from inside the sandbox with a token present | Breaks the credentials invariant (§6) |
| Repo-defined hooks or MCP servers executed on the host | Host code execution from repo-controlled files |
| Auto-merge | Draft PRs only |
| A long-running server with its own scheduler loop | One user's batch job: a systemd timer runs `dsf tick` instead (§3) |

## Spike findings

The four open questions were answered on 2026-10-18 with Claude Code 2.1.292 in the hardened container shape above. Sanitised captures are in [`crates/dsf/tests/fixtures/stream-json/`](../crates/dsf/tests/fixtures/stream-json/).

| # | Question | Answer |
|---|---|---|
| S1 | Does Claude Code authenticate with `CLAUDE_CODE_OAUTH_TOKEN` in the hardened container, and what do `--json-schema` and `--restricted -p` emit? | **Yes.** It works with a read-only root, tmpfs `HOME`, non-root user and `cap-drop ALL`. |
| S2 | With `.git` read-only, does Claude Code still get git context, and do host commits work? | **Yes to both.** |
| S3 | Does Claude Code work through an internal network plus a CONNECT proxy, sidecar or host-bound? | **Yes, as a sidecar.** Only `api.anthropic.com` is needed. |
| S4 | What does a usage-limit error look like, and does it carry a reset time? | **A typed error with the reset time.** Usage is also reported live on every run. |

**S1: authentication and output.**
- `system/init` reports `apiKeySource: "none"`, which confirms OAuth is in use.
- The stream is `system/init`, then `assistant` and `user` (tool result) messages, then `rate_limit_event`, then `result`.
- The `result` message carries `subtype`, `is_error`, `num_turns`, `result`, `usage`, `total_cost_usd`, `permission_denials`, `stop_reason` and `terminal_reason`. The cost is notional on a subscription, but useful in the digest.
- With `--json-schema`, Claude gets a `StructuredOutput` tool. The validated object arrives as `result.structured_output`, with the same JSON as text in `result`. dsf reads the stage outcome from `structured_output.outcome` and never parses prose.
- With `--restricted --tools Read,Grep,Glob`, the tool list is exactly Glob, Grep and Read, plus `StructuredOutput` when a schema is given.
- The write-stage flags in §6.4 ran edits and shell commands with no prompts and no permission denials.
- The CLI defaults to a model of its own choosing, so dsf always passes `--model`.

**S2: read-only `.git`.**
- Claude Code starts normally with `.git` mounted read-only. `git status`, `log` and `diff` work in the container.
- `git commit` fails with `Unable to create '/work/.git/index.lock': Read-only file system` (exit 128). Claude reports the failure rather than retrying.
- Writing a hook or git config fails the same way.
- Host-side `HostGit`-style commands see the agent's changes and can commit them.
- Docker creates an empty `.git/` directory in the host worktree as the mount point. Git ignores it.
- A hostile `.gitattributes` filter does nothing, because filter drivers need config the agent can't write.
- To re-check on the Linux VM in P6: these runs were on macOS, where Docker Desktop masks file ownership, so git's "dubious ownership" check couldn't be exercised. Containers run as the `factory` user's uid, so ownership should match.

**S3: egress.**
- Claude Code honours `HTTPS_PROXY`. With only `api.anthropic.com` allowed, it completes normally.
- With default settings it also tries to send logs to Datadog (`http-intake.logs.us5.datadoghq.com`); the proxy refuses this with no ill effect.
- With `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` it tries nothing else, and analytics report as disabled. `rate_limit_event` still arrives.
- From the internal network, direct HTTPS fails because there is no DNS and no route, and the proxy answers 403 to a host that isn't allowlisted.
- The proxy runs as a sidecar container attached to both networks (§3). Traffic between the agent and the proxy stays on Docker networks, so the host firewall's inbound policy never applies to it, unlike a proxy bound on the host.

**S4: usage limits.**
- Every run emits `rate_limit_event`. Its `rate_limit_info` holds:
  - `status`: `allowed`, `allowed_warning` or `rejected`;
  - `resetsAt`: unix seconds;
  - `rateLimitType`: `five_hour`, `seven_day`, `seven_day_opus`, `seven_day_sonnet`, `seven_day_overage_included` or `overage`;
  - `utilization`, plus overage fields;
  - `unifiedWindows`, giving `utilization` and `resetsAt` for the five-hour and seven-day windows.
- A refused request ends in an API error of kind `usage_limit_reached`, with `error: "rate_limit"`. Its `api_error_params.rate_limit_info` has the same shape, with status `rejected`.
- This comes from the CLI's own message schema; a real refusal wasn't triggered, to avoid spending the plan. dsf stores every stage's raw stream, so the first real one becomes a fixture.
- Because usage is visible on every run, a *headroom* setting is possible: stop claiming while either window's utilization is above a threshold, leaving the rest of the plan for interactive use. Proposed for P4.

## Phases and exit criteria

| Phase | Exit criteria |
|---|---|
| S — spikes | The four questions above answered here; fixtures committed |
| P0 — foundations | CI green; `.factory/verify.sh` passes |
| P1 — core model | Default workflow validates; property test shows termination within Σ `max_visits`; routing, clamping and DST window tests; `dsf-core` has no IO dependencies |
| P2 — local engine | A fake agent in real Docker produces checkpoints, the verify/fix loop and a review, and publishes to a `file://` remote. The hostile-worktree suite passes (hooks, fsmonitor, filters, symlink escapes, verify tampering, `.gitmodules`, secrets in diffs are all refused). A kill-and-resume test passes. No `GH_`/`GITHUB_` variables in any container. |
| P3 — GitHub | Mocked-API suite covers claiming, the allowlist, blocked-by, comment filtering, idempotent publishing and a stale label list (no double claims). One real draft PR on a sandbox repo. |
| P4 — unattended running | Clock-injected scenario tests for `dsf tick`: the window, pausing and backoff, usage-limit requeue and resume, approval expiry, crash recovery and label repair. Digest snapshot. `dsf doctor` passes. |
| P5 — packaging | A tag publishes a static x86_64 musl binary with checksums, and a worker image with a pinned Claude Code version |
| P6 — first deployment | A real overnight run produces a draft PR. An agent container provably cannot reach a non-allowlisted host. The digest arrives. dsf opens its first draft PR against itself. |
| P7 — board | The kanban overview and run pages render as HTML snapshots from a fixture database; a route-enumeration test finds no mutating routes; logs are masked |
