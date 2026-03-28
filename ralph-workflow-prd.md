# Ralph Workflow PRD

## 1. Purpose

This document is a self-sufficient product and implementation specification for building Ralph from scratch in a new project.

You should be able to copy this file into an empty folder and use it as the primary design document for implementation.

Ralph is a controller-driven workflow for durable planning and execution of repository work using external agent CLIs. The controller owns state, files, retries, validation, menus, and user interaction. Planner and builder runs are delegated to fresh external agent invocations such as:

```bash
opencode run --format default --thinking "$PROMPT"
```

Ralph is not the model. Ralph is the orchestration layer around the model.

## 2. Product Summary

Ralph helps a developer:

- create a durable spec for a task
- refine that spec through planning passes
- execute the work through bounded builder passes
- preserve state on disk between runs
- manage multiple specs in one project

All durable artifacts live under:

- `project-dir/ralph/`

Default artifacts:

- `project-dir/ralph/spec-<slug>.md`
- `project-dir/ralph/progress-<slug>.txt`

## 3. Goals

- Build a portable planning/execution workflow that works with external agent CLIs.
- Keep all durable workflow state in plain files under `project-dir/ralph/`.
- Separate planner responsibilities from builder responsibilities.
- Make completion machine-verifiable with strict markers.
- Support multiple concurrent specs in the same project directory.
- Support interactive create, continue, review, edit, revise, and run flows.
- Support clarification during planning.

## 4. Non-Goals

- Hidden long-term memory inside the agent.
- Direct implementation during planning mode.
- Spec mutation by the builder.
- Completion based on prose or heuristics alone.
- Dependence on any specific LLM provider or SDK.

## 5. Primary Users

### End User

A terminal-first developer working in a project directory who wants durable planning and controlled execution.

### Implementer

An engineer implementing Ralph itself on top of an external agent CLI.

## 6. Core Product Principles

- Durable state is file-based.
- Every planner or builder pass is a fresh process.
- The controller is authoritative.
- Agent output is validated, not trusted blindly.
- The workflow must remain portable across model backends.

## 7. Definitions

### Project Directory

The repository or working folder where Ralph is being used. This document refers to it as `project-dir/`.

### Spec

A durable markdown document containing goal, user requirements, and implementation plan.

### Progress

A mutable text file containing the current builder-facing task breakdown, remaining work, and controller notes.

### Planner

An external agent run used only to create or revise the spec and initialize or revise progress.

### Builder

An external agent run used only to execute one implementation slice and update progress.

### Controller

The Ralph application code. It owns orchestration, storage, validation, menus, prompting, and retries.

### Iteration

One fresh planner or builder agent invocation.

## 8. Artifact Layout

All artifacts live under:

- `project-dir/ralph/`

### Default Files

For a generated slug `otter-thread-sage`:

- `project-dir/ralph/spec-otter-thread-sage.md`
- `project-dir/ralph/progress-otter-thread-sage.txt`

### Custom Spec Paths

If the user targets a custom spec path, Ralph must still derive a matching progress path in the same directory.

Example:

- spec: `project-dir/ralph/my-feature.md`
- progress: `project-dir/ralph/my-feature.progress.txt`

### Optional Future Files

This version of Ralph does not require any additional metadata files, databases, or logs.

## 9. File Contracts

### 9.1 Spec Contract

The spec is planner-owned durable state.

Required structure:

```md
# Goal
<durable project goal>

# User Specification
<durable user requirements, constraints, exclusions>

# Plan
<durable implementation plan or checklist>
```

Rules:

- The spec must be non-empty.
- The planner may create or revise it.
- The builder must treat it as read-only.
- The spec should not contain execution history or temporary progress notes.

### 9.2 Progress Contract

The progress file is builder-facing mutable state.

Purpose:

- current task breakdown
- task ordering
- remaining work
- short durable notes
- controller notes

Rules:

- Planning initializes or revises it.
- Builder updates it during execution.
- It may exist without a completion marker while work is in progress.
- It is plain text, not structured JSON.

## 10. Completion Markers

Ralph uses exact machine-validated markers.

### 10.1 Planning Markers

Valid planning markers:

- `<plan-promise>DONE</plan-promise>`
- `<plan-promise>CONTINUE</plan-promise>`

### 10.2 Builder Markers

Valid builder markers:

- `<promise>DONE</promise>`
- `<promise>CONTINUE</promise>`

### 10.3 Validation Rules

For both planning and builder:
- the last emitted marker is taken as source of truth

### 10.4 Persisted Completion

Only accepted builder `DONE` marks a spec completed.

When builder returns accepted `DONE`, the controller must persist exactly one final line:

```text
<promise>DONE</promise>
```

inside the progress file.

That persisted marker is the source of truth for completion status in the dashboard.

