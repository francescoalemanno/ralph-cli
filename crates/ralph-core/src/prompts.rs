use crate::QuestionSupportMode;

#[derive(Debug, Clone)]
pub struct PlanningPromptContext {
    pub planning_request: String,
    pub spec_path: String,
    pub progress_path: String,
    pub feedback_path: String,
    pub controller_warnings: Vec<String>,
    pub question_support: QuestionSupportMode,
}

#[derive(Debug, Clone)]
pub struct BuildPromptContext {
    pub spec_path: String,
    pub progress_path: String,
    pub feedback_path: String,
}

#[derive(Debug, Clone)]
pub struct ProgressRevisionPromptContext {
    pub previous_spec_path: String,
    pub current_spec_path: String,
    pub progress_path: String,
    pub diff_path: String,
}

pub fn planning_prompt(context: &PlanningPromptContext) -> String {
    let clarification_protocol = match context.question_support {
        QuestionSupportMode::Disabled => {
            "Clarification is disabled. Make the best conservative planning decision and continue."
                .to_owned()
        }
        QuestionSupportMode::TextProtocol => r#"If you need clarification, emit exactly one block:
<ralph-question>
{"question":"...","options":[{"label":"...","description":"..."}]}
</ralph-question>
Then stop. Do not emit a planning marker in the same response."#
            .to_owned(),
    };

    let controller_warnings = if context.controller_warnings.is_empty() {
        "None.".to_owned()
    } else {
        context.controller_warnings.join("\n")
    };

    format!(
        r#"You are the planner for one Ralph planning iteration.

Do this:
- read the current spec, progress, and feedback from disk first
- use the planning request and all clarification feedback to update the spec
- make the spec reflect the latest authoritative user intent
- keep the spec in the required format
- update the progress file surgically into the concrete builder task list a careful engineer should execute next
- produce planning artifacts only
- finish with exactly one valid planning marker

Artifacts:
- spec: {spec_path}
- progress: {progress_path}
- feedback: {feedback_path}

Feedback file contract:
- the newest authoritative Q&A lives between <RECENT-USER-FEEDBACK> and </RECENT-USER-FEEDBACK>
- older authoritative Q&A history lives between <OLDER-USER-FEEDBACK> and </OLDER-USER-FEEDBACK>
- treat the full feedback file as authoritative user guidance unless superseded by newer entries

Required spec format:
# Goal
...

# User Requirements And Constraints
...

# Non-Goals
...

# Proposed Design
...

# Implementation Plan
...

# Acceptance Criteria
...

# Risks
...

# Open Questions
...

Planning request:
{planning_request}

Controller warnings:
{controller_warnings}

Clarification rules:
{clarification_protocol}

End with exactly one of:
- <plan-promise>DONE</plan-promise>
- <plan-promise>CONTINUE</plan-promise>
"#,
        spec_path = context.spec_path,
        progress_path = context.progress_path,
        feedback_path = context.feedback_path,
        planning_request = context.planning_request.trim(),
        controller_warnings = controller_warnings,
        clarification_protocol = clarification_protocol,
    )
}

pub fn build_prompt(context: &BuildPromptContext) -> String {
    format!(
        r#"You are the builder for one Ralph builder iteration.

Do this:
- read the spec, progress, and feedback from disk first
- treat the spec as read-only
- treat the feedback file as authoritative user intent and constraints
- choose the one highest-leverage open task from progress
- complete that task fully
- run the relevant checks for that task
- update progress before finishing

Artifacts:
- spec: {spec_path}
- progress: {progress_path}
- feedback: {feedback_path}

End with exactly one of:
- <promise>DONE</promise>   only when the full spec is complete and verified
- <promise>CONTINUE</promise>  otherwise
"#,
        spec_path = context.spec_path,
        progress_path = context.progress_path,
        feedback_path = context.feedback_path,
    )
}

