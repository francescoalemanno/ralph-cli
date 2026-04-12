use std::process::Stdio;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use camino::Utf8PathBuf;
use ralph_core::{
    AnsiStyle, CommandMode, PromptInput, RunControl, RunnerConfig, RunnerInvocation, RunnerResult,
    agent_events_wal_path, format_timeout_duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command as AsyncCommand,
    sync::mpsc::{self, UnboundedSender},
    task::JoinHandle,
    time::{Duration, Instant, sleep, timeout},
};
use tracing::debug;

#[derive(Debug, Clone)]
pub enum RunnerStreamEvent {
    Output(String),
    StartedWorking,
    ParsedEvents {
        child_pid: u32,
        events: Vec<ralph_core::ParsedAgentEvent>,
    },
}

#[derive(Debug, Clone)]
struct TemplateContext {
    prompt_text: String,
    project_dir: Utf8PathBuf,
    run_dir: Utf8PathBuf,
    prompt_path: Option<Utf8PathBuf>,
    prompt_name: String,
    channel_id: String,
}

impl TemplateContext {
    fn from_invocation(invocation: RunnerInvocation) -> Self {
        Self {
            prompt_text: invocation.prompt_text,
            project_dir: invocation.project_dir,
            run_dir: invocation.run_dir,
            prompt_path: Some(invocation.prompt_path),
            prompt_name: invocation.prompt_name,
            channel_id: invocation.channel_id,
        }
    }
}

#[async_trait]
pub trait RunnerAdapter: Send + Sync {
    async fn run(
        &self,
        config: &RunnerConfig,
        invocation: RunnerInvocation,
        control: &RunControl,
        stream: Option<UnboundedSender<RunnerStreamEvent>>,
    ) -> Result<RunnerResult>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct CommandRunner;

#[async_trait]
impl RunnerAdapter for CommandRunner {
    async fn run(
        &self,
        config: &RunnerConfig,
        invocation: RunnerInvocation,
        control: &RunControl,
        stream: Option<UnboundedSender<RunnerStreamEvent>>,
    ) -> Result<RunnerResult> {
        let context = TemplateContext::from_invocation(invocation);

        let mut command = build_async_command(config, &context)?;
        command.current_dir(context.project_dir.as_std_path());
        command.kill_on_drop(true);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());

        if matches!(config.prompt_input, PromptInput::Stdin) {
            command.stdin(Stdio::piped());
        }
        command.envs(rendered_envs(config, &context));

        debug!(
            program = config.command_preview(),
            prompt = context.prompt_name,
            "starting runner process"
        );

        let mut child = command.spawn().context("failed to spawn runner process")?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("runner stdout was not available"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("runner stderr was not available"))?;

        if matches!(config.prompt_input, PromptInput::Stdin) {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| anyhow!("runner stdin was not available"))?;
            stdin
                .write_all(context.prompt_text.as_bytes())
                .await
                .context("failed to write prompt to runner stdin")?;
            stdin
                .shutdown()
                .await
                .context("failed to close runner stdin after writing prompt")?;
        }

        let (chunk_tx, mut chunk_rx) = mpsc::unbounded_channel();
        let stdout_task =
            AbortOnDropHandle::new(tokio::spawn(read_stream(stdout, chunk_tx.clone())));
        let stderr_task = AbortOnDropHandle::new(tokio::spawn(read_stream(stderr, chunk_tx)));