Before every new builder iteration, the controller must strip any existing persisted promise markers from progress so stale markers do not affect the next pass.

## 11. State Model

Ralph exposes three effective states:

- `empty`
- `planned`
- `completed`

### 11.1 Empty

The spec file is missing or empty.

### 11.2 Planned

The spec exists and is non-empty.

This state applies whether progress exists, has no marker, or ends in `CONTINUE`.

### 11.3 Completed

The progress file's final non-empty line is:

```text
<promise>DONE</promise>
```

Important:

- planning `DONE` means builder-ready
- builder `DONE` means workflow-complete

## 12. Architecture

Ralph has four main components:

### 12.1 Controller

Owns:

- command entrypoints
- dashboard and menus
- spec allocation
- spec discovery
- prompt generation
- runner execution
- state inspection
- validation
- retries
- review rendering
- clarification UX
- editor launch

### 12.2 Artifact Store

Owns:

- reading and writing files under `project-dir/ralph/`
- deriving matching progress paths
- listing available specs
- determining state from file contents

### 12.3 Runner Adapter

Owns:

- invoking the external agent CLI
- passing prompt text
- setting working directory
- collecting stdout, stderr, and exit code

### 12.4 Interaction Layer

Owns:

- interactive menu selection
- free-form prompt capture
- clarification question answering
- review rendering

## 13. Runner Adapter Contract

Ralph must be implementable against external agent CLIs. The runner adapter is therefore a first-class interface.

### 13.1 Required Input

The adapter must accept:

- prompt text
- project directory
- mode: `plan` or `build`
- spec path
- progress path

### 13.2 Required Output

The adapter must return:

- stdout
- stderr
- exit code

### 13.3 Fresh Run Requirement

Each planner or builder iteration must be a new process invocation. No shared in-memory agent session is required.

### 13.4 Supported Prompt Transport

The implementation must support at least one safe way to provide long prompts:

- shell variable expansion such as `$PROMPT`
- stdin piping
- temp file placeholder such as `{prompt_file}`

The implementation should not rely solely on command-line argument length being sufficient.

### 13.5 Example OpenCode Adapter

The system must support an adapter equivalent to:

```bash
opencode run --format default --thinking "$PROMPT"
```

Safer equivalent forms are allowed, for example temp-file based prompt loading.

### 13.6 Recommended Command Template Variables

Recommended variables/placeholders for runner templates:

- prompt text
- prompt file path
- project directory
- mode
- spec path
- progress path

## 14. Planner Contract

Planner mode exists only to create or revise the durable spec and builder-facing progress.

### 14.1 Planner Responsibilities

The planner must:

- read existing spec and progress if present
- read `AGENTS.md` if present
- read `README.md` if present
- read `specs/` if present
- inspect relevant implementation files for context
- create or revise the spec
- create or revise the progress file
- ask clarifying questions when uncertainty materially affects scope or sequencing
- end with one valid planning marker

### 14.2 Planner Restrictions

The planner must not:

- edit implementation files
- run builds
- run tests
- run migrations
- run verification commands

### 14.3 Planner Iteration Policy

At the start of each planning iteration, the planner should choose one highest-leverage planning task, such as:

- clarifying scope
- refining acceptance criteria
- inspecting feasibility in code
- tightening sequencing
- de-risking the next builder slice

### 14.4 Planner Loop Limit

Default maximum planning iterations: `8`

## 15. Builder Contract

Builder mode exists only to execute one implementation slice and update progress.

### 15.1 Builder Responsibilities

The builder must:

- read the spec and progress first
- treat the spec as read-only durable input
- create the progress file if missing before finishing
- choose one concrete highest-leverage open task from progress
- do only that one task in the current iteration
- prefer foundational work that unlocks later tasks
- run relevant checks for that task
- update progress to reflect what changed and what remains
- end with one valid builder marker

### 15.2 Builder Restrictions

The builder must not:

- modify the spec
- claim `DONE` unless the full spec is complete and verified

### 15.3 Builder Loop Limit

Default maximum builder iterations: `25`

## 16. Clarification Protocol

Planning must support user clarification.

### 16.1 Preferred Mode: Native Tool Support

If the external agent supports tools, expose a `question` tool that accepts:

- `question`
- `options`

Rules:

- one question required
- one to three suggested options allowed
- free-form answer must always be possible

### 16.2 Portable Mode: Text Protocol

If the external agent only returns stdout/stderr, support this fallback block:

```xml
<ralph-question>
{"question":"Which target matters most?","options":[{"label":"CLI","description":"Interactive CLI users first."},{"label":"Library","description":"Embedded library users first."}]}
</ralph-question>
```

Controller behavior:

1. Detect the block in stdout.
2. Parse the JSON payload.
3. Ask the user through Ralph UI.
4. Accept either one suggested option or a free-form answer.
5. Rerun planning with that answer included in the next prompt.

