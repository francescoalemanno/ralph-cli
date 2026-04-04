# Fake Workflow Agent

Ralph can host a custom fake agent that drives workflow graphs quickly without calling a real model.

This is useful when you want to:

- smoke-test a workflow graph
- validate pause and rerun behavior
- exercise plan/rebase/build loops quickly
- test interactive goal interview transitions deterministically

## Config Snippet

Add this custom agent to `~/.config/ralph/config.toml`:

```toml
[[agents]]
id = "fake"
name = "Fake Workflow Agent"
builtin = false

[agents.non_interactive]
mode = "exec"
program = "{ralph_bin}"
args = ["fake-agent", "run"]
prompt_input = "file"
prompt_env_var = "PROMPT"

[agents.non_interactive.env]

[agents.interactive]
mode = "exec"
program = "{ralph_bin}"
args = ["fake-agent", "interactive"]
prompt_input = "file"
prompt_env_var = "PROMPT"

[agents.interactive.env]
```

Then select it with:

```bash
ralph agent set fake --scope project
```

or:

```bash
ralph agent set fake --scope user
```

## Behavior

The fake agent is deterministic and intentionally fast.

For built-in workflow graphs it behaves like this:

- `plan_driven_plan`: creates `specs/`, writes `plan.toml`, appends to `journal.txt`
- `plan_driven_build`: marks the next incomplete item in `plan.toml` as complete
- `task_driven_rebase`: writes `progress.toml`, appends to `journal.txt`
- `task_driven_build`: marks the next incomplete item in `progress.toml` as complete
- interactive goal interview: appends a refinement note to `GOAL.md`

It also writes `smoke_ran.txt` during build steps so a workflow can observe side effects quickly.

## Notes

- The custom agent command uses `{ralph_bin}`, which Ralph resolves to the current Ralph binary path before spawning the runner.
- The fake agent is meant for workflow validation, not for product work.
- If a prompt does not look like a built-in plan/progress step, the fake agent falls back to writing `fake-agent.log` in the target directory.
