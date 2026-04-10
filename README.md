# Ralph

Ralph is a workflow runner for iterative coding-agent loops.

Think of it as the Ralph Wiggum technique packaged into named workflows: study durable project memory, choose one high-leverage action, do the work, record what changed, and either loop again or stop. By default it opens a terminal UI; add `--cli` when you want a scriptable, plain-terminal run.

![Ralph TUI](tui.png)

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
ralph doctor
ralph agent list
ralph ls
```

`ralph doctor` validates config, seeds the built-in workflow registry if missing, ensures `.ralph/` can be created in the current project, and reports which supported agents were detected on `PATH`.

## Choose The Right Workflow

- `bare`: use this when your request already contains the exact loop discipline you want and you just need Ralph to run it durably.
- `simple`: use this when you want Ralph to study the request and current codebase, then take the single highest-leverage next step each loop.
- `default`: use this when you want Ralph to keep a durable `PLAN.md`, execute one plan item per loop, and finish with a whole-project verification pass.
- `dbv`: use this when you want a durable plan in `PLAN.md`, one-item-at-a-time execution, and a final whole-project verification pass before declaring success.
- `task-based`: use this when the work already lives in a request list or `progress.txt` and you want one right-sized item completed per loop.
- `pdd`: use this when the idea is still rough and you need an interactive path to research, design, and an implementation plan before autonomous loops.

## Core Concepts

- Workflow: a YAML definition selected with `ralph run <workflow-id> ...`
- Agent: the coding tool Ralph launches underneath the workflow
- Request: the task text for the workflow
- Design docs/specs: optional durable reference material when the request needs them
- Plan file: a prioritized list of right-sized build items, usually `PLAN.md`
- Progress file: the handoff memory for the next loop, usually `progress.txt`
- User config: `~/.config/ralph/config.toml`
- Workflow registry: `~/.config/ralph/workflows/`
- Project config: `.ralph/config.toml`

Every command also accepts `--project-dir <PATH>` if you want to operate on a different repository without changing directories.

## Running Workflows

### Default Mode: TUI

These open the runner UI:

```bash
ralph run task-based "fix the failing tests"
ralph run default "ship the auth refactor"
ralph run dbv "ship the auth refactor"
ralph run pdd --file rough-idea.md
```

Important behavior:

- TUI mode requires a workflow and a request.
- In TUI mode, provide the request either as argv text or with `--file`.
- Piped stdin is not supported in TUI mode because the terminal is needed for interaction.

### CLI Mode

Use `--cli` for a plain terminal run:

```bash
ralph run --cli bare "summarize the current repository"
ralph run --cli task-based --progressfile progress.txt "finish the top task"
cat REQ.md | ralph run --cli bare
```

CLI mode also accepts piped stdin. For workflows with interactive prompts, do not use stdin; use argv text or `--file` so the terminal stays available.

### Request Input Rules

Ralph accepts the workflow request in exactly one runtime form:

- argv text
- `--file <FILE>`
- stdin, but only in `--cli` mode

If you provide more than one, Ralph exits with a usage error.

## Writing Better Ralph Requests

- Define success criteria in observable terms: what should work, what should pass, and what files or docs should be updated.
- Keep the active loop narrow. If the work is broad or ambiguous, start with `pdd` to design it first or `dbv` to repair the plan before building.
- Point Ralph at durable memory such as `PLAN.md`, design docs, or `progress.txt`.
- Tell Ralph to study the code before deciding something is missing. This is one of the most common failure modes in agentic loops.
- Ask for the relevant checks after each change so the loop has real backpressure.
- Treat plan and progress files as living control surfaces. If they get stale, rewrite them and keep looping.

## Built-In Workflows

| Workflow | What it does | Useful options |
| --- | --- | --- |
| `bare` | Minimal wrapper when your request already contains the loop discipline you want. | None |
| `simple` | Studies the request and project state, then executes the single highest-leverage next step toward completion each pass. | None |
| `default` | Repairs a durable `PLAN.md`, executes one plan item per loop, and verifies the whole project when the plan is complete. | `--planfile` (default: `PLAN.md`) |
| `dbv` | Uses a durable `PLAN.md` as the control surface, decomposes when needed, builds one item per loop, and performs whole-project verification when the plan is complete. | `--planfile` (default: `PLAN.md`) |
| `task-based` | Reads the request list, chooses one high-priority right-sized item, executes it, and updates a handoff file for the next loop. | `--progressfile` (default: `progress.txt`) |
| `pdd` | Interactive prompt-driven development for turning a rough idea into research, design, and an implementation plan. | `--pdddir` (default: `docs/planning/{project_name}`) |

List them at any time with:

```bash
ralph ls
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
ralph run --help
ralph run dbv --help
ralph ls
ralph show dbv
ralph edit dbv
ralph agent list
ralph agent current
ralph agent set claude --scope user
ralph config show --scope effective
ralph config path
ralph init --agent opencode --editor nvim --max-iterations 20
ralph doctor
```

## Agents And Config

Inspect detected agents:

```bash
ralph agent list
```

Show the effective agent for the current project:

```bash
ralph agent current
```

Persist a default agent:

```bash
ralph agent set opencode --scope user
ralph agent set claude --scope project
```

Config scopes:

- `user`: your global Ralph defaults
- `project`: overrides for the current repository
- `effective`: merged view of both

Set `RALPH_CONFIG_HOME` if you want Ralph's user config and workflow registry somewhere other than `~/.config/ralph`.

Show config:

```bash
ralph config show --scope user
ralph config show --scope project
ralph config show --scope effective
```

Create a project config file:

```bash
ralph init --agent opencode --editor nvim --max-iterations 20
```

Notes:

- The default built-in agent is `opencode`.
- On startup Ralph keeps the configured agent when it is available; otherwise it falls back to the first detected agent in priority order: `opencode`, `raijin`, then the remaining configured agents.
- The default workflow iteration limit is `40`.
- `ralph init` writes `.ralph/config.toml`.
- Re-run `ralph init` with `--force` to overwrite an existing project config.

## Inspecting And Editing Workflows

List workflow definitions and where they live:

```bash
ralph ls
```

Print the raw YAML for one workflow:

```bash
ralph show task-based
```

Edit a workflow in place:

```bash
ralph edit task-based
```

Editor resolution order:

1. project `editor_override`
2. `VISUAL`
3. `EDITOR`
4. Ralph's built-in terminal editor

If Ralph falls back to the built-in editor, `Ctrl-S` saves and `Ctrl-Q` closes it.

## Workflow Option Flags

Workflow option ids are turned into long flags by removing `-` and `_`.

Examples:

- `progress-file` becomes `--progressfile`
- `plan-file` becomes `--planfile`
- `pdd-dir` becomes `--pdddir`

That is why the workflow-specific help output looks slightly different from the YAML option ids.

## Files Ralph Creates

- `~/.config/ralph/config.toml`: user-level config and built-in agent registry
- `~/.config/ralph/workflows/*.yml`: workflow registry; built-ins are seeded here automatically
- `.ralph/config.toml`: project-level config
- `.ralph/runs/<workflow-id>/<run-id>/request.txt`: saved request text for a run
- `.ralph/runs/<workflow-id>/<run-id>/.ralph-runtime/agent-events.wal.ndjson`: loop-control event log

Files Ralph commonly reads or updates as part of the workflow itself:

- `PLAN.md`: durable execution plan for `default` and `dbv`
- `progress.txt`: task handoff memory for `task-based`
- `docs/planning/<project>/design/detailed-design.md`: design output from `pdd`
- `docs/planning/<project>/implementation/plan.md`: execution-ready plan output from `pdd`

## Advanced: Agent Events

Ralph can read events directly from the text output emitted by a non-interactive agent run.

- Emit an event with no body by printing `<<<SIGNAL:event-name>>>`
- Emit an event with a body by printing `<<<PAYLOAD:event-name>>>body<<<END-PAYLOAD>>>`
- Read the latest stored payload for an event inside a Ralph agent run with `"$RALPH_BIN" get <event-name>`

Built-in workflows use this mechanism for loop control, for example:

- `loop-continue`
- `loop-route` with the target prompt id in the payload body
- `loop-stop:ok` with an optional success reason in the payload body
- `loop-stop:error` with an optional failure reason in the payload body

See the built-in workflow definitions with `ralph show <workflow-id>` if you want to study how loop control works in practice.

## A Good Daily Flow

If the work is still fuzzy, start with `pdd` and turn the idea into durable docs:

```bash
ralph run pdd --file rough-idea.md
```

If the work is implementation-ready and you want a durable plan plus one-item-at-a-time execution, use `default`:

```bash
ralph run default "add SSO to the admin app"
```

If you want the more explicit dispatcher-style plan gating, use `dbv`:

```bash
ralph run dbv "add SSO to the admin app"
```

If you already have a request list and want one-item handoffs, use `task-based` with a maintained `progress.txt`:

```bash
ralph run task-based "work through the next highest-priority backlog item"
```

If you want plain terminal output instead of the UI, or you are scripting a run:

```bash
ralph run --cli dbv "add SSO to the admin app"
```
