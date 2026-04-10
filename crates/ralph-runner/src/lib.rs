use std::process::Stdio;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use camino::Utf8PathBuf;
use ralph_core::{
    AgentOutputProcessor, CommandMode, PromptInput, RunControl, RunnerConfig, RunnerInvocation,
    RunnerResult, agent_events_wal_path,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command as AsyncCommand,
    sync::mpsc::{self, UnboundedSender},
    task::JoinHandle,
    time::{Duration, sleep, timeout},
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

#[derive(Debug, Default)]
struct EventNoticeState {
    pending: Vec<String>,
    last_visible_ended_with_newline: bool,
}

#[derive(Debug, Clone)]
struct TemplateContext {
    prompt_text: String,
    project_dir: Utf8PathBuf,
    run_dir: Utf8PathBuf,
    prompt_path: Option<Utf8PathBuf>,
    prompt_name: String,
}

impl TemplateContext {
    fn from_invocation(invocation: RunnerInvocation) -> Self {
        Self {
            prompt_text: invocation.prompt_text,
            project_dir: invocation.project_dir,
            run_dir: invocation.run_dir,
            prompt_path: Some(invocation.prompt_path),
            prompt_name: invocation.prompt_name,
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
        let child_pid = child.id().unwrap_or_default();
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
        let stdout_task = tokio::spawn(read_stream(stdout, chunk_tx.clone()));
        let stderr_task = tokio::spawn(read_stream(stderr, chunk_tx));

        let mut output_buffer = String::new();
        let mut processor = AgentOutputProcessor::default();
        let mut notice_state = EventNoticeState {
            pending: Vec::new(),
            last_visible_ended_with_newline: true,
        };
        let mut started_working = false;
        let exit_code = loop {
            if control.is_cancelled() {
                let _ = child.start_kill();
                stdout_task.abort();
                stderr_task.abort();
                let _ = timeout(Duration::from_millis(250), child.wait()).await;
                return Err(anyhow!("runner canceled"));
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
                    handle_runner_output_chunk(
                        &mut processor,
                        &mut notice_state,
                        child_pid,
                        &chunk,
                        &mut output_buffer,
                        &stream,
                    )?;
                }
                break status.code().unwrap_or(-1);
            }

            tokio::select! {
                maybe = chunk_rx.recv() => {
                    if let Some(event) = maybe {
                        let RunnerStreamEvent::Output(chunk) = event else {
                            continue;
                        };
                        if !started_working {
                            started_working = true;
                            if let Some(tx) = &stream {
                                let _ = tx.send(RunnerStreamEvent::StartedWorking);
                            }
                        }
                        if let Err(error) = handle_runner_output_chunk(
                            &mut processor,
                            &mut notice_state,
                            child_pid,
                            &chunk,
                            &mut output_buffer,
                            &stream,
                        ) {
                            let _ = child.start_kill();
                            stdout_task.abort();
                            stderr_task.abort();
                            let _ = timeout(Duration::from_millis(250), child.wait()).await;
                            return Err(error);
                        }
                    }
                }
                _ = sleep(Duration::from_millis(40)) => {}
            }
        };

        flush_runner_output_processor(
            &mut processor,
            &mut notice_state,
            child_pid,
            &mut output_buffer,
            &stream,
        )?;

        await_stream_task(stdout_task, "stdout").await?;
        await_stream_task(stderr_task, "stderr").await?;

        Ok(RunnerResult {
            output: output_buffer,
            exit_code,
        })
    }
}

fn build_async_command(
    config: &RunnerConfig,
    context: &TemplateContext,
) -> Result<AsyncCommand> {
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
    if matches!(config.prompt_input, PromptInput::Env) {
        envs.push((config.prompt_env_var.clone(), context.prompt_text.clone()));
    }

    envs
}

async fn await_stream_task(task: JoinHandle<Result<()>>, name: &str) -> Result<()> {
    match task.await {
        Ok(result) => result.with_context(|| format!("runner {name} stream failed")),
        Err(error) => Err(anyhow!("runner {name} stream task failed: {error}")),
    }
}

fn handle_runner_output_chunk(
    processor: &mut AgentOutputProcessor,
    notice_state: &mut EventNoticeState,
    child_pid: u32,
    chunk: &str,
    output_buffer: &mut String,
    stream: &Option<UnboundedSender<RunnerStreamEvent>>,
) -> Result<()> {
    let parsed = processor.push_str(chunk);
    forward_parsed_events(stream, child_pid, &parsed.events);
    enqueue_event_notices(notice_state, &parsed.events);
    forward_visible_output(
        notice_state,
        parsed.visible_text,
        false,
        output_buffer,
        stream,
    );
    Ok(())
}

fn flush_runner_output_processor(
    processor: &mut AgentOutputProcessor,
    notice_state: &mut EventNoticeState,
    child_pid: u32,
    output_buffer: &mut String,
    stream: &Option<UnboundedSender<RunnerStreamEvent>>,
) -> Result<()> {
    let parsed = processor.finish();
    forward_parsed_events(stream, child_pid, &parsed.events);
    enqueue_event_notices(notice_state, &parsed.events);
    forward_visible_output(
        notice_state,
        parsed.visible_text,
        true,
        output_buffer,
        stream,
    );
    Ok(())
}

fn forward_parsed_events(
    stream: &Option<UnboundedSender<RunnerStreamEvent>>,
    child_pid: u32,
    events: &[ralph_core::ParsedAgentEvent],
) {
    if events.is_empty() {
        return;
    }
    if let Some(tx) = stream {
        let _ = tx.send(RunnerStreamEvent::ParsedEvents {
            child_pid,
            events: events.to_vec(),
        });
    }
}
fn enqueue_event_notices(
    notice_state: &mut EventNoticeState,
    events: &[ralph_core::ParsedAgentEvent],
) {
    notice_state
        .pending
        .extend(events.iter().map(render_event_notice));
}

fn forward_visible_output(
    notice_state: &mut EventNoticeState,
    visible_text: String,
    flush_pending: bool,
    output_buffer: &mut String,
    stream: &Option<UnboundedSender<RunnerStreamEvent>>,
) {
    let decorated = decorate_visible_output(notice_state, visible_text, flush_pending);
    if decorated.is_empty() {
        return;
    }
    notice_state.last_visible_ended_with_newline = decorated.ends_with('\n');
    output_buffer.push_str(&decorated);
    if let Some(tx) = stream {
        let _ = tx.send(RunnerStreamEvent::Output(decorated));
    }
}

fn decorate_visible_output(
    notice_state: &mut EventNoticeState,
    visible_text: String,
    flush_pending: bool,
) -> String {
    let mut rendered = visible_text;
    if notice_state.pending.is_empty() {
        return rendered;
    }

    if notice_state.last_visible_ended_with_newline {
        let notices = drain_pending_notices(notice_state);
        rendered.insert_str(0, &notices);
        return rendered;
    }

    if let Some(newline_index) = rendered.find('\n') {
        let notices = drain_pending_notices(notice_state);
        rendered.insert_str(newline_index + 1, &notices);
        return rendered;
    }

    if flush_pending {
        let needs_separator_newline = (!rendered.is_empty() && !rendered.ends_with('\n'))
            || (rendered.is_empty() && !notice_state.last_visible_ended_with_newline);
        if needs_separator_newline {
            rendered.push('\n');
        }
        rendered.push_str(&drain_pending_notices(notice_state));
    }

    rendered
}

fn drain_pending_notices(notice_state: &mut EventNoticeState) -> String {
    let mut drained = String::new();
    for notice in notice_state.pending.drain(..) {
        drained.push_str(&notice);
    }
    drained
}

pub fn format_event_notice(
    channel_id: Option<&str>,
    event: &ralph_core::ParsedAgentEvent,
) -> String {
    const ANSI_BOLD_RED: &str = "\x1b[1;31m";
    const ANSI_RESET: &str = "\x1b[0m";

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
        if event.body.contains('\n') {
            message.push('\n');
            message.push_str(
                &event
                    .body
                    .lines()
                    .map(|line| format!("  {line}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        } else {
            message.push_str(" | ");
            message.push_str(&event.body);
        }
    }

    format!("{ANSI_BOLD_RED}{message}{ANSI_RESET}\n")
}

fn render_event_notice(event: &ralph_core::ParsedAgentEvent) -> String {
    format_event_notice(None, event)
}

async fn read_stream<R>(mut reader: R, tx: UnboundedSender<RunnerStreamEvent>) -> Result<()>
where
    R: AsyncRead + Unpin,
{
    let mut buffer = [0_u8; 4096];
    loop {
        let bytes_read = reader
            .read(&mut buffer)
            .await
            .context("failed while reading runner output")?;
        if bytes_read == 0 {
            break;
        }
        let chunk = String::from_utf8_lossy(&buffer[..bytes_read]).into_owned();
        if tx.send(RunnerStreamEvent::Output(chunk)).is_err() {
            break;
        }
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
        for removed in [
            "RALPH_PROJECT_DIR",
            "RALPH_RUN_ID",
            "RALPH_PROMPT_NAME",
            "RALPH_CHANNEL_ID",
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
    fn decorates_event_notice_immediately_when_already_at_line_start() {
        let mut state = super::EventNoticeState {
            pending: vec![super::render_event_notice(&ralph_core::ParsedAgentEvent {
                event: "loop-route".to_owned(),
                body: "beta".to_owned(),
            })],
            last_visible_ended_with_newline: true,
        };

        let rendered = super::decorate_visible_output(&mut state, "hello\n".to_owned(), false);

        assert!(rendered.starts_with("\x1b[1;31m◆ event emitted: loop-route | beta\x1b[0m\n"));
        assert!(rendered.ends_with("hello\n"));
        assert!(state.pending.is_empty());
    }

    #[test]
    fn decorates_event_notice_after_first_newline_when_mid_line() {
        let mut state = super::EventNoticeState {
            pending: vec![super::render_event_notice(&ralph_core::ParsedAgentEvent {
                event: "loop-stop:ok".to_owned(),
                body: "done".to_owned(),
            })],
            last_visible_ended_with_newline: false,
        };

        let rendered =
            super::decorate_visible_output(&mut state, "before\nafter".to_owned(), false);

        assert!(
            rendered.starts_with("before\n\x1b[1;31m◆ event emitted: loop-stop:ok | done\x1b[0m\n")
        );
        assert!(rendered.ends_with("after"));
        assert!(state.pending.is_empty());
    }

    #[test]
    fn flushes_pending_event_notice_with_newline_at_end_of_stream() {
        let mut state = super::EventNoticeState {
            pending: vec![super::render_event_notice(&ralph_core::ParsedAgentEvent {
                event: "handoff".to_owned(),
                body: "alpha\nbeta".to_owned(),
            })],
            last_visible_ended_with_newline: false,
        };

        let rendered = super::decorate_visible_output(&mut state, "tail".to_owned(), true);

        assert_eq!(
            rendered,
            "tail\n\x1b[1;31m◆ event emitted: handoff\n  alpha\n  beta\x1b[0m\n"
        );
        assert!(state.pending.is_empty());
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
        };
        assert_eq!(config.command_preview(), "cat prompt.txt | myagent");
    }
}
