use std::{
    path::Path,
    process::{Child as StdChild, Command as StdCommand, Stdio},
};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use camino::Utf8PathBuf;
use ralph_core::{
    AgentOutputProcessor, CommandMode, PromptInput, RunControl, RunnerConfig, RunnerInvocation,
    RunnerResult, agent_events_wal_path,
};
use tempfile::NamedTempFile;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractiveSessionInvocation {
    pub session_name: String,
    pub initial_prompt: String,
    pub project_dir: Utf8PathBuf,
    pub run_dir: Utf8PathBuf,
    pub run_id: Option<String>,
    pub prompt_path: Option<Utf8PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InteractiveSessionOutcome {
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone)]
struct TemplateContext {
    run_id: String,
    channel_id: String,
    prompt_text: String,
    project_dir: Utf8PathBuf,
    run_dir: Utf8PathBuf,
    prompt_path: Option<Utf8PathBuf>,
    prompt_name: String,
}

impl TemplateContext {
    fn from_invocation(invocation: RunnerInvocation) -> Self {
        Self {
            run_id: invocation.run_id,
            channel_id: invocation.channel_id,
            prompt_text: invocation.prompt_text,
            project_dir: invocation.project_dir,
            run_dir: invocation.run_dir,
            prompt_path: Some(invocation.prompt_path),
            prompt_name: invocation.prompt_name,
        }
    }

