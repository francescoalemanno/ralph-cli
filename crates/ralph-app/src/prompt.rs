use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};

const RALPH_TOKEN_START: &str = "{ralph-";
const RALPH_TOKEN_PREFIX: &str = "ralph-";
const RALPH_ENV_PROJECT_DIR_TOKEN: &str = "env:PROJECT_DIR";
const RALPH_REQUEST_TOKEN: &str = "request";
const RALPH_OPTION_PREFIX_TOKEN: &str = "option:";
const RALPH_SKILL_EMIT_TOKEN: &str = "skill-emit";
const RALPH_GET_PREFIX_TOKEN: &str = "get:";
const RALPH_ROUTE_PREFIX_TOKEN: &str = "route:";
const RALPH_STOP_PREFIX_TOKEN: &str = "stop:";
const RALPH_SKILL_EMIT_NAME: &str = "Ralph event emission";

pub(crate) fn interpolate_workflow_prompt(
    prompt_text: &str,
    project_dir: &Utf8Path,
    request: Option<&str>,
    workflow_options: &BTreeMap<String, String>,
) -> Result<String> {
    interpolate_workflow_value(prompt_text, project_dir, request, workflow_options)
}

pub(crate) fn interpolate_workflow_value(
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
        _ if token.starts_with(RALPH_GET_PREFIX_TOKEN) => {
            let spec = &token[RALPH_GET_PREFIX_TOKEN.len()..];
            render_get_macro(spec)
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
    Ok(render_payload_instruction("loop-route", route))
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

    let event = format!("loop-stop:{status}");
    Ok(if body.is_empty() {
        render_signal_instruction(&event)
    } else {
        render_payload_instruction(&event, body)
    })
}

fn render_get_macro(spec: &str) -> Result<String> {
    let spec = spec.trim();
    let Some((left, event_name)) = spec.rsplit_once(':') else {
        let event_name = spec.trim();
        if event_name.is_empty() {
            return Err(anyhow!(
                "workflow token '{{ralph-get:...}}' requires an event name"
            ));
        }
        return Ok(render_get_instruction(event_name));
    };

    let event_name = event_name.trim();
    if event_name.is_empty() {
        return Err(anyhow!(
            "workflow token '{{ralph-get:...}}' requires an event name"
        ));
    }

    let channel_id = left.trim();
    if channel_id.is_empty() {
        return Ok(render_get_instruction(event_name));
    }

    Ok(render_get_in_channel_instruction(channel_id, event_name))
}

fn render_signal_instruction(event: &str) -> String {
    format!(
        "emit event `{event}` with no body by writing `{}`",
        render_signal_marker(event)
    )
}

fn render_get_instruction(event: &str) -> String {
    format!(
        "read the latest payload for event `{event}` across all channels by running `\"$RALPH_BIN\" get {event}`"
    )
}

fn render_get_in_channel_instruction(channel_id: &str, event: &str) -> String {
    format!(
        "read the latest payload for event `{event}` from channel `{channel_id}` by running `\"$RALPH_BIN\" get --channel {channel_id} {event}`"
    )
}

fn render_payload_instruction(event: &str, body: &str) -> String {
    format!(
        "emit event `{event}` with body `{body}` by writing `{}`",
        render_payload_marker(event, body)
    )
}

fn render_signal_marker(event: &str) -> String {
    format!("<<<SIGNAL:{event}>>>")
}

fn render_payload_marker(event: &str, body: &str) -> String {
    format!("<<<PAYLOAD:{event}>>>{body}<<<END-PAYLOAD>>>")
}

fn render_skill_emit_content() -> String {
    format!(
        r#"<skill name="{RALPH_SKILL_EMIT_NAME}">
definitions:
event-name = the logical name of the event or payload to emit
channel-id = the logical output channel identifier
event-body = the body of the event and payload to emit
- To emit an event without a body, write `{}`
- To emit an event with a body, write `{}`
- Event bodies may span multiple lines.
- Do not explain the event in prose; output the marker itself.
- `RALPH_BIN` points to the Ralph binary for this run.
- Use `RALPH_BIN` as an executable, not as a file to inspect, print, or `cat`.
- Ralph automatically records each emitted event on the correct channel for the current prompt or worker.
- `"$RALPH_BIN" get <event-name>` reads the latest payload for `<event-name>` across all channels in the current run.
- `"$RALPH_BIN" get --channel <channel-id> <event-name>` reads the latest payload for `<event-name>` from one specific channel.
- When workflow instructions tell you to read state with `"$RALPH_BIN" get ...`, treat that command's stdout as the canonical current-run state for the requested event.
- Do not replace `"$RALPH_BIN" get ...` reads with guesses from the filesystem, WAL files, or scratch files.
</skill>"#,
        render_signal_marker("event-name"),
        render_payload_marker("event-name", "event-body"),
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
        assert!(rendered.contains("<<<SIGNAL:event-name>>>"));
        assert!(rendered.contains("<<<PAYLOAD:event-name>>>"));
        assert!(rendered.contains("event-body"));
        assert!(rendered.contains("<<<END-PAYLOAD>>>"));
        assert!(rendered.contains("\"$RALPH_BIN\" get <event-name>"));
        assert!(rendered.contains("\"$RALPH_BIN\" get --channel <channel-id> <event-name>"));
        assert!(rendered.contains("Use `RALPH_BIN` as an executable"));
        assert!(rendered.contains("canonical current-run state"));
        assert!(rendered.contains("WAL files, or scratch files"));
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

        assert!(rendered.contains(
            "emit event `loop-route` with body `build` by writing `<<<PAYLOAD:loop-route>>>build<<<END-PAYLOAD>>>`"
        ));
        assert!(rendered.contains(
            "emit event `loop-stop:ok` with body `verification-passed` by writing `<<<PAYLOAD:loop-stop:ok>>>verification-passed<<<END-PAYLOAD>>>`"
        ));
        assert!(rendered.contains(
            "emit event `loop-stop:error` with no body by writing `<<<SIGNAL:loop-stop:error>>>`"
        ));
    }

    #[test]
    fn workflow_prompt_interpolates_get_macros() {
        let rendered = interpolate_workflow_prompt(
            "{ralph-get:handoff}\n{ralph-get:QT:review}",
            Utf8Path::new("/tmp/project"),
            None,
            &BTreeMap::new(),
        )
        .unwrap();

        assert!(rendered.contains(
            "read the latest payload for event `handoff` across all channels by running `\"$RALPH_BIN\" get handoff`"
        ));
        assert!(rendered.contains(
            "read the latest payload for event `review` from channel `QT` by running `\"$RALPH_BIN\" get --channel QT review`"
        ));
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

    #[test]
    fn workflow_prompt_rejects_missing_get_event_name() {
        let error = interpolate_workflow_prompt(
            "review={ralph-get:}",
            Utf8Path::new("/tmp/project"),
            None,
            &BTreeMap::new(),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("{ralph-get:...}"));
        assert!(error.contains("requires an event name"));
    }
}
