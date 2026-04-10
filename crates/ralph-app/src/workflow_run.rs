use std::{
    collections::BTreeMap,
    fs,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};
use ralph_core::{
    AgentEventRecord, LastRunStatus, LoopControlDecision, MAIN_CHANNEL_ID, NO_ROUTE_ERROR,
    NO_ROUTE_OK, RunControl, RunnerConfig, RunnerInvocation, WorkflowDefinition,
    WorkflowParallelWorkerDefinition, WorkflowRunSummary, WorkflowRuntimeRequest,
    append_agent_event, current_agent_events_offset, load_workflow, read_agent_events_since,
    reduce_loop_control, validate_agent_event, workflow_option_flag,
};
use ralph_runner::{
    InteractiveSessionInvocation, RunnerAdapter, RunnerStreamEvent, format_event_notice,
};
use tokio::{
    sync::{Mutex as AsyncMutex, mpsc::unbounded_channel},
    task::JoinSet,
};

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

#[derive(Debug, Clone)]
enum ParallelWorkerUiEvent {
    Started {
        channel_id: String,
        label: String,
    },
    Output {
        chunk: String,
    },
    Finished {
        channel_id: String,
        label: String,
        exit_code: i32,
    },
}

#[derive(Debug, Clone)]
struct ParallelWorkerOutcome {
    channel_id: String,
    label: String,
    exit_code: i32,
}