    fn from_interactive(invocation: &InteractiveSessionInvocation) -> Self {
        Self {
            run_id: invocation.run_id.clone().unwrap_or_default(),
            channel_id: ralph_core::MAIN_CHANNEL_ID.to_owned(),
            prompt_text: invocation.initial_prompt.clone(),
            project_dir: invocation.project_dir.clone(),
            run_dir: invocation.run_dir.clone(),
            prompt_path: invocation.prompt_path.clone(),
            prompt_name: invocation.session_name.clone(),
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

    fn run_interactive_session(
        &self,
        _config: &RunnerConfig,
        _invocation: &InteractiveSessionInvocation,
    ) -> Result<InteractiveSessionOutcome> {
        Err(anyhow!(
            "interactive sessions are not supported by this runner"
        ))
    }
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
        let prompt_file = staged_prompt_file(&context.prompt_text)?;
        let prompt_file_path = prompt_file.path().to_string_lossy().to_string();

        let mut command = build_async_command(config, &context, &prompt_file_path)?;
        command.current_dir(context.project_dir.as_std_path());
        command.kill_on_drop(true);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());

        if matches!(config.prompt_input, PromptInput::Stdin) {
            command.stdin(Stdio::piped());
        }
        command.envs(rendered_envs(config, &context, &prompt_file_path));

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

    fn run_interactive_session(
        &self,
        config: &RunnerConfig,
        invocation: &InteractiveSessionInvocation,
    ) -> Result<InteractiveSessionOutcome> {
        CommandRunner::run_interactive_session(self, config, invocation)
    }
}

impl CommandRunner {
    pub fn run_interactive_session(
        &self,
        config: &RunnerConfig,
        invocation: &InteractiveSessionInvocation,
    ) -> Result<InteractiveSessionOutcome> {
        if matches!(config.mode, CommandMode::Exec)
            && matches!(config.prompt_input, PromptInput::Stdin)
        {
            return Err(anyhow!(
                "interactive exec commands do not support prompt_input=stdin; use argv, env, file, or shell mode"
            ));
        }

        let context = TemplateContext::from_interactive(invocation);
        let prompt_file = staged_prompt_file(&context.prompt_text)?;
        let prompt_file_path = prompt_file.path().to_string_lossy().to_string();

        let mut command = build_std_command(config, &context, &prompt_file_path)?;
        configure_std_process_group(&mut command);
        command.current_dir(context.project_dir.as_std_path());
        command.stdin(Stdio::inherit());
        command.stdout(Stdio::inherit());
        command.stderr(Stdio::inherit());
        command.envs(rendered_envs(config, &context, &prompt_file_path));

        let mut child: StdChild = command
            .spawn()
            .context("failed to spawn interactive runner process")?;
        let mut cleanup_guard = ChildCleanupGuard::new(Some(child.id()));
        #[cfg(unix)]
        let _terminal_guard = TerminalForegroundGuard::handoff(Some(child.id()))?;
        let status = wait_for_child(&mut child)?;
        cleanup_guard.disarm();

        Ok(InteractiveSessionOutcome {
            exit_code: status.code(),
        })
    }
}

fn staged_prompt_file(prompt: &str) -> Result<NamedTempFile> {
    let file = NamedTempFile::new().context("failed to create prompt temp file")?;
    std::fs::write(file.path(), prompt).context("failed to write prompt temp file")?;
    Ok(file)
}

fn build_async_command(
    config: &RunnerConfig,
    context: &TemplateContext,
    prompt_file: &str,
) -> Result<AsyncCommand> {
    let command = match config.mode {
        CommandMode::Exec => {
            let (program, args) = rendered_exec_parts(config, context, prompt_file)?;
            let mut command = AsyncCommand::new(program);
            for arg in args {
                command.arg(arg);
            }
            command
        }
        CommandMode::Shell => {
            let mut command = shell_async_command();
            command.arg(rendered_shell_command(config, context, prompt_file)?);
            command
        }
    };
    Ok(command)
}

fn build_std_command(
    config: &RunnerConfig,
    context: &TemplateContext,
    prompt_file: &str,
) -> Result<StdCommand> {
    let command = match config.mode {
        CommandMode::Exec => {
            let (program, args) = rendered_exec_parts(config, context, prompt_file)?;
            let mut command = StdCommand::new(program);
            for arg in args {
                command.arg(arg);
            }
            command
        }
        CommandMode::Shell => {
            let mut command = shell_std_command();
            command.arg(rendered_shell_command(config, context, prompt_file)?);
            command
        }
    };
    Ok(command)
}

fn rendered_exec_parts(
    config: &RunnerConfig,
    context: &TemplateContext,
    prompt_file: &str,
) -> Result<(String, Vec<String>)> {
    let program = config
        .program
        .as_deref()
        .ok_or_else(|| anyhow!("exec command is missing program"))?;
    let args = config
        .args
        .iter()
        .map(|arg| render_template(arg, context, prompt_file))
        .collect();
    Ok((render_template(program, context, prompt_file), args))
}

fn rendered_shell_command(
    config: &RunnerConfig,
    context: &TemplateContext,
    prompt_file: &str,
) -> Result<String> {
    let template = config
        .command
        .as_deref()
        .ok_or_else(|| anyhow!("shell command is missing command"))?;
    Ok(render_template(template, context, prompt_file))
}

fn rendered_envs(
    config: &RunnerConfig,
    context: &TemplateContext,
    prompt_file: &str,
) -> Vec<(String, String)> {
    let mut envs = config
        .env
        .iter()
        .map(|(key, value)| (key.clone(), render_template(value, context, prompt_file)))
        .collect::<Vec<_>>();

    envs.push((
        "RALPH_PROJECT_DIR".to_owned(),
        context.project_dir.to_string(),
    ));
    if !context.run_id.is_empty() {
        envs.push(("RALPH_RUN_ID".to_owned(), context.run_id.clone()));
    }
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
        resolved_prompt_path(context, prompt_file).to_owned(),
    ));
    envs.push(("RALPH_PROMPT_NAME".to_owned(), context.prompt_name.clone()));
    envs.push(("RALPH_CHANNEL_ID".to_owned(), context.channel_id.clone()));
    envs.push((
        "RALPH_MODE".to_owned(),
        invocation_mode(&context.prompt_name),
    ));
    envs.push(("RALPH_PROMPT_FILE".to_owned(), prompt_file.to_owned()));
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

#[cfg(unix)]
fn configure_std_process_group(command: &mut StdCommand) {
    use std::os::unix::process::CommandExt;

    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
}

#[cfg(not(unix))]
fn configure_std_process_group(_command: &mut StdCommand) {}

#[cfg(unix)]
fn signal_process_group(pid: u32, signal: i32) -> Result<()> {
    let result = unsafe { libc::kill(-(pid as i32), signal) };
    if result == 0 {
        return Ok(());
    }

    let error = std::io::Error::last_os_error();
    if matches!(error.raw_os_error(), Some(libc::ESRCH)) {
        Ok(())
    } else {
        Err(anyhow!("failed to signal runner process group: {error}"))
    }
}

