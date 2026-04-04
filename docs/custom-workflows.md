# Custom Workflow Authoring

This document explains how to create, register, and run custom Ralph workflows from the user configuration directory.

It is written for workflow authors, not for Ralph engine developers. Everything below is about the files you write, the fields Ralph reads, and the commands you run.

## Scope

With the current workflow system, a target can expose:

- prompt entrypoints
- flow entrypoints

A prompt entrypoint is a single runnable prompt.

A flow entrypoint is a graph of nodes such as:

- `prompt`
- `decision`
- `pause`
- `interactive`
- `action`
- `finish`

You use prompt entrypoints when one prompt is enough.

You use flow entrypoints when you need:

- multiple phases
- loops
- manual choices
- file-derived routing
- scratch rebuilds
- goal interview steps

## Where User Workflows Live

Ralph uses the user config directory:

- `$XDG_CONFIG_HOME/ralph/` when `XDG_CONFIG_HOME` is set
- otherwise `~/.config/ralph/`

The standard layout for user-authored workflow assets is:

```text
~/.config/ralph/
  config.toml
  flows/
    build_revise.toml
    release_loop.toml
  prompts/
    build_revise/
      revise.md
      build.md
    release_loop/
      prepare.md
      release.md
```

Recommended convention:

- put reusable flow graphs in `flows/`
- put reusable prompt files in `prompts/`
- group prompt files by workflow name

## How A Target Opts Into A User Workflow

User workflows are not discovered globally on their own. A target must reference them in its `target.toml`.

Example target configuration:

```toml
id = "demo"
scaffold = "single_prompt"
default_entrypoint = "main"

[[entrypoints]]
id = "main"
kind = "flow"
flow = "user://flows/build_revise.toml"
edit_path = "GOAL.md"

[entrypoints.params]
goal_file = "GOAL.md"
state_file = "state.toml"
journal_file = "journal.txt"

[[entrypoints]]
id = "scratch"
kind = "prompt"
path = "scratch.md"
edit_path = "scratch.md"
```

Important points:

- `default_entrypoint` selects what `ralph run <target>` uses by default.
- `kind = "flow"` points to a workflow graph.
- `kind = "prompt"` points to a single prompt file.
- `edit_path` tells Ralph what file to open for `ralph edit <target>` when the entrypoint is selected as default.
- `[entrypoints.params]` defines template parameters for that entrypoint.

## Artifact Reference Schemes

Flow and prompt references can point to four places.

### `user://`

Use this for user-config assets.

Examples:

- `user://flows/build_revise.toml`
- `user://prompts/build_revise/revise.md`

### `project://`

Use this for project-local shared assets under `.ralph/`.

Examples:

- `project://flows/release.toml`
- `project://prompts/release/checklist.md`

### `builtin://`

Use this for Ralph’s embedded builtins.

Examples:

- `builtin://flows/plan_driven.toml`
- `builtin://flows/task_driven.toml`

### Relative Paths

A plain path is resolved relative to the target directory.

Examples:

- `flow = "flow.toml"`
- `path = "review.md"`

## EntryPoint Schema

Each `[[entrypoints]]` item supports one of two shapes.

### Prompt Entrypoint

```toml
[[entrypoints]]
id = "scratch"
kind = "prompt"
path = "scratch.md"
hidden = false
edit_path = "scratch.md"
```

Fields:

- `id`: unique identifier inside the target
- `kind`: must be `prompt`
- `path`: prompt artifact reference or target-relative file
- `hidden`: optional, defaults to `false`
- `edit_path`: optional file Ralph should open for editing

### Flow Entrypoint

```toml
[[entrypoints]]
id = "main"
kind = "flow"
flow = "user://flows/build_revise.toml"
hidden = false
edit_path = "GOAL.md"

[entrypoints.params]
goal_file = "GOAL.md"
state_file = "state.toml"
```

Fields:

- `id`: unique identifier inside the target
- `kind`: must be `flow`
- `flow`: flow artifact reference
- `hidden`: optional, defaults to `false`
- `edit_path`: optional file Ralph should open for editing
- `[entrypoints.params]`: optional string-to-string template parameters

## Template Parameters

Template parameters are expanded before Ralph parses the flow or prompt.

