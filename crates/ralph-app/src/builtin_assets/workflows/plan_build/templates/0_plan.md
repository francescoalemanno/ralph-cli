0a. Study `specs/*`.
0b. Study `IMPLEMENTATION_PLAN.md` if present in the repository root.
0c. Study the codebase areas that appear to hold shared utilities, core modules, or reusable components.
0d. Study the existing source code before deciding something is missing.

1. Identify missing, incomplete, inconsistent, or unverified work by comparing `specs/*`, `IMPLEMENTATION_PLAN.md`, and the existing source code. Prefer shared, consolidated solutions in the codebase over ad hoc duplication.
2. If specifications are missing or ambiguous, update `specs/*` conservatively until a builder could implement without guessing. Capture, when relevant:
   a. user-visible outcomes and acceptance checks
   b. explicit scope boundaries and non-goals
   c. interfaces, data flow, storage, and integration points touched
   d. migrations, rollout or backward-compatibility needs, and operational constraints
   e. verification strategy, failure modes, and observability or debugging notes
   f. risks, open questions, and assumptions that must be resolved before coding
3. If unresolved uncertainty would materially change implementation order, architecture, or correctness, keep refining `specs/*` instead of pushing guesses into `IMPLEMENTATION_PLAN.md`.
4. Update `IMPLEMENTATION_PLAN.md` in the repository root as a prioritized bullet list of remaining work.
5. Each bullet must describe one concrete, observable outcome, not a vague activity or component area.
6. Order the bullets so earlier work unlocks later work and front-loads risk reduction, shared interfaces, migrations, and compatibility.
7. Keep each bullet small enough that one build loop can finish the top item completely, including verification, while leaving the repository in a coherent state.
8. Fold low-value chores into the bullet they validate; do not create standalone busywork bullets unless they materially unblock later work.
9. Plan only. Do not implement anything.
10. If `IMPLEMENTATION_PLAN.md` is already up to date and sufficient for the next build loop, leave it unchanged.

ULTIMATE GOAL - We want to achieve:
[project-specific goal].

Consider missing elements and plan accordingly. If an element is missing, search first to confirm it does not already exist, then, if needed, author the specification at `specs/FILENAME.md`.

{"ralph":"watch","path":"IMPLEMENTATION_PLAN.md"}
