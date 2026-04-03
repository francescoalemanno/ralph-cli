use anyhow::{Context, Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;

const RALPH_ENV_PROJECT_DIR: &str = "{ralph-env:PROJECT_DIR}";
const RALPH_ENV_TARGET_DIR: &str = "{ralph-env:TARGET_DIR}";
const RALPH_ENV_PROMPT_PATH: &str = "{ralph-env:PROMPT_PATH}";
const RALPH_ENV_PROMPT_NAME: &str = "{ralph-env:PROMPT_NAME}";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedPrompt {
    pub(crate) prompt_text: String,
    pub(crate) completion_criteria: Vec<CompletionCriterion>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CompletionCriterion {
    Watch { path: String },
    NoLineContainsAll { path: String, tokens: Vec<String> },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "ralph", rename_all = "snake_case")]
enum PromptDirective {
    Watch {
        path: String,
    },
    CompleteWhen {
        #[serde(rename = "type")]
        kind: CompletionDirectiveType,
        path: String,
        tokens: Vec<String>,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CompletionDirectiveType {
    NoLineContainsAll,
}

pub(crate) fn parse_prompt_directives(prompt_text: &str) -> ParsedPrompt {
    let mut completion_criteria = Vec::new();
    let mut cleaned_lines = Vec::new();

    for line in prompt_text.lines() {
        let trimmed = line.trim();
        let directive = serde_json::from_str::<PromptDirective>(trimmed);
        match directive {
            Ok(PromptDirective::Watch { path }) if !path.trim().is_empty() => {
                completion_criteria.push(CompletionCriterion::Watch { path });
            }
            Ok(PromptDirective::CompleteWhen { kind, path, tokens })
                if !path.trim().is_empty() && !tokens.is_empty() =>
            {
                match kind {
                    CompletionDirectiveType::NoLineContainsAll => {
                        completion_criteria
                            .push(CompletionCriterion::NoLineContainsAll { path, tokens });
                    }
                }
            }
            _ => {
                if !line.trim().is_empty() {
                    cleaned_lines.push(line.to_owned());
                }
            }
        }
    }

    ParsedPrompt {
        prompt_text: cleaned_lines.join("\n"),
        completion_criteria,
    }
}

pub(crate) fn interpolate_prompt_env(
    prompt_text: &str,
    project_dir: &Utf8Path,
    target_dir: &Utf8Path,
    prompt_path: &Utf8Path,
    prompt_name: &str,
) -> Result<String> {
    let replacements = [
        (RALPH_ENV_PROJECT_DIR, absolute_unix_path(project_dir)?),
        (RALPH_ENV_TARGET_DIR, absolute_unix_path(target_dir)?),
        (RALPH_ENV_PROMPT_PATH, absolute_unix_path(prompt_path)?),
        (RALPH_ENV_PROMPT_NAME, prompt_name.to_owned()),
    ];

    let mut interpolated = prompt_text.to_owned();
    for (needle, value) in replacements {
        interpolated = interpolated.replace(needle, &value);
    }
    Ok(interpolated)
}

fn absolute_unix_path(path: &Utf8Path) -> Result<String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        let cwd =
            Utf8PathBuf::from_path_buf(std::env::current_dir().context("failed to read cwd")?)
                .map_err(|_| anyhow!("current directory is not valid UTF-8"))?;
        cwd.join(path)
    };
    Ok(absolute.as_str().replace('\\', "/"))
}