Syntax:

```text
{{param_name}}
```

Example:

If the target entrypoint defines:

```toml
[entrypoints.params]
goal_file = "GOAL.md"
```

then a prompt containing:

```text
Read {{goal_file}}.
```

is expanded to:

```text
Read GOAL.md.
```

Use template parameters for:

- target-local filenames
- reusable prompt paths
- journal names
- archive prefixes

Do not use them for runtime paths Ralph already provides through prompt environment placeholders.

## Prompt Files

Prompt files used by custom workflows are ordinary Ralph prompts.

They support the existing prompt environment placeholders such as:

- `{ralph-env:PROJECT_DIR}`
- `{ralph-env:TARGET_DIR}`
- `{ralph-env:PROMPT_PATH}`
- `{ralph-env:PROMPT_NAME}`

They also support the existing prompt-side directives:

- `{"ralph":"watch","path":"..."}`
- `{"ralph":"complete_when","type":"no_line_contains_all","path":"...","tokens":["..."]}`

Use `watch` when a prompt should stop once a watched file remains unchanged across an iteration.

Use `complete_when` when a prompt should stop once a file no longer contains unresolved markers such as `completed = false`.

## Flow File Structure

A flow file is a TOML document with:

- `version`
- `start`
- one or more `[[nodes]]`

Minimal example:

```toml
version = 1
start = "review"

[[nodes]]
id = "review"
kind = "prompt"
prompt = "user://prompts/build_revise/revise.md"
on_completed = "done"

[[nodes]]
id = "done"
kind = "finish"
summary = "Review complete."
status = "completed"
```

Rules:

- `version` must currently be `1`
- `start` must name an existing node
- every node `id` must be unique

## Node Kinds

### `prompt`

Runs a prompt loop using a prompt artifact.

Example:

```toml
[[nodes]]
id = "build"
kind = "prompt"
prompt = "user://prompts/build_revise/build.md"
max_iterations = 8
on_completed = "record_hash"
on_max_iterations = "paused"
on_failed = "failed"
on_canceled = "canceled"
```

Supported fields:

- `id`
- `kind = "prompt"`
- `prompt`
- `max_iterations` optional
- `rules` optional
- `on_completed`
- `on_max_iterations`
- `on_failed`
- `on_canceled`

Behavior:

- Ralph resolves the prompt artifact
- expands `{{params}}`
- interpolates `{ralph-env:...}` placeholders
- runs the prompt loop
- evaluates `rules` first, if provided
- otherwise follows the status-specific transition fields

Use `rules` when branching depends on state or conditions.

Use `on_completed` and similar fields when the transition is fixed.

### `decision`

Evaluates routing rules without running a prompt.

Example:

```toml
[[nodes]]
id = "dispatch"
kind = "decision"

[[nodes.rules]]
when = { kind = "missing", path = "{{state_file}}" }
goto = "revise"

[[nodes.rules]]
goto = "build"
```

Supported fields:

- `id`
- `kind = "decision"`
- `rules`

At least one rule should always match. End your rule list with an unconditional fallback.

### `pause`

Stops the run and exposes manual actions.

Example:

```toml
[[nodes]]
id = "paused"
kind = "pause"
message = "The workflow is paused."
summary = "Paused."

[[nodes.actions]]
id = "build"
label = "Build"
shortcut = "B"
goto = "build"

[[nodes.actions]]
id = "revise"
label = "Revise"
shortcut = "R"
goto = "revise"
```

Supported fields:

- `id`
- `kind = "pause"`
- `message` optional
- `summary` optional
- `[[nodes.actions]]`

Each action supports:

- `id`
- `label`
- `shortcut` optional
- `goto`

CLI usage for a paused custom workflow:

```bash
ralph run <target> --action build
```

Current practical guidance:

- use CLI `--action` for arbitrary custom actions
- builtin workflows also surface workflow-specific shortcuts in the TUI

### `interactive`

Runs an interactive agent session.

Example:

```toml
[[nodes]]
id = "interview"
kind = "interactive"
prompt = "user://prompts/build_revise/interview.md"
on_completed = "dispatch"
on_failed = "paused"
```

Supported fields:

