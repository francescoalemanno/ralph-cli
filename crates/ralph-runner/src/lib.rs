use std::{
    path::Path,
    process::{Child as StdChild, Command as StdCommand, Stdio},
};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use camino::Utf8PathBuf;
use ralph_core::{
    CommandMode, PromptInput, RunControl, RunnerConfig, RunnerInvocation, RunnerResult,
};
use tempfile::NamedTempFile;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::{Child as AsyncChild, Command as AsyncCommand},
    sync::mpsc::{self, UnboundedSender},
    task::JoinHandle,
    time::{Duration, sleep, timeout},
};
use tracing::debug;

#[derive(Debug, Clone)]
pub enum RunnerStreamEvent {
    Output(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractiveSessionInvocation {
    pub initial_prompt: String,
    pub project_dir: Utf8PathBuf,
    pub target_dir: Utf8PathBuf,
    pub goal_path: Utf8PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InteractiveSessionOutcome {
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone)]
struct TemplateContext {
    prompt_text: String,
    project_dir: Utf8PathBuf,
    target_dir: Utf8PathBuf,
    prompt_path: Utf8PathBuf,
    prompt_name: String,
    goal_path: Option<Utf8PathBuf>,
}

impl TemplateContext {
    fn from_invocation(invocation: RunnerInvocation) -> Self {
        Self {
            prompt_text: invocation.prompt_text,
            project_dir: invocation.project_dir,
            target_dir: invocation.target_dir,
            prompt_path: invocation.prompt_path,
            prompt_name: invocation.prompt_name,
            goal_path: None,
        }
    }

    fn from_interactive(invocation: &InteractiveSessionInvocation) -> Self {
        Self {
            prompt_text: invocation.initial_prompt.clone(),
            project_dir: invocation.project_dir.clone(),
            target_dir: invocation.target_dir.clone(),
            prompt_path: invocation.goal_path.clone(),
            prompt_name: "workflow_goal_interview".to_owned(),
            goal_path: Some(invocation.goal_path.clone()),
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
        let prompt_file = staged_prompt_file(&context.prompt_text)?;
        let prompt_file_path = prompt_file.path().to_string_lossy().to_string();

        let mut command = build_async_command(config, &context, &prompt_file_path)?;
        configure_async_process_group(&mut command);
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
        let child_pid = child.id();
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
        let mut sent_cancel_stage = 0_u8;
        let exit_code = loop {
            let cancel_stage = control.cancel_stage();
            if cancel_stage > sent_cancel_stage {
                match cancel_stage {
                    1 => interrupt_runner(&mut child, child_pid).await,
                    _ => force_kill_runner(&mut child, child_pid).await,
                }
                sent_cancel_stage = cancel_stage;
            }

            if sent_cancel_stage >= 2 {
                stdout_task.abort();
                stderr_task.abort();
                let _ = timeout(Duration::from_millis(250), child.wait()).await;
                return Err(anyhow!("runner canceled"));
            }

            if sent_cancel_stage >= 1
                && child
                    .try_wait()
                    .context("failed while polling canceled runner")?
                    .is_some()
            {
                stdout_task.abort();
                stderr_task.abort();
                return Err(anyhow!("runner canceled"));
            }

            if let Some(status) = child.try_wait().context("failed while polling runner")? {
                while let Some(event) = chunk_rx.recv().await {
                    let RunnerStreamEvent::Output(chunk) = event;
                    output_buffer.push_str(&chunk);
                    if let Some(tx) = &stream {
                        let _ = tx.send(RunnerStreamEvent::Output(chunk));
                    }
                }
                break status.code().unwrap_or(-1);
            }

            tokio::select! {
                maybe = chunk_rx.recv() => {
                    if let Some(event) = maybe {
                        let RunnerStreamEvent::Output(chunk) = event;
                        output_buffer.push_str(&chunk);
                        if let Some(tx) = &stream {
                            let _ = tx.send(RunnerStreamEvent::Output(chunk));
                        }
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
            let program = config
                .program
                .as_deref()
                .ok_or_else(|| anyhow!("exec command is missing program"))?;
            let mut command = AsyncCommand::new(program);
            for arg in &config.args {
                command.arg(render_template(arg, context, prompt_file));
            }
            command
        }
        CommandMode::Shell => {
            let template = config
                .command
                .as_deref()
                .ok_or_else(|| anyhow!("shell command is missing command"))?;
            let mut command = shell_async_command();
            command.arg(render_template(template, context, prompt_file));
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
            let program = config
                .program
                .as_deref()
                .ok_or_else(|| anyhow!("exec command is missing program"))?;
            let mut command = StdCommand::new(program);
            for arg in &config.args {
                command.arg(render_template(arg, context, prompt_file));
            }
            command
        }
        CommandMode::Shell => {
            let template = config
                .command
                .as_deref()
                .ok_or_else(|| anyhow!("shell command is missing command"))?;
            let mut command = shell_std_command();
            command.arg(render_template(template, context, prompt_file));
            command
        }
    };
    Ok(command)
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
    envs.push((
        "RALPH_TARGET_DIR".to_owned(),
        context.target_dir.to_string(),
    ));
    envs.push((
        "RALPH_PROMPT_PATH".to_owned(),
        context.prompt_path.to_string(),
    ));
    envs.push(("RALPH_PROMPT_NAME".to_owned(), context.prompt_name.clone()));
    envs.push((
        "RALPH_MODE".to_owned(),
        invocation_mode(&context.prompt_name),
    ));
    envs.push(("RALPH_PROMPT_FILE".to_owned(), prompt_file.to_owned()));
    if let Some(goal_path) = &context.goal_path {
        envs.push(("RALPH_GOAL_PATH".to_owned(), goal_path.to_string()));
    }
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
fn configure_async_process_group(command: &mut AsyncCommand) {
    use std::io;

    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        });
    }
}

#[cfg(not(unix))]
fn configure_async_process_group(_command: &mut AsyncCommand) {}

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

async fn interrupt_runner(child: &mut AsyncChild, child_pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = child_pid {
        let _ = signal_process_group(pid, libc::SIGINT);
        return;
    }

    let _ = child.start_kill();
}

async fn force_kill_runner(child: &mut AsyncChild, child_pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = child_pid {
        let _ = signal_process_group(pid, libc::SIGKILL);
    }

    let _ = child.start_kill();
}

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
    let goal_path = context.goal_path.as_ref().unwrap_or(&context.prompt_path);
    let replacements = [
        ("{project_dir}", context.project_dir.as_str()),
        ("{target_dir}", context.target_dir.as_str()),
        ("{prompt_name}", context.prompt_name.as_str()),
        ("{mode}", mode.as_str()),
        ("{prompt_path}", context.prompt_path.as_str()),
        ("{goal_path}", goal_path.as_str()),
        ("{prompt}", context.prompt_text.as_str()),
        ("{prompt_file}", prompt_file),
    ];
    for (needle, value) in replacements {
        rendered = rendered.replace(needle, value);
    }
    rendered
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
        assert_eq!(super::invocation_mode("prompt_main.md"), "prompt_main");
        assert_eq!(super::invocation_mode("0_plan.md"), "0_plan");
        assert_eq!(
            super::invocation_mode("goal_driven_build"),
            "goal_driven_build"
        );
    }

    #[test]
    fn mode_template_is_distinct_from_prompt_name() {
        let context = TemplateContext::from_invocation(RunnerInvocation {
            prompt_text: "hello".to_owned(),
            project_dir: "/tmp/project".into(),
            target_dir: "/tmp/project/.ralph/targets/demo".into(),
            prompt_path: "/tmp/project/.ralph/targets/demo/prompt_main.md".into(),
            prompt_name: "prompt_main.md".to_owned(),
        });

        let rendered = render_template("{prompt_name}|{mode}", &context, "/tmp/prompt.txt");

        assert_eq!(rendered, "prompt_main.md|prompt_main");
    }

    #[test]
    fn interactive_context_exposes_goal_path() {
        let context = TemplateContext::from_interactive(&InteractiveSessionInvocation {
            initial_prompt: "hello".to_owned(),
            project_dir: Utf8PathBuf::from("/tmp/project"),
            target_dir: Utf8PathBuf::from("/tmp/project/.ralph/targets/demo"),
            goal_path: Utf8PathBuf::from("/tmp/project/.ralph/targets/demo/GOAL.md"),
        });

        let rendered = render_template("{goal_path}|{prompt_file}", &context, "/tmp/prompt.txt");

        assert_eq!(
            rendered,
            "/tmp/project/.ralph/targets/demo/GOAL.md|/tmp/prompt.txt"
        );
    }

    #[test]
    fn builtin_agents_define_interactive_non_stdin_exec_modes() {
        for builtin in [
            CodingAgent::Codex.definition(),
            CodingAgent::Opencode.definition(),
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
            prompt_text: "hello".to_owned(),
            project_dir: "/tmp/project".into(),
            target_dir: "/tmp/project/.ralph/targets/demo".into(),
            prompt_path: "/tmp/project/.ralph/targets/demo/prompt_main.md".into(),
            prompt_name: "prompt_main.md".to_owned(),
        });
        let rendered = render_template("X={prompt} Y={prompt_file}", &context, "/tmp/prompt.txt");
        assert_eq!(rendered, "X=hello Y=/tmp/prompt.txt");
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
            Some(r#"{"$schema":"https://opencode.ai/config.json","permission":"allow"}"#)
        );
        assert_eq!(
            opencode
                .non_interactive
                .env
                .get("OPENCODE_CONFIG_CONTENT")
                .map(String::as_str),
            Some(r#"{"$schema":"https://opencode.ai/config.json","permission":"allow"}"#)
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
