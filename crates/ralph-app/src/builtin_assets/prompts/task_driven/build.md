1. Study these inputs before building:
   a. Study `{ralph-env:TARGET_DIR}/{{goal_file}}`.
   b. Study `{ralph-env:TARGET_DIR}/{{derived_file}}`.
   c. Study `{ralph-env:TARGET_DIR}/{{journal_file}}` if it exists.
   d. Study `AGENTS.md` if it exists.
2. Study the relevant repository documentation and source code.
3. Select the single highest-priority open item with the highest leverage from `{ralph-env:TARGET_DIR}/{{derived_file}}`.
4. Execute only that item completely. Do not leave placeholders or partial implementations behind.
5. Run the checks relevant to the code you changed.
6. Update `{ralph-env:TARGET_DIR}/{{derived_file}}` so it accurately records completed work and any remaining follow-up.
7. Create or update `{ralph-env:TARGET_DIR}/{{journal_file}}` as a free-form builder journal for future iterations.
8. Do not edit `{ralph-env:TARGET_DIR}/{{goal_file}}`.

{"ralph":"complete_when","type":"no_line_contains_all","path":"{ralph-env:TARGET_DIR}/{{derived_file}}","tokens":["completed","false"]}