- `id`
- `kind = "interactive"`
- `prompt`
- `rules` optional
- `on_completed`
- `on_failed`

Use this for:

- goal interviews
- operator-guided refinement
- interactive clarification loops

### `action`

Runs a builtin workflow action.

Example:

```toml
[[nodes]]
id = "record_state_hash"
kind = "action"
action = "set_path_hash_var"
on_success = "paused"

[nodes.args]
key = "state_hash"
path = "{{state_file}}"
```

Supported fields:

- `id`
- `kind = "action"`
- `action`
- `[nodes.args]`
- `on_success`
- `on_error` optional

### `finish`

Ends the current run explicitly.

Example:

```toml
[[nodes]]
id = "done"
kind = "finish"
summary = "Workflow complete."
status = "completed"
```

Supported fields:

- `id`
- `kind = "finish"`
- `summary` optional
- `status` optional

`status` values:

- `completed`
- `max_iterations`
- `failed`
- `canceled`

## Conditions

Conditions live under `when = { ... }`.

### `always`

Always matches.

```toml
when = { kind = "always" }
```

### `exists`

Matches when a file or directory exists.

```toml
when = { kind = "exists", path = "{{state_file}}" }
```

### `missing`

Matches when a file or directory does not exist.

```toml
when = { kind = "missing", path = "{{state_file}}" }
```

### `missing_var`

Matches when a runtime variable is unset.

```toml
when = { kind = "missing_var", key = "state_hash" }
```

### `open_items`

Matches when the referenced file contains a line with both `completed` and `false`.

This is designed for Ralph-style task or plan files.

```toml
when = { kind = "open_items", path = "{{state_file}}" }
```

### `no_open_items`

Inverse of `open_items`.

```toml
when = { kind = "no_open_items", path = "{{state_file}}" }
```

### `path_hash_changed`

Matches when the current hash of a file differs from the runtime variable stored under `key`.

```toml
when = { kind = "path_hash_changed", path = "{{goal_file}}", key = "goal_hash" }
```

Use this together with `set_path_hash_var`.

### `path_hash_equals`

Matches when the current hash of a file equals the runtime variable stored under `key`.

```toml
when = { kind = "path_hash_equals", path = "{{goal_file}}", key = "goal_hash" }
```

### `var_equals`

Matches when a runtime variable equals a specific string value.

```toml
when = { kind = "var_equals", key = "phase", value = "build" }
```

### `selected_action`

Matches the action id selected by the operator.

```toml
when = { kind = "selected_action", action = "rebuild" }
```

This is usually less convenient than modeling operator choices directly with `pause.actions`, but it is available.

### `last_status`

Matches the status produced by the previous runnable node.

```toml
when = { kind = "last_status", status = "completed" }
```

### `any`

Matches when any nested condition matches.

```toml
when = { kind = "any", conditions = [
  { kind = "missing", path = "{{state_file}}" },
  { kind = "missing_var", key = "state_hash" },
] }
```

### `all`

Matches when every nested condition matches.

```toml
when = { kind = "all", conditions = [
  { kind = "exists", path = "{{state_file}}" },
  { kind = "path_hash_changed", path = "{{state_file}}", key = "state_hash" },
] }
```

### `not`

Negates one nested condition.

```toml
when = { kind = "not", condition = { kind = "open_items", path = "{{state_file}}" } }
```

## Actions

These are the builtin actions available to custom workflows today.

### `archive_paths`

Archives files and directories into a timestamped subdirectory.

Example:

```toml
[[nodes]]
id = "rebuild"
kind = "action"
action = "archive_paths"
on_success = "revise"

[nodes.args]
files = ["{{state_file}}", "{{journal_file}}"]
dirs = ["specs"]
archive_root = ".history"
prefix = "rebuild"
```

Arguments:

- `files`: array of target-relative file paths
- `dirs`: array of target-relative directory paths
- `archive_root`: target-relative directory that will hold archive snapshots
- `prefix`: archive filename prefix

### `set_path_hash_var`

Stores the current file hash into a runtime variable.

```toml
[nodes.args]
key = "goal_hash"
path = "{{goal_file}}"
```

Arguments:

- `key`
- `path`

### `set_var`

Stores a plain string value in a runtime variable.