### 16.3 Clarification Cancellation

If the user cancels clarification after the planner has already written partial spec or progress files, those files must remain on disk.

## 17. Prompt Requirements

Ralph must generate the prompts itself. Prompt text is controller logic, not agent-owned logic.

### 17.1 Planning Prompt Requirements

The planning prompt must instruct the planner to:

- operate inside a Ralph planning iteration
- read spec, progress, project context, and docs first
- keep the spec in the required format
- keep progress builder-facing
- do planning only
- ask clarification when required
- end with exactly one valid planning marker

### 17.2 Builder Prompt Requirements

The builder prompt must instruct the builder to:

- operate inside a Ralph builder iteration
- read spec and progress first
- use spec as read-only input
- pick one task only
- run checks for that task
- update progress
- end with exactly one valid builder marker

## 18. User Flows

### 18.1 Root Command

With no argument, Ralph opens the root dashboard.

If interactive UI is unavailable, the implementation may return an interactive-required error.

### 18.2 Root Dashboard Actions

If specs exist:

- Continue spec
- Create new spec
- Edit spec
- Review spec
- Revise spec
- Run spec

If no specs exist:

- Create new spec

### 18.3 Direct Targeting

If the user passes a spec slug or path, Ralph opens a scoped flow for that target.

If the input looks like a path but does not yet exist, Ralph treats it as a valid target for revise/create behavior.

If the input is just a freeform planning request rather than a spec identifier, this version of Ralph rejects it. Planning requests must be collected from the dedicated create or revise flows.

### 18.4 Create New Spec

Steps:

1. Collect planning request.
2. Allocate a new unique spec/progress pair.
3. Run planning loop.
4. Return to scoped menu if interactive UI remains available.

### 18.5 Continue Spec

Steps:

1. List specs.
2. Show active/completed status.
3. Show previews.
4. Select a spec.
5. Open scoped menu.

### 18.6 Scoped Menu

For the selected spec:

- Run spec
- Edit spec
- Review spec
- Revise spec
- Replan from scratch
- Close

### 18.7 Review

Render both current spec and progress to terminal output without invoking an agent.

### 18.8 Edit

Open the spec in `$EDITOR` or a fallback editor.

### 18.9 Revise

Collect a new planning request and rerun planning for the same spec path without deleting existing files first.

### 18.10 Replan From Scratch

Delete the current spec and progress pair, collect a new planning request, then rerun planning for the same target path.

### 18.11 Run

Run builder loop against the selected spec.

## 19. Controller Algorithms

### 19.1 Planning Loop Algorithm

1. Resolve `project-dir/`.
2. Ensure `project-dir/ralph/` exists.
3. Resolve or allocate the spec pair.
4. Read initial spec and progress contents.
5. Generate planning prompt.
6. Invoke planner runner adapter.
7. If exit code is non-zero:
   append a controller note to progress, summarize failure, and retry unless max iterations reached.
8. If stdout contains a valid clarification block:
   ask the user and rerun planning.
9. Validate that spec exists and is non-empty.
10. Validate that progress exists and is non-empty.
11. Validate that at least one artifact changed during the iteration.
12. Validate final planning marker.
13. If marker is `CONTINUE`, loop.
14. If marker is `DONE`, exit successfully.

### 19.2 Builder Loop Algorithm

1. Resolve `project-dir/`.
2. Resolve the target spec pair.
3. Validate that spec exists and is non-empty.
4. Strip persisted promise markers from progress.
5. Generate builder prompt.
6. Invoke builder runner adapter.
7. If exit code is non-zero:
   append a controller note to progress, summarize failure, and retry unless max iterations reached.
8. Validate final builder marker.
9. If marker is `CONTINUE`, loop.
10. If marker is `DONE`, append exactly one persisted done marker to progress and exit successfully.

## 20. Spec Discovery and Sorting

### 20.1 Listing Specs

Ralph lists every file under `project-dir/ralph/` matching:

- `spec-*.md`

and optionally custom spec targets the controller explicitly knows about.

### 20.2 Completion Sorting

The dashboard should sort:

1. active specs first
2. completed specs after active specs

Within those groups, sort by slug or path deterministically.

### 20.3 Labels and Preview

Each listed spec should display:

- active/completed indicator
- slug or file name
- relative spec path

Preview should show:

- spec path
- progress path
- current state
- spec contents
- progress contents

## 21. Slug Generation

Default spec names should use a human-readable hyphenated slug. Any deterministic or random unique strategy is acceptable as long as collisions are handled safely.

Example:

- `otter-thread-sage`

If a generated slug collides with an existing spec, Ralph must generate another one.

## 22. Error Handling

Ralph must handle:

- missing spec files
- empty spec files
- invalid markers
- multiple markers
- malformed marker-like lines
- runner failures
- non-zero exit codes
- user cancellation during clarification
- missing TTY for interactive flows
- missing editor for manual edit flow