fn wait_for_child(child: &mut StdChild) -> Result<std::process::ExitStatus> {
    loop {
        match child.wait() {
            Ok(status) => return Ok(status),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error).context("failed while waiting for child process"),
        }
    }
}

#[cfg(unix)]
struct TerminalForegroundGuard {
    previous_pgid: libc::pid_t,
    _sigttou_guard: SignalIgnoreGuard,
}

#[cfg(unix)]
impl TerminalForegroundGuard {
    fn handoff(child_pid: Option<u32>) -> Result<Option<Self>> {
        let Some(child_pid) = child_pid else {
            return Ok(None);
        };
        if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
            return Ok(None);
        }

        let sigttou_guard = SignalIgnoreGuard::ignore(libc::SIGTTOU);
        let previous_pgid = unsafe { libc::tcgetpgrp(libc::STDIN_FILENO) };
        if previous_pgid < 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed to read controlling terminal process group");
        }
        if unsafe { libc::tcsetpgrp(libc::STDIN_FILENO, child_pid as libc::pid_t) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed to hand terminal to interactive runner");
        }

        Ok(Some(Self {
            previous_pgid,
            _sigttou_guard: sigttou_guard,
        }))
    }
}

#[cfg(unix)]
impl Drop for TerminalForegroundGuard {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetpgrp(libc::STDIN_FILENO, self.previous_pgid);
        }
    }
}

#[cfg(unix)]
struct SignalIgnoreGuard {
    signal: i32,
    previous: libc::sighandler_t,
}

#[cfg(unix)]
impl SignalIgnoreGuard {
    fn ignore(signal: i32) -> Self {
        let previous = unsafe { libc::signal(signal, libc::SIG_IGN) };
        Self { signal, previous }
    }
}

#[cfg(unix)]
impl Drop for SignalIgnoreGuard {
    fn drop(&mut self) {
        unsafe {
            libc::signal(self.signal, self.previous);
        }
    }
}

struct ChildCleanupGuard {
    pid: Option<u32>,
    armed: bool,
}

impl ChildCleanupGuard {
    fn new(pid: Option<u32>) -> Self {
        Self { pid, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ChildCleanupGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            let _ = signal_process_group(pid, libc::SIGKILL);
        }
    }
}

fn render_template(template: &str, context: &TemplateContext, prompt_file: &str) -> String {
    let mut rendered = template.to_owned();
    let mode = invocation_mode(&context.prompt_name);
    let prompt_path = resolved_prompt_path(context, prompt_file);
    let replacements = [
        ("{project_dir}", context.project_dir.as_str()),
        ("{run_dir}", context.run_dir.as_str()),
        ("{prompt_name}", context.prompt_name.as_str()),
        ("{mode}", mode.as_str()),
        ("{prompt_path}", prompt_path),
        ("{prompt}", context.prompt_text.as_str()),
        ("{prompt_file}", prompt_file),
    ];
    for (needle, value) in replacements {
        rendered = rendered.replace(needle, value);
    }
    rendered
}

