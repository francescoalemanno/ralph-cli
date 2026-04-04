use std::{
    collections::BTreeMap,
    fs,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};
use ralph_core::{LastRunStatus, RunControl, TargetConfig, TargetEntrypoint, TargetSummary};
use ralph_runner::{InteractiveSessionInvocation, RunnerAdapter};

use crate::{
    RalphApp, RunDelegate, RunEvent,
    engine::{
        FlowEvalContext, FlowNodeSpec, FlowPauseAction, FlowStatusSummary, clear_inflight,
        ensure_runtime, load_flow, load_prompt_text, resolve_default_entrypoint,
        resolve_prompt_entrypoint, resolve_target_entrypoints, select_transition, set_inflight,
    },
    workflow::{
        PLAN_DRIVEN_BUILD_PROMPT, PLAN_DRIVEN_PLAN_PROMPT, TASK_DRIVEN_BUILD_PROMPT,
        TASK_DRIVEN_REBASE_PROMPT, plan_driven_hashes, task_driven_hashes,
    },
};

impl<R> RalphApp<R>
where
    R: RunnerAdapter,
{
    pub async fn run_target<D>(
        &self,
        target: &str,
        prompt_name: Option<&str>,
        delegate: &mut D,
    ) -> Result<TargetSummary>
    where
        D: RunDelegate,
    {
        self.run_target_with_options(target, prompt_name, None, None, RunControl::new(), delegate)
            .await
    }

    pub async fn run_prompt_file<D>(
        &self,
        prompt_path: &Utf8Path,
        delegate: &mut D,
    ) -> Result<LastRunStatus>
    where
        D: RunDelegate,
    {
        self.run_prompt_file_with_control(prompt_path, RunControl::new(), delegate)
            .await
    }

    pub async fn run_target_with_control<D>(
        &self,
        target: &str,
        prompt_name: Option<&str>,
        control: RunControl,
        delegate: &mut D,
    ) -> Result<TargetSummary>
    where
        D: RunDelegate,
    {
        self.run_target_with_options(target, prompt_name, None, None, control, delegate)
            .await
    }

    pub async fn run_target_with_options<D>(
        &self,
        target: &str,
        prompt_name: Option<&str>,
        entrypoint_name: Option<&str>,
        action_id: Option<&str>,
        control: RunControl,
        delegate: &mut D,
    ) -> Result<TargetSummary>
    where
        D: RunDelegate,
    {
        if prompt_name.is_some() && entrypoint_name.is_some() {
            return Err(anyhow!(
                "choose either --prompt or --entrypoint for target runs, not both"
            ));
        }

        let target_config = self.store.read_target_config(target)?;
        let target_summary = self.store.load_target(target)?;
        let entrypoints = resolve_target_entrypoints(&target_config, &target_summary);

        if !entrypoints.is_empty() {
            let entrypoint = if let Some(entrypoint_name) = entrypoint_name {
                entrypoints
                    .iter()
                    .find(|entrypoint| entrypoint.id() == entrypoint_name)
                    .cloned()
                    .ok_or_else(|| {
                        anyhow!(
                            "entrypoint '{}' does not exist for '{}'",
                            entrypoint_name,
                            target
                        )
                    })?
            } else if let Some(prompt_name) = prompt_name {
                resolve_prompt_entrypoint(&entrypoints, prompt_name)
                    .cloned()
                    .ok_or_else(|| {
                        anyhow!("prompt '{}' does not exist for '{}'", prompt_name, target)
                    })?
            } else {
                resolve_default_entrypoint(&target_config, &entrypoints)
                    .cloned()
                    .ok_or_else(|| anyhow!("target '{}' has no runnable entrypoint", target))?
            };

            return match entrypoint {
                TargetEntrypoint::Prompt { path, .. } => {
                    self.run_prompt_entrypoint(
                        target,
                        target_config,
                        &target_summary,
                        &path,
                        control,
                        delegate,
                    )
                    .await
                }
                TargetEntrypoint::Flow { .. } => {
                    self.run_flow_entrypoint(
                        target,
                        target_config,
                        &target_summary,
                        &entrypoint,
                        action_id,
                        control,
                        delegate,
                    )
                    .await
                }
            };
        }

        let prompt = self.select_prompt(&target_summary, prompt_name)?;
        let prepared = self.prepare_prompt_run(&prompt.path, &target_summary.dir)?;
        let max_iterations = self
            .store
            .read_target_config(target)?
            .max_iterations
            .unwrap_or(self.config.max_iterations);
        let status = self
            .run_prepared_prompt(
                &prepared,
                max_iterations,
                &control,
                delegate,
                &format!("Run complete for {}", target_summary.id),
                &format!("Reached max iterations for {}", target_summary.id),
            )
            .await
            .inspect_err(|_| {
                let status = if control.is_cancelled() {
                    LastRunStatus::Canceled
                } else {
                    LastRunStatus::Failed
                };
                let _ = self.store.set_last_run(target, &prompt.name, status);
            })?;

        self.store.set_last_run(target, &prompt.name, status)?;
        self.store.load_target(target)
    }

    pub async fn run_prompt_file_with_control<D>(
        &self,
        prompt_path: &Utf8Path,
        control: RunControl,
        delegate: &mut D,
    ) -> Result<LastRunStatus>
    where
        D: RunDelegate,
    {
        let target_dir = prompt_path.parent().ok_or_else(|| {
            anyhow!("prompt path '{prompt_path}' must have a parent directory for TARGET_DIR")
        })?;
        let prepared = self.prepare_prompt_run(prompt_path, target_dir)?;
        self.run_prepared_prompt(
            &prepared,
            self.config.max_iterations,
            &control,
            delegate,
            &format!("Run complete for {}", prompt_path),
            &format!("Reached max iterations for {}", prompt_path),
        )
        .await
    }

    async fn run_prompt_entrypoint<D>(
        &self,
        target: &str,
        _target_config: TargetConfig,
        target_summary: &TargetSummary,
        path: &str,
        control: RunControl,
        delegate: &mut D,
    ) -> Result<TargetSummary>
    where
        D: RunDelegate,
    {
        let prompt_path = if Utf8Path::new(path).is_absolute() {
            Utf8PathBuf::from(path)
        } else {
            target_summary.dir.join(path)
        };
        let prepared = self.prepare_prompt_run(&prompt_path, &target_summary.dir)?;
        let max_iterations = self
            .store
            .read_target_config(target)?
            .max_iterations
            .unwrap_or(self.config.max_iterations);
        let status = self
            .run_prepared_prompt(
                &prepared,
                max_iterations,
                &control,
                delegate,
                &format!("Run complete for {}", target_summary.id),
                &format!("Reached max iterations for {}", target_summary.id),
            )
            .await
            .inspect_err(|_| {
                let status = if control.is_cancelled() {
                    LastRunStatus::Canceled
                } else {
                    LastRunStatus::Failed
                };
                let _ =
                    self.store
                        .set_last_run(target, self.prepared_prompt_name(&prepared), status);
            })?;
        self.store
            .set_last_run(target, self.prepared_prompt_name(&prepared), status)?;
        self.store.load_target(target)
    }

    async fn run_flow_entrypoint<D>(
        &self,
        target: &str,
        mut target_config: TargetConfig,
        target_summary: &TargetSummary,
        entrypoint: &TargetEntrypoint,
        action_id: Option<&str>,
        control: RunControl,
        delegate: &mut D,
    ) -> Result<TargetSummary>
    where
        D: RunDelegate,
    {
        let loaded_flow = load_flow(&self.project_dir, &target_summary.dir, entrypoint)?;
        let mut current_node_id = if let Some(action_id) = action_id {
            let status = self.flow_status_summary(&target_config, target_summary)?;
            let pause = status
                .and_then(|status| status.pause)
                .ok_or_else(|| anyhow!("target '{}' is not waiting on a flow action", target))?;
            pause
                .actions
                .iter()
                .find(|action| action.id == action_id)
                .map(|action| action.goto.clone())
                .ok_or_else(|| {
                    anyhow!(
                        "flow action '{}' is not available for '{}'",
                        action_id,
                        target
                    )
                })?
        } else {
            loaded_flow.definition.start.clone()
        };
        let params = match entrypoint {
            TargetEntrypoint::Flow { params, .. } => params.clone(),
            TargetEntrypoint::Prompt { .. } => BTreeMap::new(),
        };
        let mut emitted_finished = false;
        let mut ran_prompt_like_step = false;
        let mut selected_action = action_id;
        let max_transitions = 64;

        for _ in 0..max_transitions {
            if control.is_cancelled() {
                self.persist_flow_failure(
                    &mut target_config,
                    &current_node_id,
                    LastRunStatus::Canceled,
                )?;
                return Err(anyhow!("operation canceled"));
            }

            {
                let runtime = ensure_runtime(&mut target_config, entrypoint.id());
                set_inflight(runtime, &current_node_id, current_unix_timestamp());
            }
            self.store.write_target_config(&target_config)?;

            let node = loaded_flow
                .definition
                .node(&current_node_id)
                .ok_or_else(|| anyhow!("flow node '{}' does not exist", current_node_id))?;

            match &node.spec {
                FlowNodeSpec::Decision { rules } => {
                    let next = {
                        let runtime = target_config.runtime.as_ref().expect("runtime must exist");
                        let context = FlowEvalContext {
                            target_dir: &target_summary.dir,
                            runtime,
                            selected_action,
                            last_status: None,
                        };
                        let rule = select_transition(rules, &context)?.ok_or_else(|| {
                            anyhow!("decision node '{}' has no matching transition", node.id)
                        })?;
                        if let Some(note) = &rule.note {
                            delegate.on_event(RunEvent::Note(note.clone())).await?;
                        }
                        rule.goto.clone()
                    };
                    {
                        let runtime = ensure_runtime(&mut target_config, entrypoint.id());
                        clear_inflight(runtime);
                        runtime.current_node = Some(node.id.clone());
                    }
                    self.store.write_target_config(&target_config)?;
                    current_node_id = next;
                    selected_action = None;
                }
                FlowNodeSpec::Pause {
                    message,
                    summary,
                    actions,
                } => {
                    let selected = selected_action.take();
                    if let Some(selected) = selected {
                        let next = actions
                            .iter()
                            .find(|action| action.id == selected)
                            .map(|action| action.goto.clone())
                            .ok_or_else(|| {
                                anyhow!(
                                    "flow action '{}' is not available on node '{}'",
                                    selected,
                                    node.id
                                )
                            })?;
                        {
                            let runtime = ensure_runtime(&mut target_config, entrypoint.id());
                            clear_inflight(runtime);
                            runtime.current_node = Some(node.id.clone());
                            runtime.last_note = message.clone();
                        }
                        self.store.write_target_config(&target_config)?;
                        current_node_id = next;
                        continue;
                    }

                    {
                        let runtime = ensure_runtime(&mut target_config, entrypoint.id());
                        clear_inflight(runtime);
                        runtime.current_node = Some(node.id.clone());
                        runtime.last_note = message.clone();
                    }
                    if !ran_prompt_like_step {
                        target_config.last_prompt = Some(node.id.clone());
                    }
                    target_config.last_run_status = LastRunStatus::Completed;
                    self.store.write_target_config(&target_config)?;

                    let note = render_pause_message(message.as_deref(), actions);
                    if !note.is_empty() {
                        delegate.on_event(RunEvent::Note(note)).await?;
                    }
                    if !emitted_finished {
                        delegate
                            .on_event(RunEvent::Finished {
                                status: LastRunStatus::Completed,
                                summary: summary
                                    .clone()
                                    .unwrap_or_else(|| format!("Paused at {}", node.id)),
                            })
                            .await?;
                    }
                    return self.store.load_target(target);
                }
                FlowNodeSpec::Finish { summary, status } => {
                    {
                        let runtime = ensure_runtime(&mut target_config, entrypoint.id());
                        clear_inflight(runtime);
                        runtime.current_node = Some(node.id.clone());
                    }
                    if !ran_prompt_like_step {
                        target_config.last_prompt = Some(node.id.clone());
                    }
                    target_config.last_run_status = status.unwrap_or(LastRunStatus::Completed);
                    self.store.write_target_config(&target_config)?;
                    if !emitted_finished {
                        delegate
                            .on_event(RunEvent::Finished {
                                status: status.unwrap_or(LastRunStatus::Completed),
                                summary: summary
                                    .clone()
                                    .unwrap_or_else(|| format!("Flow complete for {}", target)),
                            })
                            .await?;
                    }
                    return self.store.load_target(target);
                }
                FlowNodeSpec::Prompt {
                    prompt,
                    max_iterations,
                    rules,
                    on_completed,
                    on_max_iterations,
                    on_failed,
                    on_canceled,
                } => {
                    let prompt_text =
                        load_prompt_text(&self.project_dir, &target_summary.dir, prompt, &params)?;
                    let prepared = self.prepare_inline_prompt_run(
                        &target_summary.dir,
                        &node.id,
                        &prompt_text,
                    )?;
                    let max_iterations = max_iterations.unwrap_or(
                        target_config
                            .max_iterations
                            .unwrap_or(self.config.max_iterations),
                    );
                    let status = self
                        .run_prepared_prompt(
                            &prepared,
                            max_iterations,
                            &control,
                            delegate,
                            &format!("Step '{}' complete for {}", node.id, target),
                            &format!(
                                "Reached max iterations at step '{}' for {}",
                                node.id, target
                            ),
                        )
                        .await
                        .inspect_err(|_| {
                            let status = if control.is_cancelled() {
                                LastRunStatus::Canceled
                            } else {
                                LastRunStatus::Failed
                            };
                            let _ = self.persist_flow_failure(
                                &mut target_config.clone(),
                                &node.id,
                                status,
                            );
                        })?;
                    emitted_finished = true;
                    ran_prompt_like_step = true;
                    {
                        let runtime = ensure_runtime(&mut target_config, entrypoint.id());
                        clear_inflight(runtime);
                        runtime.current_node = Some(node.id.clone());
                    }
                    target_config.last_prompt = Some(node.id.clone());
                    target_config.last_run_status = status;
                    self.sync_legacy_workflow_state_after_prompt(
                        &mut target_config,
                        &target_summary.dir,
                        &node.id,
                        status,
                    )?;
                    self.store.write_target_config(&target_config)?;
                    current_node_id = self.next_node_for_status(
                        &target_summary.dir,
                        &target_config,
                        rules,
                        selected_action,
                        status,
                        on_completed.as_deref(),
                        on_max_iterations.as_deref(),
                        on_failed.as_deref(),
                        on_canceled.as_deref(),
                    )?;
                    selected_action = None;
                }
                FlowNodeSpec::Interactive {
                    prompt,
                    rules,
                    on_completed,
                    on_failed,
                } => {
                    let prompt_text =
                        load_prompt_text(&self.project_dir, &target_summary.dir, prompt, &params)?;
                    let config = self.interactive_runner_config_for(&control)?;
                    let goal_path = loaded_flow
                        .edit_path
                        .clone()
                        .unwrap_or_else(|| target_summary.dir.join("GOAL.md"));
                    let outcome = self.runner.run_interactive_session(
                        &config,
                        &InteractiveSessionInvocation {
                            session_name: node.id.clone(),
                            initial_prompt: prompt_text,
                            project_dir: self.project_dir.clone(),
                            target_dir: target_summary.dir.clone(),
                            goal_path,
                        },
                    )?;
                    let status = if outcome.exit_code == Some(0) || outcome.exit_code.is_none() {
                        LastRunStatus::Completed
                    } else {
                        LastRunStatus::Failed
                    };
                    ran_prompt_like_step = true;
                    {
                        let runtime = ensure_runtime(&mut target_config, entrypoint.id());
                        clear_inflight(runtime);
                        runtime.current_node = Some(node.id.clone());
                    }
                    target_config.last_prompt = Some(node.id.clone());
                    target_config.last_run_status = status;
                    self.store.write_target_config(&target_config)?;
                    current_node_id = self.next_node_for_status(
                        &target_summary.dir,
                        &target_config,
                        rules,
                        selected_action,
                        status,
                        on_completed.as_deref(),
                        None,
                        on_failed.as_deref(),
                        None,
                    )?;
                    selected_action = None;
                }
                FlowNodeSpec::Action {
                    action,
                    args,
                    on_success,
                    on_error,
                } => {
                    let action_result = self.execute_flow_action(
                        &target_summary.dir,
                        &mut target_config,
                        args,
                        action,
                    );
                    {
                        let runtime = ensure_runtime(&mut target_config, entrypoint.id());
                        clear_inflight(runtime);
                        runtime.current_node = Some(node.id.clone());
                    }
                    match action_result {
                        Ok(()) => {
                            target_config.last_run_status = LastRunStatus::Completed;
                            self.store.write_target_config(&target_config)?;
                            current_node_id = on_success.clone().ok_or_else(|| {
                                anyhow!("action node '{}' is missing on_success", node.id)
                            })?;
                        }
                        Err(error) => {
                            if !ran_prompt_like_step {
                                target_config.last_prompt = Some(node.id.clone());
                            }
                            target_config.last_run_status = LastRunStatus::Failed;
                            self.store.write_target_config(&target_config)?;
                            if let Some(next) = on_error {
                                delegate
                                    .on_event(RunEvent::Note(format!(
                                        "action '{}' failed at node '{}': {error:#}",
                                        action, node.id
                                    )))
                                    .await?;
                                current_node_id = next.clone();
                            } else {
                                return Err(error);
                            }
                        }
                    }
                    selected_action = None;
                }
            }
        }

        Err(anyhow!(
            "flow '{}' exceeded the maximum number of transitions in one run",
            entrypoint.id()
        ))
    }

    fn next_node_for_status(
        &self,
        target_dir: &Utf8Path,
        target_config: &TargetConfig,
        rules: &[crate::engine::FlowTransitionRule],
        selected_action: Option<&str>,
        status: LastRunStatus,
        on_completed: Option<&str>,
        on_max_iterations: Option<&str>,
        on_failed: Option<&str>,
        on_canceled: Option<&str>,
    ) -> Result<String> {
        if let Some(runtime) = target_config.runtime.as_ref() {
            let context = FlowEvalContext {
                target_dir,
                runtime,
                selected_action,
                last_status: Some(status),
            };
            if let Some(rule) = select_transition(rules, &context)? {
                return Ok(rule.goto.clone());
            }
        }

        match status {
            LastRunStatus::Completed => on_completed
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("step completed without a follow-up transition")),
            LastRunStatus::MaxIterations => on_max_iterations.map(str::to_owned).ok_or_else(|| {
                anyhow!("step reached max iterations without a follow-up transition")
            }),
            LastRunStatus::Failed => on_failed
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("step failed without a follow-up transition")),
            LastRunStatus::Canceled => on_canceled
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("step was canceled without a follow-up transition")),
            LastRunStatus::NeverRun => Err(anyhow!("cannot transition from never_run status")),
        }
    }

    fn execute_flow_action(
        &self,
        target_dir: &Utf8Path,
        target_config: &mut TargetConfig,
        args: &toml::Table,
        action: &str,
    ) -> Result<()> {
        match action {
            "archive_paths" => execute_archive_paths(target_dir, args),
            "set_path_hash_var" => {
                let key = table_string(args, "key")?;
                let path = table_string(args, "path")?;
                let value = hash_file_or_missing(&target_dir.join(path))?;
                let runtime = target_config.runtime.get_or_insert_with(Default::default);
                match value {
                    Some(value) => {
                        runtime.vars.insert(key, value);
                    }
                    None => {
                        runtime.vars.remove(&key);
                    }
                }
                Ok(())
            }
            "set_var" => {
                let key = table_string(args, "key")?;
                let value = table_string(args, "value")?;
                let runtime = target_config.runtime.get_or_insert_with(Default::default);
                runtime.vars.insert(key, value);
                Ok(())
            }
            "clear_var" => {
                let key = table_string(args, "key")?;
                let runtime = target_config.runtime.get_or_insert_with(Default::default);
                runtime.vars.remove(&key);
                Ok(())
            }
            other => Err(anyhow!("unsupported flow action '{}'", other)),
        }
    }

    fn persist_flow_failure(
        &self,
        target_config: &mut TargetConfig,
        node_id: &str,
        status: LastRunStatus,
    ) -> Result<()> {
        let runtime = target_config.runtime.get_or_insert_with(Default::default);
        clear_inflight(runtime);
        runtime.current_node = Some(node_id.to_owned());
        target_config.last_prompt = Some(node_id.to_owned());
        target_config.last_run_status = status;
        self.store.write_target_config(target_config)
    }

    pub(crate) fn flow_status_summary(
        &self,
        target_config: &TargetConfig,
        target_summary: &TargetSummary,
    ) -> Result<Option<FlowStatusSummary>> {
        crate::engine::load_flow_status(
            &self.project_dir,
            &target_summary.dir,
            target_config,
            target_summary,
        )
    }
}

