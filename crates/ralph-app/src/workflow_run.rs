use std::{
    collections::BTreeMap,
    fs,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};
use ralph_core::{
    LastRunStatus, LoopControlDecision, NO_ROUTE_ERROR, NO_ROUTE_OK, RunControl, RunnerConfig,
    RunnerInvocation, WorkflowDefinition, WorkflowRunSummary, WorkflowRuntimeRequest,
    current_agent_events_offset, load_workflow, read_agent_events_since, reduce_loop_control,
    workflow_option_flag,
};
use ralph_runner::{InteractiveSessionInvocation, RunnerAdapter, RunnerStreamEvent};
use tokio::sync::mpsc::unbounded_channel;

use crate::{RalphApp, RunDelegate, RunEvent, prompt::interpolate_workflow_prompt};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkflowRequestInput {
    pub argv: Option<String>,
    pub stdin: Option<String>,
    pub request_file: Option<Utf8PathBuf>,
}

impl WorkflowRequestInput {
    fn provided_source_count(&self) -> usize {
        usize::from(self.argv.is_some())
            + usize::from(self.stdin.is_some())
            + usize::from(self.request_file.is_some())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkflowRunInput {
    pub request: WorkflowRequestInput,
    pub options: BTreeMap<String, String>,
}

async fn forward_stream_event<D>(delegate: &mut D, event: RunnerStreamEvent) -> Result<()>
where
    D: RunDelegate,
{
    match event {
        RunnerStreamEvent::Output(chunk) => delegate.on_event(RunEvent::Output(chunk)).await,
    }
}

impl<R> RalphApp<R>
where
    R: RunnerAdapter,
{
    pub async fn run_workflow<D>(
        &self,
        workflow_id: &str,
        input: WorkflowRunInput,
        delegate: &mut D,
    ) -> Result<WorkflowRunSummary>
    where
        D: RunDelegate,
    {
        self.run_workflow_with_control(workflow_id, input, RunControl::new(), delegate)
            .await
    }

    pub async fn run_workflow_with_control<D>(
        &self,
        workflow_id: &str,
        input: WorkflowRunInput,
        control: RunControl,
        delegate: &mut D,
    ) -> Result<WorkflowRunSummary>
    where
        D: RunDelegate,
    {
        let workflow = load_workflow(workflow_id)?;
        let workflow_path = workflow
            .source_path()
            .ok_or_else(|| anyhow!("workflow '{}' does not have a source path", workflow_id))?
            .to_path_buf();
        let workflow_options = self.resolve_workflow_options(&workflow, input.options)?;
        let request = self.resolve_workflow_request(&workflow, input.request)?;
        let run_id = next_workflow_run_id();
        let run_dir = self.workflow_run_dir(&workflow.workflow_id, &run_id);
        fs::create_dir_all(run_dir.as_std_path())
            .with_context(|| format!("failed to create run directory {}", run_dir))?;
        if let Some(request) = &request {
            fs::write(run_dir.join("request.txt").as_std_path(), request)
                .with_context(|| format!("failed to write request.txt under {}", run_dir))?;
        }

        let max_iterations = self.config.max_iterations;
        let mut current_prompt_id = workflow.entrypoint.clone();

        for iteration in 1..=max_iterations {
            if control.is_cancelled() {
                return Err(anyhow!("operation canceled"));
            }

            let prompt = workflow.prompt(&current_prompt_id).ok_or_else(|| {
                anyhow!("workflow prompt '{}' no longer exists", current_prompt_id)
            })?;
            let prompt_text = interpolate_workflow_prompt(
                &prompt.prompt,
                &self.project_dir,
                request.as_deref(),
                &workflow_options,
            )?;

            delegate
                .on_event(RunEvent::IterationStarted {
                    prompt_name: current_prompt_id.clone(),
                    iteration,
                    max_iterations,
                })
                .await?;

            let wal_offset = current_agent_events_offset(&run_dir)?;
            let exit_code = if prompt.is_interactive {
                self.run_interactive_workflow_prompt(
                    &workflow_path,
                    &current_prompt_id,
                    prompt_text,
                    &run_dir,
                    &run_id,
                    &control,
                    delegate,
                )
                .await?
            } else {
                let config = self.non_interactive_runner_config_for(&control)?;
                execute_runner(
                    &self.runner,
                    &config,
                    RunnerInvocation {
                        run_id: run_id.clone(),
                        prompt_text,
                        project_dir: self.project_dir.clone(),
                        run_dir: run_dir.clone(),
                        prompt_path: workflow_path.clone(),
                        prompt_name: current_prompt_id.clone(),
                    },
                    &control,
                    delegate,
                )
                .await?
                .exit_code
            };

            if exit_code != 0 {
                let message = format!("runner exited with code {}", exit_code);
                delegate.on_event(RunEvent::Note(message.clone())).await?;
                return finish_workflow(
                    delegate,
                    &workflow.workflow_id,
                    workflow_path,
                    run_dir,
                    current_prompt_id,
                    LastRunStatus::Failed,
                    &format_summary("Workflow failed", &message),
                    run_id,
                )
                .await;
            }

            let log_read = read_agent_events_since(&run_dir, wal_offset)?;
            let run_events = log_read
                .records
                .into_iter()
                .filter(|record| record.run_id == run_id)
                .collect::<Vec<_>>();

            let next_step = match reduce_loop_control(&run_events, &current_prompt_id) {
                Some(LoopControlDecision::Continue) => NextStep::Continue,
                Some(LoopControlDecision::StopOk(body)) => {
                    return finish_workflow(
                        delegate,
                        &workflow.workflow_id,
                        workflow_path,
                        run_dir,
                        current_prompt_id,
                        LastRunStatus::Completed,
                        &format_summary("Workflow complete", &body),
                        run_id,
                    )
                    .await;
                }
                Some(LoopControlDecision::StopError(body)) => {
                    return finish_workflow(
                        delegate,
                        &workflow.workflow_id,
                        workflow_path,
                        run_dir,
                        current_prompt_id,
                        LastRunStatus::Failed,
                        &format_summary("Workflow failed", &body),
                        run_id,
                    )
                    .await;
                }
                Some(LoopControlDecision::Route(route)) => NextStep::Route(route),
                None => {
                    self.fallback_step(&workflow, &current_prompt_id, delegate)
                        .await?
                }
            };

            match next_step {
                NextStep::Continue => {}
                NextStep::Route(route) => {
                    current_prompt_id = route;
                }
                NextStep::FinishOk(summary) => {
                    return finish_workflow(
                        delegate,
                        &workflow.workflow_id,
                        workflow_path,
                        run_dir,
                        current_prompt_id,
                        LastRunStatus::Completed,
                        &summary,
                        run_id,
                    )
                    .await;
                }
                NextStep::FinishError(summary) => {
                    return finish_workflow(
                        delegate,
                        &workflow.workflow_id,
                        workflow_path,
                        run_dir,
                        current_prompt_id,
                        LastRunStatus::Failed,
                        &summary,
                        run_id,
                    )
                    .await;
                }
            }
        }

        finish_workflow(
            delegate,
            &workflow.workflow_id,
            workflow_path,
            run_dir,
            current_prompt_id,
            LastRunStatus::MaxIterations,
            &format!(
                "Reached max iterations for workflow {}",
                workflow.workflow_id
            ),
            run_id,
        )
        .await
    }

    async fn fallback_step<D>(
        &self,
        workflow: &WorkflowDefinition,
        prompt_id: &str,
        delegate: &mut D,
    ) -> Result<NextStep>
    where
        D: RunDelegate,
    {
        let prompt = workflow
            .prompt(prompt_id)
            .ok_or_else(|| anyhow!("workflow prompt '{}' no longer exists", prompt_id))?;
        match prompt.fallback_route.as_str() {
            NO_ROUTE_OK => Ok(NextStep::FinishOk(format!(
                "Workflow complete: prompt '{}' finished without an explicit route",
                prompt_id
            ))),
            NO_ROUTE_ERROR => Ok(NextStep::FinishError(format!(
                "Workflow failed: prompt '{}' finished without an explicit route",
                prompt_id
            ))),
            route => {
                delegate
                    .on_event(RunEvent::Note(format!(
                        "prompt '{}' finished without a loop event; following fallback-route '{}'",
                        prompt_id, route
                    )))
                    .await?;
                Ok(NextStep::Route(route.to_owned()))
            }
        }
    }

    fn resolve_workflow_request(
        &self,
        workflow: &WorkflowDefinition,
        request_input: WorkflowRequestInput,
    ) -> Result<Option<String>> {
        let Some(request) = workflow.request.as_ref() else {
            if workflow.uses_request_token() {
                return Err(anyhow!(
                    "workflow '{}' requires a request",
                    workflow.workflow_id
                ));
            }
            return Ok(None);
        };

        if request_input.provided_source_count() > 1 {
            return Err(anyhow!(
                "provide the workflow request in exactly one runtime form: argv, stdin, or --request-file"
            ));
        }

        if let Some(runtime) = &request.runtime {
            if workflow.has_interactive_prompts() && request_input.stdin.is_some() {
                return Err(anyhow!(
                    "workflow '{}' cannot use stdin as the request source because interactive prompts need the terminal; use argv or --request-file instead",
                    workflow.workflow_id
                ));
            }
            return self.resolve_runtime_request(workflow, runtime, request_input);
        }

        if let Some(file) = &request.file {
            let path = self.resolve_project_relative_path(&file.path);
            let contents = fs::read_to_string(path.as_std_path())
                .with_context(|| format!("failed to read workflow request file {}", path))?;
            return Ok(Some(contents));
        }

        if let Some(inline) = &request.inline {
            return Ok(Some(inline.clone()));
        }

        Ok(None)
    }

    fn resolve_runtime_request(
        &self,
        workflow: &WorkflowDefinition,
        runtime: &WorkflowRuntimeRequest,
        request_input: WorkflowRequestInput,
    ) -> Result<Option<String>> {
        if request_input.argv.is_some() && !runtime.argv {
            return Err(anyhow!(
                "workflow '{}' does not accept argv requests",
                workflow.workflow_id
            ));
        }
        if request_input.stdin.is_some() && !runtime.stdin {
            return Err(anyhow!(
                "workflow '{}' does not accept stdin requests",
                workflow.workflow_id
            ));
        }
        if request_input.request_file.is_some() && !runtime.file_flag {
            return Err(anyhow!(
                "workflow '{}' does not accept --request-file",
                workflow.workflow_id
            ));
        }

        match (
            request_input.argv,
            request_input.stdin,
            request_input.request_file,
        ) {
            (Some(argv), None, None) => Ok(Some(argv)),
            (None, Some(stdin), None) => Ok(Some(stdin)),
            (None, None, Some(path)) => {
                let path = self.resolve_project_relative_path(&path);
                let contents = fs::read_to_string(path.as_std_path())
                    .with_context(|| format!("failed to read request file {}", path))?;
                Ok(Some(contents))
            }
            (None, None, None) if workflow.uses_request_token() => Err(anyhow!(
                "workflow '{}' requires a request via argv, stdin, or --request-file",
                workflow.workflow_id
            )),
            (None, None, None) => Ok(None),
            _ => Err(anyhow!(
                "provide the workflow request in exactly one runtime form: argv, stdin, or --request-file"
            )),
        }
    }

    fn resolve_project_relative_path(&self, path: &Utf8Path) -> Utf8PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.project_dir.join(path)
        }
    }

