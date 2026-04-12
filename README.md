# Ralph

Ralph is a workflow runner for iterative coding-agent loops.

Think of it as the Ralph Wiggum technique packaged into named workflows: study durable project memory, choose one high-leverage action, do the work, record what changed, and either loop again or stop. Ralph runs in the terminal by default, with guided interactive steps where the workflow needs operator input.

## Ralph Philosophy

- Iteration beats one-shot prompting. Ralph is for repeated loops that tighten the repository over time, not for hoping one giant prompt gets everything right.
- One item per loop. The built-in workflows deliberately try to pick one high-leverage item, or send the agent back to planning when the work is still ambiguous.
- Durable memory beats bloated context. Files such as `PLAN.md`, `progress.txt`, and design docs under `docs/` are the stable memory each loop reloads.
- Failures are data. A bad search result, broken build, or stale plan is usually a signal to tune the workflow inputs or guardrails.
- Backpressure matters. Ralph works best when each loop can run the checks that reject placeholders and shallow implementations.
- Operator skill still matters. Ralph automates the loop, not engineering judgment; you still need to define success clearly and tune the artifacts when the loop drifts.

## When Ralph Works Best

- Well-defined engineering work with observable success criteria
- Greenfield work or bounded refactors where automated checks can provide fast feedback
- Repositories where `PLAN.md`, `progress.txt`, or design docs can stay current
- Long-running or unattended iteration where you want auditable handoffs between loops

## When Ralph Is A Bad Fit

- Tasks whose success is mostly taste, negotiation, or external approval
- Huge vague requests that actually need design work first
- Changes that cannot be validated with tests, linters, type checkers, or smoke tests
- Repositories where nobody will maintain the plan/spec artifacts Ralph depends on

## What Ralph Gives You

- Named, repeatable loops instead of rebuilding the prompt stack every run
- Built-in workflows that separate planning from building when needed
- Agent portability across Codex, Claude Code, Gemini CLI, OpenCode, Droid, Pi Coding, and Raijin
- User defaults and per-project overrides kept separate
- Editable workflow definitions and auditable run artifacts under `.ralph/`

## Install

### Release Installer

```bash
curl -fsSL https://raw.githubusercontent.com/francescoalemanno/ralph-cli/main/install | bash
```

The installer downloads a release binary for macOS or Linux on `x86_64` and `arm64`, installs `ralph` into `~/.local/bin` by default, and adds that directory to common shell profiles if needed.

Useful installer overrides:

```bash
curl -fsSL https://raw.githubusercontent.com/francescoalemanno/ralph-cli/main/install | \
  RALPH_INSTALL_DIR="$HOME/bin" RALPH_VERSION="vX.Y.Z" bash
```

### From Source

```bash
cargo install --path crates/ralph-cli
```

Or build the binary directly:

```bash
cargo build --release -p ralph-cli
./target/release/ralph --help
```

## First Run

Start with:

```bash
ralph --workflows
ralph --show-config
```

Ralph seeds its user config and built-in workflow registry automatically the first time it loads them.

## Choose The Right Workflow

- `bare`: use this when your request already contains the exact loop discipline you want and you just need Ralph to run it durably.
- `default`: use this when you want Ralph to keep a durable `PLAN.md`, execute one plan item per loop, and finish with a whole-project verification pass.
- `dbv`: use this when you want a durable plan in `PLAN.md`, one-item-at-a-time execution, and a final whole-project verification pass before declaring success.
- `plan`: use this when you want a host-mediated planning loop that explores the codebase, asks one clarifying question at a time, drafts a markdown plan, and only writes the exact accepted draft to disk.

## Core Concepts

- Workflow: a YAML definition selected with `ralph w <workflow-id> ...`
- Agent: the coding tool Ralph launches underneath the workflow
- Request: the task text for the workflow
- Design docs/specs: optional durable reference material when the request needs them
- Plan file: a prioritized list of right-sized build items, usually `PLAN.md`
- User config: `~/.config/ralph/config.toml`
- Workflow registry: `~/.config/ralph/workflows/`
- Project config: `.ralph/config.toml`

