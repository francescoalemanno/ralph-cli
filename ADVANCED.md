# Ralph — Advanced Usage

- [Custom workflows](#custom-workflows)
- [Custom agent definitions](#custom-agent-definitions)
- [Configuration](#configuration)
- [Architecture](#architecture)
- [Building from source](#building-from-source)

---

## Custom workflows

Workflows are YAML files stored in `~/.config/ralph/workflows/`. Ralph loads builtin workflows from the compiled binary and user-defined workflows from this directory. A workflow is a state machine: each state is a **prompt** sent to the agent, and the agent's emitted events determine the next state.

### Workflow structure

Every workflow file must follow this top-level schema:

```yaml
version: 1                    # Always 1 (required)
workflow_id: my-workflow       # Unique identifier used on the CLI (required)
title: My Workflow             # Human-readable title (required)
description: What it does.     # Shown in `ralph --workflows` (optional)
hidden: false                  # If true, hidden from help but still invocable (optional)
max_iterations: 40             # Upper bound on total prompt invocations (default: 40)
entrypoint: main               # Which prompt to run first (required, must match a key under `prompts`)
options: {}                    # Workflow-specific CLI options (optional)
request: ...                   # How the workflow receives its request text (optional)
prompts: {}                    # The prompt states (required, at least one)
```

### Options

Options let workflows accept CLI flags. Each option becomes a `--flagname` argument on `ralph w <workflow-id>`:

```yaml
options:
  plan-file:
    help: Markdown plan file to execute.     # Help text shown in --help
    default: PLAN.md                          # Default value if the flag is omitted
    value_name: FILE                          # Placeholder name shown in usage
  base-ref:
    help: Base git ref for diffs.
    default: main
    value_name: REF
```

Option IDs may use ASCII letters, digits, `-`, and `_`. The CLI flag is derived by stripping hyphens and underscores (e.g., `plan-file` becomes `--planfile`). Duplicate flag collisions are rejected at validation time.

Options are referenced in prompts with `{ralph-option:plan-file}`.

### Request definition

The `request` block declares how the workflow receives its input text. Exactly one source must be configured:

**Runtime request** — accepts text from argv, stdin, or `--file`:

```yaml
request:
  runtime:
    argv: true       # Accept positional arguments: ralph w my-workflow "do this"
    stdin: true      # Accept piped input: echo "do this" | ralph w my-workflow
    file_flag: true  # Accept --file flag: ralph w my-workflow --file REQ.md
```

**File request** — reads text from a fixed file path:

```yaml
request:
  file:
    path: specs/requirements.md
```

**Inline request** — embeds the request directly in the workflow:

```yaml
request:
  inline: "Run the full test suite and report results."
```

Workflows that use `{ralph-request}` in any prompt **must** define a `request` block. Workflows without `{ralph-request}` should omit it.

### Prompts

Each prompt is one state in the workflow. A prompt can be either a **single prompt** (one agent invocation) or a **parallel prompt** (multiple concurrent agents):

```yaml
prompts:
  main:
    title: Main                     # Human-readable label
    fallback-route: main            # Where to go if the agent emits no loop-control event
    prompt: |                       # The prompt text template
      Do one task from the plan.
```

The `fallback-route` determines what happens when the agent exits without emitting any loop-control event. It can be:

- Another prompt ID — route to that prompt
- `no-route-ok` — end the workflow successfully
- `no-route-error` — end the workflow with an error

### Prompt template tokens

Ralph expands special tokens in prompt text before sending it to the agent:

| Token | Expands to |
|---|---|
| `{ralph-request}` | The user-supplied request text |
| `{ralph-env:PROJECT_DIR}` | Absolute path to the project directory |
| `{ralph-option:name}` | The value of workflow option `name` |
| `{ralph-skill-emit}` | A block of instructions teaching the agent how to use `"$RALPH_BIN" signal` and `"$RALPH_BIN" payload` to emit events |
| `{ralph-route:target}` | An instruction telling the agent how to route to prompt `target` |
| `{ralph-stop:ok}` | An instruction telling the agent how to stop the workflow successfully (no body) |
| `{ralph-stop:ok:message}` | Same, with a body message |
| `{ralph-stop:error}` | An instruction telling the agent how to stop with an error (no body) |
| `{ralph-stop:error:message}` | Same, with a body message |
| `{ralph-get:event-name}` | An instruction telling the agent how to read the latest payload for `event-name` from the WAL |
| `{ralph-get:channel:event-name}` | Same, but filtered to a specific channel |

Tokens like `{ralph-route:...}`, `{ralph-stop:...}`, and `{ralph-get:...}` are **macros**: they expand into full English instructions including the exact shell command the agent should run, so the agent knows how to interact with Ralph's event protocol without needing to understand it up front.

Non-Ralph tokens like `{anything_else}` are left untouched.

### Parallel workers

Any prompt can be replaced with a `parallel` block to fan out work to multiple concurrent agents:

```yaml
prompts:
  review-fanout:
    title: Review fanout
    fallback-route: fix-pass
    parallel:
      join: wait_all         # Wait for all workers to finish (default, currently the only option)
      fail_fast: false       # If true, cancel remaining workers when one fails (optional)
      workers:
        quality:
          title: Quality review
          prompt: |
            Review code for bugs and security issues.
            Emit one payload named `review-findings` with your report.
            Do not emit loop-control events.
        testing:
          title: Testing review
          prompt: |
            Review test coverage.
            Emit one payload named `review-findings` with your report.
            Do not emit loop-control events.
```

Key mechanics:

- Each worker runs as an isolated agent process with its own **channel ID** (matching the worker key, e.g., `quality`, `testing`).
- Workers share the same WAL file but write to their own channel. **Workers cannot emit loop-control events** — only the main channel can control routing.
- After all workers finish, Ralph follows the `fallback-route` to the next prompt.
- The next prompt can read each worker's output using `"$RALPH_BIN" get --channel quality review-findings`.
- Worker IDs may only contain ASCII letters, digits, `-`, and `_`.
- Each worker's full output is captured to `.ralph-runtime/channels/<channel-id>/output.log` under the run directory.

### Transition guards

Transition guards let you override the agent's routing decision based on verifiable conditions. They attach to a specific transition and run **after** the agent emits a loop-control event but **before** Ralph acts on it.

```yaml
prompts:
  task:
    title: Task executor
    fallback-route: task
    transition-guards:
      stop-ok:                          # Guard the "stop-ok" transition
        - type: file_not_contains       # Check type
          path: "{ralph-option:plan-file}"  # File to inspect (supports option tokens)
          literal: "- [ ]"              # Text to search for
          on-fail: continue             # What to do if the guard fails
          note: "task completion ignored: plan still contains unchecked items"
    prompt: |
      ...
```

**Transition targets** that can be guarded:

| Target | Triggered when agent emits |
|---|---|
| `continue` | `loop-continue` |
| `stop-ok` | `loop-stop:ok` |
| `stop-error` | `loop-stop:error` |
| `route:<prompt-id>` | `loop-route` with body `<prompt-id>` |

**Guard types:**

| Type | Fields | Passes when |
|---|---|---|
| `file_exists` | `path` | The file exists on disk |
| `file_contains` | `path`, `literal` | The file exists and contains the literal text |
| `file_not_contains` | `path`, `literal` | The file exists and does NOT contain the literal text |
| `event_exists` | `event`, `channel` (optional) | The WAL contains at least one record for this event |
| `event_contains` | `event`, `literal`, `channel` (optional) | The latest WAL record for this event contains the literal text |

Guard paths support `{ralph-env:PROJECT_DIR}` and `{ralph-option:...}` tokens. Relative paths resolve against the project directory.

**Failure actions** — what happens when a guard fails:

| `on-fail` | Effect |
|---|---|
| `continue` | Ignore the agent's stop/route and re-run the same prompt |
| `error` | End the workflow with an error |
| `route` | Route to a different prompt (requires `route: <prompt-id>` field) |

Additional failure fields:
- `note` — logged to the console when the guard fails (optional)
- `summary` — used as the workflow summary if the failure ends the run (optional)

Multiple guards on the same transition are evaluated in order. The first failure determines the outcome.

### Workflow management

```bash
ralph --workflows                  # List all available workflows
ralph --show-workflow <ID>         # Print the full YAML source of a workflow
ralph --edit-workflow <ID>         # Open the workflow file in $EDITOR / $VISUAL
ralph w <WORKFLOW> [REQUEST]       # Run any workflow directly
ralph w <WORKFLOW> --file REQ.md   # Read request from a file
cat REQ.md | ralph w bare          # Pipe request via stdin
```

Protected builtin workflows (`plan`, `task`, `review`, `finalize`) cannot be edited with `--edit-workflow`. To customize them, create a new workflow with a different ID.

---

## Custom agent definitions

Ralph is agent-agnostic. Any CLI tool that can accept a prompt and run headlessly can be wired up as a Ralph agent.

### Agent configuration schema

Agents are defined in `~/.config/ralph/config.toml` under `[[agents]]`:

```toml
[[agents]]
id = "my-agent"               # Unique identifier (required)
name = "My Custom Agent"       # Human-readable name (required)
hidden = false                 # If true, hidden from `--agents` list (optional)

[agents.runner]
mode = "exec"                  # "exec" or "shell" (required)
program = "my-agent"           # Binary name or path (required for exec mode)
args = ["--headless", "--json", "{prompt}"]   # Arguments (exec mode)
prompt_input = "argv"          # How the prompt is delivered (required)
prompt_env_var = "PROMPT"      # Env var name for env prompt input (default: "PROMPT")
session_timeout_secs = 3600    # Max wall-clock time per invocation in seconds (optional)
idle_timeout_secs = 600        # Kill after this many seconds of no output (optional)

[agents.runner.env]            # Extra environment variables (optional)
MY_CONFIG = '{"auto_approve": true}'
```

### Runner modes

**`exec` mode** — runs a binary directly:

```toml
[agents.runner]
mode = "exec"
program = "codex"
args = ["exec", "--ephemeral", "--json"]
prompt_input = "stdin"
```

Ralph resolves the `program` from `$PATH`. If it's not found, the agent is marked as "unavailable" and Ralph falls back to the next available agent.

**`shell` mode** — runs a shell command string:

```toml
[agents.runner]
mode = "shell"
command = "cat {prompt} | my-custom-pipeline"
prompt_input = "argv"
```

Shell mode agents are always marked as available since Ralph cannot pre-check arbitrary shell commands.

### Prompt input methods

| Method | Behavior |
|---|---|
| `argv` | The prompt text replaces `{prompt}` in the `args` array |
| `stdin` | The prompt text is piped to the process's stdin, which is then closed |
| `env` | The prompt text is set as the environment variable named by `prompt_env_var` |

### Template variables

The `args` array and `command` string support the `{prompt}` placeholder, which is replaced with the full interpolated prompt text at runtime.

### Environment variables injected by Ralph

Every agent process receives these environment variables automatically:

| Variable | Value |
|---|---|
| `RALPH_BIN` | Absolute path to the Ralph binary (used for `"$RALPH_BIN" signal/payload/get`) |
| `RALPH_RUN_DIR` | Absolute path to the current run directory |
| `RALPH_WAL_PATH` | Absolute path to the WAL file for this run |
| `RALPH_PROMPT_PATH` | Absolute path to the workflow YAML source file |
| `RALPH_CHANNEL_ID` | Channel ID for this invocation (`main` for single prompts, worker ID for parallel workers) |

Plus any entries from the `[agents.runner.env]` table.

### Agent availability and fallback

Ralph resolves the effective agent at startup:

1. If a project-level `agent` is set in `.ralph/config.toml`, use that.
2. Otherwise, use the `default_agent` from `~/.config/ralph/config.toml`.
3. If the configured agent is unavailable (binary not found on `$PATH`), Ralph falls back to the highest-priority available agent.

Priority order for fallback: OpenCode → Raijin → (user-defined agents) → others.

```bash
ralph --agents                     # Show configured, effective, and available agents
ralph --set-project-agent claude   # Persist for this project
ralph --set-user-agent opencode    # Persist as user default
ralph --agent codex -b "fix it"    # One-time override (no persistence)
```

### Timeouts

Timeouts can be set per-agent in config or overridden at runtime:

```toml
[agents.runner]
session_timeout_secs = 3600    # Kill after 1 hour total
idle_timeout_secs = 600        # Kill after 10 minutes of no output
```

```bash
ralph --session-timeout 30m --idle-timeout 5m "implement caching"
```

Duration syntax: `45s`, `5m`, `2h`. When a timeout fires, the agent process is killed and the workflow ends with an error.

---

## Configuration

### File locations

| Scope | Path | Purpose |
|---|---|---|
| User | `~/.config/ralph/config.toml` | Default agent, agent registry, theme, editor |
| Project | `.ralph/config.toml` | Per-project agent override |
| Workflows | `~/.config/ralph/workflows/` | Custom workflow YAML files |
| Artifacts | `.ralph/runs/` | Run directories, WAL files, output logs |

The user config path can be overridden with the `RALPH_CONFIG_HOME` environment variable.

The `.ralph/` directory is automatically gitignored (Ralph seeds a `.ralph/.gitignore` containing `*`).

### Config merging

Ralph loads configuration in layers:

1. **Defaults** — built-in agent definitions and default theme
2. **User config** — `~/.config/ralph/config.toml` overrides defaults
3. **Project config** — `.ralph/config.toml` overrides the merged result

If the user config contains only builtin agents (auto-seeded on first run), Ralph keeps it in sync with new releases—adding new builtin agents automatically. If you've added custom agents, Ralph leaves your registry untouched.

### Full config schema

```toml
# Default agent when no project-level override exists
default_agent = "opencode"

# Project-level agent override (usually only in .ralph/config.toml)
agent = "claude"

# Editor for --edit-workflow and plan review (falls back to $VISUAL, then $EDITOR)
editor_override = "code -w"

# Agent registry — replacing this list replaces the entire registry
[[agents]]
id = "opencode"
name = "OpenCode"
builtin = true

[agents.runner]
mode = "exec"
program = "opencode"
args = ["run", "--thinking", "--format", "json"]
prompt_input = "stdin"

[agents.runner.env]
OPENCODE_CONFIG_CONTENT = '{"$schema":"https://opencode.ai/config.json","permission":"allow","lsp":false}'

# Theme configuration
[theme]
mode = "auto"              # auto | dark | light
accent_color = "cyan"      # Used for headers, banners, active UI
success_color = "green"    # Completed status
warning_color = "yellow"   # Max-iterations status
error_color = "red"        # Failed status
```

### Theming

Ralph detects your terminal background automatically using the `COLORFGBG` environment variable or the `RALPH_THEME_MODE` override.

Available named colors: `black`, `red`, `green`, `yellow`, `blue`, `magenta`, `cyan`, `gray`, `dark_gray`, `light_red`, `light_green`, `light_yellow`, `light_blue`, `light_magenta`, `light_cyan`, `white`.

Each semantic color has different defaults for dark and light terminals:

| Semantic | Dark default | Light default |
|---|---|---|
| `accent_color` | `cyan` | `blue` |
| `success_color` | `light_green` | `green` |
| `warning_color` | `light_yellow` | `magenta` |
| `error_color` | `light_red` | `red` |

Colors affect the run header banner, iteration markers, event notices, status labels, and the workflow result line. Set `NO_COLOR` to disable colors entirely.

### Inspecting configuration

```bash
ralph --show-config              # Effective merged configuration
ralph --show-config=user         # User-level config file
ralph --show-config=project      # Project-level config file
```

---

## Architecture

### Crate layout

Ralph is a Rust workspace with four crates, each with a clear responsibility:

```
crates/
├── ralph-core       Zero-dependency foundation
├── ralph-runner     Async process execution
├── ralph-app        Workflow orchestration and planning
└── ralph-cli        CLI entry point
```

**`ralph-core`** — the foundation layer with no runtime dependencies. Contains:
- **Workflow engine**: YAML loading, validation, prompt/option/request/guard definitions, builtin workflow embedding (compiled into the binary via `include_str!`)
- **Agent definitions**: the `CodingAgent` enum with all seven builtin agents, `RunnerConfig` schema, `AgentConfig`, availability detection via `$PATH` probing
- **Configuration**: layered TOML loading/merging, atomic file writes, project artifact directory management
- **Event protocol**: WAL record schema (`AgentEventRecord`), loop-control event parsing, planning event constants
- **Theming**: terminal background detection, ANSI color system, resolved theme palettes

**`ralph-runner`** — the async process execution layer. Handles:
- Spawning agent processes with `tokio::process::Command`
- Routing the prompt to stdin, argv, or env depending on the agent config
- Injecting `RALPH_BIN`, `RALPH_RUN_DIR`, `RALPH_WAL_PATH`, `RALPH_PROMPT_PATH`, `RALPH_CHANNEL_ID` into the environment
- Streaming stdout/stderr in real-time via `tokio::sync::mpsc` channels
- Session timeout (total wall-clock) and idle timeout (time since last output) enforcement
- Cooperative cancellation via `RunControl`
- Abort-on-drop handles for clean process cleanup

**`ralph-app`** — the application logic layer. Contains:
- **Workflow execution**: the main iteration loop, loop-control event reduction, transition guard evaluation, fallback routing
- **Planning intercept**: special handling for `planning-question` and `planning-target-path` events — pausing the loop to ask the user questions, present drafts for review, and record accept/revise/reject decisions
- **Parallel execution**: spawning concurrent workers via `JoinSet`, channel isolation, fail-fast support, output log capture
- **Prompt interpolation**: expanding `{ralph-...}` tokens, rendering skill-emit blocks, generating CLI command instructions
- **Console delegate**: terminal UI for iteration banners, event notices, planning question prompts, draft review with accept/revise/reject/external-editor support, JSON output extraction for structured agent output

**`ralph-cli`** — the CLI frontend. Contains:
- Argument parsing via `clap` with dynamic workflow-specific subcommands
- The `ralph signal`, `ralph payload`, and `ralph get` internal commands (only available inside agent runs, gated by `RALPH_WAL_PATH`)
- Output formatting for run headers, workflow results, agent lists, and workflow definitions
- Guided mode orchestration chaining plan → task → review

### Event system and WAL

The Write-Ahead Log (WAL) is the communication backbone between Ralph and its agents. It's an append-only ndjson file at:

```
.ralph/runs/<workflow-id>/<run-id>/.ralph-runtime/agent-events.wal.ndjson
```

Each line is a JSON record:

```json
{
  "v": 1,
  "ts_unix_ms": 1718000000000,
  "run_id": "12345-1718000000000",
  "channel_id": "main",
  "event": "loop-route",
  "body": "verify",
  "project_dir": "/home/user/project",
  "run_dir": "/home/user/project/.ralph/runs/dbv/12345-1718000000000",
  "prompt_path": "/home/user/.config/ralph/workflows/dbv.yml",
  "prompt_name": "build",
  "pid": 98765
}
```

**Agents emit events** using the `RALPH_BIN` binary:

```bash
# Signal (event with no body)
"$RALPH_BIN" signal 'loop-continue'

# Payload (event with a body)
"$RALPH_BIN" payload 'loop-stop:ok' 'all tasks complete'
"$RALPH_BIN" payload 'loop-route' 'verify'
"$RALPH_BIN" payload 'review-findings' 'Findings:\n- none'

# Read (get the latest payload for an event)
"$RALPH_BIN" get planning-target-path
"$RALPH_BIN" get --channel quality review-findings
```

**Loop-control events** drive the state machine:

| Event | Effect |
|---|---|
| `loop-continue` | Re-run the current prompt in the next iteration |
| `loop-stop:ok` | End the workflow with status `completed` |
| `loop-stop:error` | End the workflow with status `failed` |
| `loop-route` | Transition to the prompt named in the body |

If the agent emits multiple loop-control events, the **last one wins**. If no loop-control event is emitted, Ralph follows the prompt's `fallback-route`.

Events on non-main channels (parallel workers) cannot emit loop-control events — this is enforced at write time.

**Planning events** are intercepted by the host rather than driving loop control:

| Event | Purpose |
|---|---|
| `planning-question` | Ask the user a clarifying question (JSON body with `question`, `options`, `context`) |
| `planning-answer` | Host records the user's answer |
| `planning-target-path` | Agent announces a draft plan file for host review |
| `planning-review` | Host records accept/revise/reject decision |
| `planning-progress` | Host records cumulative planning state for iteration continuity |
| `planning-plan-file` | Host records the final accepted plan file path |

### Run directory structure

Each workflow execution creates a run directory:

```
.ralph/runs/<workflow-id>/<pid>-<timestamp>/
├── request.txt                              # The original request text
└── .ralph-runtime/
    ├── agent-events.wal.ndjson              # The WAL
    └── channels/                            # Parallel worker output logs
        ├── quality/
        │   └── output.log
        └── testing/
            └── output.log
```

The run ID is `<process-pid>-<unix-timestamp-ms>`, ensuring uniqueness across concurrent runs.

### Iteration lifecycle

Each iteration follows this sequence:

1. **Check cancellation** — abort if the user pressed Ctrl+C
2. **Resolve the current prompt** — look up the prompt definition by ID
3. **Emit IterationStarted event** — display the iteration banner
4. **Execute the prompt**:
   - For single prompts: interpolate tokens, invoke the runner, stream output
   - For parallel prompts: spawn all workers concurrently, wait for all (or fail-fast)
5. **Read new WAL events** — scan from the pre-invocation offset
6. **Planning intercept** — if the agent emitted `planning-question` or `planning-target-path`, pause for user interaction and loop
7. **Reduce loop control** — find the last loop-control event from the main channel
8. **Evaluate transition guards** — if the requested transition has guards, check them
9. **Apply the result**: continue (same prompt), route (different prompt), finish (ok/error/canceled)
10. **If max iterations reached** — end with status `max_iterations`

---

## Building from source

### Prerequisites

- Rust stable (edition 2024)
- `cargo`, `bash`, optionally `shellcheck`

### Build

```bash
git clone https://github.com/francescoalemanno/ralph-cli
cd ralph-cli
cargo build --release
# Binary: target/release/ralph
```

### Test suite

The `test.sh` script runs the full validation pipeline:

```bash
./test.sh
```

This executes, in order:

1. **Shell syntax** — `bash -n` on `test.sh`, `install`, `scripts/release.sh`
2. **Shell lint** — `shellcheck` on the same files (skipped if shellcheck is not installed)
3. **Format** — `cargo fmt --all --check`
4. **Typecheck** — `cargo check --workspace --all-targets --all-features`
5. **Clippy** — `cargo clippy --workspace --all-targets --all-features -- -D warnings`
6. **Tests** — `cargo test --workspace --all-targets` and `cargo test --workspace --doc`
7. **Docs** — `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps`

### Release process

Releases are cut with `scripts/release.sh`:

```bash
scripts/release.sh patch          # 0.5.3 → 0.5.4
scripts/release.sh minor          # 0.5.3 → 0.6.0
scripts/release.sh 1.0.0          # explicit version
scripts/release.sh patch --no-push  # create commit and tag locally only
```

The script bumps the workspace version, syncs internal crate dependency versions, refreshes `Cargo.lock`, runs the full test suite, creates a release commit and tag, and pushes to origin. Pushing the tag triggers the CI pipeline which cross-compiles binaries for Linux (x86_64, aarch64 musl) and macOS (x86_64, aarch64) and uploads them to a GitHub release with auto-generated conventional-commit release notes.

### Installation

Pre-built binaries for Linux and macOS:

```bash
curl -fsSL https://raw.githubusercontent.com/francescoalemanno/ralph-cli/main/install | bash
```

The installer detects the OS and architecture, downloads the matching binary from GitHub releases, installs to `~/.local/bin` (configurable via `RALPH_INSTALL_DIR`), and adds the directory to `$PATH` in common shell profiles.

To install a specific version:

```bash
RALPH_VERSION=v0.5.3 curl -fsSL https://raw.githubusercontent.com/francescoalemanno/ralph-cli/main/install | bash
```
