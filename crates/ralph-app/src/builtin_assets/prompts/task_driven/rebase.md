1. Study these inputs before rebasing the backlog:
   a. Study `{ralph-env:TARGET_DIR}/{{goal_file}}` as the authoritative intent.
   b. Study `{ralph-env:TARGET_DIR}/{{derived_file}}` if it exists.
   c. Study `{ralph-env:TARGET_DIR}/{{journal_file}}` if it exists.
2. Study the relevant repository documentation and source code.
3. Rebase `{ralph-env:TARGET_DIR}/{{derived_file}}` to match the current goal.
4. Preserve completed items that are still coherent with the goal. Rewrite or remove stale ones. Add missing work.
5. Keep the backlog small, concrete, and execution-oriented.
6. `{{derived_file}}` must stay valid TOML and follow this exact shape:

```toml
version = 1

[[items]]
description = "..."
steps = ["..."]
completed = false
```

7. Backlog rebase only. Do not implement product code or tests in this run.

{"ralph":"watch","path":"{ralph-env:TARGET_DIR}/{{derived_file}}"}