pub fn progress_revision_prompt(context: &ProgressRevisionPromptContext) -> String {
    format!(
        r#"You are the Ralph progress revisor for one focused revision pass.

Do this:
- read the previous spec, current spec, progress, and diff from disk first
- treat both spec files as read-only
- use the spec diff to understand what changed
- update only the progress file so it matches the edited spec
- preserve completed or still-valid work where possible
- remove obsolete tasks, reorder tasks if needed, and add new concrete tasks
- keep progress builder-facing and execution-oriented
- finish with exactly one valid planning marker

Artifacts:
- previous spec: {previous_spec_path}
- current spec: {current_spec_path}
- progress: {progress_path}
- spec diff: {diff_path}

Rules:
- do not rewrite either spec file
- do not ask clarification questions
- do not perform implementation work
- if progress already matches the edited spec, leave it as-is and emit DONE

End with exactly one of:
- <plan-promise>DONE</plan-promise>
- <plan-promise>CONTINUE</plan-promise>
"#,
        previous_spec_path = context.previous_spec_path,
        current_spec_path = context.current_spec_path,
        progress_path = context.progress_path,
        diff_path = context.diff_path,
    )
}

#[cfg(test)]
mod tests {
    use super::{
        BuildPromptContext, PlanningPromptContext, ProgressRevisionPromptContext, build_prompt,
        planning_prompt, progress_revision_prompt,
    };
    use crate::QuestionSupportMode;

    #[test]
    fn planning_prompt_references_feedback_file_without_inlining_history() {
        let prompt = planning_prompt(&PlanningPromptContext {
            planning_request: "Implement feature".to_owned(),
            spec_path: "/tmp/spec.md".to_owned(),
            progress_path: "/tmp/progress.txt".to_owned(),
            feedback_path: "/tmp/feedback.txt".to_owned(),
            controller_warnings: vec![],
            question_support: QuestionSupportMode::TextProtocol,
        });

        assert!(prompt.contains("read the current spec, progress, and feedback from disk first"));
        assert!(prompt.contains("- feedback: /tmp/feedback.txt"));
        assert!(prompt.contains("<RECENT-USER-FEEDBACK>"));
        assert!(prompt.contains("<OLDER-USER-FEEDBACK>"));
        assert!(prompt.contains("# User Requirements And Constraints"));
        assert!(prompt.contains("# Non-Goals"));
        assert!(prompt.contains("# Proposed Design"));
        assert!(prompt.contains("# Implementation Plan"));
        assert!(prompt.contains("# Acceptance Criteria"));
        assert!(prompt.contains("# Risks"));
        assert!(prompt.contains("# Open Questions"));
        assert!(prompt.contains(
            "use the planning request and all clarification feedback to update the spec"
        ));
        assert!(prompt.contains(
            "update the progress file surgically into the concrete builder task list a careful engineer should execute next"
        ));
        assert!(!prompt.contains("Existing spec:"));
        assert!(!prompt.contains("Existing progress:"));
        assert!(!prompt.contains("Q: Which database?"));
    }

    #[test]
    fn build_prompt_references_paths_without_inlining_file_contents() {
        let prompt = build_prompt(&BuildPromptContext {
            spec_path: "/tmp/spec.md".to_owned(),
            progress_path: "/tmp/progress.txt".to_owned(),
            feedback_path: "/tmp/feedback.txt".to_owned(),
        });

        assert!(prompt.contains("read the spec, progress, and feedback from disk first"));
        assert!(prompt.contains("- spec: /tmp/spec.md"));
        assert!(prompt.contains("- progress: /tmp/progress.txt"));
        assert!(prompt.contains("- feedback: /tmp/feedback.txt"));
        assert!(!prompt.contains("Spec contents:"));
        assert!(!prompt.contains("Progress contents:"));
    }

    #[test]
    fn progress_revision_prompt_references_snapshot_and_diff_paths() {
        let prompt = progress_revision_prompt(&ProgressRevisionPromptContext {
            previous_spec_path: "/tmp/spec.past-spec.md".to_owned(),
            current_spec_path: "/tmp/spec.md".to_owned(),
            progress_path: "/tmp/progress.txt".to_owned(),
            diff_path: "/tmp/spec.spec-edit.diff.txt".to_owned(),
        });

        assert!(
            prompt.contains(
                "read the previous spec, current spec, progress, and diff from disk first"
            )
        );
        assert!(prompt.contains("- previous spec: /tmp/spec.past-spec.md"));
        assert!(prompt.contains("- current spec: /tmp/spec.md"));
        assert!(prompt.contains("- progress: /tmp/progress.txt"));
        assert!(prompt.contains("- spec diff: /tmp/spec.spec-edit.diff.txt"));
        assert!(prompt.contains("do not rewrite either spec file"));
        assert!(prompt.contains("update only the progress file so it matches the edited spec"));
    }
}
