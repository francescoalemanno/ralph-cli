use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};
use ralph_core::{
    AppConfig, FlowRuntimeInflight, FlowRuntimeState, LastRunStatus, TargetConfig,
    TargetEntrypoint, TargetSummary,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

pub(crate) const DEFAULT_FLOW_ENTRYPOINT_ID: &str = "main";

#[derive(Debug, Clone)]
pub(crate) struct LoadedFlow {
    pub(crate) artifact_ref: String,
    pub(crate) definition: FlowDefinition,
    pub(crate) edit_path: Option<Utf8PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct FlowDefinition {
    pub(crate) version: u32,
    pub(crate) start: String,
    #[serde(default)]
    pub(crate) nodes: Vec<FlowNode>,
}

impl FlowDefinition {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.version != 1 {
            return Err(anyhow!(
                "unsupported flow version {}; only version 1 is supported",
                self.version
            ));
        }
        if self.start.trim().is_empty() {
            return Err(anyhow!("flow start node cannot be empty"));
        }

        let mut ids = BTreeSet::new();
        for node in &self.nodes {
            if node.id.trim().is_empty() {
                return Err(anyhow!("flow node id cannot be empty"));
            }
            if !ids.insert(node.id.clone()) {
                return Err(anyhow!("duplicate flow node id '{}'", node.id));
            }
        }

        self.node(&self.start)
            .ok_or_else(|| anyhow!("flow start node '{}' does not exist", self.start))?;
        Ok(())
    }

    pub(crate) fn node(&self, id: &str) -> Option<&FlowNode> {
        self.nodes.iter().find(|node| node.id == id)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct FlowNode {
    pub(crate) id: String,
    #[serde(flatten)]
    pub(crate) spec: FlowNodeSpec,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum FlowNodeSpec {
    Prompt {
        prompt: String,
        #[serde(default)]
        max_iterations: Option<usize>,
        #[serde(default)]
        rules: Vec<FlowTransitionRule>,
        #[serde(default)]
        on_completed: Option<String>,
        #[serde(default)]
        on_max_iterations: Option<String>,
        #[serde(default)]
        on_failed: Option<String>,
        #[serde(default)]
        on_canceled: Option<String>,
    },
    Decision {
        #[serde(default)]
        rules: Vec<FlowTransitionRule>,
    },
    Pause {
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        summary: Option<String>,
        #[serde(default)]
        actions: Vec<FlowPauseAction>,
    },
    Interactive {
        prompt: String,
        #[serde(default)]
        rules: Vec<FlowTransitionRule>,
        #[serde(default)]
        on_completed: Option<String>,
        #[serde(default)]
        on_failed: Option<String>,
    },
    Action {
        action: String,
        #[serde(default)]
        args: toml::Table,
        #[serde(default)]
        on_success: Option<String>,
        #[serde(default)]
        on_error: Option<String>,
    },
    Finish {
        #[serde(default)]
        summary: Option<String>,
        #[serde(default)]
        status: Option<LastRunStatus>,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct FlowPauseAction {
    pub(crate) id: String,
    pub(crate) label: String,
    #[serde(default)]
    pub(crate) shortcut: Option<String>,
    #[serde(default)]
    pub(crate) confirm_title: Option<String>,
    #[serde(default)]
    pub(crate) confirm_message: Option<String>,
    pub(crate) goto: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct FlowTransitionRule {
    #[serde(default)]
    pub(crate) when: Option<FlowCondition>,
    pub(crate) goto: String,
    #[serde(default)]
    pub(crate) note: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum FlowCondition {
    Always,
    Exists { path: String },
    Missing { path: String },
    MissingVar { key: String },
    OpenItems { path: String },
    NoOpenItems { path: String },
    PathHashChanged { path: String, key: String },
    PathHashEquals { path: String, key: String },
    VarEquals { key: String, value: String },
    SelectedAction { action: String },
    LastStatus { status: LastRunStatus },
    Any { conditions: Vec<FlowCondition> },
    All { conditions: Vec<FlowCondition> },
    Not { condition: Box<FlowCondition> },
}

#[derive(Debug, Clone)]
pub(crate) struct FlowPauseState {
    pub(crate) node_id: String,
    pub(crate) message: Option<String>,
    pub(crate) summary: Option<String>,
    pub(crate) actions: Vec<FlowPauseAction>,
}

#[derive(Debug, Clone)]
pub(crate) struct FlowStatusSummary {
    pub(crate) entrypoint_id: String,
    pub(crate) current_node: Option<String>,
    pub(crate) pause: Option<FlowPauseState>,
    pub(crate) actions: Vec<FlowPauseAction>,
    pub(crate) flow_ref: String,
}

pub(crate) struct FlowEvalContext<'a> {
    pub(crate) target_dir: &'a Utf8Path,
    pub(crate) runtime: &'a FlowRuntimeState,
    pub(crate) selected_action: Option<&'a str>,
    pub(crate) last_status: Option<LastRunStatus>,
}

pub(crate) fn resolve_target_entrypoints(
    target_config: &TargetConfig,
    target_summary: &TargetSummary,
) -> Vec<TargetEntrypoint> {
    if !target_config.entrypoints.is_empty() {
        return target_config.entrypoints.clone();
    }

    target_summary
        .prompt_files
        .iter()
        .map(|prompt| TargetEntrypoint::Prompt {
            id: prompt.name.clone(),
            path: prompt.name.clone(),
            hidden: false,
            edit_path: Some(prompt.name.clone()),
        })
        .collect()
}

pub(crate) fn resolve_default_entrypoint<'a>(
    target_config: &'a TargetConfig,
    entrypoints: &'a [TargetEntrypoint],
) -> Option<&'a TargetEntrypoint> {
    if let Some(default_id) = &target_config.default_entrypoint
        && let Some(entrypoint) = entrypoints
            .iter()
            .find(|entrypoint| entrypoint.id() == default_id)
    {
        return Some(entrypoint);
    }

    if let Some(entrypoint) = entrypoints
        .iter()
        .find(|entrypoint| entrypoint.id() == DEFAULT_FLOW_ENTRYPOINT_ID)
    {
        return Some(entrypoint);
    }

    entrypoints.iter().find(|entrypoint| !entrypoint.hidden())
}

pub(crate) fn resolve_prompt_entrypoint<'a>(
    entrypoints: &'a [TargetEntrypoint],
    prompt_name: &str,
) -> Option<&'a TargetEntrypoint> {
    entrypoints.iter().find(|entrypoint| match entrypoint {
        TargetEntrypoint::Prompt { id, path, .. } => id == prompt_name || path == prompt_name,
        TargetEntrypoint::Flow { .. } => false,
    })
}

pub(crate) fn load_flow(
    project_dir: &Utf8Path,
    target_dir: &Utf8Path,
    entrypoint: &TargetEntrypoint,
) -> Result<LoadedFlow> {
    let (_id, flow_ref, params, edit_path) = match entrypoint {
        TargetEntrypoint::Flow {
            id,
            flow,
            params,
            edit_path,
            ..
        } => (id.clone(), flow.clone(), params.clone(), edit_path.clone()),
        TargetEntrypoint::Prompt { .. } => {
            return Err(anyhow!(
                "attempted to load a flow from a prompt entrypoint '{}'",
                entrypoint.id()
            ));
        }
    };

    let raw = load_text_artifact(project_dir, target_dir, &flow_ref, None)
        .with_context(|| format!("failed to load flow '{flow_ref}'"))?;
    let rendered = render_params(&raw, &params);
    let definition: FlowDefinition = toml::from_str(&rendered)
        .with_context(|| format!("failed to parse flow artifact '{flow_ref}'"))?;
    definition.validate()?;
    Ok(LoadedFlow {
        artifact_ref: resolve_artifact_reference(None, &flow_ref)?,
        definition,
        edit_path: edit_path.map(|path| resolve_artifact_path(project_dir, target_dir, &path)),
    })
}

pub(crate) fn load_prompt_text(
    project_dir: &Utf8Path,
    target_dir: &Utf8Path,
    reference: &str,
    base_ref: Option<&str>,
    params: &BTreeMap<String, String>,
) -> Result<String> {
    let raw = load_text_artifact(project_dir, target_dir, reference, base_ref)
        .with_context(|| format!("failed to load prompt '{reference}'"))?;
    Ok(render_params(&raw, params))
}

pub(crate) fn resolve_artifact_path(
    project_dir: &Utf8Path,
    target_dir: &Utf8Path,
    reference: &str,
) -> Utf8PathBuf {
    if let Some(rest) = reference.strip_prefix("project://") {
        return project_dir.join(".ralph").join(rest);
    }
    if let Some(rest) = reference.strip_prefix("user://") {
        if let Ok(Some(path)) = AppConfig::user_config_path()
            && let Some(root) = path.parent()
        {
            return root.join(rest);
        }
    }
    target_dir.join(reference)
}

pub(crate) fn evaluate_condition(
    condition: &FlowCondition,
    context: &FlowEvalContext<'_>,
) -> Result<bool> {
    match condition {
        FlowCondition::Always => Ok(true),
        FlowCondition::Exists { path } => Ok(resolve_flow_path(context.target_dir, path).exists()),
        FlowCondition::Missing { path } => {
            Ok(!resolve_flow_path(context.target_dir, path).exists())
        }
        FlowCondition::MissingVar { key } => Ok(!context.runtime.vars.contains_key(key)),
        FlowCondition::OpenItems { path } => {
            let path = resolve_flow_path(context.target_dir, path);
            Ok(read_contains_open_items(&path)?.unwrap_or(false))
        }
        FlowCondition::NoOpenItems { path } => {
            let path = resolve_flow_path(context.target_dir, path);
            Ok(!read_contains_open_items(&path)?.unwrap_or(false))
        }
        FlowCondition::PathHashChanged { path, key } => {
            let current_hash = hash_optional_file(&resolve_flow_path(context.target_dir, path))?;
            Ok(context.runtime.vars.get(key) != current_hash.as_ref())
        }
        FlowCondition::PathHashEquals { path, key } => {
            let current_hash = hash_optional_file(&resolve_flow_path(context.target_dir, path))?;
            Ok(context.runtime.vars.get(key) == current_hash.as_ref())
        }
        FlowCondition::VarEquals { key, value } => Ok(context.runtime.vars.get(key) == Some(value)),
        FlowCondition::SelectedAction { action } => {
            Ok(context.selected_action == Some(action.as_str()))
        }
        FlowCondition::LastStatus { status } => Ok(context.last_status == Some(*status)),
        FlowCondition::Any { conditions } => conditions
            .iter()
            .map(|condition| evaluate_condition(condition, context))
            .collect::<Result<Vec<_>>>()
            .map(|results| results.into_iter().any(|result| result)),
        FlowCondition::All { conditions } => conditions
            .iter()
            .map(|condition| evaluate_condition(condition, context))
            .collect::<Result<Vec<_>>>()
            .map(|results| results.into_iter().all(|result| result)),
        FlowCondition::Not { condition } => Ok(!evaluate_condition(condition, context)?),
    }
}

pub(crate) fn select_transition<'a>(
    rules: &'a [FlowTransitionRule],
    context: &FlowEvalContext<'_>,
) -> Result<Option<&'a FlowTransitionRule>> {
    for rule in rules {
        let matches = match &rule.when {
            Some(condition) => evaluate_condition(condition, context)?,
            None => true,
        };
        if matches {
            return Ok(Some(rule));
        }
    }
    Ok(None)
}

pub(crate) fn load_flow_status(
    project_dir: &Utf8Path,
    target_dir: &Utf8Path,
    target_config: &TargetConfig,
    target_summary: &TargetSummary,
) -> Result<Option<FlowStatusSummary>> {
    let entrypoints = resolve_target_entrypoints(target_config, target_summary);
    let Some(entrypoint) = resolve_default_entrypoint(target_config, &entrypoints) else {
        return Ok(None);
    };
    let TargetEntrypoint::Flow { flow, .. } = entrypoint else {
        return Ok(None);
    };
    let flow_definition = load_flow(project_dir, target_dir, entrypoint)?;
    let current_node_id = target_config.runtime.as_ref().and_then(|runtime| {
        if runtime.active_entrypoint.as_deref() == Some(entrypoint.id()) {
            runtime.current_node.clone()
        } else {
            None
        }
    });
    let pause = current_node_id
        .as_deref()
        .and_then(|node_id| flow_definition.definition.node(node_id))
        .and_then(|node| match &node.spec {
            FlowNodeSpec::Pause {
                message,
                summary,
                actions,
            } => Some(FlowPauseState {
                node_id: node.id.clone(),
                message: message.clone(),
                summary: summary.clone(),
                actions: actions.clone(),
            }),
            _ => None,
        });
    let mut actions = Vec::new();
    let mut seen_action_ids = std::collections::BTreeSet::new();
    if let Some(current_pause) = &pause {
        for action in &current_pause.actions {
            if seen_action_ids.insert(action.id.clone()) {
                actions.push(action.clone());
            }
        }
    }
    for node in &flow_definition.definition.nodes {
        if let FlowNodeSpec::Pause {
            actions: node_actions,
            ..
        } = &node.spec
        {
            for action in node_actions {
                if seen_action_ids.insert(action.id.clone()) {
                    actions.push(action.clone());
                }
            }
        }
    }

    Ok(Some(FlowStatusSummary {
        entrypoint_id: entrypoint.id().to_owned(),
        current_node: current_node_id,
        pause,
        actions,
        flow_ref: flow.clone(),
    }))
}

pub(crate) fn ensure_runtime<'a>(
    target_config: &'a mut TargetConfig,
    entrypoint_id: &str,
) -> &'a mut FlowRuntimeState {
    let runtime = target_config
        .runtime
        .get_or_insert_with(FlowRuntimeState::default);
    if runtime.active_entrypoint.as_deref() != Some(entrypoint_id) {
        runtime.active_entrypoint = Some(entrypoint_id.to_owned());
        runtime.current_node = None;
        runtime.last_signal = None;
        runtime.last_note = None;
        runtime.inflight = None;
    }
    runtime
}

