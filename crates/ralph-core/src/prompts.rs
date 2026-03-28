use crate::{ClarificationExchange, QuestionSupportMode};

#[derive(Debug, Clone)]
pub struct PlanningPromptContext {
    pub planning_request: String,
    pub spec_path: String,
    pub progress_path: String,
    pub existing_spec: String,
    pub existing_progress: String,
    pub clarification_history: Vec<ClarificationExchange>,
    pub controller_warnings: Vec<String>,
    pub question_support: QuestionSupportMode,
}

#[derive(Debug, Clone)]
pub struct BuildPromptContext {
    pub spec_path: String,
    pub progress_path: String,
    pub spec_contents: String,
    pub progress_contents: String,
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
        r#"You are operating inside a Ralph planning iteration.

You must:
- read existing spec and progress first
- keep the spec in the required format
- keep progress plain-text and builder-facing
- do planning only
- never edit implementation files
- end with exactly one valid planning marker when finished

Artifacts:
- spec: {spec_path}
- progress: {progress_path}

Required spec format:
# Goal
...

# User Specification
...

# Plan
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

Existing spec:
{existing_spec}

Existing progress:
{existing_progress}

Clarification rules:
{clarification_protocol}

When you finish a planning pass, end with exactly one of:
- <plan-promise>DONE</plan-promise>
- <plan-promise>CONTINUE</plan-promise>
"#,
        spec_path = context.spec_path,
        progress_path = context.progress_path,
        planning_request = context.planning_request.trim(),
        recent_feedback = recent_feedback,
        older_feedback = older_feedback,
        controller_warnings = controller_warnings,
        existing_spec = display_block(&context.existing_spec),
        existing_progress = display_block(&context.existing_progress),
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
        r#"You are operating inside a Ralph builder iteration.

You must:
- read the spec and progress first
- treat the spec as read-only
- choose one concrete highest-leverage open task only
- do that one task fully
- run relevant checks for that task
- update progress before finishing
- never modify the spec
- never claim DONE unless the full spec is complete and verified

Artifacts:
- spec: {spec_path}
- progress: {progress_path}

Spec contents:
{spec_contents}

Progress contents:
{progress_contents}

When you finish this builder pass, end with exactly one of:
- <promise>DONE</promise>
- <promise>CONTINUE</promise>
"#,
        spec_path = context.spec_path,
        progress_path = context.progress_path,
        spec_contents = display_block(&context.spec_contents),
        progress_contents = display_block(&context.progress_contents),
    )
}

fn display_block(contents: &str) -> String {
    if contents.trim().is_empty() {
        "<empty>".to_owned()
    } else {
        contents.trim().to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::{PlanningPromptContext, planning_prompt};
    use crate::{ClarificationExchange, QuestionSupportMode};

    #[test]
    fn planning_prompt_splits_recent_and_older_feedback() {
        let prompt = planning_prompt(&PlanningPromptContext {
            planning_request: "Implement feature".to_owned(),
            spec_path: "/tmp/spec.md".to_owned(),
            progress_path: "/tmp/progress.txt".to_owned(),
            existing_spec: String::new(),
            existing_progress: String::new(),
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
    }
}
