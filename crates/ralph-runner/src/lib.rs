use std::process::Stdio;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use ralph_core::{PromptTransport, RunControl, RunnerConfig, RunnerInvocation, RunnerResult};
use tempfile::NamedTempFile;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::mpsc::{self, UnboundedSender},
    time::{Duration, sleep},
};
use tracing::debug;

#[derive(Debug, Clone)]
pub enum RunnerStreamEvent {
    Stdout(String),
    Stderr(String),
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
        let mut temp_prompt = None;
        let prompt_file = if matches!(config.prompt_transport, PromptTransport::TempFile) {
            let file = NamedTempFile::new().context("failed to create prompt temp file")?;
            std::fs::write(file.path(), &invocation.prompt_text)
                .context("failed to write prompt temp file")?;
            let path = file.path().to_string_lossy().to_string();
            temp_prompt = Some(file);
            Some(path)
        } else {
            None
        };

        let mut command = if let Some(template) = &config.shell_template {
            let rendered = render_template(template, &invocation, prompt_file.as_deref());
            let mut command = shell_command();
            command.arg(rendered);
            command
        } else {
            let mut command = Command::new(&config.program);
            for arg in &config.args {
                command.arg(render_template(arg, &invocation, prompt_file.as_deref()));
            }
            command
        };

        command.current_dir(invocation.project_dir.as_std_path());
        command.stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut envs = config
            .env
            .iter()
            .map(|(key, value)| {
                (
                    key.clone(),
                    render_template(value, &invocation, prompt_file.as_deref()),
                )
            })
            .collect::<Vec<_>>();

        envs.push(("RALPH_MODE".to_owned(), invocation.mode.as_str().to_owned()));
        envs.push((
            "RALPH_PROJECT_DIR".to_owned(),
            invocation.project_dir.to_string(),
        ));
        envs.push((
            "RALPH_SPEC_PATH".to_owned(),
            invocation.spec_path.to_string(),
        ));
        envs.push((
            "RALPH_PROGRESS_PATH".to_owned(),
            invocation.progress_path.to_string(),
        ));
        if matches!(config.prompt_transport, PromptTransport::EnvVar) {
            envs.push((
                config.prompt_env_var.clone(),
                invocation.prompt_text.clone(),
            ));
        }
        if let Some(prompt_file) = &prompt_file {
            envs.push(("RALPH_PROMPT_FILE".to_owned(), prompt_file.clone()));
        }
        command.envs(envs);

        if matches!(config.prompt_transport, PromptTransport::Stdin) {
            command.stdin(Stdio::piped());
        }

        debug!(
            program = config.program,
            mode = invocation.mode.as_str(),
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

        if matches!(config.prompt_transport, PromptTransport::Stdin) {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| anyhow!("runner stdin was not available"))?;
            stdin
                .write_all(invocation.prompt_text.as_bytes())
                .await
                .context("failed to write prompt to runner stdin")?;
        }

        let (chunk_tx, mut chunk_rx) = mpsc::unbounded_channel();
        let stdout_task = tokio::spawn(read_stream(stdout, true, chunk_tx.clone()));
        let stderr_task = tokio::spawn(read_stream(stderr, false, chunk_tx));

        let mut stdout_buffer = String::new();
        let mut stderr_buffer = String::new();
        let exit_code = loop {
            if control.is_cancelled() {
                let _ = child.kill().await;
                let _ = child.wait().await;
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                drop(temp_prompt);
                return Err(anyhow!("runner canceled"));
            }

            if let Some(status) = child.try_wait().context("failed while polling runner")? {
                while let Some(event) = chunk_rx.recv().await {
                    match event {
                        RunnerStreamEvent::Stdout(chunk) => {
                            stdout_buffer.push_str(&chunk);
                            if let Some(tx) = &stream {
                                let _ = tx.send(RunnerStreamEvent::Stdout(chunk));
                            }
                        }
                        RunnerStreamEvent::Stderr(chunk) => {
                            stderr_buffer.push_str(&chunk);
                            if let Some(tx) = &stream {
                                let _ = tx.send(RunnerStreamEvent::Stderr(chunk));
                            }
                        }
                    }
                }
                break status.code().unwrap_or(-1);
            }

            tokio::select! {
                maybe = chunk_rx.recv() => {
                    if let Some(event) = maybe {
                        match event {
                            RunnerStreamEvent::Stdout(chunk) => {
                                stdout_buffer.push_str(&chunk);
                                if let Some(tx) = &stream {
                                    let _ = tx.send(RunnerStreamEvent::Stdout(chunk));
                                }
                            }
                            RunnerStreamEvent::Stderr(chunk) => {
                                stderr_buffer.push_str(&chunk);
                                if let Some(tx) = &stream {
                                    let _ = tx.send(RunnerStreamEvent::Stderr(chunk));
                                }
                            }
                        }
                    }
                }
                _ = sleep(Duration::from_millis(40)) => {}
            }
        };

        let _ = stdout_task.await;
        let _ = stderr_task.await;
        drop(temp_prompt);

        Ok(RunnerResult {
            stdout: stdout_buffer,
            stderr: stderr_buffer,
            exit_code,
        })
    }
}

async fn read_stream<R>(
    mut reader: R,
    is_stdout: bool,
    tx: UnboundedSender<RunnerStreamEvent>,
) -> Result<()>
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
        let event = if is_stdout {
            RunnerStreamEvent::Stdout(chunk)
        } else {
            RunnerStreamEvent::Stderr(chunk)
        };
        if tx.send(event).is_err() {
            break;
        }
    }
    Ok(())
}

fn render_template(
    template: &str,
    invocation: &RunnerInvocation,
    prompt_file: Option<&str>,
) -> String {
    let mut rendered = template.to_owned();
    let replacements = [
        ("{project_dir}", invocation.project_dir.as_str()),
        ("{mode}", invocation.mode.as_str()),
        ("{spec_path}", invocation.spec_path.as_str()),
        ("{progress_path}", invocation.progress_path.as_str()),
        ("{prompt}", invocation.prompt_text.as_str()),
        ("{prompt_file}", prompt_file.unwrap_or("")),
    ];
    for (needle, value) in replacements {
        rendered = rendered.replace(needle, value);
    }
    rendered
}

fn shell_command() -> Command {
    if cfg!(windows) {
        let mut command = Command::new("cmd");
        command.arg("/C");
        command
    } else {
        let mut command = Command::new("sh");
        command.arg("-lc");
        command
    }
}
