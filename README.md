# Ralph

`ralph` is a Rust CLI and terminal UI for running a durable planning and execution workflow inside a repository.

It keeps planning state on disk, separates planning from building, and makes agent-driven work resumable instead of ephemeral.

Source:

- [github.com/francescoalemanno/ralph-cli](https://github.com/francescoalemanno/ralph-cli)

## Lineage

This project is inspired by Geoffrey Huntley's original Ralph technique: a simple, persistent agent loop centered on specs, iteration, and durable on-disk state.

Original writeup:

- [Ralph Wiggum as a "software engineer"](https://ghuntley.com/ralph/)

The framing here is intentionally narrower and more user-ready. Huntley's original concept is hacker-first and extremely lightweight: a loop, a prompt, and operator skill. This repository turns that idea into a simpler end-user tool with a TUI, explicit artifacts, local config, agent presets, review screens, and a more structured planner/builder workflow.

## What Ralph Does

Ralph manages a loop around three persistent artifacts:

- a spec file
- a progress file
- a feedback file

The planner updates the spec and rewrites the progress plan. The builder reads the spec, executes the next highest-leverage task, and updates progress. Clarifications are persisted into feedback so future iterations keep the latest user intent.

By default, project artifacts live under `./.ralph/`.

## Why It Exists

Most agent workflows lose context between runs or bury the state inside chat history. Ralph keeps the working state in files so you can:

- inspect and edit the plan directly
- pause and resume work across sessions
- switch agents without losing state
- review exactly what changed
- keep user clarifications as durable input instead of transient messages

## Core Workflow

1. Create or open a target.
2. Run a planning pass.
3. Run builder iterations until the spec is complete.
4. Review the resulting spec and progress.
5. Edit the spec manually when needed, then let Ralph revise progress to match.

Ralph supports both TUI-first and CLI-first workflows.

## Installation

For local development:

```bash
cargo run -p ralph-cli -- --help
```

To install the CLI from the workspace:

```bash
cargo install --path crates/ralph-cli
```

To install from crates.io once published:

```bash
cargo install ralph-cli
```

The installed command remains:

```bash
ralph
```

## Quick Start

Start the TUI:

```bash
ralph
```

Or run it from the workspace without installing:

```bash
cargo run -p ralph-cli --
```

Typical flow:

1. Open the TUI.
2. Press `n` to create a new spec from a planning request.
3. Press `Ctrl-B` on a selected spec to run the builder.
4. Press `Ctrl-V` to review spec, progress, and feedback.
5. Press `Ctrl-E` to edit the spec and automatically revise progress afterward.

You can also jump directly into a scoped TUI for one target:

```bash
ralph <target>
```

## CLI Commands

```text
ralph tui
ralph list
ralph review <target>
ralph run <target>
ralph revise <target> [--prompt "..."]
ralph edit <target>
ralph create [--prompt "..."]
ralph replan <target> [--prompt "..."]
```

What they do:

- `tui`: open the full terminal UI
- `list`: print known specs and their states
- `review`: print the spec and progress for a target
- `run`: execute builder iterations for a target
- `revise`: send new planning input for an existing target
- `edit`: open the spec in your editor
- `create`: create a new spec from a planning request
- `replan`: replace the plan for an existing target from a fresh request

If `--prompt` is omitted for `create`, `revise`, or `replan`, Ralph reads the planning request from stdin or interactively from the terminal.

## Artifact Layout

By default, Ralph stores project-local state in:

```text
./.ralph/
  config.toml
  spec-<slug>.md
  progress-<slug>.txt
  feedback-<slug>.txt
  <custom>.progress.txt
  <custom>.feedback.txt
  <spec>.past-spec.md
  <spec>.spec-edit.diff.txt
```

Notes:

- Named targets are stored as `spec-<target>.md` under `./.ralph/`.
- Path-like targets such as `docs/feature.md` are respected as explicit locations.
- When a spec lives outside `./.ralph/`, its derived progress, feedback, and edit sidecars are written beside that spec.

## Configuration

Ralph loads configuration from two places:

- user defaults: `~/.config/ralph/config.toml`
- project overrides: `./.ralph/config.toml`

Project config overrides user config.

The TUI persists the selected coding agent into `./.ralph/config.toml`.

### Example

```toml
[planner]
program = "codex"
args = ["exec", "--dangerously-bypass-approvals-and-sandbox", "--ephemeral"]
prompt_transport = "stdin"
question_support = "text_protocol"

[builder]
program = "codex"
args = ["exec", "--dangerously-bypass-approvals-and-sandbox", "--ephemeral"]
prompt_transport = "stdin"
question_support = "text_protocol"

planning_max_iterations = 8
builder_max_iterations = 25
editor_override = "nvim"

[theme]
accent_color = "cyan"
success_color = "green"
warning_color = "yellow"
```

## Built-In Agent Presets

Ralph currently supports three external coding-agent CLIs via built-in presets. "Supported" here means Ralph knows how to:

- detect the agent on `PATH`
- persist it in config
- switch to it in the TUI
- invoke it with the expected command shape for planner and builder passes

Supported agents:

- OpenCode: [anomalyco/opencode](https://github.com/anomalyco/opencode)
  Ralph treats this as the open source terminal coding agent and invokes it as `opencode run --format default --thinking`.
- Codex: [openai/codex](https://github.com/openai/codex)
  Ralph treats this as OpenAI's terminal coding agent and invokes it as `codex exec --dangerously-bypass-approvals-and-sandbox --ephemeral`.
- Raijin: [francescoalemanno/raijin-mono](https://github.com/francescoalemanno/raijin-mono/)
  Ralph treats this as a fast terminal AI assistant with one-shot CLI support and invokes it as `raijin -ephemeral "$PROMPT"`.

The TUI only cycles among supported agents that are actually detected on `PATH`.

If you configure a different command manually, Ralph may still run it if the command shape is compatible, but only the three presets above are first-class supported agents.

## Prompting Model

Ralph does not inline artifact contents into prompts by default. Instead, it tells the planner or builder which files to read from disk first. This keeps prompts smaller and makes the files on disk the source of truth.

Planner prompts include:

- the spec path
- the progress path
- the feedback path
- clarification rules
- controller warnings

Builder prompts include:

- the spec path
- the progress path
- the feedback path

Spec-edit revision prompts include:

- the previous spec snapshot
- the current spec
- the progress file
- the spec diff

## Clarifications

When clarification is enabled, the planner can emit a single structured question block. Ralph surfaces it in the UI, records the answer, and persists the exchange into the feedback artifact so later runs inherit the answer.

The feedback file keeps:

- the newest authoritative clarification in `<RECENT-USER-FEEDBACK>`
- older clarification history in `<OLDER-USER-FEEDBACK>`

## Project Structure

This workspace is split into focused crates:

- `crates/ralph-core`: prompts, config, artifact storage, shared types
- `crates/ralph-runner`: process execution and runner transport
- `crates/ralph-app`: orchestration and workflow logic
- `crates/ralph-tui`: terminal UI
- `crates/ralph-cli`: command-line entrypoint

## Development

Useful checks:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test
```

Package the CLI locally:

```bash
cargo package -p ralph-cli --allow-dirty --no-verify
```

## Current Status

Ralph is usable as a local workflow tool and is designed around durable repo-local state, editable specs, and pluggable agents. The sharp edge to keep in mind is that explicit path targets intentionally allow artifacts to live outside `./.ralph/` when you point Ralph at a spec elsewhere in the repository.
