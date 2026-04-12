use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::ErrorKind,
};

use anyhow::{Context, Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};

use crate::{atomic_write, config::global_config_dir};

const WORKFLOW_VERSION: u8 = 1;
const REQUEST_TOKEN: &str = "{ralph-request}";
const PROJECT_DIR_TOKEN: &str = "{ralph-env:PROJECT_DIR}";
const REMOVED_RUN_DIR_TOKEN: &str = "{ralph-env:RUN_DIR}";
const OPTION_TOKEN_PREFIX: &str = "{ralph-option:";
const RALPH_TOKEN_START: &str = "{ralph-";

pub const NO_ROUTE_OK: &str = "no-route-ok";
pub const NO_ROUTE_ERROR: &str = "no-route-error";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowDefinition {
    pub version: u8,
    pub workflow_id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub hidden: bool,
    pub entrypoint: String,
    #[serde(default)]
    pub options: BTreeMap<String, WorkflowOptionDefinition>,
    #[serde(default)]
    pub request: Option<WorkflowRequestDefinition>,
    pub prompts: BTreeMap<String, WorkflowPromptDefinition>,
    #[serde(skip)]
    source_path: Option<Utf8PathBuf>,
}

impl WorkflowDefinition {
    pub fn validate(&self) -> Result<()> {
        if self.version != WORKFLOW_VERSION {
            return Err(anyhow!(
                "workflow '{}' uses unsupported version {}; expected {}",
                self.workflow_id,
                self.version,
                WORKFLOW_VERSION
            ));
        }
        if self.workflow_id.trim().is_empty() {
            return Err(anyhow!("workflow_id cannot be empty"));
        }
        if self.entrypoint.trim().is_empty() {
            return Err(anyhow!(
                "workflow '{}' must define a non-empty entrypoint",
                self.workflow_id
            ));
        }
        if self.prompts.is_empty() {
            return Err(anyhow!(
                "workflow '{}' must define at least one prompt",
                self.workflow_id
            ));
        }
        if !self.prompts.contains_key(&self.entrypoint) {
            return Err(anyhow!(
                "workflow '{}' entrypoint '{}' is not defined under prompts",
                self.workflow_id,
                self.entrypoint
            ));
        }

        let mut seen_flags = BTreeMap::new();
        for option_id in self.options.keys() {
            let flag = workflow_option_flag(option_id)?;
            if let Some(existing) = seen_flags.insert(flag.clone(), option_id.clone()) {
                return Err(anyhow!(
                    "workflow '{}' options '{}' and '{}' both map to CLI flag '--{}'",
                    self.workflow_id,
                    existing,
                    option_id,
                    flag
                ));
            }
        }

        for (prompt_id, prompt) in &self.prompts {
            if prompt_id.trim().is_empty() {
                return Err(anyhow!(
                    "workflow '{}' contains an empty prompt id",
                    self.workflow_id
                ));
            }
            if matches!(prompt_id.as_str(), NO_ROUTE_OK | NO_ROUTE_ERROR) {
                return Err(anyhow!(
                    "workflow '{}' uses reserved prompt id '{}'",
                    self.workflow_id,
                    prompt_id
                ));
            }
            if prompt.fallback_route.trim().is_empty() {
                return Err(anyhow!(
                    "workflow '{}' prompt '{}' must define fallback-route",
                    self.workflow_id,
                    prompt_id
                ));
            }
            if !self.is_valid_route(&prompt.fallback_route) {
                return Err(anyhow!(
                    "workflow '{}' prompt '{}' fallback-route '{}' is not a known prompt id or sentinel",
                    self.workflow_id,
                    prompt_id,
                    prompt.fallback_route
                ));
            }
            match (&prompt.prompt, &prompt.parallel) {
                (Some(prompt_text), None) => {
                    self.validate_prompt_text(prompt_id, prompt_text)?;
                }
                (None, Some(parallel)) => {
                    if parallel.workers.is_empty() {
                        return Err(anyhow!(
                            "workflow '{}' prompt '{}' must define at least one parallel worker",
                            self.workflow_id,
                            prompt_id
                        ));
                    }
                    for (worker_id, worker) in &parallel.workers {
                        validate_worker_id(worker_id).with_context(|| {
                            format!(
                                "workflow '{}' prompt '{}' has invalid worker id '{}'",
                                self.workflow_id, prompt_id, worker_id
                            )
                        })?;
                        self.validate_prompt_text(prompt_id, &worker.prompt)?;
                    }
                }
                (Some(_), Some(_)) => {
                    return Err(anyhow!(
                        "workflow '{}' prompt '{}' must define exactly one of 'prompt' or 'parallel'",
                        self.workflow_id,
                        prompt_id
                    ));
                }
                (None, None) => {
                    return Err(anyhow!(
                        "workflow '{}' prompt '{}' must define either 'prompt' or 'parallel'",
                        self.workflow_id,
                        prompt_id
                    ));
                }
            }
            self.validate_transition_guards(prompt_id, prompt)?;
        }

        let request_sources = self
            .request
            .as_ref()
            .map(WorkflowRequestDefinition::source_count)
            .unwrap_or(0);
        if self.request.is_some() && request_sources != 1 {
            return Err(anyhow!(
                "workflow '{}' request must declare exactly one of runtime, file, or inline",
                self.workflow_id
            ));
        }
        if self.uses_request_token() && self.request.is_none() {
            return Err(anyhow!(
                "workflow '{}' uses {REQUEST_TOKEN} but does not define a request block",
                self.workflow_id
            ));
        }

        if let Some(request) = &self.request {
            if let Some(runtime) = &request.runtime
                && !runtime.argv
                && !runtime.stdin
                && !runtime.file_flag
            {
                return Err(anyhow!(
                    "workflow '{}' request.runtime must enable at least one of argv, stdin, or file_flag",
                    self.workflow_id
                ));
            }
            if let Some(file) = &request.file
                && file.path.as_str().trim().is_empty()
            {
                return Err(anyhow!(
                    "workflow '{}' request.file.path cannot be empty",
                    self.workflow_id
                ));
            }
            if let Some(inline) = &request.inline
                && inline.trim().is_empty()
            {
                return Err(anyhow!(
                    "workflow '{}' request.inline cannot be empty",
                    self.workflow_id
                ));
            }
        }

        Ok(())
    }

