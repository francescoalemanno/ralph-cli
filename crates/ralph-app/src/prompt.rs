use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};

const RALPH_TOKEN_START: &str = "{ralph-";
const RALPH_TOKEN_PREFIX: &str = "ralph-";
const RALPH_ENV_PROJECT_DIR_TOKEN: &str = "env:PROJECT_DIR";
const RALPH_REQUEST_TOKEN: &str = "request";
const RALPH_OPTION_PREFIX_TOKEN: &str = "option:";
const RALPH_SKILL_EMIT_TOKEN: &str = "skill-emit";
const RALPH_ROUTE_PREFIX_TOKEN: &str = "route:";
const RALPH_STOP_PREFIX_TOKEN: &str = "stop:";
const RALPH_EMIT_COMMAND_PREFIX: &str = "$RALPH_BIN emit";
const RALPH_SKILL_EMIT_NAME: &str = "Ralph event emission";

pub(crate) fn interpolate_workflow_prompt(
    prompt_text: &str,
    project_dir: &Utf8Path,
    request: Option<&str>,
    workflow_options: &BTreeMap<String, String>,
) -> Result<String> {
    let project_dir = absolute_unix_path(project_dir)?;
    let context = PromptInterpolationContext {
        project_dir: &project_dir,
        request,
        workflow_options,
    };

    interpolate_ralph_tokens(prompt_text, &context)
}

struct PromptInterpolationContext<'a> {
    project_dir: &'a str,
    request: Option<&'a str>,
    workflow_options: &'a BTreeMap<String, String>,
}

fn interpolate_ralph_tokens(
    prompt_text: &str,
    context: &PromptInterpolationContext<'_>,
) -> Result<String> {
    let mut rendered = String::with_capacity(prompt_text.len());
    let mut remaining = prompt_text;

    while let Some(start) = remaining.find(RALPH_TOKEN_START) {
        rendered.push_str(&remaining[..start]);
        let suffix = &remaining[start + 1..];
        let Some(end) = suffix.find('}') else {
            return Err(anyhow!(
                "unterminated workflow token starting with '{{ralph-'"
            ));
        };
        rendered.push_str(&render_ralph_token(&suffix[..end], context)?);
        remaining = &suffix[end + 1..];
    }

    rendered.push_str(remaining);
    Ok(rendered)
}

fn render_ralph_token(token: &str, context: &PromptInterpolationContext<'_>) -> Result<String> {
    let Some(token) = token.strip_prefix(RALPH_TOKEN_PREFIX) else {
        return Err(anyhow!(
            "internal error: expected Ralph token, got '{{{token}}}'"
        ));
    };

    match token {
        RALPH_ENV_PROJECT_DIR_TOKEN => Ok(context.project_dir.to_owned()),
        RALPH_REQUEST_TOKEN => context
            .request
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("workflow token '{{ralph-request}}' requires a request")),
        RALPH_SKILL_EMIT_TOKEN => Ok(render_skill_emit_content()),
        _ if token.starts_with(RALPH_OPTION_PREFIX_TOKEN) => {
            let option_id = &token[RALPH_OPTION_PREFIX_TOKEN.len()..];
            context
                .workflow_options
                .get(option_id)
                .cloned()
                .ok_or_else(|| {
                    anyhow!(
                        "workflow token '{{ralph-option:{option_id}}}' requires workflow option '{option_id}'"
                    )
                })
        }
        _ if token.starts_with(RALPH_ROUTE_PREFIX_TOKEN) => {
            let route = &token[RALPH_ROUTE_PREFIX_TOKEN.len()..];
            render_route_macro(route)
        }
        _ if token.starts_with(RALPH_STOP_PREFIX_TOKEN) => {
            let spec = &token[RALPH_STOP_PREFIX_TOKEN.len()..];
            render_stop_macro(spec)
        }
        _ => Err(anyhow!("unsupported workflow token '{{ralph-{token}}}'")),
    }
}

fn render_route_macro(route: &str) -> Result<String> {
    let route = route.trim();
    if route.is_empty() {
        return Err(anyhow!(
            "workflow token '{{ralph-route:...}}' requires a route"
        ));
    }
    Ok(render_emit_command("loop-route", Some(route)))
}