pub(crate) fn set_inflight(runtime: &mut FlowRuntimeState, node_id: &str, started_at: u64) {
    runtime.inflight = Some(FlowRuntimeInflight {
        node_id: node_id.to_owned(),
        started_at,
    });
    runtime.current_node = Some(node_id.to_owned());
}

pub(crate) fn clear_inflight(runtime: &mut FlowRuntimeState) {
    runtime.inflight = None;
}

pub(crate) fn resolve_flow_path(target_dir: &Utf8Path, path: &str) -> Utf8PathBuf {
    let path = Utf8PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        target_dir.join(path)
    }
}

pub(crate) fn hash_optional_file(path: &Utf8Path) -> Result<Option<String>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            Ok(Some(format!("sha256:{:x}", hasher.finalize())))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path)),
    }
}

pub(crate) fn read_contains_open_items(path: &Utf8Path) -> Result<Option<bool>> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            Ok(Some(contents.lines().any(|line| {
                line.contains("completed") && line.contains("false")
            })))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path)),
    }
}

pub(crate) fn resolve_artifact_reference(
    base_ref: Option<&str>,
    reference: &str,
) -> Result<String> {
    if let Some(rest) = reference.strip_prefix("self://") {
        let Some(base_ref) = base_ref else {
            return Err(anyhow!(
                "self:// references require a bundle-scoped base artifact"
            ));
        };
        let Some(root) = bundle_root_reference(base_ref) else {
            return Err(anyhow!(
                "could not resolve workflow bundle root from '{base_ref}'"
            ));
        };
        return Ok(format!("{root}{rest}"));
    }

    Ok(reference.to_owned())
}

