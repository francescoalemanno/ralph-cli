# Requests (not sorted by priority)
- A
- B
- C

# Execution policy
1. Read {ralph-env:TARGET_DIR}/progress.txt.
2. Execute the single most high leverage item in "Requests".
3. If an item was executed, update progress in {ralph-env:TARGET_DIR}/progress.txt with the notions about the executed item; else if no item was left to execute, do not change progress.
4. Stop

{"ralph":"watch","path":"{ralph-env:TARGET_DIR}/progress.txt"}