fn render_pause_message(message: Option<&str>, actions: &[FlowPauseAction]) -> String {
    let mut lines = Vec::new();
    if let Some(message) = message {
        lines.push(message.to_owned());
    }
    if !actions.is_empty() {
        let actions_text = actions
            .iter()
            .map(|action| match &action.shortcut {
                Some(shortcut) => format!("{} ({})", action.id, shortcut),
                None => action.id.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!("Available actions: {actions_text}"));
    }
    lines.join("\n")
}

fn execute_archive_paths(target_dir: &Utf8Path, args: &toml::Table) -> Result<()> {
    let files = table_string_list(args, "files")?;
    let dirs = table_string_list(args, "dirs")?;
    let archive_root = table_string(args, "archive_root")?;
    let prefix = table_string(args, "prefix")?;
    let archive_dir = target_dir
        .join(archive_root)
        .join(format!("{prefix}-{}", current_unix_timestamp_millis()));
    let mut archived_any = false;

    for file in files {
        if file.trim().is_empty() {
            continue;
        }
        let path = target_dir.join(file);
        if path.exists() {
            if !archived_any {
                fs::create_dir_all(&archive_dir)
                    .with_context(|| format!("failed to create {}", archive_dir))?;
                archived_any = true;
            }
            fs::rename(
                &path,
                archive_dir.join(path.file_name().unwrap_or_default()),
            )
            .with_context(|| format!("failed to archive {}", path))?;
        }
    }

    for dir in dirs {
        if dir.trim().is_empty() {
            continue;
        }
        let path = target_dir.join(dir);
        if path.exists() {
            if !archived_any {
                fs::create_dir_all(&archive_dir)
                    .with_context(|| format!("failed to create {}", archive_dir))?;
                archived_any = true;
            }
            fs::rename(
                &path,
                archive_dir.join(path.file_name().unwrap_or_default()),
            )
            .with_context(|| format!("failed to archive {}", path))?;
        }
    }

    Ok(())
}

fn table_string(args: &toml::Table, key: &str) -> Result<String> {
    args.get(key)
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("flow action arg '{}' must be a string", key))
}