    fn resolve_workflow_options(
        &self,
        workflow: &WorkflowDefinition,
        mut provided_options: BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, String>> {
        let unknown_options = provided_options
            .keys()
            .filter(|option_id| workflow.option(option_id).is_none())
            .cloned()
            .collect::<Vec<_>>();
        if !unknown_options.is_empty() {
            return Err(anyhow!(
                "workflow '{}' does not define options: {}",
                workflow.workflow_id,
                unknown_options.join(", ")
            ));
        }

        let mut resolved = BTreeMap::new();
        for option_id in workflow.option_ids() {
            let definition = workflow
                .option(option_id)
                .expect("option ids are sourced from the workflow");
            let value = provided_options
                .remove(option_id)
                .or_else(|| definition.default.clone())
                .ok_or_else(|| {
                    let flag =
                        workflow_option_flag(option_id).unwrap_or_else(|_| option_id.to_owned());
                    anyhow!(
                        "workflow '{}' requires option '--{}'",
                        workflow.workflow_id,
                        flag
                    )
                })?;
            resolved.insert(option_id.to_owned(), value);
        }

        Ok(resolved)
    }

    fn workflow_run_dir(&self, workflow_id: &str, run_id: &str) -> Utf8PathBuf {
        self.project_dir
            .join(".ralph")
            .join("runs")
            .join(workflow_id)
            .join(run_id)
    }

    fn non_interactive_runner_config_for(&self, control: &RunControl) -> Result<RunnerConfig> {
        let agent_id = control
            .agent_id()
            .unwrap_or_else(|| self.config.agent_id().to_owned());
        let agent = self
            .config
            .agent_definition(&agent_id)
            .ok_or_else(|| anyhow!("agent '{}' is not defined", agent_id))?;
        Ok(agent.non_interactive.clone())
    }

    fn interactive_runner_config_for(&self, control: &RunControl) -> Result<RunnerConfig> {
        let agent_id = control
            .agent_id()
            .unwrap_or_else(|| self.config.agent_id().to_owned());
        let agent = self
            .config
            .agent_definition(&agent_id)
            .ok_or_else(|| anyhow!("agent '{}' is not defined", agent_id))?;
        Ok(agent.interactive.clone())
    }

    async fn run_interactive_workflow_prompt<D>(
        &self,
        workflow_path: &Utf8Path,
        prompt_id: &str,
        prompt_text: String,
        run_dir: &Utf8Path,
        run_id: &str,
        control: &RunControl,
        delegate: &mut D,
    ) -> Result<i32>
    where
        D: RunDelegate,
    {
        if control.is_cancelled() {
            return Err(anyhow!("operation canceled"));
        }
        let config = self.interactive_runner_config_for(control)?;
        let invocation = InteractiveSessionInvocation {
            session_name: prompt_id.to_owned(),
            initial_prompt: prompt_text,
            project_dir: self.project_dir.clone(),
            run_dir: run_dir.to_path_buf(),
            run_id: Some(run_id.to_owned()),
            prompt_path: Some(workflow_path.to_path_buf()),
        };
        let outcome = if let Some(outcome) = delegate
            .run_interactive_session(&config, &invocation)
            .await?
        {
            outcome
        } else {
            self.runner.run_interactive_session(&config, &invocation)?
        };
        Ok(outcome.exit_code.unwrap_or(-1))
    }
}

async fn execute_runner<R, D>(
    runner: &R,
    config: &RunnerConfig,
    invocation: RunnerInvocation,
    control: &RunControl,
    delegate: &mut D,
) -> Result<ralph_core::RunnerResult>
where
    R: RunnerAdapter,
    D: RunDelegate,
{
    let (stream_tx, mut stream_rx) = unbounded_channel();
    let run = runner.run(config, invocation, control, Some(stream_tx));
    tokio::pin!(run);

    loop {
        tokio::select! {
            result = &mut run => {
                while let Some(event) = stream_rx.recv().await {
                    forward_stream_event(delegate, event).await?;
                }
                return result;
            }
            maybe = stream_rx.recv() => {
                if let Some(event) = maybe {
                    forward_stream_event(delegate, event).await?;
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NextStep {
    Continue,
    Route(String),
    FinishOk(String),
    FinishError(String),
}

async fn finish_workflow<D>(
    delegate: &mut D,
    workflow_id: &str,
    workflow_path: Utf8PathBuf,
    run_dir: Utf8PathBuf,
    final_prompt_id: String,
    status: LastRunStatus,
    summary: &str,
    run_id: String,
) -> Result<WorkflowRunSummary>
where
    D: RunDelegate,
{
    delegate
        .on_event(RunEvent::Finished {
            status,
            summary: summary.to_owned(),
        })
        .await?;
    Ok(WorkflowRunSummary {
        workflow_id: workflow_id.to_owned(),
        run_id,
        final_prompt_id,
        run_dir,
        workflow_path,
        status,
    })
}

fn format_summary(prefix: &str, body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix}: {trimmed}")
    }
}

fn next_workflow_run_id() -> String {
    let ts_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    format!("{}-{ts_unix_ms}", std::process::id())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        sync::{Arc, Mutex},
    };

    use anyhow::Result;
    use async_trait::async_trait;
    use camino::Utf8PathBuf;
    use ralph_core::{AppConfig, RunControl, RunnerInvocation, RunnerResult};
    use ralph_runner::{
        InteractiveSessionInvocation, InteractiveSessionOutcome, RunnerAdapter, RunnerStreamEvent,
    };
    use tokio::sync::mpsc::UnboundedSender;

    use crate::{RalphApp, RunDelegate, RunEvent, WorkflowRequestInput, WorkflowRunInput};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[derive(Clone, Default)]
    struct WorkflowSpyRunner {
        invocations: Arc<Mutex<Vec<RunnerInvocation>>>,
        interactive_invocations: Arc<Mutex<Vec<InteractiveSessionInvocation>>>,
        events: Arc<Mutex<Vec<Vec<(String, String)>>>>,
    }

    #[async_trait]
    impl RunnerAdapter for WorkflowSpyRunner {
        async fn run(
            &self,
            _config: &ralph_core::RunnerConfig,
            invocation: RunnerInvocation,
            _control: &RunControl,
            _stream: Option<UnboundedSender<RunnerStreamEvent>>,
        ) -> Result<RunnerResult> {
            self.invocations.lock().unwrap().push(invocation.clone());
            let events = self.events.lock().unwrap().remove(0);
            for (event, body) in events {
                ralph_core::append_agent_event(
                    &invocation.run_dir,
                    &ralph_core::AgentEventRecord {
                        v: 1,
                        ts_unix_ms: 1,
                        run_id: invocation.run_id.clone(),
                        event,
                        body,
                        project_dir: invocation.project_dir.clone(),
                        run_dir: invocation.run_dir.clone(),
                        prompt_path: invocation.prompt_path.clone(),
                        prompt_name: invocation.prompt_name.clone(),
                        pid: 1,
                    },
                )?;
            }
            Ok(RunnerResult {
                output: String::new(),
                exit_code: 0,
            })
        }

        fn run_interactive_session(
            &self,
            _config: &ralph_core::RunnerConfig,
            invocation: &InteractiveSessionInvocation,
        ) -> Result<InteractiveSessionOutcome> {
            self.interactive_invocations
                .lock()
                .unwrap()
                .push(invocation.clone());
            ralph_core::append_agent_event(
                &invocation.run_dir,
                &ralph_core::AgentEventRecord {
                    v: 1,
                    ts_unix_ms: 1,
                    run_id: invocation.run_id.clone().unwrap_or_default(),
                    event: "loop-stop:ok".to_owned(),
                    body: "done".to_owned(),
                    project_dir: invocation.project_dir.clone(),
                    run_dir: invocation.run_dir.clone(),
                    prompt_path: invocation
                        .prompt_path
                        .clone()
                        .unwrap_or_else(|| invocation.project_dir.clone()),
                    prompt_name: invocation.session_name.clone(),
                    pid: 1,
                },
            )?;
            Ok(InteractiveSessionOutcome { exit_code: Some(0) })
        }
    }

    #[derive(Default)]
    struct TestDelegate {
        finished: Vec<(ralph_core::LastRunStatus, String)>,
    }

    #[async_trait]
    impl RunDelegate for TestDelegate {
        async fn on_event(&mut self, event: RunEvent) -> Result<()> {
            if let RunEvent::Finished { status, summary } = event {
                self.finished.push((status, summary));
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn run_workflow_routes_between_prompts_and_uses_project_dir() -> Result<()> {
        let _guard = ENV_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let config_home = temp.path().join("config-home");
        fs::create_dir_all(config_home.join("workflows")).unwrap();
        unsafe {
            std::env::set_var("RALPH_CONFIG_HOME", &config_home);
        }
        fs::write(
            config_home.join("workflows/plan-build.yml"),
            r#"
version: 1
workflow_id: plan-build
title: Plan Build
entrypoint: plan
request:
  runtime:
    argv: true
prompts:
  plan:
    title: Plan
    is_interactive: false
    fallback-route: no-route-error
    prompt: |
      request={ralph-request}
  build:
    title: Build
    is_interactive: false
    fallback-route: no-route-error
    prompt: |
      project={ralph-env:PROJECT_DIR}
"#,
        )?;

        let runner = WorkflowSpyRunner {
            events: Arc::new(Mutex::new(vec![
                vec![("loop-route".to_owned(), "build".to_owned())],
                vec![("loop-stop:ok".to_owned(), "done".to_owned())],
            ])),
            ..Default::default()
        };
        let app = RalphApp::new(
            project_dir.clone(),
            AppConfig {
                max_iterations: 4,
                ..Default::default()
            },
            runner.clone(),
        );
        let mut delegate = TestDelegate::default();

        let summary = app
            .run_workflow(
                "plan-build",
                WorkflowRunInput {
                    request: WorkflowRequestInput {
                        argv: Some("ship it".to_owned()),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                &mut delegate,
            )
            .await?;

        let invocations = runner.invocations.lock().unwrap().clone();
        assert_eq!(summary.workflow_id, "plan-build");
        assert_eq!(summary.status, ralph_core::LastRunStatus::Completed);
        assert_eq!(invocations.len(), 2);
        assert!(invocations[0].prompt_text.contains("request=ship it"));
        assert!(invocations[1].prompt_text.contains(project_dir.as_str()));
        assert!(summary.run_dir.join("request.txt").exists());

        unsafe {
            std::env::remove_var("RALPH_CONFIG_HOME");
        }
        Ok(())
    }

    #[tokio::test]
    async fn interactive_workflow_prompts_pass_run_id_and_prompt_path() -> Result<()> {
        let _guard = ENV_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let config_home = temp.path().join("config-home");
        fs::create_dir_all(config_home.join("workflows")).unwrap();
        unsafe {
            std::env::set_var("RALPH_CONFIG_HOME", &config_home);
        }
        fs::write(
            config_home.join("workflows/pdd.yml"),
            r#"
version: 1
workflow_id: pdd
title: Pdd
entrypoint: pdd
request:
  runtime:
    argv: true
prompts:
  pdd:
    title: Pdd
    is_interactive: true
    fallback-route: no-route-ok
    prompt: |
      hello {ralph-request}
"#,
        )?;

        let runner = WorkflowSpyRunner::default();
        let app = RalphApp::new(project_dir, AppConfig::default(), runner.clone());
        let mut delegate = TestDelegate::default();
        let summary = app
            .run_workflow(
                "pdd",
                WorkflowRunInput {
                    request: WorkflowRequestInput {
                        argv: Some("rough idea".to_owned()),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                &mut delegate,
            )
            .await?;

        let invocation = runner
            .interactive_invocations
            .lock()
            .unwrap()
            .first()
            .cloned()
            .unwrap();
        assert_eq!(summary.status, ralph_core::LastRunStatus::Completed);
        assert_eq!(invocation.run_id.as_deref(), Some(summary.run_id.as_str()));
        assert!(invocation.prompt_path.is_some());
        assert!(invocation.initial_prompt.contains("rough idea"));

        unsafe {
            std::env::remove_var("RALPH_CONFIG_HOME");
        }
        Ok(())
    }

    #[tokio::test]
    async fn interactive_workflow_rejects_stdin_request_source() {
        let _guard = ENV_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let config_home = temp.path().join("config-home");
        fs::create_dir_all(config_home.join("workflows")).unwrap();
        unsafe {
            std::env::set_var("RALPH_CONFIG_HOME", &config_home);
        }
        fs::write(
            config_home.join("workflows/pdd.yml"),
            r#"
version: 1
workflow_id: pdd
title: Pdd
entrypoint: pdd
request:
  runtime:
    argv: true
    stdin: true
prompts:
  pdd:
    title: Pdd
    is_interactive: true
    fallback-route: no-route-ok
    prompt: |
      hello {ralph-request}
"#,
        )
        .unwrap();

        let app = RalphApp::new(
            project_dir,
            AppConfig::default(),
            WorkflowSpyRunner::default(),
        );
        let mut delegate = TestDelegate::default();
        let error = app
            .run_workflow(
                "pdd",
                WorkflowRunInput {
                    request: WorkflowRequestInput {
                        stdin: Some("rough idea".to_owned()),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                &mut delegate,
            )
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("cannot use stdin as the request source"));
        unsafe {
            std::env::remove_var("RALPH_CONFIG_HOME");
        }
    }

    #[tokio::test]
    async fn run_workflow_interpolates_declared_option_values() -> Result<()> {
        let _guard = ENV_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let config_home = temp.path().join("config-home");
        fs::create_dir_all(config_home.join("workflows")).unwrap();
        unsafe {
            std::env::set_var("RALPH_CONFIG_HOME", &config_home);
        }
        fs::write(
            config_home.join("workflows/task-based.yml"),
            r#"
version: 1
workflow_id: task-based
title: Task Based
entrypoint: main
options:
  progress-file:
    default: progress.txt
request:
  runtime:
    argv: true
prompts:
  main:
    title: Main
    is_interactive: false
    fallback-route: no-route-error
    prompt: |
      progress={ralph-option:progress-file}
      request={ralph-request}
"#,
        )?;

        let runner = WorkflowSpyRunner {
            events: Arc::new(Mutex::new(vec![vec![(
                "loop-stop:ok".to_owned(),
                "done".to_owned(),
            )]])),
            ..Default::default()
        };
        let app = RalphApp::new(project_dir, AppConfig::default(), runner.clone());
        let mut delegate = TestDelegate::default();

        app.run_workflow(
            "task-based",
            WorkflowRunInput {
                request: WorkflowRequestInput {
                    argv: Some("ship it".to_owned()),
                    ..Default::default()
                },
                options: BTreeMap::from([(
                    "progress-file".to_owned(),
                    "custom-progress.txt".to_owned(),
                )]),
            },
            &mut delegate,
        )
        .await?;

        let invocation = runner.invocations.lock().unwrap().first().cloned().unwrap();
        assert!(
            invocation
                .prompt_text
                .contains("progress=custom-progress.txt")
        );
        assert!(invocation.prompt_text.contains("request=ship it"));

        unsafe {
            std::env::remove_var("RALPH_CONFIG_HOME");
        }
        Ok(())
    }

    #[test]
    fn request_input_counts_only_populated_sources() {
        let input = WorkflowRequestInput {
            argv: Some("a".to_owned()),
            stdin: None,
            request_file: Some(Utf8PathBuf::from("b")),
        };
        assert_eq!(input.provided_source_count(), 2);
    }
}