        let mut output_buffer = String::new();
        let mut started_working = false;
        let started_at = Instant::now();
        let mut last_output_at = started_at;
        let exit_code = loop {
            if control.is_cancelled() {
                let _ = child.start_kill();
                drop(stdout_task);
                drop(stderr_task);
                let _ = timeout(Duration::from_millis(250), child.wait()).await;
                return Err(anyhow!("runner canceled"));
            }
            if let Some(session_timeout_secs) = config.session_timeout_secs {
                let session_timeout = Duration::from_secs(session_timeout_secs);
                if started_at.elapsed() >= session_timeout {
                    let _ = child.start_kill();
                    drop(stdout_task);
                    drop(stderr_task);
                    let _ = timeout(Duration::from_millis(250), child.wait()).await;
                    return Err(anyhow!(
                        "runner session timeout after {}",
                        format_timeout_duration(session_timeout_secs)
                    ));
                }
            }
            if let Some(idle_timeout_secs) = config.idle_timeout_secs {
                let idle_timeout = Duration::from_secs(idle_timeout_secs);
                if last_output_at.elapsed() >= idle_timeout {
                    let _ = child.start_kill();
                    drop(stdout_task);
                    drop(stderr_task);
                    let _ = timeout(Duration::from_millis(250), child.wait()).await;
                    return Err(anyhow!(
                        "runner idle timeout after {}",
                        format_timeout_duration(idle_timeout_secs)
                    ));
                }
            }

            if let Some(status) = child.try_wait().context("failed while polling runner")? {
                while let Some(event) = chunk_rx.recv().await {
                    let RunnerStreamEvent::Output(chunk) = event else {
                        continue;
                    };
                    if !started_working {
                        started_working = true;
                        if let Some(tx) = &stream {
                            let _ = tx.send(RunnerStreamEvent::StartedWorking);
                        }
                    }
                    forward_output_chunk(&chunk, &mut output_buffer, &stream);
                }
                break status.code().unwrap_or(-1);
            }

            tokio::select! {
                maybe = chunk_rx.recv() => {
                    if let Some(event) = maybe {
                        let RunnerStreamEvent::Output(chunk) = event else {
                            continue;
                        };
                        last_output_at = Instant::now();
                        if !started_working {
                            started_working = true;
                            if let Some(tx) = &stream {
                                let _ = tx.send(RunnerStreamEvent::StartedWorking);
                            }
                        }
                        forward_output_chunk(&chunk, &mut output_buffer, &stream);
                    }
                }
                _ = sleep(Duration::from_millis(40)) => {}
            }
        };

        await_stream_task(stdout_task, "stdout").await?;
        await_stream_task(stderr_task, "stderr").await?;

        Ok(RunnerResult {
            output: output_buffer,
            exit_code,
        })
    }
}

fn build_async_command(config: &RunnerConfig, context: &TemplateContext) -> Result<AsyncCommand> {
    let command = match config.mode {
        CommandMode::Exec => {
            let (program, args) = rendered_exec_parts(config, context)?;
            let mut command = AsyncCommand::new(program);
            for arg in args {
                command.arg(arg);
            }
            command
        }
        CommandMode::Shell => {
            let mut command = shell_async_command();
            command.arg(rendered_shell_command(config, context)?);
            command
        }
    };
    Ok(command)
}

fn rendered_exec_parts(
    config: &RunnerConfig,
    context: &TemplateContext,
) -> Result<(String, Vec<String>)> {
    let program = config
        .program
        .as_deref()
        .ok_or_else(|| anyhow!("exec command is missing program"))?;
    let args = config
        .args
        .iter()
        .map(|arg| render_template(arg, context))
        .collect();
    Ok((render_template(program, context), args))
}

fn rendered_shell_command(config: &RunnerConfig, context: &TemplateContext) -> Result<String> {
    let template = config
        .command
        .as_deref()
        .ok_or_else(|| anyhow!("shell command is missing command"))?;
    Ok(render_template(template, context))
}

fn rendered_envs(config: &RunnerConfig, context: &TemplateContext) -> Vec<(String, String)> {
    let mut envs = config
        .env
        .iter()
        .map(|(key, value)| (key.clone(), render_template(value, context)))
        .collect::<Vec<_>>();

    let ralph_bin = current_binary_path();
    if !ralph_bin.is_empty() {
        envs.push(("RALPH_BIN".to_owned(), ralph_bin));
    }
    envs.push(("RALPH_RUN_DIR".to_owned(), context.run_dir.to_string()));
    envs.push((
        "RALPH_WAL_PATH".to_owned(),
        agent_events_wal_path(&context.run_dir).to_string(),
    ));
    envs.push((
        "RALPH_PROMPT_PATH".to_owned(),
        resolved_prompt_path(context).to_owned(),
    ));
    envs.push(("RALPH_CHANNEL_ID".to_owned(), context.channel_id.clone()));
    if matches!(config.prompt_input, PromptInput::Env) {
        envs.push((config.prompt_env_var.clone(), context.prompt_text.clone()));
    }

    envs
}