    pub fn prompt(&self, prompt_id: &str) -> Option<&WorkflowPromptDefinition> {
        self.prompts.get(prompt_id)
    }

    pub fn prompt_ids(&self) -> Vec<&str> {
        self.prompts.keys().map(String::as_str).collect()
    }

    pub fn uses_request_token(&self) -> bool {
        self.prompts.values().any(|prompt| {
            prompt
                .prompt
                .as_deref()
                .is_some_and(|prompt_text| prompt_text.contains(REQUEST_TOKEN))
                || prompt.parallel.as_ref().is_some_and(|parallel| {
                    parallel
                        .workers
                        .values()
                        .any(|worker| worker.prompt.contains(REQUEST_TOKEN))
                })
        })
    }

    pub fn option(&self, option_id: &str) -> Option<&WorkflowOptionDefinition> {
        self.options.get(option_id)
    }

    pub fn option_ids(&self) -> Vec<&str> {
        self.options.keys().map(String::as_str).collect()
    }

    pub fn source_path(&self) -> Option<&Utf8Path> {
        self.source_path.as_deref()
    }

    fn is_valid_route(&self, route: &str) -> bool {
        matches!(route, NO_ROUTE_OK | NO_ROUTE_ERROR) || self.prompts.contains_key(route)
    }

    fn validate_prompt_text(&self, prompt_id: &str, prompt_text: &str) -> Result<()> {
        if prompt_text.contains(REMOVED_RUN_DIR_TOKEN) {
            return Err(anyhow!(
                "workflow '{}' prompt '{}' uses unsupported interpolation '{}'; use '{}' or plain project-relative paths instead",
                self.workflow_id,
                prompt_id,
                REMOVED_RUN_DIR_TOKEN,
                PROJECT_DIR_TOKEN
            ));
        }
        for option_id in referenced_option_ids(prompt_text) {
            if !self.options.contains_key(option_id) {
                return Err(anyhow!(
                    "workflow '{}' prompt '{}' references undefined option '{}'",
                    self.workflow_id,
                    prompt_id,
                    option_id
                ));
            }
        }
        Ok(())
    }

    fn validate_transition_guards(
        &self,
        prompt_id: &str,
        prompt: &WorkflowPromptDefinition,
    ) -> Result<()> {
        for (transition_id, guards) in &prompt.transition_guards {
            self.validate_transition_guard_transition_id(prompt_id, transition_id)?;
            if guards.is_empty() {
                return Err(anyhow!(
                    "workflow '{}' prompt '{}' transition guard list '{}' cannot be empty",
                    self.workflow_id,
                    prompt_id,
                    transition_id
                ));
            }

            for guard in guards {
                self.validate_transition_guard(prompt_id, transition_id, guard)?;
            }
        }

        Ok(())
    }

    fn validate_transition_guard_transition_id(
        &self,
        prompt_id: &str,
        transition_id: &str,
    ) -> Result<()> {
        match transition_id {
            "continue" | "stop-ok" | "stop-error" => Ok(()),
            _ if transition_id.starts_with("route:") => {
                let route = transition_id["route:".len()..].trim();
                if route.is_empty() {
                    return Err(anyhow!(
                        "workflow '{}' prompt '{}' transition guard route target cannot be empty",
                        self.workflow_id,
                        prompt_id
                    ));
                }
                if matches!(route, NO_ROUTE_OK | NO_ROUTE_ERROR)
                    || !self.prompts.contains_key(route)
                {
                    return Err(anyhow!(
                        "workflow '{}' prompt '{}' transition guard route target '{}' is not a known prompt id",
                        self.workflow_id,
                        prompt_id,
                        route
                    ));
                }
                Ok(())
            }
            _ => Err(anyhow!(
                "workflow '{}' prompt '{}' uses unsupported transition guard target '{}'; expected continue, stop-ok, stop-error, or route:<prompt-id>",
                self.workflow_id,
                prompt_id,
                transition_id
            )),
        }
    }

    fn validate_transition_guard(
        &self,
        prompt_id: &str,
        transition_id: &str,
        guard: &WorkflowTransitionGuard,
    ) -> Result<()> {
        match guard {
            WorkflowTransitionGuard::FileExists { path, failure } => {
                self.validate_transition_guard_path(prompt_id, path)?;
                self.validate_transition_guard_failure(prompt_id, transition_id, failure)?;
            }
            WorkflowTransitionGuard::FileContains {
                path,
                literal,
                failure,
            }
            | WorkflowTransitionGuard::FileNotContains {
                path,
                literal,
                failure,
            } => {
                self.validate_transition_guard_path(prompt_id, path)?;
                if literal.trim().is_empty() {
                    return Err(anyhow!(
                        "workflow '{}' prompt '{}' transition guard '{}' literal cannot be empty",
                        self.workflow_id,
                        prompt_id,
                        transition_id
                    ));
                }
                self.validate_transition_guard_failure(prompt_id, transition_id, failure)?;
            }
            WorkflowTransitionGuard::EventExists {
                event,
                channel,
                failure,
            } => {
                self.validate_transition_guard_event(prompt_id, transition_id, event, channel)?;
                self.validate_transition_guard_failure(prompt_id, transition_id, failure)?;
            }
            WorkflowTransitionGuard::EventContains {
                event,
                channel,
                literal,
                failure,
            } => {
                self.validate_transition_guard_event(prompt_id, transition_id, event, channel)?;
                if literal.trim().is_empty() {
                    return Err(anyhow!(
                        "workflow '{}' prompt '{}' transition guard '{}' literal cannot be empty",
                        self.workflow_id,
                        prompt_id,
                        transition_id
                    ));
                }
                self.validate_transition_guard_failure(prompt_id, transition_id, failure)?;
            }
        }

        Ok(())
    }