Every command also accepts `--project-dir <PATH>` if you want to operate on a different repository without changing directories.

## Running Workflows

### Guided Mode

These use the new top-level guided entrypoint:

```bash
ralph
ralph "add caching for API responses"
ralph --file REQ.md
ralph --plan
ralph --plan="add caching for API responses"
cat REQ.md | ralph
cat REQ.md | ralph --plan
ralph -t docs/plans/2026-04-11-cache-plan.md
ralph -r
ralph -f
```

Guided behavior:

- `ralph` accepts the planning request from argv text, root `--file <FILE>`, piped stdin, or an interactive prompt, then runs `plan` and can continue into `task` then `review`.
- `ralph --plan[=DESCRIPTION]` runs the same planning flow and stops after the plan file is written. The request may come from `--plan=...`, root `--file <FILE>`, piped stdin, or the interactive prompt.
- `ralph -t <PLAN_FILE>` runs only `task`.
- `ralph -r [PLAN_FILE]` runs only `review`. If you omit `PLAN_FILE`, Ralph first reuses the latest accepted plan from `ralph --plan`; if none can be found, it still runs by injecting the sentinel plan value `"<unavailable, ignore>"`.
- `ralph -f [PLAN_FILE]` runs only `finalize`. If you omit `PLAN_FILE`, Ralph first reuses the latest accepted plan from `ralph --plan`; if none can be found, it still runs by injecting the sentinel plan value `"<unavailable, ignore>"`.

### Workflow Runner

These run directly in the terminal:

```bash
ralph w bare "fix the failing tests"
ralph w default "ship the auth refactor"
ralph w dbv "ship the auth refactor"
ralph w plan "add caching for API responses"
```

Important behavior:

- The allowed request forms come from each workflow's `request.runtime` definition.
- Built-in `bare`, `default`, `dbv`, and `plan` accept argv text, root `--file <FILE>`, or piped stdin.
- The request must be provided in exactly one runtime form.

## Theme

Ralph resolves one shared terminal theme for all console output.

- `theme.mode = "auto"` uses `COLORFGBG` when available and falls back to a dark palette.
- `theme.mode = "dark"` or `theme.mode = "light"` forces a specific palette.
- `RALPH_THEME_MODE=dark|light` overrides auto detection for the current process.

Example:

```toml
[theme]
mode = "auto"
accent_color = "cyan"
success_color = "green"
warning_color = "yellow"
error_color = "red"
```

### Request Input Rules

Ralph accepts the workflow request in exactly one runtime form, but the allowed forms depend on the selected workflow.

- Guided mode: argv text, `--file <FILE>`, or stdin
- `bare`, `default`, `dbv`, and `plan`: argv text, `--file <FILE>`, or stdin

If you provide a form that the workflow does not allow, or provide more than one form, Ralph exits with a usage error.

## Writing Better Ralph Requests

- Define success criteria in observable terms: what should work, what should pass, and what files or docs should be updated.
- Keep the active loop narrow. If the work is broad or ambiguous, start with `plan` or `dbv` before building.
- Point Ralph at durable memory such as `PLAN.md` or design docs.
- Tell Ralph to study the code before deciding something is missing. This is one of the most common failure modes in agentic loops.
- Ask for the relevant checks after each change so the loop has real backpressure.
- Treat plan and progress files as living control surfaces. If they get stale, rewrite them and keep looping.

## Built-In Workflows

