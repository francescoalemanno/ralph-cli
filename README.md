# Ralph

Ralph is a workflow runner for coding agents.

It gives you named, repeatable execution loops like "take the next highest-priority task", "plan then build", or "turn this rough idea into a design and implementation plan". By default it opens a terminal UI; add `--cli` when you want a scriptable, plain-terminal run.

![Ralph TUI](tui.png)

## Why Use Ralph

- Run the same request through a durable workflow instead of a one-off prompt.
- Switch between supported agents such as Codex, Claude Code, Gemini CLI, OpenCode, Droid, and Raijin.
- Keep user defaults and per-project overrides separate.
- Inspect and edit workflow definitions from the CLI.
- Keep run artifacts under `.ralph/` so work is auditable and inspectable.

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

## Core Concepts

- Workflow: a YAML definition selected with `ralph run <workflow-id> ...`
- Agent: the coding tool Ralph launches underneath the workflow
- Request: the task text for the workflow
- User config: `~/.config/ralph/config.toml`
- Workflow registry: `~/.config/ralph/workflows/`
- Project config: `.ralph/config.toml`

Every command also accepts `--project-dir <PATH>` if you want to operate on a different repository without changing directories.

## Running Workflows

### Default Mode: TUI

These open the runner UI:

```bash
ralph run task-based "fix the failing tests"
ralph run plan-build "ship the auth refactor"
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

## Built-In Workflows

| Workflow | What it does | Useful options |
| --- | --- | --- |
| `bare` | Sends your request to the agent with no extra scaffolding. | None |
| `task-based` | Executes one high-priority task at a time and updates a progress file for handoff. | `--progressfile` (default: `progress.txt`) |
| `plan-build` | Alternates between planning and implementation against one goal. | `--specsglob` (default: `specs/*`), `--planfile` (default: `IMPLEMENTATION_PLAN.md`), `--agentsfile` (default: `AGENTS.md`) |
| `pdd` | Interactive prompt-driven development: rough idea to research, design, and implementation plan. | None |

List them at any time with:

```bash
ralph ls
```

## Common Commands

```bash
ralph --help
ralph run --help
ralph run plan-build --help
ralph ls
ralph show plan-build
ralph edit plan-build
ralph agent list
ralph agent current
ralph agent set claude --scope user
ralph config show --scope effective
ralph config path
ralph init --agent codex --editor nvim --max-iterations 20
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
ralph agent set codex --scope user
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
ralph init --agent codex --editor nvim --max-iterations 20
```

Notes:

- The default built-in agent is `codex`.
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
- `specs-glob` becomes `--specsglob`

That is why the workflow-specific help output looks slightly different from the YAML option ids.

## Files Ralph Creates

- `~/.config/ralph/config.toml`: user-level config and built-in agent registry
- `~/.config/ralph/workflows/*.yml`: workflow registry; built-ins are seeded here automatically
- `.ralph/config.toml`: project-level config
- `.ralph/runs/<workflow-id>/<run-id>/request.txt`: saved request text for a run
- `.ralph/runs/<workflow-id>/<run-id>/.ralph-runtime/agent-events.wal.ndjson`: loop-control event log

## Advanced: `ralph emit`

`ralph emit` is mainly for workflow authors and the agent processes Ralph launches. Most users can ignore it.

It only works inside an active Ralph run and appends events to the current run's WAL. Built-in workflows use it to control looping behavior, for example:

- `loop-continue`
- `loop-route <prompt-id>`
- `loop-stop:ok <reason>`
- `loop-stop:error <reason>`

See the built-in workflow definitions with `ralph show <workflow-id>` if you want to study how loop control works in practice.

## A Good Daily Flow

```bash
ralph doctor
ralph agent set codex --scope user
ralph init --agent codex --editor nvim
ralph run plan-build "add SSO to the admin app"
```

If you want plain terminal output instead of the UI:

```bash
ralph run --cli plan-build "add SSO to the admin app"
```
