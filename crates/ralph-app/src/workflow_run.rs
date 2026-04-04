use anyhow::{Result, anyhow};
use camino::Utf8Path;
use ralph_core::{
    GoalDrivenInflight, GoalDrivenPhase, GoalDrivenWorkflowState, LastRunStatus, RunControl,
    TargetConfig, TargetSummary, WorkflowMode,
};
use ralph_runner::RunnerAdapter;

use crate::{
    RalphApp, RunDelegate, RunEvent, WorkflowAction, WorkflowRunAdvice, WorkflowStatus,
    workflow::{
        GOAL_DRIVEN_BUILD_PROMPT, GOAL_DRIVEN_GOAL_FILE, GOAL_DRIVEN_PAUSED_PROMPT,
        GOAL_DRIVEN_PLAN_PROMPT, GoalDrivenAction, TASK_BASED_BUILD_PROMPT,
        TASK_BASED_PAUSED_PROMPT, TASK_BASED_PROGRESS_FILE, TASK_BASED_REBASE_PROMPT,
        TaskBasedAction, current_unix_timestamp, goal_driven_build_prompt, goal_driven_hashes,
        goal_driven_plan_prompt, goal_driven_workflow_status, task_based_build_prompt,
        task_based_hashes, task_based_rebase_prompt, task_based_workflow_status,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkflowRunMode {
    Smart,
    Build,
    Rebase,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkflowRunRequest {
    prompt_name: String,
    prompt_text: String,
    completed_summary: String,
    max_iterations_summary: String,
    inflight_phase: GoalDrivenPhase,
}

impl<R> RalphApp<R>
where
    R: RunnerAdapter,
{
    pub fn workflow_status(&self, target: &str) -> Result<Option<WorkflowStatus>> {
        let target_config = self.store.read_target_config(target)?;
        let target_dir = self.store.target_paths(target)?.dir;
        match target_config.mode {
            Some(WorkflowMode::GoalDriven) => Ok(Some(goal_driven_workflow_status(
                &target_config,
                &goal_driven_hashes(&self.store, &target_dir)?,
                &target_dir,
            ))),
            Some(WorkflowMode::TaskBased) => Ok(Some(task_based_workflow_status(
                &target_config,
                &task_based_hashes(&self.store, &target_dir)?,
                &target_dir,
            ))),
            None => Ok(None),
        }
    }

    pub async fn run_workflow_action_with_control<D>(
        &self,
        target: &str,
        action: WorkflowAction,
        control: RunControl,
        delegate: &mut D,
    ) -> Result<TargetSummary>
    where
        D: RunDelegate,
    {
        let target_config = self.store.read_target_config(target)?;
        match target_config.mode {
            Some(WorkflowMode::GoalDriven) => {
                self.run_goal_driven_target_with_control(
                    target,
                    None,
                    target_config,
                    control,
                    delegate,
                    match action {
                        WorkflowAction::Build => WorkflowRunMode::Build,
                        WorkflowAction::Rebase => WorkflowRunMode::Rebase,
                    },
                )
                .await
            }
            Some(WorkflowMode::TaskBased) => {
                self.run_task_based_target_with_control(
                    target,
                    None,
                    target_config,
                    control,
                    delegate,
                    match action {
                        WorkflowAction::Build => WorkflowRunMode::Build,
                        WorkflowAction::Rebase => WorkflowRunMode::Rebase,
                    },
                )
                .await
            }
            None => Err(anyhow!(
                "target '{target}' does not use a hidden workflow and has no workflow actions"
            )),
        }
    }

    pub(crate) async fn run_goal_driven_target_with_control<D>(
        &self,
        target: &str,
        prompt_name: Option<&str>,
        mut target_config: TargetConfig,
        control: RunControl,
        delegate: &mut D,
        mode: WorkflowRunMode,
    ) -> Result<TargetSummary>
    where
        D: RunDelegate,
    {
        if let Some(prompt_name) = prompt_name {
            return Err(anyhow!(
                "goal_driven targets select plan/build internally; remove --prompt ('{prompt_name}')"
            ));
        }

        let target_dir = self.store.target_paths(target)?.dir;
        let hashes = goal_driven_hashes(&self.store, &target_dir)?;
        let workflow_status = goal_driven_workflow_status(&target_config, &hashes, &target_dir);
        let action = match mode {
            WorkflowRunMode::Smart => match workflow_status.run_advice {
                WorkflowRunAdvice::Rebase => GoalDrivenAction::Plan,
                WorkflowRunAdvice::Build => GoalDrivenAction::Build,
                WorkflowRunAdvice::Choose => {
                    return self
                        .finish_paused_workflow_target(
                            target,
                            &mut target_config,
                            GOAL_DRIVEN_PAUSED_PROMPT,
                            format!(
                                "{target} has a stale plan derived from an older GOAL; choose B to build the current plan, G to rebase the plan, X to rebuild from scratch, or I to refine GOAL."
                            ),
                            delegate,
                        )
                        .await;
                }
                WorkflowRunAdvice::NoWork => unreachable!(),
            },
            WorkflowRunMode::Build => {
                if workflow_status.derived_state == crate::WorkflowDerivedState::Missing {
                    return self
                        .finish_paused_workflow_target(
                            target,
                            &mut target_config,
                            GOAL_DRIVEN_PAUSED_PROMPT,
                            format!(
                                "{target} has no current plan; use G to create or rebase the plan, or X to rebuild from scratch."
                            ),
                            delegate,
                        )
                        .await;
                }
                GoalDrivenAction::Build
            }
            WorkflowRunMode::Rebase => GoalDrivenAction::Plan,
        };

        let request = match action {
            GoalDrivenAction::Plan => WorkflowRunRequest {
                prompt_name: GOAL_DRIVEN_PLAN_PROMPT.to_owned(),
                prompt_text: goal_driven_plan_prompt(),
                completed_summary: format!("Planning complete for {}", target),
                max_iterations_summary: format!("Reached max iterations while planning {}", target),
                inflight_phase: GoalDrivenPhase::Plan,
            },
            GoalDrivenAction::Build => WorkflowRunRequest {
                prompt_name: GOAL_DRIVEN_BUILD_PROMPT.to_owned(),
                prompt_text: goal_driven_build_prompt(),
                completed_summary: format!("Build complete for {}", target),
                max_iterations_summary: format!("Reached max iterations while building {}", target),
                inflight_phase: GoalDrivenPhase::Build,
            },
        };

        self.mark_workflow_inflight(
            &mut target_config,
            request.inflight_phase,
            &hashes.goal_hash,
            &hashes.content_hash,
        )?;
        let status = self
            .run_workflow_request(
                target,
                &target_dir,
                &target_config,
                &request,
                &control,
                delegate,
            )
            .await?;

        if status == LastRunStatus::Completed {
            let after_hashes = goal_driven_hashes(&self.store, &target_dir)?;
            let workflow = target_config
                .workflow
                .get_or_insert_with(GoalDrivenWorkflowState::default);
            workflow.last_content_hash = Some(after_hashes.content_hash);
            match action {
                GoalDrivenAction::Plan => {
                    workflow.phase = GoalDrivenPhase::Build;
                    workflow.last_goal_hash = Some(after_hashes.goal_hash);
                    workflow.last_planned_at = Some(current_unix_timestamp());
                }
                GoalDrivenAction::Build => {
                    workflow.phase = GoalDrivenPhase::Paused;
                    workflow.last_built_at = Some(current_unix_timestamp());
                }
            }
            target_config.inflight = None;
        }

        self.persist_workflow_last_run(&mut target_config, &request.prompt_name, status)?;
        self.store.load_target(target)
    }

    pub(crate) async fn run_task_based_target_with_control<D>(
        &self,
        target: &str,
        prompt_name: Option<&str>,
        mut target_config: TargetConfig,
        control: RunControl,
        delegate: &mut D,
        mode: WorkflowRunMode,
    ) -> Result<TargetSummary>
    where
        D: RunDelegate,
    {
        if let Some(prompt_name) = prompt_name {
            return Err(anyhow!(
                "task_based targets select the workflow action internally; remove --prompt ('{prompt_name}')"
            ));
        }

        let target_dir = self.store.target_paths(target)?.dir;
        let hashes = task_based_hashes(&self.store, &target_dir)?;
        let workflow_status = task_based_workflow_status(&target_config, &hashes, &target_dir);
        let action = match mode {
            WorkflowRunMode::Smart => match workflow_status.run_advice {
                WorkflowRunAdvice::Rebase => TaskBasedAction::Rebase,
                WorkflowRunAdvice::Build => TaskBasedAction::Build,
                WorkflowRunAdvice::Choose => {
                    return self
                        .finish_paused_workflow_target(
                            target,
                            &mut target_config,
                            TASK_BASED_PAUSED_PROMPT,
                            format!(
                                "{target} has a stale task backlog derived from an older GOAL; choose B to build the current backlog, G to rebase the backlog, X to rebuild from scratch, or I to refine GOAL."
                            ),
                            delegate,
                        )
                        .await;
                }
                WorkflowRunAdvice::NoWork => {
                    return self
                        .finish_paused_workflow_target(
                            target,
                            &mut target_config,
                            TASK_BASED_PAUSED_PROMPT,
                            format!(
                                "{target} is paused; edit {GOAL_DRIVEN_GOAL_FILE} or {TASK_BASED_PROGRESS_FILE} to resume work, or press B to continue the current backlog."
                            ),
                            delegate,
                        )
                        .await;
                }
            },
            WorkflowRunMode::Build => {
                if workflow_status.derived_state == crate::WorkflowDerivedState::Missing {
                    return self
                        .finish_paused_workflow_target(
                            target,
                            &mut target_config,
                            TASK_BASED_PAUSED_PROMPT,
                            format!(
                                "{target} has no current task backlog; use G to create or rebase the backlog, or X to rebuild from scratch."
                            ),
                            delegate,
                        )
                        .await;
                }
                TaskBasedAction::Build
            }
            WorkflowRunMode::Rebase => TaskBasedAction::Rebase,
        };

        let request = match action {
            TaskBasedAction::Rebase => WorkflowRunRequest {
                prompt_name: TASK_BASED_REBASE_PROMPT.to_owned(),
                prompt_text: task_based_rebase_prompt(),
                completed_summary: format!("Backlog rebase complete for {}", target),
                max_iterations_summary: format!(
                    "Reached max iterations while rebasing task backlog {}",
                    target
                ),
                inflight_phase: GoalDrivenPhase::Plan,
            },
            TaskBasedAction::Build => WorkflowRunRequest {
                prompt_name: TASK_BASED_BUILD_PROMPT.to_owned(),
                prompt_text: task_based_build_prompt(),
                completed_summary: format!("Build complete for {}", target),
                max_iterations_summary: format!("Reached max iterations while building {}", target),
                inflight_phase: GoalDrivenPhase::Build,
            },
        };

        self.mark_workflow_inflight(
            &mut target_config,
            request.inflight_phase,
            &hashes.goal_hash,
            &hashes.content_hash,
        )?;
        let status = self
            .run_workflow_request(
                target,
                &target_dir,
                &target_config,
                &request,
                &control,
                delegate,
            )
            .await?;

        if status == LastRunStatus::Completed {
            let after_hashes = task_based_hashes(&self.store, &target_dir)?;
            let workflow = target_config
                .workflow
                .get_or_insert_with(GoalDrivenWorkflowState::default);
            workflow.last_content_hash = Some(after_hashes.content_hash);
            match action {
                TaskBasedAction::Rebase => {
                    workflow.phase = GoalDrivenPhase::Build;
                    workflow.last_goal_hash = Some(after_hashes.goal_hash);
                    workflow.last_planned_at = Some(current_unix_timestamp());
                }
                TaskBasedAction::Build => {
                    workflow.phase = GoalDrivenPhase::Paused;
                    workflow.last_built_at = Some(current_unix_timestamp());
                }
            }
            target_config.inflight = None;
        }

        self.persist_workflow_last_run(&mut target_config, &request.prompt_name, status)?;
        self.store.load_target(target)
    }

    fn mark_workflow_inflight(
        &self,
        target_config: &mut TargetConfig,
        phase: GoalDrivenPhase,
        goal_hash: &str,
        content_hash: &str,
    ) -> Result<()> {
        target_config.inflight = Some(GoalDrivenInflight {
            phase,
            goal_hash: goal_hash.to_owned(),
            content_hash: content_hash.to_owned(),
            started_at: current_unix_timestamp(),
        });
        self.store.write_target_config(target_config)
    }

    fn persist_workflow_last_run(
        &self,
        target_config: &mut TargetConfig,
        prompt_name: &str,
        status: LastRunStatus,
    ) -> Result<()> {
        target_config.last_prompt = Some(prompt_name.to_owned());
        target_config.last_run_status = status;
        self.store.write_target_config(target_config)
    }

    async fn finish_paused_workflow_target<D>(
        &self,
        target: &str,
        target_config: &mut TargetConfig,
        paused_prompt_name: &str,
        note: String,
        delegate: &mut D,
    ) -> Result<TargetSummary>
    where
        D: RunDelegate,
    {
        self.persist_workflow_last_run(
            target_config,
            paused_prompt_name,
            LastRunStatus::Completed,
        )?;
        delegate.on_event(RunEvent::Note(note)).await?;
        delegate
            .on_event(RunEvent::Finished {
                status: LastRunStatus::Completed,
                summary: format!("No run needed for {}", target),
            })
            .await?;
        self.store.load_target(target)
    }

    async fn run_workflow_request<D>(
        &self,
        target: &str,
        target_dir: &Utf8Path,
        target_config: &TargetConfig,
        request: &WorkflowRunRequest,
        control: &RunControl,
        delegate: &mut D,
    ) -> Result<LastRunStatus>
    where
        D: RunDelegate,
    {
        let prepared =
            self.prepare_inline_prompt_run(target_dir, &request.prompt_name, &request.prompt_text)?;
        let max_iterations = target_config
            .max_iterations
            .unwrap_or(self.config.max_iterations);
        self.run_prepared_prompt(
            &prepared,
            max_iterations,
            control,
            delegate,
            &request.completed_summary,
            &request.max_iterations_summary,
        )
        .await
        .inspect_err(|_| {
            let status = if control.is_cancelled() {
                LastRunStatus::Canceled
            } else {
                LastRunStatus::Failed
            };
            let _ = self
                .store
                .set_last_run(target, &request.prompt_name, status);
        })
    }
}