| Workflow | What it does | Useful options |
| --- | --- | --- |
| `bare` | Minimal wrapper when your request already contains the loop discipline you want. | None |
| `default` | Repairs a durable `PLAN.md`, executes one plan item per loop, and verifies the whole project when the plan is complete. | `--planfile` (default: `PLAN.md`) |
| `dbv` | Uses a durable `PLAN.md` as the control surface, decomposes when needed, builds one item per loop, and performs whole-project verification when the plan is complete. | `--planfile` (default: `PLAN.md`) |
| `finalize` | Runs the best-effort finalization pass: fetch, rebase onto the base ref, tidy commits, and rerun relevant checks. Top-level `ralph -f [PLAN_FILE]` can run without a plan by falling back to the latest accepted plan or `"<unavailable, ignore>"`. | `--planfile`, `--baseref` (default: `main`) |
| `plan` | Runs a host-mediated planner loop that explores the repo, asks one clarifying question at a time, drafts a plan, and writes the accepted markdown file under `docs/plans/`. | `--plansdir` (default: `docs/plans`) |
| `review` | Runs the standalone multi-agent review passes and fixes confirmed findings until the branch is clean. Top-level `ralph -r [PLAN_FILE]` can run without a plan by falling back to the latest accepted plan or `"<unavailable, ignore>"`. | `--planfile`, `--baseref` (default: `main`) |
| `task` | Executes markdown task sections one at a time until the plan's implementation stage is complete, then stops before review. | `--planfile` |

List them at any time with:

```bash
ralph --workflows
```

## Tuning The Loop

- If Ralph keeps grabbing work that is too large, shrink the plan items until one loop can finish one item completely.
- If Ralph duplicates code that already exists, strengthen the instruction to study the codebase first and keep specs aligned with reality.
- If Ralph compiles but does shallow work, tighten the success criteria and require the checks that would fail on placeholders.
- If `progress.txt` or `PLAN.md` turns into noise, rewrite it into a shorter prioritized list and continue the loop.
- Use `--max-iterations` as a safety net when you are testing a workflow or running unattended.

## Common Commands

```bash
ralph --help
ralph w --help
ralph w dbv --help
ralph --workflows
ralph --show-workflow dbv
ralph --edit-workflow dbv
ralph --show-config
ralph --show-config=user
ralph --set-user-agent claude
ralph --set-project-agent opencode
ralph --agent claude w dbv "ship the auth refactor"
```

## Agents And Config

Persist a default agent:

```bash
ralph --set-user-agent opencode
ralph --set-project-agent claude
```

Config scopes:

- `user`: your global Ralph defaults
- `project`: overrides for the current repository
- `effective`: merged view of both

Set `RALPH_CONFIG_HOME` if you want Ralph's user config and workflow registry somewhere other than `~/.config/ralph`.

Show config:

```bash
ralph --show-config=user
ralph --show-config=project
ralph --show-config=effective
```

Notes:

- The default built-in agent is `opencode`.
- On startup Ralph keeps the configured agent when it is available; otherwise it falls back to the first detected agent in priority order: `opencode`, `raijin`, then the remaining configured agents.
- Workflow iteration limits live in each workflow YAML as `max_iterations`; the user-facing built-in workflows default to `40`, and `--max-iterations` overrides that per run.
- Persisting a project agent writes `.ralph/config.toml` on demand.

## Inspecting And Editing Workflows

List workflow definitions and where they live:

```bash
ralph --workflows
```

Print the raw YAML for one workflow:

```bash
ralph --show-workflow default
```

Edit a workflow in place:

```bash
ralph --edit-workflow default
```

Protected built-ins:

- `plan`, `task`, `review`, and `finalize` are auto-generated built-ins and cannot be edited or overridden.
- Their seeded registry files include `AUTOGENERATED DO NOT MODIFY` at the top.

Editor resolution order:

1. project `editor_override`
2. `VISUAL`
3. `EDITOR`

If no editor is configured, `ralph --edit-workflow ...` exits with an error.

## Workflow Option Flags

Workflow option ids are turned into long flags by removing `-` and `_`.

Examples:

- `plan-file` becomes `--planfile`
- `base-ref` becomes `--baseref`

That is why the workflow-specific help output looks slightly different from the YAML option ids.

## Files Ralph Creates

