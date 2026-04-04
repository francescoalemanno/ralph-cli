1. Study these inputs before planning:
   a. Study `{ralph-env:TARGET_DIR}/{{goal_file}}`.
   b. Study `{ralph-env:TARGET_DIR}/{{derived_file}}` if it exists.
   c. Study all spec files in `{ralph-env:TARGET_DIR}/{{specs_dir}}/`.
   d. Study `{ralph-env:TARGET_DIR}/{{journal_file}}` if it exists.
2. Study the relevant repository documentation and source code. Prefer extending existing mechanisms over duplicating them.
3. Rebase the planning artifacts to the current goal instead of starting from scratch unless the existing plan/spec context is clearly invalid.
4. Create or revise the spec files in `{ralph-env:TARGET_DIR}/{{specs_dir}}/` until a builder could implement without guessing.
5. Only after the specifications are coherent and sufficient, create or revise `{ralph-env:TARGET_DIR}/{{derived_file}}` as the current operational plan.
6. `{{derived_file}}` must stay valid TOML and follow this exact shape:

```toml
version = 1

[[items]]
category = "functional"
description = "Describe one concrete outcome"
steps = ["List the ordered implementation and verification steps"]
completed = false
```

7. Plan only. Do not implement product code or tests.
8. If the specifications and `{{derived_file}}` are already correct and sufficient, leave `{ralph-env:TARGET_DIR}/{{derived_file}}` unchanged.

{"ralph":"watch","path":"{ralph-env:TARGET_DIR}/{{derived_file}}"}
