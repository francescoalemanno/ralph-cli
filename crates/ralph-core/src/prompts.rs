use crate::QuestionSupportMode;

#[derive(Debug, Clone)]
pub struct PlanningPromptContext {
    pub planning_request: String,
    pub spec_path: String,
    pub progress_path: String,
    pub existing_spec: String,
    pub existing_progress: String,
    pub prior_answers: Vec<String>,
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

    let prior_answers = if context.prior_answers.is_empty() {
        "None.".to_owned()
    } else {
        context
            .prior_answers
            .iter()
            .enumerate()
            .map(|(index, answer)| format!("{}. {}", index + 1, answer))
            .collect::<Vec<_>>()
            .join("\n")
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

Prior clarification answers:
{prior_answers}

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
        prior_answers = prior_answers,
        existing_spec = display_block(&context.existing_spec),
        existing_progress = display_block(&context.existing_progress),
        clarification_protocol = clarification_protocol,
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