- `~/.config/ralph/config.toml`: user-level config and built-in agent registry
- `~/.config/ralph/workflows/*.yml`: workflow registry; built-ins are seeded here automatically and protected ones are kept in sync with the embedded canonical copy
- `.ralph/config.toml`: project-level config
- `.ralph/runs/<workflow-id>/<run-id>/request.txt`: saved request text for a run
- `.ralph/runs/<workflow-id>/<run-id>/.ralph-runtime/agent-events.wal.ndjson`: loop-control event log
- `.ralph/runs/<workflow-id>/<run-id>/.ralph-runtime/channels/<channel-id>/output.log`: suppressed text output for a parallel worker channel

Files Ralph commonly reads or updates as part of the workflow itself:

- `PLAN.md`: durable execution plan for `default` and `dbv`
- `docs/plans/*.md`: accepted plan drafts written by `plan`

## Advanced: Agent Events

Ralph records agent events through explicit CLI commands during an agent run.

- Emit an event with no body by running `"$RALPH_BIN" signal <event-name>`
- Emit an event with a body by running `"$RALPH_BIN" payload <event-name> "<body>"`
- Read the latest stored payload for an event across all channels in the current run with `"$RALPH_BIN" get <event-name>`
- Read the latest stored payload for an event from one specific channel with `"$RALPH_BIN" get --channel <channel-id> <event-name>`

Built-in workflows use this mechanism for loop control, for example:

- `loop-continue`
- `loop-route` with the target prompt id in the payload body
- `loop-stop:ok` with an optional success reason in the payload body
- `loop-stop:error` with an optional failure reason in the payload body

Planning workflows also use a host-intercepted event contract:

Agent-emitted planning payloads:
- `planning-question`: asks exactly one clarifying question; Ralph intercepts it, asks the user directly, and then appends host-side planning state into the WAL before rerouting
- `planning-target-path`: the current proposed project-relative output path for the draft plan file; Ralph reads that file and intercepts it for `accept` / `revise` / `reject`

Host-emitted planning payloads on channel `host`:
- `planning-answer`: the user's answer to the latest `planning-question`
- `planning-review`: the user's latest `accept` / `revise` / `reject` decision for the current draft
- `planning-progress`: cumulative host-maintained transcript of all answered questions and all draft review decisions in the order they happened
- `planning-plan-file`: the final written plan path after the user accepts a draft

Important planning rules:

- `planning-question` and `planning-target-path` are special host-intercepted payloads, not ordinary loop-control signals
- do not emit `planning-question` and `planning-target-path` in the same iteration
- do not emit planning payloads together with `loop-route` or `loop-stop:*` in the same iteration
- the latest `planning-target-path` plus the file at that path are the current working draft state
- when you emit `planning-target-path`, write the markdown draft to that file first
- on `accept`, Ralph keeps the exact draft file you emitted and ends the workflow successfully

See the built-in workflow definitions with `ralph --show-workflow <workflow-id>` if you want to study how loop control works in practice.

## Parallel Prompts

Workflows can fan out non-interactive workers in parallel and then continue on a serial route:

```yml
prompts:
  reviews:
    title: Reviews
    fallback-route: fixer
    parallel:
      workers:
        QT:
          title: quality tester
          prompt: |
            ...
        OE:
          title: over-engineering detector
          prompt: |
            ...
  fixer:
    title: Fixer
    fallback-route: no-route-error
    prompt: |
      qt=$("$RALPH_BIN" get --channel QT review)
      oe=$("$RALPH_BIN" get --channel OE review)
      ...
```

Parallel workers emit events on their own channel automatically. Their text output is suppressed from the main console output, but saved under `.ralph-runtime/channels/<channel-id>/output.log`.

## A Good Daily Flow

If the work is still fuzzy but you already want a concrete implementation plan in the repo, start with `plan`:

```bash
ralph --plan="add SSO to the admin app"
```

If the work is implementation-ready and you want a durable plan plus one-item-at-a-time execution, use `default`:

```bash
ralph w default "add SSO to the admin app"
```

If you want the more explicit dispatcher-style plan gating, use `dbv`:

```bash
ralph w dbv "add SSO to the admin app"
```

If you are scripting a run, piping the request works directly:

```bash
cat REQ.md | ralph w dbv
```
