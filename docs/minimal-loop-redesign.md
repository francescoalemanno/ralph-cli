# Minimal Loop Redesign

## Summary

Ralph should be reduced to a minimal looping harness with one protocol token:

`<iteration-promise:done>`

The harness must:

- execute a prompt exactly as written
- inject nothing into the prompt
- terminate only when the output contains `<iteration-promise:done>`
- otherwise continue until `max_iterations` or user cancellation

Everything else that is currently opinionated in Ralph must move out of the core.

That includes:

- spec/progress/feedback workflows
- planner/builder separation
- prompt wording
- artifact layout
- review conventions
- target-specific instructions

There should be no runtime template system. Instead, the tool may generate scaffolds on demand during target initialization. Scaffolds only create files. They do not change runner behavior.

## Goals

- make the core match the original Ralph spirit: loop + prompt + operator skill
- simplify the codebase by removing planner/builder as architectural concepts
- simplify the CLI and TUI so they present one model instead of multiple special cases
- keep scaffolds out of the runtime model

## Non-Goals

- no clarification protocol
- no `CONTINUE` marker
- no behavioral validation or stall detection
- no prompt injection of iteration count, artifact paths, or instructions
- no controller knowledge of spec/progress/feedback

## Core Model

Ralph has three first-class concepts:

- `Target`
- `Run`
- `Scaffold`

### Target

A target is a loopable work unit.

A target has:

- a human-readable id
- one or more prompt files
- optional per-target settings such as `max_iterations`
- a directory at `./.ralph/targets/<target-id>/`

Target discovery is directory-based:

- every folder directly under `./.ralph/targets/` is a target
- the target id is the folder name
- `target.toml` is optional metadata, not the source of truth

Prompt files are discovered by location and extension:

- any file in the target folder that ends in `.md`

Examples:

- `0_plan.md`
- `1_build.md`
- `prompt_main.md`

Files in the target folder that do not end in `.md` are normal target files and are never considered runnable prompts by the core.

### Scaffold

A scaffold is an initialization helper.

A scaffold may create:

- one or more prompt files ending in `.md`
- companion files such as `IMPLEMENTATION_PLAN.md`
- initial target metadata

A scaffold does not introduce a new runtime mode and does not affect loop semantics.

### Run

A run is one invocation of the looping harness against a target and one selected prompt file.

The runner:

1. reads the selected prompt file
2. parses any `<<ralph-watch:filename>>` tags from the prompt
3. removes those watch tags before sending the prompt to the coding agent
4. snapshots watched files relative to the repository root before and after each iteration
5. sends the trimmed prompt to the configured coding agent
6. streams the agent output
7. if watched files are declared and unchanged after an iteration, marks the run complete
8. stops if the output contains `<iteration-promise:done>`
9. otherwise starts the next iteration

## Runner Semantics

The core runner knows only:

- `max_iterations`
- cancellation
- watched-file directives declared as `<<ralph-watch:filename>>`
- the done token `<iteration-promise:done>`

It does not know:

- prompt meaning
- artifacts
- repo conventions
- what "progress" means
- whether the agent changed files

Recognized prompt-side protocol:

- exact literal substring match for `<iteration-promise:done>`
- zero or more `<<ralph-watch:filename>>` tags inside prompt files

## Proposed Target Layout

Store each target under:

```text
.ralph/targets/<target-id>/
```

Minimum files:

```text
.ralph/targets/<target-id>/
  target.toml
  prompt_main.md
```

Additional files may exist, but the core does not assign them any meaning.

Prompt files are the only runnable units. Companion files may exist in the same target directory, but they are never executed unless they end in `.md`.

Example minimal target:

```text
.ralph/targets/<target-id>/
  target.toml
  prompt_main.md
```

Example default-style target:

```text
.ralph/targets/<target-id>/
  target.toml
  0_plan.md
  1_build.md
```

Repository-root operational guidance remains outside the target:

```text
AGENTS.md
IMPLEMENTATION_PLAN.md
```

## Scaffold Model

Scaffolds are setup-time only.

Reason:

- they help with initialization without leaking workflow semantics into the runner
- they preserve a bare runtime model
- they avoid rebuilding planner/build modes into the architecture

The first milestone should support:

- a target with one or more `.md` prompt files
- a loop runner
- a done marker

The first scaffold set already identified is a default scaffold inspired by Clayton Farr's Ralph workflow, but cleaned of Claude-, Sonnet-, Opus-, and subagent-specific instructions.