fn bundle_root_reference(reference: &str) -> Option<String> {
    for marker in ["/workflow.toml", "/flows/", "/prompts/", "/templates/"] {
        if let Some((prefix, _)) = reference.split_once(marker) {
            return Some(format!("{prefix}/"));
        }
    }
    None
}

pub(crate) fn load_text_artifact(
    project_dir: &Utf8Path,
    target_dir: &Utf8Path,
    reference: &str,
    base_ref: Option<&str>,
) -> Result<String> {
    let reference = resolve_artifact_reference(base_ref, reference)?;
    if let Some(rest) = reference.strip_prefix("builtin://") {
        return builtin_asset(rest)
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("builtin artifact '{}' does not exist", reference));
    }
    let path = if let Some(rest) = reference.strip_prefix("project://") {
        project_dir.join(".ralph").join(rest)
    } else if let Some(rest) = reference.strip_prefix("user://") {
        let Some(config_path) = AppConfig::user_config_path()? else {
            return Err(anyhow!("unable to resolve user config path"));
        };
        let Some(root) = config_path.parent() else {
            return Err(anyhow!("user config path has no parent directory"));
        };
        root.join(rest)
    } else {
        target_dir.join(reference)
    };

    std::fs::read_to_string(&path).with_context(|| format!("failed to read {}", path))
}

