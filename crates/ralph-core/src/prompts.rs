use crate::{ClarificationExchange, QuestionSupportMode};

#[derive(Debug, Clone)]
pub struct PlanningPromptContext {
    pub planning_request: String,
    pub spec_path: String,
    pub progress_path: String,
    pub clarification_history: Vec<ClarificationExchange>,
    pub controller_warnings: Vec<String>,
    pub question_support: QuestionSupportMode,
}

#[derive(Debug, Clone)]
pub struct BuildPromptContext {
    pub spec_path: String,
    pub progress_path: String,
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
        QuestionSupportMode::NativeTool => {
            "If clarification is required and your runtime exposes the Ralph question tool, use it exactly once and then stop.".to_owned()
        }
    };

    let recent_feedback = context
        .clarification_history
        .last()
        .map(|exchange| format_exchange(1, exchange))
        .unwrap_or_else(|| "None.".to_owned());

    let older_feedback = if context.clarification_history.len() <= 1 {
        "None.".to_owned()
    } else {
        context
            .clarification_history
            .iter()
            .take(context.clarification_history.len() - 1)
            .enumerate()
            .map(|(index, exchange)| format_exchange(index + 1, exchange))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let controller_warnings = if context.controller_warnings.is_empty() {
        "None.".to_owned()
    } else {
        context.controller_warnings.join("\n")
    };

    format!(
        r#"You are the planner for one Ralph planning iteration.

Do this:
- read the current spec and progress from disk first
- use the planning request and all clarification feedback to update the spec
- make the spec reflect the latest authoritative user intent
- keep the spec in the required format
- update the progress file surgically into the concrete builder task list a careful engineer should execute next
- produce planning artifacts only
- finish with exactly one valid planning marker

Artifacts:
- spec: {spec_path}
- progress: {progress_path}

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

Recent feedback to shape the plan:
This is the most recent authoritative user guidance collected in the previous planning iteration.
{recent_feedback}

Older feedbacks from past iterations:
These older clarifications remain authoritative unless superseded by newer feedback above.
{older_feedback}

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
        planning_request = context.planning_request.trim(),
        recent_feedback = recent_feedback,
        older_feedback = older_feedback,
        controller_warnings = controller_warnings,
        clarification_protocol = clarification_protocol,
    )
}

fn format_exchange(index: usize, exchange: &ClarificationExchange) -> String {
    format!(
        "{}. Q: {}\n   A: {}",
        index,
        exchange.question.trim(),
        exchange.answer.trim()
    )
}

pub fn build_prompt(context: &BuildPromptContext) -> String {
    format!(
        r#"You are the builder for one Ralph builder iteration.

Do this:
- read the spec and progress from disk first
- treat the spec as read-only
- choose the one highest-leverage open task from progress
- complete that task fully
- run the relevant checks for that task
- update progress before finishing

Artifacts:
- spec: {spec_path}
- progress: {progress_path}

End with exactly one of:
- <promise>DONE</promise>   only when the full spec is complete and verified
- <promise>CONTINUE</promise>  otherwise
"#,
        spec_path = context.spec_path,
        progress_path = context.progress_path,
    )
}

#[cfg(test)]
mod tests {
    use super::{BuildPromptContext, PlanningPromptContext, build_prompt, planning_prompt};
    use crate::{ClarificationExchange, QuestionSupportMode};

    #[test]
    fn planning_prompt_splits_recent_and_older_feedback() {
        let prompt = planning_prompt(&PlanningPromptContext {
            planning_request: "Implement feature".to_owned(),
            spec_path: "/tmp/spec.md".to_owned(),
            progress_path: "/tmp/progress.txt".to_owned(),
            clarification_history: vec![
                ClarificationExchange {
                    question: "Which runtime?".to_owned(),
                    answer: "Tokio".to_owned(),
                },
                ClarificationExchange {
                    question: "Which database?".to_owned(),
                    answer: "Postgres".to_owned(),
                },
            ],
            controller_warnings: vec![],
            question_support: QuestionSupportMode::TextProtocol,
        });

        assert!(prompt.contains("Recent feedback to shape the plan:"));
        assert!(prompt.contains("1. Q: Which database?\n   A: Postgres"));
        assert!(prompt.contains("Older feedbacks from past iterations:"));
        assert!(prompt.contains("1. Q: Which runtime?\n   A: Tokio"));
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
        assert!(prompt.contains("read the current spec and progress from disk first"));
        assert!(!prompt.contains("Existing spec:"));
        assert!(!prompt.contains("Existing progress:"));
    }

    #[test]
    fn build_prompt_references_paths_without_inlining_file_contents() {
        let prompt = build_prompt(&BuildPromptContext {
            spec_path: "/tmp/spec.md".to_owned(),
            progress_path: "/tmp/progress.txt".to_owned(),
        });

        assert!(prompt.contains("read the spec and progress from disk first"));
        assert!(prompt.contains("- spec: /tmp/spec.md"));
        assert!(prompt.contains("- progress: /tmp/progress.txt"));
        assert!(!prompt.contains("Spec contents:"));
        assert!(!prompt.contains("Progress contents:"));
    }
}