```toml
[nodes.args]
key = "phase"
value = "build"
```

Arguments:

- `key`
- `value`

### `clear_var`

Deletes a runtime variable.

```toml
[nodes.args]
key = "phase"
```

Arguments:

- `key`

## Authoring Patterns

### Pattern 1: Free Prompt Plus Workflow

Use one flow for the main process and separate prompt entrypoints for ad hoc work.

Example:

```toml
default_entrypoint = "main"

[[entrypoints]]
id = "main"
kind = "flow"
flow = "user://flows/build_revise.toml"

[entrypoints.params]
goal_file = "GOAL.md"
state_file = "state.toml"

[[entrypoints]]
id = "scratch"
kind = "prompt"
path = "scratch.md"
```

### Pattern 2: Build -> Revise -> Build -> Revise

Example flow:

```toml
version = 1
start = "dispatch"

[[nodes]]
id = "dispatch"
kind = "decision"

[[nodes.rules]]
when = { kind = "missing", path = "{{state_file}}" }
goto = "revise"

[[nodes.rules]]
when = { kind = "path_hash_changed", path = "{{goal_file}}", key = "goal_hash" }
goto = "revise"

[[nodes.rules]]
when = { kind = "open_items", path = "{{state_file}}" }
goto = "build"

[[nodes.rules]]
goto = "paused"

[[nodes]]
id = "revise"
kind = "prompt"
prompt = "user://prompts/build_revise/revise.md"
on_completed = "record_goal_hash"
on_max_iterations = "paused"

[[nodes]]
id = "record_goal_hash"
kind = "action"
action = "set_path_hash_var"
on_success = "build"

[nodes.args]
key = "goal_hash"
path = "{{goal_file}}"

[[nodes]]
id = "build"
kind = "prompt"
prompt = "user://prompts/build_revise/build.md"
on_completed = "paused"
on_max_iterations = "paused"

[[nodes]]
id = "paused"
kind = "pause"
message = "Paused."
summary = "Paused."

[[nodes.actions]]
id = "build"
label = "Build"
goto = "build"

[[nodes.actions]]
id = "revise"
label = "Revise"
goto = "revise"
```

This pattern is enough for many iterative loops.

### Pattern 3: Rebuild From Scratch

Use `archive_paths`, then route back to your derivation step.

```toml
[[nodes]]
id = "rebuild"
kind = "action"
action = "archive_paths"
on_success = "revise"

[nodes.args]
files = ["{{state_file}}", "{{journal_file}}"]
dirs = ["specs"]
archive_root = ".history"
prefix = "rebuild"
```

### Pattern 4: Goal Interview

Use an `interactive` node and then branch based on whether the goal changed.

```toml
[[nodes]]
id = "interview"
kind = "interactive"
prompt = "user://prompts/build_revise/interview.md"
on_completed = "post_interview"
on_failed = "paused"

[[nodes]]
id = "post_interview"
kind = "decision"

[[nodes.rules]]
when = { kind = "path_hash_changed", path = "{{goal_file}}", key = "goal_hash" }
goto = "revise"

[[nodes.rules]]
goto = "paused"
```

## Running A Custom Workflow

Default entrypoint:

```bash
ralph run my-target
```

Specific entrypoint:

```bash
ralph run my-target --entrypoint main
```

Run a prompt entrypoint explicitly:

```bash
ralph run my-target --entrypoint scratch
```

Trigger a manual pause action:

```bash
ralph run my-target --action build
ralph run my-target --action revise
ralph run my-target --action rebuild
```

## Editing The Right File

Set `edit_path` on the default entrypoint if you want `ralph edit <target>` to open the most relevant file.

Typical choices:

- `GOAL.md` for goal-driven flows
- `state.toml` for state-file-driven flows
- `prompt_main.md` or `scratch.md` for prompt entrypoints

## Builtin Workflows

Ralph ships builtin flow artifacts you can reference directly:

- `builtin://flows/plan_driven.toml`
- `builtin://flows/task_driven.toml`

These are useful when you want the stock workflow shape but need to override filenames through entrypoint parameters.

## Validation Checklist

Before running a new workflow, verify:

