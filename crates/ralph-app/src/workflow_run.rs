use anyhow::{Result, anyhow};
use camino::Utf8Path;
use ralph_core::{
    GoalDrivenInflight, GoalDrivenPhase, GoalDrivenWorkflowState, LastRunStatus, RunControl,
    TargetConfig, TargetSummary,
};
use ralph_runner::RunnerAdapter;

use crate::{
    RalphApp, RunDelegate, RunEvent,
    workflow::{
        GOAL_DRIVEN_BUILD_PROMPT, GOAL_DRIVEN_GOAL_FILE, GOAL_DRIVEN_PAUSED_PROMPT,
        GOAL_DRIVEN_PLAN_PROMPT, GoalDrivenAction, TASK_BASED_BUILD_PROMPT,
        TASK_BASED_PAUSED_PROMPT, TASK_BASED_PROGRESS_FILE, current_unix_timestamp,
        goal_driven_build_prompt, goal_driven_hashes, goal_driven_plan_prompt,
        select_goal_driven_action, select_task_based_build_needed, task_based_build_prompt,
        task_based_hashes,
    },
};

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
    pub(crate) async fn run_goal_driven_target_with_control<D>(
        &self,
        target: &str,
        prompt_name: Option<&str>,
        mut target_config: TargetConfig,
        control: RunControl,
        delegate: &mut D,
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
        let action = select_goal_driven_action(&target_config, &hashes);

        if action == GoalDrivenAction::Paused {
            return self
                .finish_paused_workflow_target(
                    target,
                    &mut target_config,
                    GOAL_DRIVEN_PAUSED_PROMPT,
                    format!(
                        "{target} is paused; edit {GOAL_DRIVEN_GOAL_FILE} to trigger re-planning."
                    ),
                    delegate,
                )
                .await;
        }

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
            GoalDrivenAction::Paused => unreachable!(),
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
            workflow.last_goal_hash = Some(after_hashes.goal_hash);
            workflow.last_content_hash = Some(after_hashes.content_hash);
            match action {
                GoalDrivenAction::Plan => {
                    workflow.phase = GoalDrivenPhase::Build;
                    workflow.last_planned_at = Some(current_unix_timestamp());
                }
                GoalDrivenAction::Build => {
                    workflow.phase = GoalDrivenPhase::Paused;
                    workflow.last_built_at = Some(current_unix_timestamp());
                }
                GoalDrivenAction::Paused => {}
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
    ) -> Result<TargetSummary>
    where
        D: RunDelegate,
    {
        if let Some(prompt_name) = prompt_name {
            return Err(anyhow!(
                "task_based targets select the build loop internally; remove --prompt ('{prompt_name}')"
            ));
        }

        let target_dir = self.store.target_paths(target)?.dir;
        let hashes = task_based_hashes(&self.store, &target_dir)?;
        let should_build = select_task_based_build_needed(&target_config, &hashes);

        if !should_build {
            return self
                .finish_paused_workflow_target(
                    target,
                    &mut target_config,
                    TASK_BASED_PAUSED_PROMPT,
                    format!(
                        "{target} is paused; edit {GOAL_DRIVEN_GOAL_FILE} or {TASK_BASED_PROGRESS_FILE} to resume work."
                    ),
                    delegate,
                )
                .await;
        }

        let request = WorkflowRunRequest {
            prompt_name: TASK_BASED_BUILD_PROMPT.to_owned(),
            prompt_text: task_based_build_prompt(),
            completed_summary: format!("Build complete for {}", target),
            max_iterations_summary: format!("Reached max iterations while building {}", target),
            inflight_phase: GoalDrivenPhase::Build,
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
            workflow.phase = GoalDrivenPhase::Paused;
            workflow.last_goal_hash = Some(after_hashes.goal_hash);
            workflow.last_content_hash = Some(after_hashes.content_hash);
            workflow.last_built_at = Some(current_unix_timestamp());
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