async fn await_stream_task(task: AbortOnDropHandle<Result<()>>, name: &str) -> Result<()> {
    let handle = task.into_inner();
    match handle.await {
        Ok(result) => result.with_context(|| format!("runner {name} stream failed")),
        Err(error) => Err(anyhow!("runner {name} stream task failed: {error}")),
    }
}

struct AbortOnDropHandle<T>(Option<JoinHandle<T>>);

impl<T> AbortOnDropHandle<T> {
    fn new(handle: JoinHandle<T>) -> Self {
        Self(Some(handle))
    }

    fn into_inner(mut self) -> JoinHandle<T> {
        self.0.take().expect("handle already consumed")
    }
}

impl<T> Drop for AbortOnDropHandle<T> {
    fn drop(&mut self) {
        if let Some(handle) = &self.0 {
            handle.abort();
        }
    }
}

fn forward_output_chunk(
    chunk: &str,
    output_buffer: &mut String,
    stream: &Option<UnboundedSender<RunnerStreamEvent>>,
) {
    if chunk.is_empty() {
        return;
    }
    output_buffer.push_str(chunk);
    if let Some(tx) = stream {
        let _ = tx.send(RunnerStreamEvent::Output(chunk.to_owned()));
    }
}

pub fn format_event_notice(
    channel_id: Option<&str>,
    event: &ralph_core::ParsedAgentEvent,
    style: AnsiStyle,
) -> String {
    let mut message = "◆ event emitted".to_owned();
    if let Some(channel_id) = channel_id {
        message.push(' ');
        message.push('[');
        message.push_str(channel_id);
        message.push(']');
    }
    message.push_str(": ");
    message.push_str(&event.event);
    if !event.body.is_empty() {
        let preview = preview_event_body(&event.body, 3);
        if preview.inline {
            message.push_str(" | ");
            message.push_str(
                preview
                    .lines
                    .first()
                    .map(String::as_str)
                    .unwrap_or_default(),
            );
        } else {
            message.push('\n');
            message.push_str(
                &preview
                    .lines
                    .iter()
                    .map(|line| format!("  {line}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
            if preview.omitted_line_count > 0 {
                message.push('\n');
                message.push_str(&format!(
                    "  ... (+{} more line{})",
                    preview.omitted_line_count,
                    if preview.omitted_line_count == 1 {
                        ""
                    } else {
                        "s"
                    }
                ));
            }
        }
    }

    format!("{}\n", style.paint(message))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EventBodyPreview {
    lines: Vec<String>,
    omitted_line_count: usize,
    inline: bool,
}

fn preview_event_body(body: &str, max_lines: usize) -> EventBodyPreview {
    let lines = body.lines().map(str::to_owned).collect::<Vec<_>>();
    let preview_lines = lines.iter().take(max_lines).cloned().collect::<Vec<_>>();
    EventBodyPreview {
        omitted_line_count: lines.len().saturating_sub(preview_lines.len()),
        inline: lines.len() <= 1,
        lines: preview_lines,
    }
}

async fn read_stream<R>(mut reader: R, tx: UnboundedSender<RunnerStreamEvent>) -> Result<()>
where
    R: AsyncRead + Unpin,
{
    let mut buffer = [0_u8; 4096];
    let mut leftover = Vec::new();
    loop {
        let bytes_read = reader
            .read(&mut buffer)
            .await
            .context("failed while reading runner output")?;
        if bytes_read == 0 {
            if !leftover.is_empty() {
                let chunk = String::from_utf8_lossy(&leftover).into_owned();
                let _ = tx.send(RunnerStreamEvent::Output(chunk));
            }
            break;
        }
        let data = if leftover.is_empty() {
            &buffer[..bytes_read]
        } else {
            leftover.extend_from_slice(&buffer[..bytes_read]);
            leftover.as_slice()
        };
        let valid_up_to = match std::str::from_utf8(data) {
            Ok(_) => data.len(),
            Err(e) => {
                let valid = e.valid_up_to();
                if e.error_len().is_some() {
                    valid + e.error_len().unwrap()
                } else {
                    valid
                }
            }
        };
        if valid_up_to > 0 {
            let chunk = String::from_utf8_lossy(&data[..valid_up_to]).into_owned();
            if tx.send(RunnerStreamEvent::Output(chunk)).is_err() {
                break;
            }
        }
        let remainder = data[valid_up_to..].to_vec();
        leftover = remainder;
    }
    Ok(())
}

fn render_template(template: &str, context: &TemplateContext) -> String {
    let mut rendered = template.to_owned();
    let replacements = [("{prompt}", context.prompt_text.as_str())];
    for (needle, value) in replacements {
        rendered = rendered.replace(needle, value);
    }
    rendered
}

fn resolved_prompt_path(context: &TemplateContext) -> &str {
    context
        .prompt_path
        .as_deref()
        .map(|path| path.as_str())
        .unwrap_or(context.project_dir.as_str())
}

fn current_binary_path() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.into_os_string().into_string().ok())
        .unwrap_or_default()
}

fn shell_async_command() -> AsyncCommand {
    if cfg!(windows) {
        let mut command = AsyncCommand::new("cmd");
        command.arg("/C");
        command
    } else {
        let mut command = AsyncCommand::new("sh");
        command.arg("-lc");
        command
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ralph_core::{CodingAgent, CommandMode, PromptInput, RunnerInvocation};

    use super::{TemplateContext, render_template};

    #[test]
    fn prompt_templates_render_prompt_text() {
        let context = TemplateContext::from_invocation(RunnerInvocation {
            run_id: "run-1".to_owned(),
            channel_id: "main".to_owned(),
            prompt_text: "hello".to_owned(),
            project_dir: "/tmp/project".into(),
            run_dir: "/tmp/project/.ralph/runs/fixture-flow/run-1".into(),
            prompt_path: "/tmp/.config/ralph/workflows/fixture-flow.yml".into(),
            prompt_name: "task".to_owned(),
        });
        let rendered = render_template("X={prompt}", &context);
        assert_eq!(rendered, "X=hello");
    }

    #[test]
    fn removed_template_tokens_are_left_literal() {
        let context = TemplateContext::from_invocation(RunnerInvocation {
            run_id: "run-1".to_owned(),
            channel_id: "main".to_owned(),
            prompt_text: "hello".to_owned(),
            project_dir: "/tmp/project".into(),
            run_dir: "/tmp/project/.ralph/runs/fixture-flow/run-1".into(),
            prompt_path: "/tmp/.config/ralph/workflows/fixture-flow.yml".into(),
            prompt_name: "fixture-flow.yml".to_owned(),
        });

        let rendered = render_template(
            "{project_dir}|{run_dir}|{prompt_name}|{mode}|{prompt_path}|{prompt_file}",
            &context,
        );

        assert_eq!(
            rendered,
            "{project_dir}|{run_dir}|{prompt_name}|{mode}|{prompt_path}|{prompt_file}"
        );
    }

    #[test]
    fn rendered_envs_include_current_binary_path() {
        let context = TemplateContext::from_invocation(RunnerInvocation {
            run_id: "run-1".to_owned(),
            channel_id: "main".to_owned(),
            prompt_text: "hello".to_owned(),
            project_dir: "/tmp/project".into(),
            run_dir: "/tmp/project/.ralph/runs/fixture-flow/run-1".into(),
            prompt_path: "/tmp/.config/ralph/workflows/fixture-flow.yml".into(),
            prompt_name: "task".to_owned(),
        });
        let config = ralph_core::RunnerConfig {
            mode: CommandMode::Shell,
            program: None,
            args: Vec::new(),
            command: Some("echo ok".to_owned()),
            prompt_input: PromptInput::Argv,
            prompt_env_var: "PROMPT".to_owned(),
            env: BTreeMap::new(),
            session_timeout_secs: None,
            idle_timeout_secs: None,
        };

        let envs = super::rendered_envs(&config, &context);
        assert!(
            envs.iter().any(|(key, value)| {
                key == "RALPH_BIN" && std::path::Path::new(value).is_absolute()
            }),
            "RALPH_BIN must be present as an absolute path"
        );
        assert!(envs.iter().any(|(key, value)| {
            key == "RALPH_RUN_DIR" && value == "/tmp/project/.ralph/runs/fixture-flow/run-1"
        }));
        assert!(envs.iter().any(|(key, value)| {
            key == "RALPH_WAL_PATH"
                && value
                    == "/tmp/project/.ralph/runs/fixture-flow/run-1/.ralph-runtime/agent-events.wal.ndjson"
        }));
        assert!(envs.iter().any(|(key, value)| {
            key == "RALPH_PROMPT_PATH" && value == "/tmp/.config/ralph/workflows/fixture-flow.yml"
        }));
        assert!(
            envs.iter()
                .any(|(key, value)| { key == "RALPH_CHANNEL_ID" && value == "main" })
        );
        for removed in [
            "RALPH_PROJECT_DIR",
            "RALPH_RUN_ID",
            "RALPH_PROMPT_NAME",
            "RALPH_MODE",
            "RALPH_PROMPT_FILE",
        ] {
            assert!(
                envs.iter().all(|(key, _)| key != removed),
                "{removed} should not be exported to agent runs"
            );
        }
    }

    #[test]
    fn truncates_multiline_event_notice_after_three_lines() {
        let rendered = super::format_event_notice(
            Some(ralph_core::HOST_CHANNEL_ID),
            &ralph_core::ParsedAgentEvent {
                event: ralph_core::PLANNING_TARGET_PATH_EVENT.to_owned(),
                body: "docs/plans/one.md\ndocs/plans/two.md\ndocs/plans/three.md\ndocs/plans/four.md\ndocs/plans/five.md".to_owned(),
            },
            ralph_core::AnsiStyle::default(),
        );

        assert!(rendered.contains(&format!(
            "◆ event emitted [{}]: {}",
            ralph_core::HOST_CHANNEL_ID,
            ralph_core::PLANNING_TARGET_PATH_EVENT
        )));
        assert!(
            rendered
                .contains("\n  docs/plans/one.md\n  docs/plans/two.md\n  docs/plans/three.md\n")
        );
        assert!(rendered.contains("  ... (+2 more lines)"));
        assert!(!rendered.contains("\n  docs/plans/four.md\n"));
        assert!(!rendered.contains("\n  docs/plans/five.md\n"));
    }

    #[test]
    fn event_notice_uses_the_supplied_ansi_style() {
        let rendered = super::format_event_notice(
            Some("main"),
            &ralph_core::ParsedAgentEvent {
                event: "status".to_owned(),
                body: "ready".to_owned(),
            },
            ralph_core::AnsiStyle::default()
                .with_enabled(true)
                .fg(ralph_core::ThemeColor::Cyan)
                .bold(),
        );

        assert!(rendered.starts_with("\u{1b}[1;36m"));
        assert!(rendered.contains("◆ event emitted [main]: status | ready"));
        assert!(rendered.ends_with("\u{1b}[0m\n"));
    }

    #[test]
    fn opencode_permissions_live_in_agent_config_not_runner_special_cases() {
        let opencode = CodingAgent::Opencode.definition();
        assert_eq!(
            opencode
                .runner
                .env
                .get("OPENCODE_CONFIG_CONTENT")
                .map(String::as_str),
            Some(
                r#"{"$schema":"https://opencode.ai/config.json","permission":"allow","lsp":false}"#
            )
        );
    }

    #[test]
    fn shell_mode_preview_can_hold_pipeline_commands() {
        let config = ralph_core::RunnerConfig {
            mode: CommandMode::Shell,
            program: None,
            args: Vec::new(),
            command: Some("cat prompt.txt | myagent".to_owned()),
            prompt_input: PromptInput::Argv,
            prompt_env_var: "PROMPT".to_owned(),
            env: BTreeMap::new(),
            session_timeout_secs: None,
            idle_timeout_secs: None,
        };
        assert_eq!(config.command_preview(), "cat prompt.txt | myagent");
    }
}