fn resolved_prompt_path<'a>(context: &'a TemplateContext, _prompt_file: &'a str) -> &'a str {
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

fn invocation_mode(prompt_name: &str) -> String {
    Path::new(prompt_name)
        .file_stem()
        .or_else(|| Path::new(prompt_name).file_name())
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| prompt_name.to_owned())
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

fn shell_std_command() -> StdCommand {
    if cfg!(windows) {
        let mut command = StdCommand::new("cmd");
        command.arg("/C");
        command
    } else {
        let mut command = StdCommand::new("sh");
        command.arg("-lc");
        command
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use camino::Utf8PathBuf;
    use ralph_core::{CodingAgent, CommandMode, PromptInput, RunnerInvocation};

    use super::{InteractiveSessionInvocation, TemplateContext, render_template};

    #[test]
    fn mode_uses_prompt_stem_for_file_backed_prompts() {
        assert_eq!(super::invocation_mode("fixture-flow.yml"), "fixture-flow");
        assert_eq!(super::invocation_mode("review-loop.yml"), "review-loop");
        assert_eq!(
            super::invocation_mode("plan_driven_build"),
            "plan_driven_build"
        );
    }

    #[test]
    fn mode_template_uses_prompt_name_stem() {
        let context = TemplateContext::from_invocation(RunnerInvocation {
            run_id: "run-1".to_owned(),
            channel_id: "main".to_owned(),
            prompt_text: "hello".to_owned(),
            project_dir: "/tmp/project".into(),
            run_dir: "/tmp/project/.ralph/runs/fixture-flow/run-1".into(),
            prompt_path: "/tmp/.config/ralph/workflows/fixture-flow.yml".into(),
            prompt_name: "fixture-flow.yml".to_owned(),
        });

        let rendered = render_template("{prompt_name}|{mode}", &context, "/tmp/prompt.txt");

        assert_eq!(rendered, "fixture-flow.yml|fixture-flow");
    }

    #[test]
    fn interactive_context_uses_project_dir_for_prompt_path() {
        let context = TemplateContext::from_interactive(&InteractiveSessionInvocation {
            session_name: "workflow_goal_interview".to_owned(),
            initial_prompt: "hello".to_owned(),
            project_dir: Utf8PathBuf::from("/tmp/project"),
            run_dir: Utf8PathBuf::from("/tmp/project/.ralph/runs/interactive-flow/run-1"),
            run_id: None,
            prompt_path: None,
        });

        let rendered = render_template("{prompt_path}|{prompt_file}", &context, "/tmp/prompt.txt");

        assert_eq!(rendered, "/tmp/project|/tmp/prompt.txt");
    }

    #[test]
    fn builtin_agents_define_interactive_non_stdin_exec_modes() {
        for builtin in [
            CodingAgent::Codex.definition(),
            CodingAgent::Opencode.definition(),
            CodingAgent::Pi.definition(),
            CodingAgent::Raijin.definition(),
        ] {
            if builtin.interactive.mode == CommandMode::Exec {
                assert_ne!(builtin.interactive.prompt_input, PromptInput::Stdin);
            }
        }
    }

    #[test]
    fn env_templates_render_prompt_file_and_prompt() {
        let context = TemplateContext::from_invocation(RunnerInvocation {
            run_id: "run-1".to_owned(),
            channel_id: "main".to_owned(),
            prompt_text: "hello".to_owned(),
            project_dir: "/tmp/project".into(),
            run_dir: "/tmp/project/.ralph/runs/fixture-flow/run-1".into(),
            prompt_path: "/tmp/.config/ralph/workflows/fixture-flow.yml".into(),
            prompt_name: "task".to_owned(),
        });
        let rendered = render_template("X={prompt} Y={prompt_file}", &context, "/tmp/prompt.txt");
        assert_eq!(rendered, "X=hello Y=/tmp/prompt.txt");
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
            prompt_input: PromptInput::File,
            prompt_env_var: "PROMPT".to_owned(),
            env: BTreeMap::new(),
        };

        let envs = super::rendered_envs(&config, &context, "/tmp/prompt.txt");
        assert!(
            envs.iter().any(|(key, value)| {
                key == "RALPH_BIN" && std::path::Path::new(value).is_absolute()
            }),
            "RALPH_BIN must be present as an absolute path"
        );
        assert!(envs.iter().any(|(key, value)| {
            key == "RALPH_RUN_DIR" && value == "/tmp/project/.ralph/runs/fixture-flow/run-1"
        }));
        assert!(
            envs.iter()
                .any(|(key, value)| { key == "RALPH_CHANNEL_ID" && value == "main" })
        );
        assert!(envs.iter().any(|(key, value)| {
            key == "RALPH_WAL_PATH"
                && value
                    == "/tmp/project/.ralph/runs/fixture-flow/run-1/.ralph-runtime/agent-events.wal.ndjson"
        }));
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
                .interactive
                .env
                .get("OPENCODE_CONFIG_CONTENT")
                .map(String::as_str),
            Some(
                r#"{"$schema":"https://opencode.ai/config.json","permission":"allow","lsp":false}"#
            )
        );
        assert_eq!(
            opencode
                .non_interactive
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
            command: Some("cat \"{prompt_file}\" | myagent".to_owned()),
            prompt_input: PromptInput::File,
            prompt_env_var: "PROMPT".to_owned(),
            env: BTreeMap::new(),
        };
        assert_eq!(config.command_preview(), "cat \"{prompt_file}\" | myagent");
    }
}
