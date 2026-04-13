# Ralph

**A durable planning and execution workflow engine for AI coding agents.**

Ralph turns any AI coding CLI—Codex, Claude Code, OpenCode, Gemini CLI, Droid, Raijin, or your own—into a disciplined multi-phase development pipeline. It orchestrates structured workflows that plan, build, review, and verify code changes, keeping the agent in the "smart zone" by giving each iteration a fresh context window and a clear task.

```
curl -fsSL https://raw.githubusercontent.com/francescoalemanno/ralph-cli/main/install | bash
```

## Why Ralph

AI coding agents are powerful but unreliable when given large, open-ended tasks.
They lose coherence as context fills up, drift off-plan, skip validation, and produce inconsistent results across runs.

Ralph solves this by breaking work into small, scoped iterations and steering each one with structured prompts, persistent state, and backpressure from tests:

- **Fresh context every iteration** — each agent invocation starts clean, reads the current state from disk, does one task, writes results, and exits.
- **Durable plans on disk** — a markdown plan file persists between iterations, acting as shared memory the agent reads and updates.
- **Backpressure** — tests, linters, and type-checkers run inside each iteration; the agent must fix failures before it can mark work done.
- **Agent-agnostic** — swap the underlying agent with a single flag or config change. Ralph handles the orchestration; the agent handles the coding.

## Quick start

```bash
# Guided mode: plan interactively, then build and review
ralph "Add a caching layer to the API"

# Plan only
ralph --plan "Refactor the auth module"

# Execute an existing plan
ralph -t docs/plans/2025-04-13-cache-layer.md

# One-shot bare request (no planning, just loop)
ralph -b "Fix the failing tests in src/server"

# Review the latest plan's implementation
ralph -r

# Use a specific agent
ralph --agent claude "Implement rate limiting"
```

## How it works

Ralph runs **workflows** — YAML-defined state machines where each state is a prompt sent to an AI coding agent. The agent runs in headless mode, emits structured events back to Ralph via a small CLI protocol, and Ralph decides what happens next: loop the same prompt, route to a different one, or stop.

```
┌──────────┐     ┌──────────┐     ┌──────────┐     ┌──────────┐
│   Plan   │────▶│  Build   │────▶│  Review  │────▶│ Finalize │
│          │     │ (loop)   │     │ (fanout) │     │          │
└──────────┘     └──────────┘     └──────────┘     └──────────┘
     │                │                │                │
     ▼                ▼                ▼                ▼
  plan file       code + tests    fix findings     rebase + tidy
  on disk         committed       committed        pushed
```

### The guided pipeline

Running `ralph` with no flags enters **guided mode**, which chains three workflows:

1. **Plan** — the agent explores the codebase, asks clarifying questions one at a time, and drafts a markdown plan. You review, revise, or accept.
2. **Task** — the agent reads the plan, picks the first uncompleted task section, implements it, runs tests, marks it done, commits, and exits. Ralph loops until all tasks are checked off.
3. **Review** — five parallel review agents (quality, implementation, testing, simplification, documentation) inspect the diff. A fix pass addresses confirmed findings, then a focused second review checks for remaining critical issues.

An optional **Finalize** step rebases onto main and tidies the commit history.

### Built-in workflows

| Workflow | Description |
|---|---|
| `plan` | Interactive planning loop with clarifying questions, draft review, and external editor support |
| `task` | Execute one plan task per iteration until the implementation stage is complete |
| `review` | Parallel five-channel code review with two-pass fix-and-verify cycle |
| `finalize` | Rebase, squash, and rerun checks |
| `bare` | Pass a request straight to the agent with no scaffolding |
| `default` | Single-file decompose → build → verify loop |
| `dbv` | Decompose-build-verify with explicit dispatch routing |

## Supported agents

Ralph ships with built-in definitions for seven agents:

| Agent | CLI | Prompt delivery |
|---|---|---|
| OpenCode | `opencode run` | stdin |
| Codex | `codex exec` | stdin |
| Claude Code | `claude -p` | argv |
| Droid | `droid exec` | argv |
| Gemini CLI | `gemini -y -p` | argv |
| Pi Coding | `pi --no-session -p` | argv |
| Raijin | `raijin -ephemeral` | argv |

Switch agents per-project or per-run:

```bash
ralph --set-project-agent claude     # persist for this project
ralph --agent codex -b "fix tests"   # one-time override
```

## CLI reference

```
ralph                                  Guided mode: plan → build → review
ralph --plan[=DESCRIPTION]             Plan only
ralph -t <PLAN_FILE>                   Execute tasks from plan
ralph -b [REQUEST]                     Bare workflow
ralph -r [PLAN_FILE]                   Review only
ralph -f [PLAN_FILE]                   Finalize only
ralph --agents                         List agents and availability
ralph --agent <ID>                     Override agent for this run
ralph --max-iterations <N>             Cap iterations
ralph --session-timeout 30m            Kill after 30 minutes
ralph --idle-timeout 5m                Kill after 5 minutes of no output
```

## Advanced usage

Custom workflows, custom agent definitions, theming, architecture details, and building from source are covered in **[ADVANCED.md](ADVANCED.md)**.

## License

[MIT](LICENSE) — Copyright © 2026 Francesco Alemanno

---

### Acknowledgements

Ralph builds on the ideas behind the [Ralph Wiggum technique](https://github.com/ghuntley/how-to-ralph-wiggum) by [Geoffrey Huntley](https://ghuntley.com/ralph/), which demonstrated that a simple bash loop piping a prompt into an AI CLI (`while : ; do cat PROMPT.md | claude ; done`) can drive surprisingly effective autonomous coding sessions. The original technique relies on the agent reading a shared `IMPLEMENTATION_PLAN.md` from disk each iteration, with `PROMPT.md` and `AGENTS.md` providing the steering context.

Ralph takes that core insight—**fresh context per iteration, persistent state on disk, one task at a time**—and makes it durable, structured, and reliable:

- **Typed workflow engine** instead of a bash while-loop: YAML-defined state machines with validated routing, transition guards, and parallel fanout.
- **Agent-agnostic orchestration**: plug in any coding CLI instead of being tied to a single tool.
- **Structured event protocol**: agents communicate through a WAL-backed event system instead of relying on file conventions.
- **Interactive planning with human review**: clarifying questions, draft inspection, external editor support, and accept/revise/reject cycles rather than hoping the agent writes a good plan on the first try.
- **Multi-pass parallel code review**: five concurrent review channels with automated fix passes instead of trusting the agent's self-assessment.
- **Runtime controls**: iteration limits, session timeouts, idle timeouts, and cancellation for predictable execution.

The original technique is a brilliant demonstration of the power of simplicity. Ralph wraps that simplicity in the guardrails needed for production use.