impl<R> RalphApp<R>
where
    R: RunnerAdapter + Clone + 'static,
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
        let wal_write_lock = Arc::new(AsyncMutex::new(()));
        let mut current_prompt_id = workflow.entrypoint.clone();

        for iteration in 1..=max_iterations {
            if control.is_cancelled() {
                return Err(anyhow!("operation canceled"));
            }

            let prompt = workflow.prompt(&current_prompt_id).ok_or_else(|| {
                anyhow!("workflow prompt '{}' no longer exists", current_prompt_id)
            })?;

            delegate
                .on_event(RunEvent::IterationStarted {
                    prompt_name: current_prompt_id.clone(),
                    iteration,
                    max_iterations,
                })
                .await?;

            let next_step = if let Some(parallel) = &prompt.parallel {
                self.run_parallel_prompt(
                    &workflow,
                    &current_prompt_id,
                    parallel,
                    request.as_deref(),
                    &workflow_options,
                    &run_id,
                    &run_dir,
                    &workflow_path,
                    &control,
                    wal_write_lock.clone(),
                    delegate,
                )
                .await?
            } else {
                let prompt_text = interpolate_workflow_prompt(
                    prompt
                        .prompt
                        .as_deref()
                        .expect("validated workflow prompt must have prompt text"),
                    &self.project_dir,
                    request.as_deref(),
                    &workflow_options,
                )?;

                let wal_offset = current_agent_events_offset(&run_dir)?;
                let exit_code = if prompt.is_interactive {
                    self.run_interactive_workflow_prompt(
                        InteractiveSessionInvocation {
                            session_name: current_prompt_id.clone(),
                            initial_prompt: prompt_text,
                            project_dir: self.project_dir.clone(),
                            run_dir: run_dir.clone(),
                            run_id: Some(run_id.clone()),
                            prompt_path: Some(workflow_path.clone()),
                        },
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
                            channel_id: MAIN_CHANNEL_ID.to_owned(),
                            prompt_text,
                            project_dir: self.project_dir.clone(),
                            run_dir: run_dir.clone(),
                            prompt_path: workflow_path.clone(),
                            prompt_name: current_prompt_id.clone(),
                        },
                        &control,
                        wal_write_lock.clone(),
                        true,
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
                        WorkflowRunSummary {
                            workflow_id: workflow.workflow_id.clone(),
                            run_id,
                            final_prompt_id: current_prompt_id,
                            run_dir,
                            workflow_path,
                            status: LastRunStatus::Failed,
                        },
                        format_summary("Workflow failed", &message),
                    )
                    .await;
                }

                let log_read = read_agent_events_since(&run_dir, wal_offset)?;
                let run_events = log_read
                    .records
                    .into_iter()
                    .filter(|record| record.run_id == run_id)
                    .collect::<Vec<_>>();

                match reduce_loop_control(&run_events, &current_prompt_id) {
                    Some(LoopControlDecision::Continue) => NextStep::Continue,
                    Some(LoopControlDecision::StopOk(body)) => {
                        return finish_workflow(
                            delegate,
                            WorkflowRunSummary {
                                workflow_id: workflow.workflow_id.clone(),
                                run_id,
                                final_prompt_id: current_prompt_id,
                                run_dir,
                                workflow_path,
                                status: LastRunStatus::Completed,
                            },
                            format_summary("Workflow complete", &body),
                        )
                        .await;
                    }
                    Some(LoopControlDecision::StopError(body)) => {
                        return finish_workflow(
                            delegate,
                            WorkflowRunSummary {
                                workflow_id: workflow.workflow_id.clone(),
                                run_id,
                                final_prompt_id: current_prompt_id,
                                run_dir,
                                workflow_path,
                                status: LastRunStatus::Failed,
                            },
                            format_summary("Workflow failed", &body),
                        )
                        .await;
                    }
                    Some(LoopControlDecision::Route(route)) => NextStep::Route(route),
                    None => {
                        self.fallback_step(&workflow, &current_prompt_id, delegate)
                            .await?
                    }
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
                        WorkflowRunSummary {
                            workflow_id: workflow.workflow_id.clone(),
                            run_id,
                            final_prompt_id: current_prompt_id,
                            run_dir,
                            workflow_path,
                            status: LastRunStatus::Completed,
                        },
                        summary,
                    )
                    .await;
                }
                NextStep::FinishError(summary) => {
                    return finish_workflow(
                        delegate,
                        WorkflowRunSummary {
                            workflow_id: workflow.workflow_id.clone(),
                            run_id,
                            final_prompt_id: current_prompt_id,
                            run_dir,
                            workflow_path,
                            status: LastRunStatus::Failed,
                        },
                        summary,
                    )
                    .await;
                }
            }
        }

        finish_workflow(
            delegate,
            WorkflowRunSummary {
                workflow_id: workflow.workflow_id.clone(),
                run_id,
                final_prompt_id: current_prompt_id,
                run_dir,
                workflow_path,
                status: LastRunStatus::MaxIterations,
            },
            format!(
                "Reached max iterations for workflow {}",
                workflow.workflow_id
            ),
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

    #[allow(clippy::too_many_arguments)]
    async fn run_parallel_prompt<D>(
        &self,
        workflow: &WorkflowDefinition,
        prompt_id: &str,
        parallel: &ralph_core::WorkflowParallelDefinition,
        request: Option<&str>,
        workflow_options: &BTreeMap<String, String>,
        run_id: &str,
        run_dir: &Utf8Path,
        workflow_path: &Utf8Path,
        control: &RunControl,
        wal_write_lock: Arc<AsyncMutex<()>>,
        delegate: &mut D,
    ) -> Result<NextStep>
    where
        D: RunDelegate,
    {
        if control.is_cancelled() {
            return Err(anyhow!("operation canceled"));
        }

        let config = self.non_interactive_runner_config_for(control)?;
        let (ui_tx, mut ui_rx) = unbounded_channel();
        let mut join_set = JoinSet::new();
        let mut worker_count = 0usize;

        for (channel_id, worker) in &parallel.workers {
            let label = parallel_worker_label(channel_id, worker);
            delegate
                .on_event(RunEvent::ParallelWorkerLaunched {
                    channel_id: channel_id.to_owned(),
                    label: label.clone(),
                })
                .await?;

            let prompt_text = interpolate_workflow_prompt(
                &worker.prompt,
                &self.project_dir,
                request,
                workflow_options,
            )?;
            let worker_runner = self.runner.clone();
            let worker_config = config.clone();
            let worker_invocation = RunnerInvocation {
                run_id: run_id.to_owned(),
                channel_id: channel_id.to_owned(),
                prompt_text,
                project_dir: self.project_dir.clone(),
                run_dir: run_dir.to_path_buf(),
                prompt_path: workflow_path.to_path_buf(),
                prompt_name: prompt_id.to_owned(),
            };
            let worker_control = control.clone();
            let worker_ui_tx = ui_tx.clone();
            let worker_wal_write_lock = wal_write_lock.clone();
            let output_log_path = parallel_output_log_path(run_dir, channel_id);
            let worker_label = label.clone();
            join_set.spawn(async move {
                execute_parallel_worker(
                    worker_runner,
                    worker_config,
                    worker_invocation,
                    worker_control,
                    worker_wal_write_lock,
                    worker_ui_tx,
                    worker_label,
                    output_log_path,
                )
                .await
            });
            worker_count += 1;
        }
        drop(ui_tx);

        let mut remaining_workers = worker_count;
        let mut failures = Vec::new();
        let mut fail_fast_triggered = false;

        while remaining_workers > 0 {
            tokio::select! {
                maybe_ui = ui_rx.recv() => {
                    if let Some(event) = maybe_ui {
                        match event {
                            ParallelWorkerUiEvent::Started { channel_id, label } => {
                                delegate.on_event(RunEvent::ParallelWorkerStarted { channel_id, label }).await?;
                            }
                            ParallelWorkerUiEvent::Output { chunk } => {
                                delegate.on_event(RunEvent::Output(chunk)).await?;
                            }
                            ParallelWorkerUiEvent::Finished { channel_id, label, exit_code } => {
                                delegate.on_event(RunEvent::ParallelWorkerFinished {
                                    channel_id,
                                    label,
                                    exit_code,
                                }).await?;
                            }
                        }
                    }
                }
                maybe_join = join_set.join_next() => {
                    let Some(join_result) = maybe_join else {
                        break;
                    };
                    remaining_workers = remaining_workers.saturating_sub(1);
                    match join_result.map_err(|error| anyhow!("parallel worker task failed: {error}"))? {
                        Ok(outcome) => {
                            if outcome.exit_code != 0 {
                                failures.push((outcome.channel_id.clone(), outcome.label.clone(), outcome.exit_code));
                                if parallel.fail_fast && !fail_fast_triggered {
                                    fail_fast_triggered = true;
                                    control.cancel();
                                }
                            }
                        }
                        Err(error) => {
                            if control.is_cancelled() && fail_fast_triggered && !failures.is_empty() {
                                continue;
                            }
                            return Err(error);
                        }
                    }
                }
            }

            if control.is_cancelled() && failures.is_empty() && !fail_fast_triggered {
                while let Some(join_result) = join_set.join_next().await {
                    let _ = join_result;
                }
                return Err(anyhow!("operation canceled"));
            }
        }

        while let Some(event) = ui_rx.recv().await {
            match event {
                ParallelWorkerUiEvent::Started { channel_id, label } => {
                    delegate
                        .on_event(RunEvent::ParallelWorkerStarted { channel_id, label })
                        .await?;
                }
                ParallelWorkerUiEvent::Output { chunk } => {
                    delegate.on_event(RunEvent::Output(chunk)).await?;
                }
                ParallelWorkerUiEvent::Finished {
                    channel_id,
                    label,
                    exit_code,
                } => {
                    delegate
                        .on_event(RunEvent::ParallelWorkerFinished {
                            channel_id,
                            label,
                            exit_code,
                        })
                        .await?;
                }
            }
        }

        if let Some((channel_id, label, exit_code)) = failures.first() {
            let summary = format!(
                "parallel worker '{}' ({}) exited with code {}",
                label, channel_id, exit_code
            );
            delegate.on_event(RunEvent::Note(summary.clone())).await?;
            return Ok(NextStep::FinishError(format_summary(
                "Workflow failed",
                &summary,
            )));
        }

        self.fallback_step(workflow, prompt_id, delegate).await
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
        invocation: InteractiveSessionInvocation,
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
    wal_write_lock: Arc<AsyncMutex<()>>,
    allow_loop_control: bool,
    delegate: &mut D,
) -> Result<ralph_core::RunnerResult>
where
    R: RunnerAdapter,
    D: RunDelegate,
{
    let (stream_tx, mut stream_rx) = unbounded_channel();
    let stream_invocation = invocation.clone();
    let run = runner.run(config, invocation, control, Some(stream_tx));
    tokio::pin!(run);

    loop {
        tokio::select! {
            result = &mut run => {
                while let Some(event) = stream_rx.recv().await {
                    match event {
                        RunnerStreamEvent::Output(chunk) => {
                            delegate.on_event(RunEvent::Output(chunk)).await?;
                        }
                        RunnerStreamEvent::StartedWorking => {}
                        RunnerStreamEvent::ParsedEvents { child_pid, events } => {
                            persist_agent_events(
                                &stream_invocation,
                                child_pid,
                                &events,
                                wal_write_lock.clone(),
                                allow_loop_control,
                            )
                            .await?;
                        }
                    }
                }
                return result;
            }
            maybe = stream_rx.recv() => {
                if let Some(event) = maybe {
                    match event {
                        RunnerStreamEvent::Output(chunk) => {
                            delegate.on_event(RunEvent::Output(chunk)).await?;
                        }
                        RunnerStreamEvent::StartedWorking => {}
                        RunnerStreamEvent::ParsedEvents { child_pid, events } => {
                            persist_agent_events(
                                &stream_invocation,
                                child_pid,
                                &events,
                                wal_write_lock.clone(),
                                allow_loop_control,
                            )
                            .await?;
                        }
                    }
                }
            }
        }
    }
}

async fn execute_parallel_worker<R>(
    runner: R,
    config: RunnerConfig,
    invocation: RunnerInvocation,
    control: RunControl,
    wal_write_lock: Arc<AsyncMutex<()>>,
    ui_tx: tokio::sync::mpsc::UnboundedSender<ParallelWorkerUiEvent>,
    label: String,
    output_log_path: Utf8PathBuf,
) -> Result<ParallelWorkerOutcome>
where
    R: RunnerAdapter,
{
    let (stream_tx, mut stream_rx) = unbounded_channel();
    let stream_invocation = invocation.clone();
    let channel_id = invocation.channel_id.clone();
    let run = runner.run(&config, invocation, &control, Some(stream_tx));
    tokio::pin!(run);

    loop {
        tokio::select! {
            result = &mut run => {
                let result = match result {
                    Ok(result) => result,
                    Err(error) => {
                        let _ = ui_tx.send(ParallelWorkerUiEvent::Finished {
                            channel_id: channel_id.clone(),
                            label: label.clone(),
                            exit_code: -1,
                        });
                        return Err(error);
                    }
                };
                while let Some(event) = stream_rx.recv().await {
                    match event {
                        RunnerStreamEvent::Output(_) => {}
                        RunnerStreamEvent::StartedWorking => {
                            let _ = ui_tx.send(ParallelWorkerUiEvent::Started {
                                channel_id: channel_id.clone(),
                                label: label.clone(),
                            });
                        }
                        RunnerStreamEvent::ParsedEvents { child_pid, events } => {
                            persist_agent_events(
                                &stream_invocation,
                                child_pid,
                                &events,
                                wal_write_lock.clone(),
                                false,
                            )
                            .await?;
                            for event in events {
                                let _ = ui_tx.send(ParallelWorkerUiEvent::Output {
                                    chunk: format_event_notice(Some(&channel_id), &event),
                                });
                            }
                        }
                    }
                }

                write_parallel_output_log(&output_log_path, &result.output)?;
                let _ = ui_tx.send(ParallelWorkerUiEvent::Finished {
                    channel_id: channel_id.clone(),
                    label: label.clone(),
                    exit_code: result.exit_code,
                });
                return Ok(ParallelWorkerOutcome {
                    channel_id,
                    label,
                    exit_code: result.exit_code,
                });
            }
            maybe = stream_rx.recv() => {
                if let Some(event) = maybe {
                    match event {
                        RunnerStreamEvent::Output(_) => {}
                        RunnerStreamEvent::StartedWorking => {
                            let _ = ui_tx.send(ParallelWorkerUiEvent::Started {
                                channel_id: channel_id.clone(),
                                label: label.clone(),
                            });
                        }
                        RunnerStreamEvent::ParsedEvents { child_pid, events } => {
                            persist_agent_events(
                                &stream_invocation,
                                child_pid,
                                &events,
                                wal_write_lock.clone(),
                                false,
                            )
                            .await?;
                            for event in events {
                                let _ = ui_tx.send(ParallelWorkerUiEvent::Output {
                                    chunk: format_event_notice(Some(&channel_id), &event),
                                });
                            }
                        }
                    }
                }
            }
        }
    }
}

async fn persist_agent_events(
    invocation: &RunnerInvocation,
    child_pid: u32,
    events: &[ralph_core::ParsedAgentEvent],
    wal_write_lock: Arc<AsyncMutex<()>>,
    allow_loop_control: bool,
) -> Result<()> {
    if events.is_empty() {
        return Ok(());
    }

    for event in events {
        if !allow_loop_control && event.event.starts_with("loop-") {
            return Err(anyhow!(
                "parallel worker '{}' cannot emit loop-control event '{}'",
                invocation.channel_id,
                event.event
            ));
        }
        validate_agent_event(&event.event, &event.body, Some(&invocation.prompt_path))?;
    }

    let _guard = wal_write_lock.lock().await;
    for event in events {
        append_agent_event(
            &invocation.run_dir,
            &AgentEventRecord {
                v: 1,
                ts_unix_ms: current_unix_timestamp_ms(),
                run_id: invocation.run_id.clone(),
                channel_id: invocation.channel_id.clone(),
                event: event.event.clone(),
                body: event.body.clone(),
                project_dir: invocation.project_dir.clone(),
                run_dir: invocation.run_dir.clone(),
                prompt_path: invocation.prompt_path.clone(),
                prompt_name: invocation.prompt_name.clone(),
                pid: child_pid,
            },
        )?;
    }
    Ok(())
}

fn current_unix_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn parallel_worker_label(channel_id: &str, worker: &WorkflowParallelWorkerDefinition) -> String {
    worker
        .title
        .clone()
        .unwrap_or_else(|| channel_id.to_owned())
}

fn parallel_output_log_path(run_dir: &Utf8Path, channel_id: &str) -> Utf8PathBuf {
    run_dir
        .join(".ralph-runtime")
        .join("channels")
        .join(channel_id)
        .join("output.log")
}

fn write_parallel_output_log(path: &Utf8Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent.as_std_path())
            .with_context(|| format!("failed to create {}", parent))?;
    }
    fs::write(path.as_std_path(), contents)
        .with_context(|| format!("failed to write parallel output log {}", path))?;
    Ok(())
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
    run_summary: WorkflowRunSummary,
    event_summary: String,
) -> Result<WorkflowRunSummary>
where
    D: RunDelegate,
{
    delegate
        .on_event(RunEvent::Finished {
            status: run_summary.status,
            summary: event_summary,
        })
        .await?;
    Ok(run_summary)
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
        sync::{Arc, Mutex, OnceLock},
    };

    use anyhow::Result;
    use async_trait::async_trait;
    use camino::Utf8PathBuf;
    use ralph_core::{AppConfig, RunControl, RunnerInvocation, RunnerResult};
    use ralph_runner::{
        InteractiveSessionInvocation, InteractiveSessionOutcome, RunnerAdapter, RunnerStreamEvent,
    };
    use tokio::sync::{Mutex as AsyncMutex, MutexGuard as AsyncMutexGuard, mpsc::UnboundedSender};

    use crate::{RalphApp, RunDelegate, RunEvent, WorkflowRequestInput, WorkflowRunInput};

    const RALPH_CONFIG_HOME_ENV: &str = "RALPH_CONFIG_HOME";
    type WorkflowEvent = (String, String);
    type WorkflowEventBatch = Vec<WorkflowEvent>;
    type WorkflowEventQueue = Vec<WorkflowEventBatch>;

    fn env_lock() -> &'static AsyncMutex<()> {
        static ENV_LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();
        ENV_LOCK.get_or_init(|| AsyncMutex::new(()))
    }

    struct ScopedConfigHome {
        _guard: AsyncMutexGuard<'static, ()>,
    }

    impl ScopedConfigHome {
        async fn new(config_home: &std::path::Path) -> Self {
            let guard = env_lock().lock().await;
            unsafe {
                std::env::set_var(RALPH_CONFIG_HOME_ENV, config_home);
            }
            Self { _guard: guard }
        }
    }

    impl Drop for ScopedConfigHome {
        fn drop(&mut self) {
            unsafe {
                std::env::remove_var(RALPH_CONFIG_HOME_ENV);
            }
        }
    }

    #[derive(Clone, Default)]
    struct WorkflowSpyRunner {
        invocations: Arc<Mutex<Vec<RunnerInvocation>>>,
        interactive_invocations: Arc<Mutex<Vec<InteractiveSessionInvocation>>>,
        events: Arc<Mutex<WorkflowEventQueue>>,
        channel_events: Arc<Mutex<BTreeMap<String, WorkflowEventBatch>>>,
    }

    #[async_trait]
    impl RunnerAdapter for WorkflowSpyRunner {
        async fn run(
            &self,
            _config: &ralph_core::RunnerConfig,
            invocation: RunnerInvocation,
            _control: &RunControl,
            stream: Option<UnboundedSender<RunnerStreamEvent>>,
        ) -> Result<RunnerResult> {
            self.invocations.lock().unwrap().push(invocation.clone());
            let events = if invocation.channel_id != ralph_core::MAIN_CHANNEL_ID {
                self.channel_events
                    .lock()
                    .unwrap()
                    .remove(&invocation.channel_id)
                    .unwrap_or_default()
            } else {
                self.events.lock().unwrap().remove(0)
            };
            if let Some(tx) = stream {
                let _ = tx.send(RunnerStreamEvent::StartedWorking);
                let _ = tx.send(RunnerStreamEvent::ParsedEvents {
                    child_pid: 1,
                    events: events
                        .into_iter()
                        .map(|(event, body)| ralph_core::ParsedAgentEvent { event, body })
                        .collect(),
                });
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
                    channel_id: ralph_core::MAIN_CHANNEL_ID.to_owned(),
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
        parallel_events: Vec<String>,
        outputs: Vec<String>,
    }

    #[async_trait]
    impl RunDelegate for TestDelegate {
        async fn on_event(&mut self, event: RunEvent) -> Result<()> {
            match event {
                RunEvent::Finished { status, summary } => {
                    self.finished.push((status, summary));
                }
                RunEvent::ParallelWorkerLaunched { channel_id, .. } => {
                    self.parallel_events.push(format!("launched:{channel_id}"));
                }
                RunEvent::ParallelWorkerStarted { channel_id, .. } => {
                    self.parallel_events.push(format!("started:{channel_id}"));
                }
                RunEvent::ParallelWorkerFinished {
                    channel_id,
                    exit_code,
                    ..
                } => {
                    self.parallel_events
                        .push(format!("finished:{channel_id}:{exit_code}"));
                }
                RunEvent::Output(chunk) => {
                    self.outputs.push(chunk);
                }
                _ => {}
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn run_workflow_routes_between_prompts_and_uses_project_dir() -> Result<()> {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let config_home = temp.path().join("config-home");
        fs::create_dir_all(config_home.join("workflows")).unwrap();
        let _config_home = ScopedConfigHome::new(&config_home).await;
        fs::write(
            config_home.join("workflows/route-test.yml"),
            r#"
version: 1
workflow_id: route-test
title: Route Test
entrypoint: alpha
request:
  runtime:
    argv: true
prompts:
  alpha:
    title: Alpha
    is_interactive: false
    fallback-route: no-route-error
    prompt: |
      request={ralph-request}
  beta:
    title: Beta
    is_interactive: false
    fallback-route: no-route-error
    prompt: |
      project={ralph-env:PROJECT_DIR}
"#,
        )?;

        let runner = WorkflowSpyRunner {
            events: Arc::new(Mutex::new(vec![
                vec![("loop-route".to_owned(), "beta".to_owned())],
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
                "route-test",
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
        assert_eq!(summary.workflow_id, "route-test");
        assert_eq!(summary.status, ralph_core::LastRunStatus::Completed);
        assert_eq!(invocations.len(), 2);
        assert!(invocations[0].prompt_text.contains("request=ship it"));
        assert!(invocations[1].prompt_text.contains(project_dir.as_str()));
        assert!(summary.run_dir.join("request.txt").exists());
        Ok(())
    }

    #[tokio::test]
    async fn parallel_prompt_records_channel_scoped_events_and_routes_to_fixer() -> Result<()> {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let config_home = temp.path().join("config-home");
        fs::create_dir_all(config_home.join("workflows")).unwrap();
        let _config_home = ScopedConfigHome::new(&config_home).await;
        fs::write(
            config_home.join("workflows/parallel-review.yml"),
            r#"
version: 1
workflow_id: parallel-review
title: Parallel Review
entrypoint: reviews
request:
  runtime:
    argv: true
prompts:
  reviews:
    title: Reviews
    is_interactive: false
    fallback-route: fixer
    parallel:
      workers:
        QT:
          title: quality tester
          prompt: |
            quality {ralph-request}
        OE:
          title: over-engineering detector
          prompt: |
            overengineering {ralph-request}
        CR:
          title: correctness reviewer
          prompt: |
            correctness {ralph-request}
  fixer:
    title: Fixer
    is_interactive: false
    fallback-route: no-route-error
    prompt: |
      fix with all reviews
"#,
        )?;

        let runner = WorkflowSpyRunner {
            events: Arc::new(Mutex::new(vec![vec![(
                "loop-stop:ok".to_owned(),
                "done".to_owned(),
            )]])),
            channel_events: Arc::new(Mutex::new(BTreeMap::from([
                (
                    "CR".to_owned(),
                    vec![("review".to_owned(), "cr-review".to_owned())],
                ),
                (
                    "OE".to_owned(),
                    vec![("review".to_owned(), "oe-review".to_owned())],
                ),
                (
                    "QT".to_owned(),
                    vec![("review".to_owned(), "qt-review".to_owned())],
                ),
            ]))),
            ..Default::default()
        };
        let app = RalphApp::new(project_dir, AppConfig::default(), runner.clone());
        let mut delegate = TestDelegate::default();

        let summary = app
            .run_workflow(
                "parallel-review",
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

        assert_eq!(summary.status, ralph_core::LastRunStatus::Completed);
        let invocations = runner.invocations.lock().unwrap().clone();
        assert_eq!(invocations.len(), 4);
        let mut channels = invocations
            .iter()
            .map(|invocation| invocation.channel_id.clone())
            .collect::<Vec<_>>();
        channels.sort();
        assert_eq!(channels, vec!["CR", "OE", "QT", "main"]);

        let wal = ralph_core::read_agent_events_since(&summary.run_dir, 0)?;
        let reviews = wal
            .records
            .iter()
            .filter(|record| record.event == "review")
            .map(|record| (record.channel_id.clone(), record.body.clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            reviews,
            vec![
                ("CR".to_owned(), "cr-review".to_owned()),
                ("OE".to_owned(), "oe-review".to_owned()),
                ("QT".to_owned(), "qt-review".to_owned()),
            ]
        );
        assert!(delegate.parallel_events.contains(&"launched:QT".to_owned()));
        assert!(delegate.parallel_events.contains(&"started:QT".to_owned()));
        assert!(
            delegate
                .parallel_events
                .contains(&"finished:QT:0".to_owned())
        );
        let output = delegate.outputs.concat();
        assert!(output.contains("◆ event emitted [QT]: review | qt-review"));
        assert!(output.contains("◆ event emitted [OE]: review | oe-review"));
        assert!(output.contains("◆ event emitted [CR]: review | cr-review"));
        Ok(())
    }

    #[tokio::test]
    async fn interactive_workflow_prompts_pass_run_id_and_prompt_path() -> Result<()> {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let config_home = temp.path().join("config-home");
        fs::create_dir_all(config_home.join("workflows")).unwrap();
        let _config_home = ScopedConfigHome::new(&config_home).await;
        fs::write(
            config_home.join("workflows/interactive-flow.yml"),
            r#"
version: 1
workflow_id: interactive-flow
title: Interactive Flow
entrypoint: main
request:
  runtime:
    argv: true
prompts:
  main:
    title: Main
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
                "interactive-flow",
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
        Ok(())
    }

    #[tokio::test]
    async fn interactive_workflow_rejects_stdin_request_source() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let config_home = temp.path().join("config-home");
        fs::create_dir_all(config_home.join("workflows")).unwrap();
        let _config_home = ScopedConfigHome::new(&config_home).await;
        fs::write(
            config_home.join("workflows/interactive-flow.yml"),
            r#"
version: 1
workflow_id: interactive-flow
title: Interactive Flow
entrypoint: main
request:
  runtime:
    argv: true
    stdin: true
prompts:
  main:
    title: Main
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
                "interactive-flow",
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
    }

    #[tokio::test]
    async fn run_workflow_interpolates_declared_option_values() -> Result<()> {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let config_home = temp.path().join("config-home");
        fs::create_dir_all(config_home.join("workflows")).unwrap();
        let _config_home = ScopedConfigHome::new(&config_home).await;
        fs::write(
            config_home.join("workflows/option-flow.yml"),
            r#"
version: 1
workflow_id: option-flow
title: Option Flow
entrypoint: main
options:
  state-file:
    default: state.txt
request:
  runtime:
    argv: true
prompts:
  main:
    title: Main
    is_interactive: false
    fallback-route: no-route-error
    prompt: |
      state={ralph-option:state-file}
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
            "option-flow",
            WorkflowRunInput {
                request: WorkflowRequestInput {
                    argv: Some("ship it".to_owned()),
                    ..Default::default()
                },
                options: BTreeMap::from([("state-file".to_owned(), "custom-state.txt".to_owned())]),
            },
            &mut delegate,
        )
        .await?;

        let invocation = runner.invocations.lock().unwrap().first().cloned().unwrap();
        assert!(invocation.prompt_text.contains("state=custom-state.txt"));
        assert!(invocation.prompt_text.contains("request=ship it"));
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