- the target `default_entrypoint` points to an existing `entrypoints.id`
- every `goto` points to an existing node
- every `prompt` or `flow` reference resolves correctly
- every `path_hash_changed` key is written somewhere with `set_path_hash_var`
- every `var_equals` key is written with `set_var`
- every `pause` node exposes the actions you expect to call
- every `decision` has a fallback rule
- prompt files use valid Ralph directives if they use any

## Common Mistakes

### The workflow pauses immediately on first run

Usually one of these is true:

- your dispatch rules never route initial state to a derivation prompt
- you forgot a `missing_var` rule for a hash or phase variable
- your fallback rule goes to `pause`

### A `path_hash_changed` condition never fires

Usually one of these is true:

- the file path is wrong
- you never wrote the reference hash with `set_path_hash_var`
- you used a different runtime key name in the condition and in the action

### `ralph edit <target>` opens the wrong file

Set `edit_path` on the target’s default entrypoint.

### `--action` says the action is unavailable

This means the target is not currently paused on a node that exposes that action.

Run:

```bash
ralph run <target>
```

first, inspect the pause message, then run the desired action id.

## Recommended Style

- keep dispatch logic explicit and short
- use runtime variables only for durable routing state
- use file hashes for external state, not ad hoc text parsing
- keep prompt files focused on one step
- put archive and reset behavior in explicit action nodes
- end every decision with a fallback rule
- make pause action ids short and stable

## Complete Example

### `~/.config/ralph/flows/build_revise.toml`

```toml
version = 1
start = "dispatch"

[[nodes]]
id = "dispatch"
kind = "decision"

[[nodes.rules]]
when = { kind = "missing", path = "{{state_file}}" }
goto = "revise"

[[nodes.rules]]
when = { kind = "missing_var", key = "goal_hash" }
goto = "revise"

[[nodes.rules]]
when = { kind = "path_hash_changed", path = "{{goal_file}}", key = "goal_hash" }
goto = "paused"

[[nodes.rules]]
when = { kind = "open_items", path = "{{state_file}}" }
goto = "build"

[[nodes.rules]]
goto = "paused"

[[nodes]]
id = "revise"
kind = "prompt"
prompt = "user://prompts/build_revise/revise.md"
on_completed = "record_goal_hash"
on_max_iterations = "paused"

[[nodes]]
id = "record_goal_hash"
kind = "action"
action = "set_path_hash_var"
on_success = "build"

[nodes.args]
key = "goal_hash"
path = "{{goal_file}}"

[[nodes]]
id = "build"
kind = "prompt"
prompt = "user://prompts/build_revise/build.md"
on_completed = "paused"
on_max_iterations = "paused"

[[nodes]]
id = "paused"
kind = "pause"
message = "Workflow paused."
summary = "Paused."

[[nodes.actions]]
id = "build"
label = "Build"
goto = "build"

[[nodes.actions]]
id = "revise"
label = "Revise"
goto = "revise"
```

### `~/.config/ralph/prompts/build_revise/revise.md`

```md
Study `{ralph-env:TARGET_DIR}/{{goal_file}}`.
Revise `{ralph-env:TARGET_DIR}/{{state_file}}` into a concrete ordered backlog.

{"ralph":"watch","path":"{ralph-env:TARGET_DIR}/{{state_file}}"}
```

### `~/.config/ralph/prompts/build_revise/build.md`

```md
Study `{ralph-env:TARGET_DIR}/{{goal_file}}`.
Study `{ralph-env:TARGET_DIR}/{{state_file}}`.
Execute the highest-priority open item and update `{ralph-env:TARGET_DIR}/{{state_file}}`.

{"ralph":"complete_when","type":"no_line_contains_all","path":"{ralph-env:TARGET_DIR}/{{state_file}}","tokens":["completed","false"]}
```

### Target `target.toml`

```toml
id = "demo"
default_entrypoint = "main"

[[entrypoints]]
id = "main"
kind = "flow"
flow = "user://flows/build_revise.toml"
edit_path = "GOAL.md"

[entrypoints.params]
goal_file = "GOAL.md"
state_file = "state.toml"
```

With these three files in place:

```bash
ralph run demo
ralph run demo --action build
ralph run demo --action revise
```

is enough to drive the workflow.