    fn validate_transition_guard_path(&self, prompt_id: &str, path: &str) -> Result<()> {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            return Err(anyhow!(
                "workflow '{}' prompt '{}' transition guard path cannot be empty",
                self.workflow_id,
                prompt_id
            ));
        }
        if trimmed.contains(REMOVED_RUN_DIR_TOKEN) {
            return Err(anyhow!(
                "workflow '{}' prompt '{}' transition guard path uses unsupported interpolation '{}'; use '{}' or plain project-relative paths instead",
                self.workflow_id,
                prompt_id,
                REMOVED_RUN_DIR_TOKEN,
                PROJECT_DIR_TOKEN
            ));
        }

        for option_id in referenced_option_ids(trimmed) {
            if !self.options.contains_key(option_id) {
                return Err(anyhow!(
                    "workflow '{}' prompt '{}' transition guard path references undefined option '{}'",
                    self.workflow_id,
                    prompt_id,
                    option_id
                ));
            }
        }

        let mut remaining = trimmed;
        while let Some(start) = remaining.find(RALPH_TOKEN_START) {
            let suffix = &remaining[start + 1..];
            let Some(end) = suffix.find('}') else {
                return Err(anyhow!(
                    "workflow '{}' prompt '{}' transition guard path contains an unterminated Ralph token",
                    self.workflow_id,
                    prompt_id
                ));
            };
            let token = &suffix[..end];
            if token != "ralph-env:PROJECT_DIR" && !token.starts_with("ralph-option:") {
                return Err(anyhow!(
                    "workflow '{}' prompt '{}' transition guard paths only support '{}', '{{ralph-option:...}}', or plain paths",
                    self.workflow_id,
                    prompt_id,
                    PROJECT_DIR_TOKEN
                ));
            }
            remaining = &suffix[end + 1..];
        }

        Ok(())
    }

    fn validate_transition_guard_event(
        &self,
        prompt_id: &str,
        transition_id: &str,
        event: &str,
        channel: &Option<String>,
    ) -> Result<()> {
        if event.trim().is_empty() {
            return Err(anyhow!(
                "workflow '{}' prompt '{}' transition guard '{}' event cannot be empty",
                self.workflow_id,
                prompt_id,
                transition_id
            ));
        }
        if let Some(channel) = channel
            && channel.trim().is_empty()
        {
            return Err(anyhow!(
                "workflow '{}' prompt '{}' transition guard '{}' channel cannot be empty",
                self.workflow_id,
                prompt_id,
                transition_id
            ));
        }
        Ok(())
    }

    fn validate_transition_guard_failure(
        &self,
        prompt_id: &str,
        transition_id: &str,
        failure: &WorkflowTransitionGuardFailure,
    ) -> Result<()> {
        match failure.on_fail {
            WorkflowTransitionGuardFailureAction::Continue
            | WorkflowTransitionGuardFailureAction::Error => {
                if let Some(route) = &failure.route {
                    return Err(anyhow!(
                        "workflow '{}' prompt '{}' transition guard '{}' must not define route '{}' when on-fail is '{}'",
                        self.workflow_id,
                        prompt_id,
                        transition_id,
                        route,
                        failure.on_fail.label()
                    ));
                }
            }
            WorkflowTransitionGuardFailureAction::Route => {
                let route = failure.route.as_deref().map(str::trim).ok_or_else(|| {
                    anyhow!(
                        "workflow '{}' prompt '{}' transition guard '{}' requires a route when on-fail is 'route'",
                        self.workflow_id,
                        prompt_id,
                        transition_id
                    )
                })?;
                if route.is_empty() || matches!(route, NO_ROUTE_OK | NO_ROUTE_ERROR) {
                    return Err(anyhow!(
                        "workflow '{}' prompt '{}' transition guard '{}' route target cannot be empty or a sentinel",
                        self.workflow_id,
                        prompt_id,
                        transition_id
                    ));
                }
                if !self.prompts.contains_key(route) {
                    return Err(anyhow!(
                        "workflow '{}' prompt '{}' transition guard '{}' route target '{}' is not a known prompt id",
                        self.workflow_id,
                        prompt_id,
                        transition_id,
                        route
                    ));
                }
            }
        }

        if let Some(note) = &failure.note
            && note.trim().is_empty()
        {
            return Err(anyhow!(
                "workflow '{}' prompt '{}' transition guard '{}' note cannot be empty",
                self.workflow_id,
                prompt_id,
                transition_id
            ));
        }
        if let Some(summary) = &failure.summary
            && summary.trim().is_empty()
        {
            return Err(anyhow!(
                "workflow '{}' prompt '{}' transition guard '{}' summary cannot be empty",
                self.workflow_id,
                prompt_id,
                transition_id
            ));
        }

        Ok(())
    }
}

