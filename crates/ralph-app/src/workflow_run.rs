use std::{
    collections::BTreeMap,
    fs,
    path::Component,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};
use ralph_core::{
    AgentEventRecord, LastRunStatus, LoopControlDecision, MAIN_CHANNEL_ID, NO_ROUTE_ERROR,
    NO_ROUTE_OK, ParsedAgentEvent, RunControl, RunnerConfig, RunnerInvocation, WorkflowDefinition,
    WorkflowParallelWorkerDefinition, WorkflowRunSummary, WorkflowRuntimeRequest,
    append_agent_event, atomic_write, current_agent_events_offset,
    latest_agent_event_body_from_wal_in_channel, load_workflow, read_agent_events_since,
    reduce_loop_control, validate_agent_event, workflow_option_flag,
};
use ralph_runner::{RunnerAdapter, RunnerStreamEvent, format_event_notice};
use tokio::{
    sync::{Mutex as AsyncMutex, mpsc::unbounded_channel},
    task::JoinSet,
};

use crate::{
    PlanningDraftDecision, PlanningDraftDecisionKind, PlanningDraftReview, PlanningQuestion,
    PlanningQuestionAnswer, RalphApp, RunDelegate, RunEvent, prompt::interpolate_workflow_prompt,
};

const HOST_CHANNEL_ID: &str = "host";
const PLANNING_QUESTION_EVENT: &str = "planning-question";
const PLANNING_ANSWER_EVENT: &str = "planning-answer";
const PLANNING_DRAFT_EVENT: &str = "planning-draft";
const PLANNING_REVIEW_EVENT: &str = "planning-review";
const PLANNING_PROGRESS_EVENT: &str = "planning-progress";
const PLANNING_PLAN_FILE_EVENT: &str = "planning-plan-file";
const PLANNING_TARGET_PATH_EVENT: &str = "planning-target-path";

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
                let config = self.runner_config_for(&control)?;
                let exit_code = execute_runner(
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
                .exit_code;

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

                if let Some(next_step) = self
                    .handle_planning_intercept(
                        &workflow,
                        &current_prompt_id,
                        request.as_deref(),
                        &workflow_options,
                        &run_id,
                        &run_dir,
                        &workflow_path,
                        &run_events,
                        wal_write_lock.clone(),
                        delegate,
                    )
                    .await?
                {
                    next_step
                } else {
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
                NextStep::FinishCanceled(summary) => {
                    return finish_workflow(
                        delegate,
                        WorkflowRunSummary {
                            workflow_id: workflow.workflow_id.clone(),
                            run_id,
                            final_prompt_id: current_prompt_id,
                            run_dir,
                            workflow_path,
                            status: LastRunStatus::Canceled,
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
    async fn handle_planning_intercept<D>(
        &self,
        _workflow: &WorkflowDefinition,
        prompt_id: &str,
        request: Option<&str>,
        workflow_options: &BTreeMap<String, String>,
        run_id: &str,
        run_dir: &Utf8Path,
        workflow_path: &Utf8Path,
        run_events: &[AgentEventRecord],
        wal_write_lock: Arc<AsyncMutex<()>>,
        delegate: &mut D,
    ) -> Result<Option<NextStep>>
    where
        D: RunDelegate,
    {
        let question_body = latest_main_event_body(run_events, PLANNING_QUESTION_EVENT);
        let draft_body = latest_main_event_body(run_events, PLANNING_DRAFT_EVENT);
        if question_body.is_none() && draft_body.is_none() {
            return Ok(None);
        }

        if question_body.is_some() && draft_body.is_some() {
            return Ok(Some(NextStep::FinishError(
                "planning prompt emitted both planning-question and planning-draft".to_owned(),
            )));
        }

        if run_events
            .iter()
            .any(|record| record.channel_id == MAIN_CHANNEL_ID && record.event.starts_with("loop-"))
        {
            return Ok(Some(NextStep::FinishError(
                "planning prompt emitted loop-control events together with planning payloads"
                    .to_owned(),
            )));
        }

        let wal_path = ralph_core::agent_events_wal_path(run_dir);
        let progress_before = latest_agent_event_body_from_wal_in_channel(
            &wal_path,
            PLANNING_PROGRESS_EVENT,
            Some(HOST_CHANNEL_ID),
        )?
        .unwrap_or_else(|| initial_planning_progress(request));

        if let Some(question_body) = question_body {
            let question = parse_planning_question(question_body)?;
            let answer = delegate.answer_planning_question(&question).await?;
            let progress_after = append_question_progress(&progress_before, &question, &answer);
            append_host_payloads(
                run_id,
                &self.project_dir,
                run_dir,
                workflow_path,
                prompt_id,
                &[
                    (
                        PLANNING_ANSWER_EVENT,
                        format_planning_answer_body(&question, &answer),
                    ),
                    (PLANNING_PROGRESS_EVENT, progress_after),
                ],
                wal_write_lock,
                delegate,
            )
            .await?;
            return Ok(Some(NextStep::Continue));
        }

        let Some(draft_body) = draft_body else {
            return Ok(None);
        };
        let plans_dir = workflow_options.get("plans-dir").map(String::as_str);
        let (target_path_body, persist_generated_target_path) =
            planning_target_path_body_for_draft(
                &self.project_dir,
                &wal_path,
                plans_dir,
                run_events,
                draft_body,
            )?;

        let target_path =
            resolve_planning_target_path(&self.project_dir, plans_dir, &target_path_body)?;
        let draft = PlanningDraftReview {
            target_path: target_path.clone(),
            draft: draft_body.to_owned(),
        };
        let decision = delegate.review_planning_draft(&draft).await?;
        let progress_after = append_review_progress(&progress_before, &decision);

        match decision.kind {
            PlanningDraftDecisionKind::Accept => {
                write_planning_draft(&target_path, &draft.draft)?;
                let mut payloads = Vec::new();
                if persist_generated_target_path {
                    payloads.push((PLANNING_TARGET_PATH_EVENT, target_path_body.clone()));
                }
                payloads.extend([
                    (
                        PLANNING_REVIEW_EVENT,
                        format_planning_review_body(&decision),
                    ),
                    (PLANNING_PROGRESS_EVENT, progress_after),
                    (
                        PLANNING_PLAN_FILE_EVENT,
                        display_project_path(&self.project_dir, &target_path),
                    ),
                ]);
                append_host_payloads(
                    run_id,
                    &self.project_dir,
                    run_dir,
                    workflow_path,
                    prompt_id,
                    &payloads,
                    wal_write_lock,
                    delegate,
                )
                .await?;
                Ok(Some(NextStep::FinishOk(format!(
                    "wrote plan to {}",
                    display_project_path(&self.project_dir, &target_path)
                ))))
            }
            PlanningDraftDecisionKind::Revise => {
                let mut payloads = Vec::new();
                if persist_generated_target_path {
                    payloads.push((PLANNING_TARGET_PATH_EVENT, target_path_body.clone()));
                }
                payloads.extend([
                    (
                        PLANNING_REVIEW_EVENT,
                        format_planning_review_body(&decision),
                    ),
                    (PLANNING_PROGRESS_EVENT, progress_after),
                ]);
                append_host_payloads(
                    run_id,
                    &self.project_dir,
                    run_dir,
                    workflow_path,
                    prompt_id,
                    &payloads,
                    wal_write_lock,
                    delegate,
                )
                .await?;
                Ok(Some(NextStep::Continue))
            }
            PlanningDraftDecisionKind::Reject => {
                let mut payloads = Vec::new();
                if persist_generated_target_path {
                    payloads.push((PLANNING_TARGET_PATH_EVENT, target_path_body));
                }
                payloads.extend([
                    (
                        PLANNING_REVIEW_EVENT,
                        format_planning_review_body(&decision),
                    ),
                    (PLANNING_PROGRESS_EVENT, progress_after),
                ]);
                append_host_payloads(
                    run_id,
                    &self.project_dir,
                    run_dir,
                    workflow_path,
                    prompt_id,
                    &payloads,
                    wal_write_lock,
                    delegate,
                )
                .await?;
                Ok(Some(NextStep::FinishCanceled(
                    "plan creation canceled by user".to_owned(),
                )))
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

        let config = self.runner_config_for(control)?;
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

    fn runner_config_for(&self, control: &RunControl) -> Result<RunnerConfig> {
        let agent_id = control
            .agent_id()
            .unwrap_or_else(|| self.config.agent_id().to_owned());
        let agent = self
            .config
            .agent_definition(&agent_id)
            .ok_or_else(|| anyhow!("agent '{}' is not defined", agent_id))?;
        Ok(agent.runner.clone())
    }
}

fn latest_main_event_body<'a>(run_events: &'a [AgentEventRecord], event: &str) -> Option<&'a str> {
    run_events
        .iter()
        .rev()
        .find(|record| record.channel_id == MAIN_CHANNEL_ID && record.event == event)
        .map(|record| record.body.as_str())
}

fn planning_target_path_body_for_draft(
    project_dir: &Utf8Path,
    wal_path: &Utf8Path,
    plans_dir: Option<&str>,
    run_events: &[AgentEventRecord],
    draft_body: &str,
) -> Result<(String, bool)> {
    if let Some(target_path_body) = latest_main_event_body(run_events, PLANNING_TARGET_PATH_EVENT) {
        return Ok((target_path_body.trim().to_owned(), false));
    }

    if let Some(target_path_body) =
        latest_agent_event_body_from_wal_in_channel(wal_path, PLANNING_TARGET_PATH_EVENT, None)?
    {
        return Ok((target_path_body.trim().to_owned(), false));
    }

    Ok((
        generate_planning_target_path_body(project_dir, plans_dir, draft_body),
        true,
    ))
}

fn parse_planning_question(body: &str) -> Result<PlanningQuestion> {
    let mut question = None;
    let mut options = Vec::new();
    let mut context = None;
    let mut in_options = false;

    for raw_line in body.lines() {
        let line = raw_line.trim();
        if let Some(rest) = line.strip_prefix("Question:") {
            question = Some(rest.trim().to_owned());
            in_options = false;
            continue;
        }
        if line == "Options:" {
            in_options = true;
            continue;
        }
        if let Some(rest) = line.strip_prefix("Context:") {
            let trimmed = rest.trim();
            context = (!trimmed.is_empty()).then(|| trimmed.to_owned());
            in_options = false;
            continue;
        }
        if in_options {
            if let Some(rest) = line.strip_prefix("- ") {
                options.push(rest.trim().to_owned());
                continue;
            }
            if !line.is_empty() {
                return Err(anyhow!(
                    "planning-question options must use '- ' bullet lines"
                ));
            }
        }
    }

    let question = question
        .filter(|question| !question.trim().is_empty())
        .ok_or_else(|| anyhow!("planning-question is missing a Question: line"))?;
    if options.is_empty() {
        return Err(anyhow!(
            "planning-question must include at least one option under Options:"
        ));
    }

    Ok(PlanningQuestion {
        question,
        options,
        context,
    })
}

fn initial_planning_progress(request: Option<&str>) -> String {
    format!(
        "# Planning Progress\n\nRequest:\n{}\n",
        request.unwrap_or("(no request provided)").trim()
    )
}

fn append_question_progress(
    progress: &str,
    question: &PlanningQuestion,
    answer: &PlanningQuestionAnswer,
) -> String {
    let mut next = ensure_trailing_blank_line(progress);
    let number = next_question_number(progress);
    next.push_str(&format!("## Question {number}\n"));
    next.push_str(&format!("Question: {}\n", question.question.trim()));
    next.push_str("Options:\n");
    for option in &question.options {
        next.push_str(&format!("- {}\n", option.trim()));
    }
    if let Some(context) = &question.context
        && !context.trim().is_empty()
    {
        next.push_str(&format!("Context: {}\n", context.trim()));
    }
    next.push_str(&format!("Answer: {}\n", answer.answer.trim()));
    next.push_str(&format!("Source: {}\n\n", answer.source.label()));
    next
}

fn append_review_progress(progress: &str, decision: &PlanningDraftDecision) -> String {
    let mut next = ensure_trailing_blank_line(progress);
    let number = next_review_number(progress);
    next.push_str(&format!("## Draft Review {number}\n"));
    next.push_str(&format!("Decision: {}\n", decision.kind.label()));
    if let Some(feedback) = &decision.feedback
        && !feedback.trim().is_empty()
    {
        next.push_str("\nFeedback:\n");
        next.push_str(feedback.trim());
        next.push('\n');
    }
    next.push('\n');
    next
}

fn ensure_trailing_blank_line(progress: &str) -> String {
    let trimmed = progress.trim_end();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}\n\n")
    }
}

fn next_question_number(progress: &str) -> usize {
    progress
        .lines()
        .filter(|line| line.starts_with("## Question "))
        .count()
        + 1
}

fn next_review_number(progress: &str) -> usize {
    progress
        .lines()
        .filter(|line| line.starts_with("## Draft Review "))
        .count()
        + 1
}

fn format_planning_answer_body(
    question: &PlanningQuestion,
    answer: &PlanningQuestionAnswer,
) -> String {
    format!(
        "Question: {}\nAnswer: {}\nSource: {}",
        question.question.trim(),
        answer.answer.trim(),
        answer.source.label()
    )
}

fn format_planning_review_body(decision: &PlanningDraftDecision) -> String {
    match &decision.feedback {
        Some(feedback) if !feedback.trim().is_empty() => format!(
            "Decision: {}\n\nFeedback:\n{}",
            decision.kind.label(),
            feedback.trim()
        ),
        _ => format!("Decision: {}", decision.kind.label()),
    }
}

fn generate_planning_target_path_body(
    project_dir: &Utf8Path,
    plans_dir: Option<&str>,
    draft_body: &str,
) -> String {
    let slug = slugify_planning_title(draft_body);
    let plans_dir = plans_dir.map(str::trim).filter(|dir| !dir.is_empty());

    for suffix in 1.. {
        let file_name = if suffix == 1 {
            format!("{slug}.md")
        } else {
            format!("{slug}-{suffix}.md")
        };
        let relative = match plans_dir {
            Some(dir) => Utf8Path::new(dir).join(&file_name),
            None => Utf8PathBuf::from(file_name),
        };
        if !project_dir.join(&relative).exists() {
            return relative.to_string();
        }
    }

    unreachable!("planning target path generation always returns")
}

fn slugify_planning_title(draft_body: &str) -> String {
    let title = draft_body
        .lines()
        .find_map(|line| line.trim().strip_prefix("# ").map(str::trim))
        .filter(|title| !title.is_empty())
        .or_else(|| {
            draft_body
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
        })
        .unwrap_or("plan");

    let mut slug = String::new();
    let mut last_was_dash = false;
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash {
            slug.push('-');
            last_was_dash = true;
        }
    }

    let slug = slug.trim_matches('-').to_owned();
    if slug.is_empty() {
        "plan".to_owned()
    } else {
        slug
    }
}

fn resolve_planning_target_path(
    project_dir: &Utf8Path,
    plans_dir: Option<&str>,
    body: &str,
) -> Result<Utf8PathBuf> {
    let relative = Utf8PathBuf::from(body.trim());
    if relative.as_str().is_empty() {
        return Err(anyhow!("planning-target-path cannot be empty"));
    }
    if relative.is_absolute() {
        return Err(anyhow!(
            "planning-target-path must be project-relative, not absolute"
        ));
    }
    for component in relative.as_std_path().components() {
        match component {
            Component::Normal(_) => {}
            _ => {
                return Err(anyhow!(
                    "planning-target-path must not contain '.', '..', or root prefixes"
                ));
            }
        }
    }
    if let Some(plans_dir) = plans_dir {
        let plans_dir = Utf8Path::new(plans_dir);
        if !relative.starts_with(plans_dir) {
            return Err(anyhow!(
                "planning-target-path '{}' must stay under '{}'",
                relative,
                plans_dir
            ));
        }
    }
    if !matches!(relative.extension(), Some("md")) {
        return Err(anyhow!(
            "planning-target-path '{}' must point to a markdown file",
            relative
        ));
    }

    let resolved = project_dir.join(&relative);
    if resolved.exists() {
        return Err(anyhow!(
            "refusing to overwrite existing plan file {}",
            display_project_path(project_dir, &resolved)
        ));
    }
    Ok(resolved)
}

fn write_planning_draft(path: &Utf8Path, draft: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent.as_std_path())
            .with_context(|| format!("failed to create {}", parent))?;
    }
    atomic_write(path.as_std_path(), draft.as_bytes())
        .with_context(|| format!("failed to write accepted plan {}", path))?;
    Ok(())
}