fn render_stop_macro(spec: &str) -> Result<String> {
    let spec = spec.trim();
    let (status, body) = spec
        .split_once(':')
        .map_or((spec, ""), |(status, body)| (status.trim(), body.trim()));

    if !matches!(status, "ok" | "error") {
        return Err(anyhow!(
            "workflow token '{{ralph-stop:...}}' requires status 'ok' or 'error', got '{status}'"
        ));
    }

    Ok(render_emit_command(
        &format!("loop-stop:{status}"),
        Some(body),
    ))
}

fn render_emit_command(event: &str, body: Option<&str>) -> String {
    match body.map(str::trim).filter(|body| !body.is_empty()) {
        Some(body) => format!("`{RALPH_EMIT_COMMAND_PREFIX} {event} {body}`"),
        None => format!("`{RALPH_EMIT_COMMAND_PREFIX} {event}`"),
    }
}

fn render_skill_emit_content() -> String {
    format!(
        r#"<skill name="{RALPH_SKILL_EMIT_NAME}">
- RALPH_BIN is an environment variable which point to the ralph binary path.
- running the command {} will emit the event.
</skill>"#,
        render_emit_command("<event-name>", Some("<event-body>"))
    )
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::interpolate_workflow_prompt;
    use camino::Utf8Path;

    #[test]
    fn workflow_prompt_interpolates_project_dir_request_and_options() {
        let rendered = interpolate_workflow_prompt(
            "{ralph-skill-emit}\nproject={ralph-env:PROJECT_DIR}\nrequest={ralph-request}\nprogress={ralph-option:progress-file}",
            Utf8Path::new("/tmp/project"),
            Some("ship it"),
            &BTreeMap::from([("progress-file".to_owned(), "progress.txt".to_owned())]),
        )
        .unwrap();

        assert!(rendered.contains("<skill name=\"Ralph event emission\">"));
        assert!(rendered.contains("$RALPH_BIN emit <event-name> <event-body>"));
        assert!(rendered.contains("project=/tmp/project"));
        assert!(rendered.contains("request=ship it"));
        assert!(rendered.contains("progress=progress.txt"));
    }

    #[test]
    fn workflow_prompt_interpolates_route_and_stop_macros() {
        let rendered = interpolate_workflow_prompt(
            "{ralph-route:build}\n{ralph-stop:ok:verification-passed}\n{ralph-stop:error}",
            Utf8Path::new("/tmp/project"),
            None,
            &BTreeMap::new(),
        )
        .unwrap();

        assert!(rendered.contains("`$RALPH_BIN emit loop-route build`"));
        assert!(rendered.contains("`$RALPH_BIN emit loop-stop:ok verification-passed`"));
        assert!(rendered.contains("`$RALPH_BIN emit loop-stop:error`"));
    }

    #[test]
    fn workflow_prompt_leaves_non_ralph_tokens_untouched() {
        let rendered = interpolate_workflow_prompt(
            "name={project_name}\nrequest={ralph-request}",
            Utf8Path::new("/tmp/project"),
            Some("{ralph-route:build}"),
            &BTreeMap::new(),
        )
        .unwrap();

        assert!(rendered.contains("name={project_name}"));
        assert!(rendered.contains("request={ralph-route:build}"));
    }

    #[test]
    fn workflow_prompt_rejects_unknown_ralph_tokens() {
        let error = interpolate_workflow_prompt(
            "unknown={ralph-unknown}",
            Utf8Path::new("/tmp/project"),
            None,
            &BTreeMap::new(),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("unsupported workflow token"));
        assert!(error.contains("{ralph-unknown}"));
    }

    #[test]
    fn workflow_prompt_rejects_missing_request_values() {
        let error = interpolate_workflow_prompt(
            "request={ralph-request}",
            Utf8Path::new("/tmp/project"),
            None,
            &BTreeMap::new(),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("{ralph-request}"));
        assert!(error.contains("requires a request"));
    }

    #[test]
    fn workflow_prompt_rejects_missing_option_values() {
        let error = interpolate_workflow_prompt(
            "progress={ralph-option:progress-file}",
            Utf8Path::new("/tmp/project"),
            None,
            &BTreeMap::new(),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("{ralph-option:progress-file}"));
        assert!(error.contains("requires workflow option 'progress-file'"));
    }
}