fn table_string_list(args: &toml::Table, key: &str) -> Result<Vec<String>> {
    let Some(value) = args.get(key) else {
        return Ok(Vec::new());
    };
    let array = value
        .as_array()
        .ok_or_else(|| anyhow!("flow action arg '{}' must be an array", key))?;
    array
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("flow action arg '{}' must contain only strings", key))
        })
        .collect()
}

fn hash_file_or_missing(path: &Utf8Path) -> Result<Option<String>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(bytes);
            Ok(Some(format!("sha256:{:x}", hasher.finalize())))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path)),
    }
}

fn current_unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn current_unix_timestamp_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

impl<R> RalphApp<R>
where
    R: RunnerAdapter,
{
    fn sync_legacy_workflow_state_after_prompt(
        &self,
        target_config: &mut TargetConfig,
        target_dir: &Utf8Path,
        node_id: &str,
        status: LastRunStatus,
    ) -> Result<()> {
        if status != LastRunStatus::Completed {
            return Ok(());
        }

        match node_id {
            PLAN_DRIVEN_PLAN_PROMPT => {
                let hashes = plan_driven_hashes(&self.store, target_dir)?;
                let workflow = target_config
                    .workflow
                    .get_or_insert_with(ralph_core::PlanDrivenWorkflowState::default);
                workflow.phase = ralph_core::PlanDrivenPhase::Build;
                workflow.last_goal_hash = Some(hashes.goal_hash);
                workflow.last_content_hash = Some(hashes.content_hash);
                workflow.last_planned_at = Some(current_unix_timestamp());
                target_config.inflight = None;
            }
            PLAN_DRIVEN_BUILD_PROMPT => {
                let hashes = plan_driven_hashes(&self.store, target_dir)?;
                let workflow = target_config
                    .workflow
                    .get_or_insert_with(ralph_core::PlanDrivenWorkflowState::default);
                workflow.phase = ralph_core::PlanDrivenPhase::Paused;
                workflow.last_content_hash = Some(hashes.content_hash);
                workflow.last_built_at = Some(current_unix_timestamp());
                target_config.inflight = None;
            }
            TASK_DRIVEN_REBASE_PROMPT => {
                let hashes = task_driven_hashes(&self.store, target_dir)?;
                let workflow = target_config
                    .workflow
                    .get_or_insert_with(ralph_core::PlanDrivenWorkflowState::default);
                workflow.phase = ralph_core::PlanDrivenPhase::Build;
                workflow.last_goal_hash = Some(hashes.goal_hash);
                workflow.last_content_hash = Some(hashes.content_hash);
                workflow.last_planned_at = Some(current_unix_timestamp());
                target_config.inflight = None;
            }
            TASK_DRIVEN_BUILD_PROMPT => {
                let hashes = task_driven_hashes(&self.store, target_dir)?;
                let workflow = target_config
                    .workflow
                    .get_or_insert_with(ralph_core::PlanDrivenWorkflowState::default);
                workflow.phase = ralph_core::PlanDrivenPhase::Paused;
                workflow.last_content_hash = Some(hashes.content_hash);
                workflow.last_built_at = Some(current_unix_timestamp());
                target_config.inflight = None;
            }
            _ => {}
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use anyhow::Result;
    use async_trait::async_trait;
    use camino::Utf8PathBuf;
    use ralph_core::{
        AppConfig, PlanDrivenPhase, RunControl, RunnerInvocation, RunnerResult, ScaffoldId,
    };
    use ralph_runner::{RunnerAdapter, RunnerStreamEvent};
    use tokio::sync::mpsc::UnboundedSender;

    use crate::{
        RalphApp, RunDelegate, RunEvent,
        workflow::{
            PLAN_DRIVEN_BUILD_PROMPT, PLAN_DRIVEN_PAUSED_PROMPT, PLAN_DRIVEN_PLAN_PROMPT,
            TASK_DRIVEN_BUILD_PROMPT, TASK_DRIVEN_PAUSED_PROMPT, TASK_DRIVEN_REBASE_PROMPT,
        },
    };

    #[derive(Clone)]
    struct ScriptedRunner {
        output: String,
        exit_code: i32,
    }

    #[derive(Clone)]
    struct PlanDrivenRunner {
        seen_prompt_names: Arc<Mutex<Vec<String>>>,
    }

    #[derive(Clone)]
    struct TaskDrivenRunner {
        seen_prompt_names: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl RunnerAdapter for ScriptedRunner {
        async fn run(
            &self,
            _config: &ralph_core::RunnerConfig,
            _invocation: RunnerInvocation,
            _control: &RunControl,
            stream: Option<UnboundedSender<RunnerStreamEvent>>,
        ) -> Result<RunnerResult> {
            if let Some(stream) = stream {
                let _ = stream.send(RunnerStreamEvent::Output(self.output.clone()));
            }
            Ok(RunnerResult {
                output: self.output.clone(),
                exit_code: self.exit_code,
            })
        }
    }

    #[async_trait]
    impl RunnerAdapter for PlanDrivenRunner {
        async fn run(
            &self,
            _config: &ralph_core::RunnerConfig,
            invocation: RunnerInvocation,
            _control: &RunControl,
            _stream: Option<UnboundedSender<RunnerStreamEvent>>,
        ) -> Result<RunnerResult> {
            self.seen_prompt_names
                .lock()
                .unwrap()
                .push(invocation.prompt_name.clone());

            let plan_path = invocation.target_dir.join("plan.toml");
            let contents = match invocation.prompt_name.as_str() {
                PLAN_DRIVEN_PLAN_PROMPT => {
                    "version = 1\n\n[[items]]\ncategory = \"functional\"\ndescription = \"Ship the feature\"\nsteps = [\"Implement it\", \"Verify it\"]\ncompleted = false\n".to_owned()
                }
                PLAN_DRIVEN_BUILD_PROMPT => {
                    "version = 1\n\n[[items]]\ncategory = \"functional\"\ndescription = \"Ship the feature\"\nsteps = [\"Implement it\", \"Verify it\"]\ncompleted = true\n".to_owned()
                }
                other => panic!("unexpected plan-driven prompt {other}"),
            };
            std::fs::write(plan_path, contents).unwrap();

            Ok(RunnerResult {
                output: String::new(),
                exit_code: 0,
            })
        }
    }

    #[async_trait]
    impl RunnerAdapter for TaskDrivenRunner {
        async fn run(
            &self,
            _config: &ralph_core::RunnerConfig,
            invocation: RunnerInvocation,
            _control: &RunControl,
            _stream: Option<UnboundedSender<RunnerStreamEvent>>,
        ) -> Result<RunnerResult> {
            self.seen_prompt_names
                .lock()
                .unwrap()
                .push(invocation.prompt_name.clone());

            let progress_path = invocation.target_dir.join("progress.toml");
            let contents = match invocation.prompt_name.as_str() {
                TASK_DRIVEN_REBASE_PROMPT => {
                    "version = 1\n\n[[items]]\ndescription = \"Ship the feature\"\nsteps = [\"Implement it\", \"Verify it\"]\ncompleted = false\n".to_owned()
                }
                TASK_DRIVEN_BUILD_PROMPT => {
                    "version = 1\n\n[[items]]\ndescription = \"Ship the feature\"\nsteps = [\"Implement it\", \"Verify it\"]\ncompleted = true\n".to_owned()
                }
                other => panic!("unexpected task-driven prompt {other}"),
            };
            std::fs::write(progress_path, contents).unwrap();

            Ok(RunnerResult {
                output: String::new(),
                exit_code: 0,
            })
        }
    }

    #[derive(Default)]
    struct TestDelegate;

    #[async_trait]
    impl RunDelegate for TestDelegate {
        async fn on_event(&mut self, _event: RunEvent) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn plan_driven_targets_plan_then_build_and_keep_building_even_if_goal_changes() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let seen_prompt_names = Arc::new(Mutex::new(Vec::new()));
        let app = RalphApp::new(
            project_dir.clone(),
            AppConfig::default(),
            PlanDrivenRunner {
                seen_prompt_names: seen_prompt_names.clone(),
            },
        );
        app.create_target("demo", Some(ScaffoldId::PlanDriven))
            .unwrap();

        let mut delegate = TestDelegate;
        let summary = app.run_target("demo", None, &mut delegate).await.unwrap();
        assert_eq!(
            summary.last_prompt.as_deref(),
            Some(PLAN_DRIVEN_PLAN_PROMPT)
        );
        assert_eq!(
            summary.last_run_status,
            ralph_core::LastRunStatus::Completed
        );
        assert_eq!(
            app.store
                .read_target_config("demo")
                .unwrap()
                .workflow
                .as_ref()
                .map(|workflow| workflow.phase),
            Some(PlanDrivenPhase::Build)
        );

        let summary = app.run_target("demo", None, &mut delegate).await.unwrap();
        assert_eq!(
            summary.last_prompt.as_deref(),
            Some(PLAN_DRIVEN_BUILD_PROMPT)
        );
        assert_eq!(
            app.store
                .read_target_config("demo")
                .unwrap()
                .workflow
                .as_ref()
                .map(|workflow| workflow.phase),
            Some(PlanDrivenPhase::Paused)
        );

        let summary = app.run_target("demo", None, &mut delegate).await.unwrap();
        assert_eq!(
            summary.last_prompt.as_deref(),
            Some(PLAN_DRIVEN_BUILD_PROMPT)
        );
        assert_eq!(
            *seen_prompt_names.lock().unwrap(),
            vec![
                PLAN_DRIVEN_PLAN_PROMPT.to_owned(),
                PLAN_DRIVEN_PLAN_PROMPT.to_owned(),
                PLAN_DRIVEN_BUILD_PROMPT.to_owned(),
                PLAN_DRIVEN_BUILD_PROMPT.to_owned()
            ]
        );

        std::fs::write(
            project_dir.join(".ralph/targets/demo/GOAL.md"),
            "# Goal\n\nChanged\n",
        )
        .unwrap();
        let summary = app.run_target("demo", None, &mut delegate).await.unwrap();
        assert_eq!(
            summary.last_prompt.as_deref(),
            Some(PLAN_DRIVEN_PAUSED_PROMPT)
        );
        assert_eq!(
            *seen_prompt_names.lock().unwrap(),
            vec![
                PLAN_DRIVEN_PLAN_PROMPT.to_owned(),
                PLAN_DRIVEN_PLAN_PROMPT.to_owned(),
                PLAN_DRIVEN_BUILD_PROMPT.to_owned(),
                PLAN_DRIVEN_BUILD_PROMPT.to_owned()
            ]
        );
    }

    #[tokio::test]
    async fn task_driven_targets_rebase_then_build_then_require_choice_on_goal_change() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let seen_prompt_names = Arc::new(Mutex::new(Vec::new()));
        let app = RalphApp::new(
            project_dir.clone(),
            AppConfig::default(),
            TaskDrivenRunner {
                seen_prompt_names: seen_prompt_names.clone(),
            },
        );
        app.create_target("demo", Some(ScaffoldId::TaskDriven))
            .unwrap();

        let mut delegate = TestDelegate;
        let summary = app.run_target("demo", None, &mut delegate).await.unwrap();
        assert_eq!(
            summary.last_prompt.as_deref(),
            Some(TASK_DRIVEN_REBASE_PROMPT)
        );
        assert_eq!(
            summary.last_run_status,
            ralph_core::LastRunStatus::Completed
        );
        assert_eq!(
            app.store
                .read_target_config("demo")
                .unwrap()
                .workflow
                .as_ref()
                .map(|workflow| workflow.phase),
            Some(PlanDrivenPhase::Build)
        );

        let summary = app.run_target("demo", None, &mut delegate).await.unwrap();
        assert_eq!(
            summary.last_prompt.as_deref(),
            Some(TASK_DRIVEN_BUILD_PROMPT)
        );
        assert_eq!(
            *seen_prompt_names.lock().unwrap(),
            vec![
                TASK_DRIVEN_REBASE_PROMPT.to_owned(),
                TASK_DRIVEN_REBASE_PROMPT.to_owned(),
                TASK_DRIVEN_BUILD_PROMPT.to_owned()
            ]
        );

        std::fs::write(
            project_dir.join(".ralph/targets/demo/GOAL.md"),
            "# Goal\n\nChanged\n",
        )
        .unwrap();
        let summary = app.run_target("demo", None, &mut delegate).await.unwrap();
        assert_eq!(
            summary.last_prompt.as_deref(),
            Some(TASK_DRIVEN_PAUSED_PROMPT)
        );
        assert_eq!(
            *seen_prompt_names.lock().unwrap(),
            vec![
                TASK_DRIVEN_REBASE_PROMPT.to_owned(),
                TASK_DRIVEN_REBASE_PROMPT.to_owned(),
                TASK_DRIVEN_BUILD_PROMPT.to_owned(),
            ]
        );
    }

    #[tokio::test]
    async fn task_driven_failures_persist_last_run_status() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let app = RalphApp::new(
            project_dir.clone(),
            AppConfig::default(),
            ScriptedRunner {
                output: "runner failed".to_owned(),
                exit_code: 1,
            },
        );
        app.create_target("demo", Some(ScaffoldId::TaskDriven))
            .unwrap();

        let mut delegate = TestDelegate;
        let error = app
            .run_target("demo", None, &mut delegate)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("runner exited with code 1"));
        let config = app.store.read_target_config("demo").unwrap();
        assert_eq!(
            config.last_prompt.as_deref(),
            Some(TASK_DRIVEN_REBASE_PROMPT)
        );
        assert_eq!(config.last_run_status, ralph_core::LastRunStatus::Failed);
    }
}