fn validate_worker_id(worker_id: &str) -> Result<()> {
    let trimmed = worker_id.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("worker id cannot be empty"));
    }

    for ch in trimmed.chars() {
        match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => {}
            _ => {
                return Err(anyhow!(
                    "worker ids may only use ASCII letters, digits, '-' or '_'"
                ));
            }
        }
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowPromptDefinition {
    pub title: String,
    #[serde(rename = "fallback-route")]
    pub fallback_route: String,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub parallel: Option<WorkflowParallelDefinition>,
    #[serde(default, rename = "transition-guards")]
    pub transition_guards: BTreeMap<String, Vec<WorkflowTransitionGuard>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowTransitionGuardFailureAction {
    Continue,
    Error,
    Route,
}

impl WorkflowTransitionGuardFailureAction {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::Error => "error",
            Self::Route => "route",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowTransitionGuardFailure {
    #[serde(rename = "on-fail")]
    pub on_fail: WorkflowTransitionGuardFailureAction,
    #[serde(default)]
    pub route: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowTransitionGuard {
    FileExists {
        path: String,
        #[serde(flatten)]
        failure: WorkflowTransitionGuardFailure,
    },
    FileContains {
        path: String,
        literal: String,
        #[serde(flatten)]
        failure: WorkflowTransitionGuardFailure,
    },
    FileNotContains {
        path: String,
        literal: String,
        #[serde(flatten)]
        failure: WorkflowTransitionGuardFailure,
    },
    EventExists {
        event: String,
        #[serde(default)]
        channel: Option<String>,
        #[serde(flatten)]
        failure: WorkflowTransitionGuardFailure,
    },
    EventContains {
        event: String,
        literal: String,
        #[serde(default)]
        channel: Option<String>,
        #[serde(flatten)]
        failure: WorkflowTransitionGuardFailure,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowParallelJoin {
    #[default]
    WaitAll,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowParallelDefinition {
    #[serde(default)]
    pub join: WorkflowParallelJoin,
    #[serde(default)]
    pub fail_fast: bool,
    pub workers: BTreeMap<String, WorkflowParallelWorkerDefinition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowParallelWorkerDefinition {
    #[serde(default)]
    pub title: Option<String>,
    pub prompt: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WorkflowOptionDefinition {
    #[serde(default)]
    pub help: String,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub value_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRequestDefinition {
    #[serde(default)]
    pub runtime: Option<WorkflowRuntimeRequest>,
    #[serde(default)]
    pub file: Option<WorkflowFileRequest>,
    #[serde(default)]
    pub inline: Option<String>,
}

impl WorkflowRequestDefinition {
    fn source_count(&self) -> usize {
        usize::from(self.runtime.is_some())
            + usize::from(self.file.is_some())
            + usize::from(self.inline.is_some())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WorkflowRuntimeRequest {
    #[serde(default)]
    pub argv: bool,
    #[serde(default)]
    pub stdin: bool,
    #[serde(default)]
    pub file_flag: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowFileRequest {
    pub path: Utf8PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkflowSummary {
    pub workflow_id: String,
    pub title: String,
    pub description: String,
    pub path: Utf8PathBuf,
}

pub fn workflow_option_flag(option_id: &str) -> Result<String> {
    if option_id.trim().is_empty() {
        return Err(anyhow!("workflow option ids cannot be empty"));
    }

    let mut flag = String::new();
    for ch in option_id.chars() {
        match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' => flag.push(ch.to_ascii_lowercase()),
            '-' | '_' => {}
            _ => {
                return Err(anyhow!(
                    "workflow option '{}' contains unsupported character '{}'; use ASCII letters, digits, '-' or '_'",
                    option_id,
                    ch
                ));
            }
        }
    }

    if flag.is_empty() {
        return Err(anyhow!(
            "workflow option '{}' must contain at least one ASCII letter or digit",
            option_id
        ));
    }

    Ok(flag)
}

fn referenced_option_ids(prompt: &str) -> BTreeSet<&str> {
    let mut option_ids = BTreeSet::new();
    let mut remaining = prompt;

    while let Some(start) = remaining.find(OPTION_TOKEN_PREFIX) {
        let token = &remaining[start + OPTION_TOKEN_PREFIX.len()..];
        let Some(end) = token.find('}') else {
            break;
        };
        option_ids.insert(&token[..end]);
        remaining = &token[end + 1..];
    }

    option_ids
}

pub fn workflow_config_dir() -> Result<Utf8PathBuf> {
    Ok(global_config_dir()?.join("workflows"))
}

pub fn seed_builtin_workflows_if_missing() -> Result<()> {
    let workflow_dir = workflow_config_dir()?;
    fs::create_dir_all(workflow_dir.as_std_path())
        .with_context(|| format!("failed to create workflow directory {}", workflow_dir))?;

    for builtin in builtin_workflows() {
        let path = workflow_dir.join(builtin.file_name);
        if builtin.protected {
            sync_builtin_workflow(&path, builtin.contents)
                .with_context(|| format!("failed to sync protected builtin workflow {}", path))?;
            continue;
        }
        if !path.exists() {
            atomic_write(&path, builtin.contents)
                .with_context(|| format!("failed to seed builtin workflow {}", path))?;
        }
    }

    Ok(())
}

pub fn load_workflow(workflow_id: &str) -> Result<WorkflowDefinition> {
    seed_builtin_workflows_if_missing()?;

    if let Some(builtin) = protected_builtin_workflow(workflow_id) {
        let path = workflow_config_dir()?.join(builtin.file_name);
        return load_workflow_from_path(&path);
    }

    let mut matches = Vec::new();
    for path in workflow_paths()? {
        let workflow = load_workflow_from_path(&path)?;
        if workflow.workflow_id == workflow_id {
            matches.push(workflow);
        }
    }

    match matches.len() {
        0 => Err(anyhow!(
            "workflow '{}' was not found under {}",
            workflow_id,
            workflow_config_dir()?
        )),
        1 => Ok(matches.remove(0)),
        _ => Err(anyhow!(
            "workflow id '{}' is defined multiple times under {}",
            workflow_id,
            workflow_config_dir()?
        )),
    }
}

pub fn list_workflows() -> Result<Vec<WorkflowSummary>> {
    list_workflow_summaries(false)
}

pub fn list_all_workflows() -> Result<Vec<WorkflowSummary>> {
    list_workflow_summaries(true)
}

fn list_workflow_summaries(include_hidden: bool) -> Result<Vec<WorkflowSummary>> {
    seed_builtin_workflows_if_missing()?;
    let mut workflows = workflow_paths()?
        .into_iter()
        .map(|path| {
            let workflow = load_workflow_from_path(&path)?;
            if is_shadowed_protected_workflow(&path, &workflow.workflow_id)? {
                return Ok(None);
            }
            if workflow.hidden && !include_hidden {
                return Ok(None);
            }
            Ok(Some(WorkflowSummary {
                workflow_id: workflow.workflow_id,
                title: workflow.title,
                description: workflow.description,
                path,
            }))
        })
        .collect::<Result<Vec<Option<_>>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    workflows.sort_by(|left, right| left.workflow_id.cmp(&right.workflow_id));
    Ok(workflows)
}

pub fn load_workflow_from_path(path: &Utf8Path) -> Result<WorkflowDefinition> {
    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read workflow {}", path))?;
    let mut workflow: WorkflowDefinition =
        serde_yaml::from_str(&raw).with_context(|| format!("failed to parse workflow {}", path))?;
    workflow.source_path = Some(path.to_path_buf());
    workflow
        .validate()
        .with_context(|| format!("invalid workflow {}", path))?;
    Ok(workflow)
}

fn sync_builtin_workflow(path: &Utf8Path, contents: &str) -> Result<()> {
    let current = match fs::read_to_string(path) {
        Ok(current) => Some(current),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };

    if current.as_deref() != Some(contents) {
        atomic_write(path, contents)
            .with_context(|| format!("failed to write workflow {}", path))?;
    }

    Ok(())
}

fn is_shadowed_protected_workflow(path: &Utf8Path, workflow_id: &str) -> Result<bool> {
    let Some(canonical_path) = builtin_workflow_path(workflow_id)? else {
        return Ok(false);
    };
    if !is_protected_builtin_workflow(workflow_id) {
        return Ok(false);
    }

    Ok(path != canonical_path)
}

fn workflow_paths() -> Result<Vec<Utf8PathBuf>> {
    let workflow_dir = workflow_config_dir()?;
    if !workflow_dir.exists() {
        return Ok(Vec::new());
    }

    let mut paths = fs::read_dir(workflow_dir.as_std_path())
        .with_context(|| format!("failed to read workflow directory {}", workflow_dir))?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| Utf8PathBuf::from_path_buf(entry.path()).ok())
        .filter(|path| matches!(path.extension(), Some("yml" | "yaml")))
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

struct BuiltinWorkflow {
    workflow_id: &'static str,
    file_name: &'static str,
    contents: &'static str,
    protected: bool,
}

fn builtin_workflows() -> [BuiltinWorkflow; 9] {
    [
        BuiltinWorkflow {
            workflow_id: "bare",
            file_name: "bare.yml",
            contents: include_str!("../workflows/bare.yml"),
            protected: false,
        },
        BuiltinWorkflow {
            workflow_id: "dbv",
            file_name: "dbv.yml",
            contents: include_str!("../workflows/dbv.yml"),
            protected: false,
        },
        BuiltinWorkflow {
            workflow_id: "default",
            file_name: "default.yml",
            contents: include_str!("../workflows/default.yml"),
            protected: false,
        },
        BuiltinWorkflow {
            workflow_id: "finalize",
            file_name: "finalize.yml",
            contents: include_str!("../workflows/finalize.yml"),
            protected: true,
        },
        BuiltinWorkflow {
            workflow_id: "plan",
            file_name: "plan.yml",
            contents: include_str!("../workflows/plan.yml"),
            protected: true,
        },
        BuiltinWorkflow {
            workflow_id: "review",
            file_name: "review.yml",
            contents: include_str!("../workflows/review.yml"),
            protected: true,
        },
        BuiltinWorkflow {
            workflow_id: "task",
            file_name: "task.yml",
            contents: include_str!("../workflows/task.yml"),
            protected: true,
        },
        BuiltinWorkflow {
            workflow_id: "test-workflow",
            file_name: "test-workflow.yml",
            contents: include_str!("../workflows/test-workflow.yml"),
            protected: false,
        },
        BuiltinWorkflow {
            workflow_id: "test-timeout-workflow",
            file_name: "test-timeout-workflow.yml",
            contents: include_str!("../workflows/test-timeout-workflow.yml"),
            protected: false,
        },
    ]
}

pub fn is_protected_builtin_workflow(workflow_id: &str) -> bool {
    builtin_workflows()
        .into_iter()
        .any(|builtin| builtin.protected && builtin.workflow_id == workflow_id)
}

fn protected_builtin_workflow(workflow_id: &str) -> Option<BuiltinWorkflow> {
    builtin_workflows()
        .into_iter()
        .find(|builtin| builtin.protected && builtin.workflow_id == workflow_id)
}

fn builtin_workflow_path(workflow_id: &str) -> Result<Option<Utf8PathBuf>> {
    let Some(builtin) = builtin_workflows()
        .into_iter()
        .find(|builtin| builtin.workflow_id == workflow_id)
    else {
        return Ok(None);
    };

    Ok(Some(workflow_config_dir()?.join(builtin.file_name)))
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs};

    use anyhow::Result;
    use camino::Utf8PathBuf;

    use super::{
        NO_ROUTE_ERROR, WorkflowDefinition, WorkflowFileRequest, WorkflowOptionDefinition,
        WorkflowPromptDefinition, WorkflowRequestDefinition, WorkflowRuntimeRequest,
        builtin_workflows, is_protected_builtin_workflow, list_all_workflows, list_workflows,
        load_workflow, load_workflow_from_path, seed_builtin_workflows_if_missing,
        workflow_config_dir,
    };
    use crate::config::configure_test_global_config_home;

    fn with_test_workflow_home(test: impl FnOnce(Utf8PathBuf)) {
        let (home, _guard) = configure_test_global_config_home();
        let workflow_dir = home.join("workflows");
        if workflow_dir.exists() {
            fs::remove_dir_all(&workflow_dir).unwrap();
        }
        fs::create_dir_all(&workflow_dir).unwrap();
        test(home);
    }

    #[test]
    fn seeding_populates_builtin_workflows() {
        with_test_workflow_home(|home| {
            seed_builtin_workflows_if_missing().unwrap();

            let workflow_dir = home.join("workflows");
            assert!(workflow_dir.join("bare.yml").exists());
            assert!(workflow_dir.join("dbv.yml").exists());
            assert!(workflow_dir.join("default.yml").exists());
            assert!(workflow_dir.join("finalize.yml").exists());
            assert!(workflow_dir.join("plan.yml").exists());
            assert!(workflow_dir.join("review.yml").exists());
            assert!(workflow_dir.join("task.yml").exists());
            assert!(workflow_dir.join("test-workflow.yml").exists());
        });
    }

    #[test]
    fn protected_builtins_are_resynced_to_canonical_contents() {
        with_test_workflow_home(|home| {
            let path = home.join("workflows/plan.yml");
            seed_builtin_workflows_if_missing().unwrap();
            fs::write(path.as_std_path(), "version: 1\nworkflow_id: plan\n").unwrap();

            seed_builtin_workflows_if_missing().unwrap();

            let contents = fs::read_to_string(path.as_std_path()).unwrap();
            assert_eq!(contents, include_str!("../workflows/plan.yml"));
        });
    }

    #[test]
    fn protected_builtins_ignore_shadow_copies() {
        with_test_workflow_home(|home| {
            let workflow_dir = home.join("workflows");
            fs::write(
                workflow_dir.join("plan-shadow.yml").as_std_path(),
                r#"
version: 1
workflow_id: plan
title: Shadow Plan
entrypoint: main
prompts:
  main:
    title: Main
    fallback-route: no-route-error
    prompt: shadow
"#,
            )
            .unwrap();

            let workflow = load_workflow("plan").unwrap();
            assert_eq!(
                workflow.source_path(),
                Some(workflow_dir.join("plan.yml").as_ref())
            );
            assert_eq!(workflow.title, "Plan");

            let listed = list_all_workflows().unwrap();
            assert_eq!(
                listed
                    .iter()
                    .filter(|workflow| workflow.workflow_id == "plan")
                    .count(),
                1
            );
        });
    }

    #[test]
    fn protected_builtin_detection_is_limited_to_blessed_workflows() {
        assert!(is_protected_builtin_workflow("plan"));
        assert!(is_protected_builtin_workflow("task"));
        assert!(is_protected_builtin_workflow("review"));
        assert!(is_protected_builtin_workflow("finalize"));
        assert!(!is_protected_builtin_workflow("default"));
        assert!(!is_protected_builtin_workflow("custom"));
    }

    #[test]
    fn list_workflows_reads_only_visible_seeded_builtins() {
        with_test_workflow_home(|_| {
            let workflows = list_workflows().unwrap();
            assert!(
                workflows
                    .iter()
                    .any(|workflow| workflow.workflow_id == "bare")
            );
            assert!(
                workflows
                    .iter()
                    .any(|workflow| workflow.workflow_id == "dbv")
            );
            assert!(
                workflows
                    .iter()
                    .any(|workflow| workflow.workflow_id == "default")
            );
            assert!(
                workflows
                    .iter()
                    .any(|workflow| workflow.workflow_id == "finalize")
            );
            assert!(
                workflows
                    .iter()
                    .any(|workflow| workflow.workflow_id == "plan")
            );
            assert!(
                workflows
                    .iter()
                    .any(|workflow| workflow.workflow_id == "review")
            );
            assert!(
                workflows
                    .iter()
                    .any(|workflow| workflow.workflow_id == "task")
            );
            assert!(
                workflows
                    .iter()
                    .all(|workflow| workflow.workflow_id != "test-workflow")
            );
        });
    }

    #[test]
    fn list_all_workflows_includes_hidden_builtins() {
        with_test_workflow_home(|_| {
            let workflows = list_all_workflows().unwrap();
            assert!(
                workflows
                    .iter()
                    .any(|workflow| workflow.workflow_id == "dbv")
            );
            assert!(
                workflows
                    .iter()
                    .any(|workflow| workflow.workflow_id == "default")
            );
            assert!(
                workflows
                    .iter()
                    .any(|workflow| workflow.workflow_id == "finalize")
            );
            assert!(
                workflows
                    .iter()
                    .any(|workflow| workflow.workflow_id == "plan")
            );
            assert!(
                workflows
                    .iter()
                    .any(|workflow| workflow.workflow_id == "review")
            );
            assert!(
                workflows
                    .iter()
                    .any(|workflow| workflow.workflow_id == "task")
            );
            assert!(
                workflows
                    .iter()
                    .any(|workflow| workflow.workflow_id == "test-workflow")
            );
        });
    }

    #[test]
    fn load_workflow_matches_on_workflow_id() {
        with_test_workflow_home(|home| {
            let workflow_dir = home.join("workflows");
            fs::create_dir_all(workflow_dir.as_std_path()).unwrap();
            fs::write(
                workflow_dir.join("custom-name.yml"),
                r#"
version: 1
workflow_id: custom
title: Custom
entrypoint: main
prompts:
  main:
    title: Main
    fallback-route: no-route-error
    prompt: hello
"#,
            )
            .unwrap();

            let workflow = load_workflow("custom").unwrap();
            assert_eq!(workflow.workflow_id, "custom");
        });
    }

    #[test]
    fn request_token_requires_request_block() {
        let temp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(temp.path().join("example.yml")).unwrap();
        let error = load_workflow_from_path_for_test(
            &path,
            r#"
version: 1
workflow_id: broken
title: Broken
entrypoint: main
prompts:
  main:
    title: Main
    fallback-route: no-route-error
    prompt: "{ralph-request}"
"#,
        )
        .unwrap_err();
        let rendered = format!("{error:#}");

        assert!(rendered.contains("does not define a request block"));
    }

    #[test]
    fn validation_requires_exactly_one_request_source() {
        let workflow = WorkflowDefinition {
            version: 1,
            workflow_id: "broken".to_owned(),
            title: "Broken".to_owned(),
            description: String::new(),
            hidden: false,
            entrypoint: "main".to_owned(),
            options: BTreeMap::new(),
            request: Some(WorkflowRequestDefinition {
                runtime: Some(WorkflowRuntimeRequest {
                    argv: true,
                    stdin: false,
                    file_flag: false,
                }),
                file: Some(WorkflowFileRequest {
                    path: Utf8PathBuf::from("TASKS.md"),
                }),
                inline: None,
            }),
            prompts: BTreeMap::from([(
                "main".to_owned(),
                WorkflowPromptDefinition {
                    title: "Main".to_owned(),
                    fallback_route: NO_ROUTE_ERROR.to_owned(),
                    prompt: Some("hello".to_owned()),
                    parallel: None,
                    transition_guards: BTreeMap::new(),
                },
            )]),
            source_path: None,
        };

        let error = workflow.validate().unwrap_err().to_string();
        assert!(error.contains("exactly one of runtime, file, or inline"));
    }

    #[test]
    fn validation_rejects_removed_run_dir_interpolation() {
        let workflow = WorkflowDefinition {
            version: 1,
            workflow_id: "broken".to_owned(),
            title: "Broken".to_owned(),
            description: String::new(),
            hidden: false,
            entrypoint: "main".to_owned(),
            options: BTreeMap::new(),
            request: None,
            prompts: BTreeMap::from([(
                "main".to_owned(),
                WorkflowPromptDefinition {
                    title: "Main".to_owned(),
                    fallback_route: NO_ROUTE_ERROR.to_owned(),
                    prompt: Some("{ralph-env:RUN_DIR}/progress.txt".to_owned()),
                    parallel: None,
                    transition_guards: BTreeMap::new(),
                },
            )]),
            source_path: None,
        };

        let error = workflow.validate().unwrap_err().to_string();
        assert!(error.contains("unsupported interpolation"));
        assert!(error.contains("{ralph-env:RUN_DIR}"));
    }

    #[test]
    fn validation_rejects_undefined_option_tokens() {
        let workflow = WorkflowDefinition {
            version: 1,
            workflow_id: "broken".to_owned(),
            title: "Broken".to_owned(),
            description: String::new(),
            hidden: false,
            entrypoint: "main".to_owned(),
            options: BTreeMap::new(),
            request: None,
            prompts: BTreeMap::from([(
                "main".to_owned(),
                WorkflowPromptDefinition {
                    title: "Main".to_owned(),
                    fallback_route: NO_ROUTE_ERROR.to_owned(),
                    prompt: Some("{ralph-option:progress-file}".to_owned()),
                    parallel: None,
                    transition_guards: BTreeMap::new(),
                },
            )]),
            source_path: None,
        };

        let error = workflow.validate().unwrap_err().to_string();
        assert!(error.contains("references undefined option 'progress-file'"));
    }

    #[test]
    fn validation_rejects_conflicting_option_flags() {
        let workflow = WorkflowDefinition {
            version: 1,
            workflow_id: "broken".to_owned(),
            title: "Broken".to_owned(),
            description: String::new(),
            hidden: false,
            entrypoint: "main".to_owned(),
            options: BTreeMap::from([
                (
                    "progress-file".to_owned(),
                    WorkflowOptionDefinition {
                        help: String::new(),
                        default: Some("progress.txt".to_owned()),
                        value_name: None,
                    },
                ),
                (
                    "progress_file".to_owned(),
                    WorkflowOptionDefinition {
                        help: String::new(),
                        default: Some("progress-2.txt".to_owned()),
                        value_name: None,
                    },
                ),
            ]),
            request: None,
            prompts: BTreeMap::from([(
                "main".to_owned(),
                WorkflowPromptDefinition {
                    title: "Main".to_owned(),
                    fallback_route: NO_ROUTE_ERROR.to_owned(),
                    prompt: Some("hello".to_owned()),
                    parallel: None,
                    transition_guards: BTreeMap::new(),
                },
            )]),
            source_path: None,
        };

        let error = workflow.validate().unwrap_err().to_string();
        assert!(error.contains("both map to CLI flag '--progressfile'"));
    }

    #[test]
    fn workflow_config_dir_uses_override_when_present() {
        with_test_workflow_home(|home| {
            assert_eq!(workflow_config_dir().unwrap(), home.join("workflows"));
        });
    }

    #[test]
    fn dbv_requires_explicit_plan_coverage_for_request_details() {
        let temp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(temp.path().join("dbv.yml")).unwrap();
        let workflow =
            load_workflow_from_path_for_test(&path, include_str!("../workflows/dbv.yml")).unwrap();

        let dispatch = workflow.prompt("dispatch").expect("dispatch prompt");
        let dispatch_prompt = dispatch.prompt.as_deref().expect("dispatch prompt text");
        assert!(dispatch_prompt.contains(
            "Every material part of the user request, including appended notes, priorities, constraints, and acceptance details, MUST appear explicitly"
        ));
        assert!(dispatch_prompt.contains(
            "If any material part of the request is missing from the plan or only implicitly covered"
        ));

        let decompose = workflow.prompt("decompose").expect("decompose prompt");
        let decompose_prompt = decompose.prompt.as_deref().expect("decompose prompt text");
        assert!(decompose_prompt.contains(
            "Every material part of the user request, including appended notes, priorities, constraints, and acceptance details, MUST become explicit"
        ));
        assert!(decompose_prompt.contains(
            "If any material part of the request is not represented explicitly in the plan"
        ));
    }

    #[test]
    fn plan_workflow_requires_wal_reads_and_uses_planner_specific_state_contract() {
        let temp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(temp.path().join("plan.yml")).unwrap();
        let workflow =
            load_workflow_from_path_for_test(&path, include_str!("../workflows/plan.yml")).unwrap();

        let planner = workflow.prompt("plan").expect("plan prompt");
        let prompt = planner.prompt.as_deref().expect("plan prompt text");
        assert!(
            prompt.contains("execute these exact commands in order to read the planning state")
        );
        assert!(
            prompt
                .contains("those reads are the canonical planning-state inputs for this iteration")
        );
        assert!(prompt.contains(
            "if planning progress or a current draft file already exists, you MUST use that state before deciding what to do next"
        ));
    }

    #[test]
    fn plan_workflow_accepts_all_runtime_request_forms() {
        let temp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(temp.path().join("plan.yml")).unwrap();
        let workflow =
            load_workflow_from_path_for_test(&path, include_str!("../workflows/plan.yml")).unwrap();

        let runtime = workflow
            .request
            .as_ref()
            .and_then(|request| request.runtime.as_ref())
            .expect("plan runtime request");
        assert!(runtime.argv);
        assert!(runtime.stdin);
        assert!(runtime.file_flag);
    }

    #[test]
    fn plan_workflow_allows_question_on_revise_when_feedback_requests_missing_choices() {
        let temp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(temp.path().join("plan.yml")).unwrap();
        let workflow =
            load_workflow_from_path_for_test(&path, include_str!("../workflows/plan.yml")).unwrap();

        let planner = workflow.prompt("plan").expect("plan prompt");
        let prompt = planner.prompt.as_deref().expect("plan prompt text");
        assert!(
            prompt
                .contains("if the feedback asks for missing user choices you cannot infer safely")
        );
        assert!(prompt.contains("emit exactly one `planning-question` instead of a new draft"));
        assert!(prompt.contains(
            "do not emit a fresh draft that ignores the existing review feedback or the latest draft file state"
        ));
    }

    #[test]
    fn task_workflow_defines_stop_ok_transition_guard() {
        let temp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(temp.path().join("task.yml")).unwrap();
        let workflow =
            load_workflow_from_path_for_test(&path, include_str!("../workflows/task.yml")).unwrap();

        let task = workflow.prompt("task").expect("task prompt");
        assert!(task.transition_guards.contains_key("stop-ok"));
    }

    #[test]
    fn builtin_workflows_do_not_use_stale_prompt_contract_fragments() {
        const BANNED_SNIPPETS: &[&str] = &[
            "<<<",
            ">>>",
            "planning-draft",
            "PLAN DRAFT",
            "QUESTIOON",
            "QUESTION markers",
            "payload --channel",
            "signal --channel",
            "always add `--channel",
        ];

        for workflow in builtin_workflows() {
            for banned in BANNED_SNIPPETS {
                assert!(
                    !workflow.contents.contains(banned),
                    "workflow '{}' contains stale prompt fragment '{}'",
                    workflow.workflow_id,
                    banned
                );
            }
        }
    }

    #[test]
    fn transition_guard_routes_must_target_known_prompt_ids() {
        let temp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(temp.path().join("guarded.yml")).unwrap();
        let error = load_workflow_from_path_for_test(
            &path,
            r#"
version: 1
workflow_id: guarded
title: Guarded
entrypoint: main
prompts:
  main:
    title: Main
    fallback-route: no-route-error
    transition-guards:
      stop-ok:
        - type: file_exists
          path: PLAN.md
          on-fail: route
          route: missing
    prompt: hello
"#,
        )
        .unwrap_err();
        let rendered = format!("{error:#}");

        assert!(rendered.contains("route target 'missing' is not a known prompt id"));
    }

    fn load_workflow_from_path_for_test(
        path: &Utf8PathBuf,
        raw: &str,
    ) -> Result<WorkflowDefinition> {
        fs::write(path, raw).unwrap();
        load_workflow_from_path(path)
    }
}
