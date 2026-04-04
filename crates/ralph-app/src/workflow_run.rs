use anyhow::{Result, anyhow};
use camino::Utf8Path;
use ralph_core::{
    LastRunStatus, PlanDrivenInflight, PlanDrivenPhase, PlanDrivenWorkflowState, RunControl,
    TargetConfig, TargetSummary, WorkflowMode,
};
use ralph_runner::RunnerAdapter;

use crate::{
    RalphApp, RunDelegate, RunEvent, WorkflowAction, WorkflowRunAdvice, WorkflowStatus,
    workflow::{
        PLAN_DRIVEN_BUILD_PROMPT, PLAN_DRIVEN_GOAL_FILE, PLAN_DRIVEN_PAUSED_PROMPT,
        PLAN_DRIVEN_PLAN_PROMPT, PlanDrivenAction, TASK_DRIVEN_BUILD_PROMPT,
        TASK_DRIVEN_PAUSED_PROMPT, TASK_DRIVEN_PROGRESS_FILE, TASK_DRIVEN_REBASE_PROMPT,
        TaskDrivenAction, current_unix_timestamp, plan_driven_build_prompt, plan_driven_hashes,
        plan_driven_plan_prompt, plan_driven_workflow_status, task_driven_build_prompt,
        task_driven_hashes, task_driven_rebase_prompt, task_driven_workflow_status,
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
    inflight_phase: PlanDrivenPhase,
}

impl<R> RalphApp<R>
where
    R: RunnerAdapter,
{
    pub fn workflow_status(&self, target: &str) -> Result<Option<WorkflowStatus>> {
        let target_config = self.store.read_target_config(target)?;
        let target_dir = self.store.target_paths(target)?.dir;
        match target_config.mode {
            Some(WorkflowMode::PlanDriven) => Ok(Some(plan_driven_workflow_status(
                &target_config,
                &plan_driven_hashes(&self.store, &target_dir)?,
                &target_dir,
            ))),
            Some(WorkflowMode::TaskDriven) => Ok(Some(task_driven_workflow_status(
                &target_config,
                &task_driven_hashes(&self.store, &target_dir)?,
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
            Some(WorkflowMode::PlanDriven) => {
                self.run_plan_driven_target_with_control(
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
            Some(WorkflowMode::TaskDriven) => {
                self.run_task_driven_target_with_control(
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

    pub(crate) async fn run_plan_driven_target_with_control<D>(
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
                "plan-driven targets select plan/build internally; remove --prompt ('{prompt_name}')"
            ));
        }

        let target_dir = self.store.target_paths(target)?.dir;
        let hashes = plan_driven_hashes(&self.store, &target_dir)?;
        let workflow_status = plan_driven_workflow_status(&target_config, &hashes, &target_dir);
        let action = match mode {
            WorkflowRunMode::Smart => match workflow_status.run_advice {
                WorkflowRunAdvice::Rebase => PlanDrivenAction::Plan,
                WorkflowRunAdvice::Build => PlanDrivenAction::Build,
                WorkflowRunAdvice::Choose => {
                    return self
                        .finish_paused_workflow_target(
                            target,
                            &mut target_config,
                            PLAN_DRIVEN_PAUSED_PROMPT,
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
                            PLAN_DRIVEN_PAUSED_PROMPT,
                            format!(
                                "{target} has no current plan; use G to create or rebase the plan, or X to rebuild from scratch."
                            ),
                            delegate,
                        )
                        .await;
                }
                PlanDrivenAction::Build
            }
            WorkflowRunMode::Rebase => PlanDrivenAction::Plan,
        };

        let request = match action {
            PlanDrivenAction::Plan => WorkflowRunRequest {
                prompt_name: PLAN_DRIVEN_PLAN_PROMPT.to_owned(),
                prompt_text: plan_driven_plan_prompt(),
                completed_summary: format!("Planning complete for {}", target),
                max_iterations_summary: format!("Reached max iterations while planning {}", target),
                inflight_phase: PlanDrivenPhase::Plan,
            },
            PlanDrivenAction::Build => WorkflowRunRequest {
                prompt_name: PLAN_DRIVEN_BUILD_PROMPT.to_owned(),
                prompt_text: plan_driven_build_prompt(),
                completed_summary: format!("Build complete for {}", target),
                max_iterations_summary: format!("Reached max iterations while building {}", target),
                inflight_phase: PlanDrivenPhase::Build,
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
            let after_hashes = plan_driven_hashes(&self.store, &target_dir)?;
            let workflow = target_config
                .workflow
                .get_or_insert_with(PlanDrivenWorkflowState::default);
            workflow.last_content_hash = Some(after_hashes.content_hash);
            match action {
                PlanDrivenAction::Plan => {
                    workflow.phase = PlanDrivenPhase::Build;
                    workflow.last_goal_hash = Some(after_hashes.goal_hash);
                    workflow.last_planned_at = Some(current_unix_timestamp());
                }
                PlanDrivenAction::Build => {
                    workflow.phase = PlanDrivenPhase::Paused;
                    workflow.last_built_at = Some(current_unix_timestamp());
                }
            }
            target_config.inflight = None;
        }

        self.persist_workflow_last_run(&mut target_config, &request.prompt_name, status)?;
        self.store.load_target(target)
    }

    pub(crate) async fn run_task_driven_target_with_control<D>(
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
                "task-driven targets select the workflow action internally; remove --prompt ('{prompt_name}')"
            ));
        }

        let target_dir = self.store.target_paths(target)?.dir;
        let hashes = task_driven_hashes(&self.store, &target_dir)?;
        let workflow_status = task_driven_workflow_status(&target_config, &hashes, &target_dir);
        let action = match mode {
            WorkflowRunMode::Smart => match workflow_status.run_advice {
                WorkflowRunAdvice::Rebase => TaskDrivenAction::Rebase,
                WorkflowRunAdvice::Build => TaskDrivenAction::Build,
                WorkflowRunAdvice::Choose => {
                    return self
                        .finish_paused_workflow_target(
                            target,
                            &mut target_config,
                            TASK_DRIVEN_PAUSED_PROMPT,
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
                            TASK_DRIVEN_PAUSED_PROMPT,
                            format!(
                                "{target} is paused; edit {PLAN_DRIVEN_GOAL_FILE} or {TASK_DRIVEN_PROGRESS_FILE} to resume work, or press B to continue the current backlog."
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
                            TASK_DRIVEN_PAUSED_PROMPT,
                            format!(
                                "{target} has no current task backlog; use G to create or rebase the backlog, or X to rebuild from scratch."
                            ),
                            delegate,
                        )
                        .await;
                }
                TaskDrivenAction::Build
            }
            WorkflowRunMode::Rebase => TaskDrivenAction::Rebase,
        };

        let request = match action {
            TaskDrivenAction::Rebase => WorkflowRunRequest {
                prompt_name: TASK_DRIVEN_REBASE_PROMPT.to_owned(),
                prompt_text: task_driven_rebase_prompt(),
                completed_summary: format!("Backlog rebase complete for {}", target),
                max_iterations_summary: format!(
                    "Reached max iterations while rebasing task backlog {}",
                    target
                ),
                inflight_phase: PlanDrivenPhase::Plan,
            },
            TaskDrivenAction::Build => WorkflowRunRequest {
                prompt_name: TASK_DRIVEN_BUILD_PROMPT.to_owned(),
                prompt_text: task_driven_build_prompt(),
                completed_summary: format!("Build complete for {}", target),
                max_iterations_summary: format!("Reached max iterations while building {}", target),
                inflight_phase: PlanDrivenPhase::Build,
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
            let after_hashes = task_driven_hashes(&self.store, &target_dir)?;
            let workflow = target_config
                .workflow
                .get_or_insert_with(PlanDrivenWorkflowState::default);
            workflow.last_content_hash = Some(after_hashes.content_hash);
            match action {
                TaskDrivenAction::Rebase => {
                    workflow.phase = PlanDrivenPhase::Build;
                    workflow.last_goal_hash = Some(after_hashes.goal_hash);
                    workflow.last_planned_at = Some(current_unix_timestamp());
                }
                TaskDrivenAction::Build => {
                    workflow.phase = PlanDrivenPhase::Paused;
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
        phase: PlanDrivenPhase,
        goal_hash: &str,
        content_hash: &str,
    ) -> Result<()> {
        target_config.inflight = Some(PlanDrivenInflight {
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