## 23. Controller Notes

When a planner or builder iteration fails validation or execution, the controller may append a short note to progress.

Controller note format:

```text
Controller note:
<brief diagnostic summary>
```

Rules:

- notes must be concise
- notes must remain in progress unless overwritten by later user or builder edits
- notes are advisory, not authoritative state

## 24. Functional Requirements

### FR1

Ralph must store all durable workflow artifacts under `project-dir/ralph/`.

### FR2

Ralph must be implementable using external agent CLIs rather than a fixed in-process SDK.

### FR3

Planning and builder loops must be distinct and independently configurable.

### FR4

The controller must validate exact completion markers and reject ambiguous output.

### FR5

The builder must never mutate the spec.

### FR6

Planning must support clarification via either native tools or portable text protocol.

### FR7

Completion state must be derived from persisted builder `DONE`, not prose.

### FR8

The system must support multiple specs within the same project.

### FR9

The review flow must render current artifacts without invoking an agent.

### FR10

Replan-from-scratch must delete the current spec/progress pair and rebuild it for the same target.

### FR11

Planner and builder runs must be fresh process invocations.

### FR12

The system must preserve already-written partial artifacts when clarification is canceled or execution is interrupted.

## 25. User Stories

### Story 1

As an implementer, I want to invoke planner and builder through an external CLI such as OpenCode so that Ralph is backend-agnostic.

Acceptance criteria:

- I can configure planner and builder command templates.
- A template equivalent to `opencode run --format default --thinking "$PROMPT"` is supported.
- The controller captures stdout, stderr, and exit code.

### Story 2

As an end user, I want to create a durable spec so that planning survives across sessions.

Acceptance criteria:

- Ralph allocates a spec and progress file under `project-dir/ralph/`.
- The spec uses the required markdown structure.
- Progress is initialized during planning.

### Story 3

As an end user, I want to continue a prior spec so that I can resume long-running work later.

Acceptance criteria:

- Ralph lists existing specs.
- I can choose one and enter a scoped menu.

### Story 4

As an end user, I want to review current spec and progress before running the builder.

Acceptance criteria:

- Review renders both files.
- Review does not invoke the agent.

### Story 5

As an end user, I want to revise a spec in place so that I can change scope without losing the plan identity.

Acceptance criteria:

- Revise keeps the same target path.
- Planning updates the current artifacts rather than always resetting them.

### Story 6

As an end user, I want replan-from-scratch so that I can recover from a bad plan.

Acceptance criteria:

- Ralph deletes the current spec/progress pair.
- Ralph asks for a new planning request.
- Ralph rebuilds the pair for the same target path.

### Story 7

As an end user, I want builder mode to execute one slice at a time so that progress remains reviewable.

Acceptance criteria:

- Builder chooses one task from progress.
- Builder updates progress after the slice.
- Builder runs checks for that slice.

### Story 8

As an end user, I want clarification during planning so that specs are not built on unresolved ambiguity.

Acceptance criteria:

- Ralph supports suggested answers and free-form answers.
- Clarification works with either tool-based or text-protocol backends.

### Story 9

As an end user, I want completed specs clearly marked so that I can focus on unfinished work.

Acceptance criteria:

- Persisted builder `DONE` marks completion.
- Completed specs sort after active specs.

### Story 10

As an implementer, I want exact marker validation so that the controller does not falsely mark work complete.

Acceptance criteria:

- Inline or malformed markers are rejected.
- Final-line validation is enforced.
- Multiple markers are rejected.

## 26. Suggested Initial Implementation Plan

Implement in this order:

1. Artifact path utilities and state inspection.
2. Spec listing, slug generation, and progress-path derivation.
3. Marker parsing and validation.
4. Runner adapter interface and one OpenCode adapter.
5. Planning and builder prompt generators.
6. Planning loop controller.
7. Builder loop controller.
8. Review rendering.
9. Edit flow.
10. Interactive dashboard and scoped menus.
11. Clarification support.

## 27. Minimum Test Matrix

At minimum, implement tests for:

- deriving progress path from spec path
- listing spec pairs
- empty/planned/completed state detection
- marker parsing and invalid marker rejection
- persisted done marker append behavior
- stripping stale markers before builder iteration
- planning loop retry on invalid marker
- builder loop retry on invalid marker
- preserve partial artifacts on clarification cancel
- direct slug/path resolution
- replan-from-scratch reset behavior
- completed specs sorting after active specs

## 28. Success Criteria

Ralph is successful when:

- an engineer can implement it from this document alone
- planner and builder can run through external CLIs
- durable state lives entirely under `project-dir/ralph/`
- completion state is deterministic and machine-validated
- multi-spec workflows are reviewable and resumable