pub(crate) fn render_params(raw: &str, params: &BTreeMap<String, String>) -> String {
    let mut rendered = raw.to_owned();
    for (key, value) in params {
        rendered = rendered.replace(&format!("{{{{{key}}}}}"), value);
    }
    rendered
}

fn builtin_asset(reference: &str) -> Option<&'static str> {
    match reference {
        "workflows/single_prompt/workflow.toml" => Some(include_str!(
            "builtin_assets/workflows/single_prompt/workflow.toml"
        )),
        "workflows/single_prompt/templates/prompt_main.md" => Some(include_str!(
            "builtin_assets/workflows/single_prompt/templates/prompt_main.md"
        )),
        "workflows/plan_build/workflow.toml" => Some(include_str!(
            "builtin_assets/workflows/plan_build/workflow.toml"
        )),
        "workflows/plan_build/templates/0_plan.md" => Some(include_str!(
            "builtin_assets/workflows/plan_build/templates/0_plan.md"
        )),
        "workflows/plan_build/templates/1_build.md" => Some(include_str!(
            "builtin_assets/workflows/plan_build/templates/1_build.md"
        )),
        "workflows/plan_driven/workflow.toml" => Some(include_str!(
            "builtin_assets/workflows/plan_driven/workflow.toml"
        )),
        "workflows/plan_driven/templates/GOAL.md" => Some(include_str!(
            "builtin_assets/workflows/plan_driven/templates/GOAL.md"
        )),
        "workflows/task_driven/workflow.toml" => Some(include_str!(
            "builtin_assets/workflows/task_driven/workflow.toml"
        )),
        "workflows/task_driven/templates/GOAL.md" => Some(include_str!(
            "builtin_assets/workflows/task_driven/templates/GOAL.md"
        )),
        "workflows/task_driven/templates/progress.toml" => Some(include_str!(
            "builtin_assets/workflows/task_driven/templates/progress.toml"
        )),
        "flows/plan_driven.toml" => Some(include_str!("builtin_assets/flows/plan_driven.toml")),
        "flows/task_driven.toml" => Some(include_str!("builtin_assets/flows/task_driven.toml")),
        "prompts/plan_driven/rebase.md" => {
            Some(include_str!("builtin_assets/prompts/plan_driven/rebase.md"))
        }
        "prompts/plan_driven/build.md" => {
            Some(include_str!("builtin_assets/prompts/plan_driven/build.md"))
        }
        "prompts/plan_driven/goal_interview.md" => Some(include_str!(
            "builtin_assets/prompts/plan_driven/goal_interview.md"
        )),
        "prompts/task_driven/rebase.md" => {
            Some(include_str!("builtin_assets/prompts/task_driven/rebase.md"))
        }
        "prompts/task_driven/build.md" => {
            Some(include_str!("builtin_assets/prompts/task_driven/build.md"))
        }
        "prompts/task_driven/goal_interview.md" => Some(include_str!(
            "builtin_assets/prompts/task_driven/goal_interview.md"
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use anyhow::Result;
    use camino::Utf8Path;
    use ralph_core::{FlowRuntimeState, LastRunStatus};

    use super::{
        FlowCondition, FlowEvalContext, evaluate_condition, hash_optional_file, load_prompt_text,
        load_text_artifact,
    };

    #[test]
    fn builtin_artifacts_are_loadable() -> Result<()> {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8Path::from_path(temp.path()).unwrap();
        let target_dir = project_dir;
        let flow = load_text_artifact(
            project_dir,
            target_dir,
            "builtin://flows/plan_driven.toml",
            None,
        )?;
        assert!(flow.contains("kind = \"decision\""));
        let prompt = load_prompt_text(
            project_dir,
            target_dir,
            "builtin://prompts/task_driven/build.md",
            None,
            &BTreeMap::new(),
        )?;
        assert!(prompt.contains("{{derived_file}}"));
        Ok(())
    }

    #[test]
    fn path_hash_condition_compares_against_runtime_vars() -> Result<()> {
        let temp = tempfile::tempdir().unwrap();
        let target_dir = Utf8Path::from_path(temp.path()).unwrap();
        let file = target_dir.join("state.txt");
        std::fs::write(&file, "one")?;
        let stored_hash = hash_optional_file(&file)?.unwrap();
        std::fs::write(&file, "two")?;

        let condition = FlowCondition::PathHashChanged {
            path: "state.txt".to_owned(),
            key: "state_hash".to_owned(),
        };
        let runtime = FlowRuntimeState {
            vars: BTreeMap::from([("state_hash".to_owned(), stored_hash)]),
            ..FlowRuntimeState::default()
        };
        let context = FlowEvalContext {
            target_dir,
            runtime: &runtime,
            selected_action: None,
            last_status: Some(LastRunStatus::Completed),
        };
        assert!(evaluate_condition(&condition, &context)?);
        Ok(())
    }
}