fn display_project_path(project_dir: &Utf8Path, path: &Utf8Path) -> String {
    path.strip_prefix(project_dir).unwrap_or(path).to_string()
}

#[allow(clippy::too_many_arguments)]
async fn append_host_payloads<D>(
    run_id: &str,
    project_dir: &Utf8Path,
    run_dir: &Utf8Path,
    workflow_path: &Utf8Path,
    prompt_id: &str,
    payloads: &[(&str, String)],
    wal_write_lock: Arc<AsyncMutex<()>>,
    delegate: &mut D,
) -> Result<()>
where
    D: RunDelegate,
{
    if payloads.is_empty() {
        return Ok(());
    }

    let guard = wal_write_lock.lock().await;
    for (event, body) in payloads {
        append_agent_event(
            run_dir,
            &AgentEventRecord {
                v: 1,
                ts_unix_ms: current_unix_timestamp_ms(),
                run_id: run_id.to_owned(),
                channel_id: HOST_CHANNEL_ID.to_owned(),
                event: (*event).to_owned(),
                body: body.clone(),
                project_dir: project_dir.to_path_buf(),
                run_dir: run_dir.to_path_buf(),
                prompt_path: workflow_path.to_path_buf(),
                prompt_name: prompt_id.to_owned(),
                pid: 0,
            },
        )?;
    }
    drop(guard);

    for (event, body) in payloads {
        let notice_body = if *event == PLANNING_PROGRESS_EVENT {
            "updated".to_owned()
        } else {
            body.clone()
        };
        delegate
            .on_event(RunEvent::Output(format_event_notice(
                Some(HOST_CHANNEL_ID),
                &ParsedAgentEvent {
                    event: (*event).to_owned(),
                    body: notice_body,
                },
            )))
            .await?;
    }

    Ok(())
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

#[allow(clippy::too_many_arguments)]
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
    FinishCanceled(String),
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
        sync::{Arc, Mutex},
    };

    use anyhow::Result;
    use async_trait::async_trait;
    use camino::Utf8PathBuf;
    use ralph_core::{
        AppConfig, RunControl, RunnerInvocation, RunnerResult, ScopedGlobalConfigDirOverride,
        scoped_global_config_dir_override,
    };
    use ralph_runner::{RunnerAdapter, RunnerStreamEvent};
    use tokio::sync::mpsc::UnboundedSender;

    use crate::workflow_run::{
        HOST_CHANNEL_ID, PLANNING_ANSWER_EVENT, PLANNING_DRAFT_EVENT, PLANNING_PLAN_FILE_EVENT,
        PLANNING_PROGRESS_EVENT, PLANNING_TARGET_PATH_EVENT,
    };
    use crate::{
        PlanningDraftDecision, PlanningDraftDecisionKind, PlanningDraftReview, PlanningQuestion,
        PlanningQuestionAnswer, RalphApp, RunDelegate, RunEvent, WorkflowRequestInput,
        WorkflowRunInput,
    };

    type WorkflowEvent = (String, String);
    type WorkflowEventBatch = Vec<WorkflowEvent>;
    type WorkflowEventQueue = Vec<WorkflowEventBatch>;

    struct ScopedConfigHome {
        _guard: ScopedGlobalConfigDirOverride,
    }

    impl ScopedConfigHome {
        fn new(config_home: Utf8PathBuf) -> Self {
            Self {
                _guard: scoped_global_config_dir_override(config_home),
            }
        }
    }

    #[derive(Clone, Default)]
    struct WorkflowSpyRunner {
        invocations: Arc<Mutex<Vec<RunnerInvocation>>>,
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
    }

    #[derive(Default)]
    struct TestDelegate {
        finished: Vec<(ralph_core::LastRunStatus, String)>,
        parallel_events: Vec<String>,
        outputs: Vec<String>,
        planning_answers: Vec<PlanningQuestionAnswer>,
        planning_decisions: Vec<PlanningDraftDecision>,
        reviewed_drafts: Vec<PlanningDraftReview>,
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

        async fn answer_planning_question(
            &mut self,
            _question: &PlanningQuestion,
        ) -> Result<PlanningQuestionAnswer> {
            Ok(self.planning_answers.remove(0))
        }

        async fn review_planning_draft(
            &mut self,
            draft: &PlanningDraftReview,
        ) -> Result<PlanningDraftDecision> {
            self.reviewed_drafts.push(draft.clone());
            Ok(self.planning_decisions.remove(0))
        }
    }

    #[tokio::test]
    async fn run_workflow_routes_between_prompts_and_uses_project_dir() -> Result<()> {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let config_home = Utf8PathBuf::from_path_buf(temp.path().join("config-home")).unwrap();
        fs::create_dir_all(config_home.join("workflows")).unwrap();
        let _config_home = ScopedConfigHome::new(config_home.clone());
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
    fallback-route: no-route-error
    prompt: |
      request={ralph-request}
  beta:
    title: Beta
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
        let config_home = Utf8PathBuf::from_path_buf(temp.path().join("config-home")).unwrap();
        fs::create_dir_all(config_home.join("workflows")).unwrap();
        let _config_home = ScopedConfigHome::new(config_home.clone());
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
    async fn run_workflow_interpolates_declared_option_values() -> Result<()> {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let config_home = Utf8PathBuf::from_path_buf(temp.path().join("config-home")).unwrap();
        fs::create_dir_all(config_home.join("workflows")).unwrap();
        let _config_home = ScopedConfigHome::new(config_home.clone());
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

    #[tokio::test]
    async fn planning_question_is_answered_by_host_and_progress_is_persisted() -> Result<()> {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let config_home = Utf8PathBuf::from_path_buf(temp.path().join("config-home")).unwrap();
        fs::create_dir_all(config_home.join("workflows")).unwrap();
        let _config_home = ScopedConfigHome::new(config_home.clone());
        fs::write(
            config_home.join("workflows/plan-fixture.yml"),
            r#"
version: 1
workflow_id: plan-fixture
title: Plan Fixture
entrypoint: plan
request:
  runtime:
    argv: true
prompts:
  plan:
    title: Plan
    fallback-route: no-route-error
    prompt: plan {ralph-request}
"#,
        )?;

        let runner = WorkflowSpyRunner {
            events: Arc::new(Mutex::new(vec![
                vec![(
                    "planning-question".to_owned(),
                    "Question: Which cache backend?\nOptions:\n- Redis\n- In-memory\nContext: Needed for the implementation plan".to_owned(),
                )],
                vec![("loop-stop:ok".to_owned(), "done".to_owned())],
            ])),
            ..Default::default()
        };
        let app = RalphApp::new(project_dir.clone(), AppConfig::default(), runner);
        let mut delegate = TestDelegate {
            planning_answers: vec![PlanningQuestionAnswer {
                answer: "Redis".to_owned(),
                source: crate::PlanningAnswerSource::Option,
            }],
            ..Default::default()
        };

        let summary = app
            .run_workflow(
                "plan-fixture",
                WorkflowRunInput {
                    request: WorkflowRequestInput {
                        argv: Some("implement caching".to_owned()),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                &mut delegate,
            )
            .await?;

        assert_eq!(summary.status, ralph_core::LastRunStatus::Completed);
        let wal = ralph_core::read_agent_events_since(&summary.run_dir, 0)?;
        assert!(wal.records.iter().any(|record| {
            record.channel_id == HOST_CHANNEL_ID
                && record.event == PLANNING_ANSWER_EVENT
                && record.body.contains("Answer: Redis")
        }));
        let progress = wal
            .records
            .iter()
            .rev()
            .find(|record| {
                record.channel_id == HOST_CHANNEL_ID && record.event == PLANNING_PROGRESS_EVENT
            })
            .map(|record| record.body.clone())
            .unwrap();
        assert!(progress.contains("## Question 1"));
        assert!(progress.contains("Answer: Redis"));
        assert!(
            delegate
                .outputs
                .concat()
                .contains("◆ event emitted [host]: planning-answer")
        );
        Ok(())
    }

    #[tokio::test]
    async fn accepted_planning_draft_is_written_by_host_verbatim() -> Result<()> {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let config_home = Utf8PathBuf::from_path_buf(temp.path().join("config-home")).unwrap();
        fs::create_dir_all(config_home.join("workflows")).unwrap();
        let _config_home = ScopedConfigHome::new(config_home.clone());
        fs::write(
            config_home.join("workflows/plan-fixture.yml"),
            r#"
version: 1
workflow_id: plan-fixture
title: Plan Fixture
entrypoint: plan
options:
  plans-dir:
    default: docs/plans
request:
  runtime:
    argv: true
prompts:
  plan:
    title: Plan
    fallback-route: no-route-error
    prompt: plan {ralph-request}
"#,
        )?;

        let draft_body = "# Cache Plan\n\n## Overview\nShip it.\n";
        let runner = WorkflowSpyRunner {
            events: Arc::new(Mutex::new(vec![vec![
                (
                    PLANNING_TARGET_PATH_EVENT.to_owned(),
                    "docs/plans/2026-04-10-cache-plan.md".to_owned(),
                ),
                (PLANNING_DRAFT_EVENT.to_owned(), draft_body.to_owned()),
            ]])),
            ..Default::default()
        };
        let app = RalphApp::new(project_dir.clone(), AppConfig::default(), runner);
        let mut delegate = TestDelegate {
            planning_decisions: vec![PlanningDraftDecision {
                kind: PlanningDraftDecisionKind::Accept,
                feedback: None,
            }],
            ..Default::default()
        };

        let summary = app
            .run_workflow(
                "plan-fixture",
                WorkflowRunInput {
                    request: WorkflowRequestInput {
                        argv: Some("implement caching".to_owned()),
                        ..Default::default()
                    },
                    options: BTreeMap::from([("plans-dir".to_owned(), "docs/plans".to_owned())]),
                },
                &mut delegate,
            )
            .await?;

        assert_eq!(summary.status, ralph_core::LastRunStatus::Completed);
        let written = project_dir.join("docs/plans/2026-04-10-cache-plan.md");
        assert_eq!(fs::read_to_string(written.as_std_path())?, draft_body);
        let wal = ralph_core::read_agent_events_since(&summary.run_dir, 0)?;
        assert!(wal.records.iter().any(|record| {
            record.channel_id == HOST_CHANNEL_ID
                && record.event == PLANNING_PLAN_FILE_EVENT
                && record.body == "docs/plans/2026-04-10-cache-plan.md"
        }));
        Ok(())
    }

    #[tokio::test]
    async fn revised_planning_draft_reuses_latest_wal_target_path() -> Result<()> {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let config_home = Utf8PathBuf::from_path_buf(temp.path().join("config-home")).unwrap();
        fs::create_dir_all(config_home.join("workflows")).unwrap();
        let _config_home = ScopedConfigHome::new(config_home.clone());
        fs::write(
            config_home.join("workflows/plan-fixture.yml"),
            r#"
version: 1
workflow_id: plan-fixture
title: Plan Fixture
entrypoint: plan
options:
  plans-dir:
    default: docs/plans
request:
  runtime:
    argv: true
prompts:
  plan:
    title: Plan
    fallback-route: no-route-error
    prompt: plan {ralph-request}
"#,
        )?;

        let first_draft = "# Cache Plan\n\n## Overview\nInitial draft.\n";
        let revised_draft = "# Cache Plan\n\n## Overview\nRevised draft.\n";
        let runner = WorkflowSpyRunner {
            events: Arc::new(Mutex::new(vec![
                vec![
                    (
                        PLANNING_TARGET_PATH_EVENT.to_owned(),
                        "docs/plans/cache-plan.md".to_owned(),
                    ),
                    (PLANNING_DRAFT_EVENT.to_owned(), first_draft.to_owned()),
                ],
                vec![(PLANNING_DRAFT_EVENT.to_owned(), revised_draft.to_owned())],
            ])),
            ..Default::default()
        };
        let app = RalphApp::new(project_dir.clone(), AppConfig::default(), runner);
        let mut delegate = TestDelegate {
            planning_decisions: vec![
                PlanningDraftDecision {
                    kind: PlanningDraftDecisionKind::Revise,
                    feedback: Some("Add validation.".to_owned()),
                },
                PlanningDraftDecision {
                    kind: PlanningDraftDecisionKind::Accept,
                    feedback: None,
                },
            ],
            ..Default::default()
        };

        let summary = app
            .run_workflow(
                "plan-fixture",
                WorkflowRunInput {
                    request: WorkflowRequestInput {
                        argv: Some("implement caching".to_owned()),
                        ..Default::default()
                    },
                    options: BTreeMap::from([("plans-dir".to_owned(), "docs/plans".to_owned())]),
                },
                &mut delegate,
            )
            .await?;

        assert_eq!(summary.status, ralph_core::LastRunStatus::Completed);
        let expected_path = project_dir.join("docs/plans/cache-plan.md");
        assert_eq!(delegate.reviewed_drafts.len(), 2);
        assert_eq!(delegate.reviewed_drafts[0].target_path, expected_path);
        assert_eq!(delegate.reviewed_drafts[1].target_path, expected_path);
        assert_eq!(
            fs::read_to_string(expected_path.as_std_path())?,
            revised_draft
        );
        Ok(())
    }

    #[tokio::test]
    async fn planning_draft_without_target_path_gets_slug_target_and_persists_it() -> Result<()> {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let config_home = Utf8PathBuf::from_path_buf(temp.path().join("config-home")).unwrap();
        fs::create_dir_all(config_home.join("workflows")).unwrap();
        let _config_home = ScopedConfigHome::new(config_home.clone());
        fs::write(
            config_home.join("workflows/plan-fixture.yml"),
            r#"
version: 1
workflow_id: plan-fixture
title: Plan Fixture
entrypoint: plan
options:
  plans-dir:
    default: docs/plans
request:
  runtime:
    argv: true
prompts:
  plan:
    title: Plan
    fallback-route: no-route-error
    prompt: plan {ralph-request}
"#,
        )?;

        let first_draft = "# Interactive Mode for JSON CLI\n\n## Overview\nInitial draft.\n";
        let revised_draft = "# Interactive Mode for JSON CLI\n\n## Overview\nAdd ls support.\n";
        let runner = WorkflowSpyRunner {
            events: Arc::new(Mutex::new(vec![
                vec![(PLANNING_DRAFT_EVENT.to_owned(), first_draft.to_owned())],
                vec![(PLANNING_DRAFT_EVENT.to_owned(), revised_draft.to_owned())],
            ])),
            ..Default::default()
        };
        let app = RalphApp::new(project_dir.clone(), AppConfig::default(), runner);
        let mut delegate = TestDelegate {
            planning_decisions: vec![
                PlanningDraftDecision {
                    kind: PlanningDraftDecisionKind::Revise,
                    feedback: Some("Add ls.".to_owned()),
                },
                PlanningDraftDecision {
                    kind: PlanningDraftDecisionKind::Accept,
                    feedback: None,
                },
            ],
            ..Default::default()
        };

        let summary = app
            .run_workflow(
                "plan-fixture",
                WorkflowRunInput {
                    request: WorkflowRequestInput {
                        argv: Some("interactive json mode".to_owned()),
                        ..Default::default()
                    },
                    options: BTreeMap::from([("plans-dir".to_owned(), "docs/plans".to_owned())]),
                },
                &mut delegate,
            )
            .await?;

        assert_eq!(summary.status, ralph_core::LastRunStatus::Completed);
        let expected_relative = "docs/plans/interactive-mode-for-json-cli.md";
        let expected_path = project_dir.join(expected_relative);
        assert_eq!(delegate.reviewed_drafts.len(), 2);
        assert_eq!(delegate.reviewed_drafts[0].target_path, expected_path);
        assert_eq!(delegate.reviewed_drafts[1].target_path, expected_path);
        assert_eq!(
            fs::read_to_string(expected_path.as_std_path())?,
            revised_draft
        );

        let wal = ralph_core::read_agent_events_since(&summary.run_dir, 0)?;
        assert!(wal.records.iter().any(|record| {
            record.channel_id == HOST_CHANNEL_ID
                && record.event == PLANNING_TARGET_PATH_EVENT
                && record.body == expected_relative
        }));
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
