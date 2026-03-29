use std::{env, process::Stdio};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use ralph_core::{PromptTransport, RunControl, RunnerConfig, RunnerInvocation, RunnerResult};
use serde_json::json;
use tempfile::NamedTempFile;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    sync::mpsc::{self, UnboundedSender},
    task::JoinHandle,
    time::{Duration, sleep, timeout},
};
use tracing::debug;

#[derive(Debug, Clone)]
pub enum RunnerStreamEvent {
    Output(String),
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

        configure_process_group(&mut command);
        command.current_dir(invocation.project_dir.as_std_path());
        command.kill_on_drop(true);
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
        if should_inject_opencode_permissions(config, &envs) {
            envs.push((
                "OPENCODE_CONFIG_CONTENT".to_owned(),
                opencode_auto_approve_config(),
            ));
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
        let child_pid = child.id();
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
                drop(temp_prompt);
                return Err(anyhow!("runner canceled"));
            }

            if sent_cancel_stage >= 1 {
                if child
                    .try_wait()
                    .context("failed while polling canceled runner")?
                    .is_some()
                {
                    stdout_task.abort();
                    stderr_task.abort();
                    drop(temp_prompt);
                    return Err(anyhow!("runner canceled"));
                }
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
        drop(temp_prompt);

        Ok(RunnerResult {
            output: output_buffer,
            exit_code,
        })
    }
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
fn configure_process_group(command: &mut Command) {
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
fn configure_process_group(_command: &mut Command) {}

async fn interrupt_runner(child: &mut Child, child_pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = child_pid {
        let _ = signal_process_group(pid, libc::SIGINT);
        return;
    }

    let _ = child.start_kill();
}

async fn force_kill_runner(child: &mut Child, child_pid: Option<u32>) {
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

fn should_inject_opencode_permissions(config: &RunnerConfig, envs: &[(String, String)]) -> bool {
    is_opencode_program(&config.program)
        && !has_explicit_opencode_config(envs)
        && env::var_os("OPENCODE_CONFIG").is_none()
        && env::var_os("OPENCODE_CONFIG_CONTENT").is_none()
}

fn has_explicit_opencode_config(envs: &[(String, String)]) -> bool {
    envs.iter()
        .any(|(key, _)| matches!(key.as_str(), "OPENCODE_CONFIG" | "OPENCODE_CONFIG_CONTENT"))
}

fn is_opencode_program(program: &str) -> bool {
    let name = program.rsplit(['/', '\\']).next().unwrap_or(program);
    name.strip_suffix(".exe")
        .unwrap_or(name)
        .eq_ignore_ascii_case("opencode")
}

fn opencode_auto_approve_config() -> String {
    json!({
        "$schema": "https://opencode.ai/config.json",
        "permission": "allow",
    })
    .to_string()
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

#[cfg(test)]
mod tests {
    use super::{has_explicit_opencode_config, is_opencode_program, opencode_auto_approve_config};

    #[test]
    fn detects_opencode_program_names() {
        assert!(is_opencode_program("opencode"));
        assert!(is_opencode_program("/usr/local/bin/opencode"));
        assert!(is_opencode_program(r"C:\\Tools\\opencode.exe"));
        assert!(!is_opencode_program("claude"));
    }

    #[test]
    fn detects_explicit_opencode_overrides() {
        assert!(has_explicit_opencode_config(&[(
            "OPENCODE_CONFIG".to_owned(),
            "/tmp/opencode.json".to_owned(),
        )]));
        assert!(has_explicit_opencode_config(&[(
            "OPENCODE_CONFIG_CONTENT".to_owned(),
            "{}".to_owned(),
        )]));
        assert!(!has_explicit_opencode_config(&[(
            "RALPH_MODE".to_owned(),
            "build".to_owned(),
        )]));
    }

    #[test]
    fn builds_allow_all_opencode_config() {
        assert_eq!(
            opencode_auto_approve_config(),
            r#"{"$schema":"https://opencode.ai/config.json","permission":"allow"}"#
        );
    }
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