That scaffold should generate both prompt files together:

- `0_plan.md`
- `1_build.md`

Those two prompts belong to one scaffold and should never be initialized separately.

## CLI Redesign

The CLI should present one simple mental model.

Proposed commands:

- `ralph init`
- `ralph new <target>`
- `ralph run <target>`
- `ralph ls`
- `ralph show <target>`
- `ralph edit <target>`

Behavior:

- `new` creates a target and may apply a scaffold
- `run` starts the loop runner for one selected prompt file
- `edit` opens a selected prompt file by default
- `show` prints the target files, or one selected file
- `ls` lists targets

Recommended flags:

- `ralph new <target>`
- `ralph new <target> --scaffold default`
- `ralph run <target> --prompt 0_plan.md`
- `ralph run <target> --prompt 1_build.md`

If a target has multiple prompt files and no prompt is specified, the CLI should ask the user to choose one.

Commands to remove:

- `plan`
- `build`
- progress-revision special commands and flags

## TUI Redesign

The TUI should also pivot to the same model:

- left pane: targets
- right pane: target summary and files
- footer actions: new, run, edit, show files, delete

Each target view should show:

- target id
- prompt files available for execution
- last run status
- list of target files

The TUI should not assume anything beyond the existence of at least one `.md` prompt file.

New target flow:

1. choose target name
2. choose scaffold
3. create files
4. open one of the generated prompt files

The TUI must support scaffold initialization too.

For the `default` scaffold, initialization creates both:

- `0_plan.md`
- `1_build.md`

Those prompt files should include `<<ralph-watch:IMPLEMENTATION_PLAN.md>>` so completion is driven by watched-file state rather than scaffold identity.

When running a target with multiple prompt files, the TUI should let the user select which prompt to execute.

## Config Redesign

Replace the current planner/builder split with a single runner config.

Proposed `AppConfig` shape:

```toml
[runner]
program = "codex"
args = ["exec", "--dangerously-bypass-approvals-and-sandbox", "--ephemeral"]
prompt_transport = "stdin"
prompt_env_var = "PROMPT"

max_iterations = 40
editor_override = "nvim"
```

Fields to remove from the core config:

- `planner`
- `builder`
- `planning_max_iterations`
- `builder_max_iterations`
- `question_support`

## Code Structure

### New Core Types

Suggested new types:

- `TargetConfig`
- `TargetSummary`
- `PromptFile`
- `ScaffoldId`
- `LoopRunner`
- `LoopResult`

Suggested removals:

- `RunnerMode`
- planning/build prompt contexts
- planning/build marker parsers
- clarification types
- progress-revision request types

### App Layer

`ralph-app` should expose a much smaller surface:

- create target
- create target from scaffold
- list targets
- load target
- list prompt files for target
- run target loop for selected prompt
- open selected prompt
- read target files
- delete target

## Migration Strategy

### Phase 1: Introduce the New Core

- add `LoopRunner`
- add `TargetConfig`
- add prompt discovery for any target-local `.md` file
- keep old commands temporarily as wrappers if needed

### Phase 2: Simplify the Product Surface

- remove planner/builder logic from the controller
- make target-local `.md` prompt files the only universal runnable concept

### Phase 3: Simplify the UI

- rewrite CLI help and commands around `target/run`
- rewrite TUI screens around prompt selection and target files instead of fixed artifacts

### Phase 4: Add Scaffolds

- add scaffold generation to CLI and TUI
- add the first default scaffold
- generate `0_plan.md` and `1_build.md` together
- keep scaffolds as initialization only, never as controller semantics

### Phase 5: Remove Legacy Concepts

- delete `RunnerMode`
- delete planner/build prompt generation
- delete question parsing
- delete progress-revision machinery
- collapse config to a single runner section

## Tradeoffs

### Pros

- much simpler mental model
- much smaller controller surface
- far closer to the original Ralph technique
- easier to add new workflows later without touching core logic
- multiple prompts per target without adding runner modes

### Cons

- fewer guardrails
- old concepts like "planning complete" and "build complete" disappear from the core
- scaffold quality becomes important for structured workflows

## Decision

Adopt a minimal harness with one recognized token:

`<iteration-promise:done>`

Use target-local `.md` files as the prompt discovery convention.

Do not introduce runtime templates. Add setup-time scaffolds only.

The first structured scaffold should generate these two prompts together:

- `0_plan.md`
- `1_build.md`
