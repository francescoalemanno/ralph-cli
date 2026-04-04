use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};
use ralph_core::{LastRunStatus, ScaffoldId, TargetConfig, TargetEntrypoint, atomic_write};
use serde::Deserialize;

use crate::{
    RalphApp,
    engine::{load_text_artifact, render_params, resolve_artifact_reference},
};

const BUILTIN_TEMPLATE_REFS: &[&str] = &[
    "builtin://workflows/single_prompt/workflow.toml",
    "builtin://workflows/plan_build/workflow.toml",
    "builtin://workflows/task_driven/workflow.toml",
    "builtin://workflows/plan_driven/workflow.toml",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowTemplateSource {
    Builtin,
    User,
    Project,
}

impl WorkflowTemplateSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowTemplateSummary {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub source: WorkflowTemplateSource,
}

#[derive(Debug, Clone)]
pub(crate) struct LoadedWorkflowTemplate {
    pub(crate) manifest_ref: String,
    pub(crate) source: WorkflowTemplateSource,
    pub(crate) definition: WorkflowTemplateDefinition,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WorkflowTemplateDefinition {
    pub(crate) version: u32,
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) description: Option<String>,
    #[serde(default)]
    pub(crate) scaffold: Option<ScaffoldId>,
    #[serde(default)]
    pub(crate) default_entrypoint: Option<String>,
    #[serde(default)]
    pub(crate) params: BTreeMap<String, WorkflowTemplateParam>,
    #[serde(default)]
    pub(crate) entrypoints: Vec<WorkflowTemplateEntrypoint>,
    #[serde(default)]
    pub(crate) materialize: Vec<WorkflowMaterialize>,
}

impl WorkflowTemplateDefinition {
    fn validate(&self) -> Result<()> {
        if self.version != 1 {
            return Err(anyhow!(
                "unsupported workflow template version {}; only version 1 is supported",
                self.version
            ));
        }
        if self.id.trim().is_empty() {
            return Err(anyhow!("workflow template id cannot be empty"));
        }
        if self.name.trim().is_empty() {
            return Err(anyhow!("workflow template name cannot be empty"));
        }
        for (param_name, param) in &self.params {
            if param_name.trim().is_empty() {
                return Err(anyhow!(
                    "workflow template '{}' has an empty param key",
                    self.id
                ));
            }
            if let Some(value_type) = &param.value_type {
                match value_type.trim() {
                    "string" | "file" | "dir" => {}
                    other => {
                        return Err(anyhow!(
                            "workflow template '{}' param '{}' has unsupported type '{}'",
                            self.id,
                            param_name,
                            other
                        ));
                    }
                }
            }
            if let Some(label) = &param.label
                && label.trim().is_empty()
            {
                return Err(anyhow!(
                    "workflow template '{}' param '{}' has an empty label",
                    self.id,
                    param_name
                ));
            }
            if let Some(description) = &param.description
                && description.trim().is_empty()
            {
                return Err(anyhow!(
                    "workflow template '{}' param '{}' has an empty description",
                    self.id,
                    param_name
                ));
            }
        }
        if self.entrypoints.is_empty() {
            return Err(anyhow!(
                "workflow template '{}' must define at least one entrypoint",
                self.id
            ));
        }

        let mut entrypoint_ids = BTreeSet::new();
        for entrypoint in &self.entrypoints {
            let id = entrypoint.id();
            if id.trim().is_empty() {
                return Err(anyhow!("workflow template entrypoint id cannot be empty"));
            }
            if !entrypoint_ids.insert(id.to_owned()) {
                return Err(anyhow!(
                    "workflow template '{}' has duplicate entrypoint '{}'",
                    self.id,
                    id
                ));
            }
        }

        if let Some(default_entrypoint) = &self.default_entrypoint
            && !self
                .entrypoints
                .iter()
                .any(|entrypoint| entrypoint.id() == default_entrypoint)
        {
            return Err(anyhow!(
                "workflow template '{}' default entrypoint '{}' does not exist",
                self.id,
                default_entrypoint
            ));
        }

        for file in self.materialize.iter().filter_map(|item| match item {
            WorkflowMaterialize::File {
                path,
                source,
                contents,
            } => Some((path, source, contents)),
            WorkflowMaterialize::Dir { .. } => None,
        }) {
            let (path, source, contents) = file;
            if path.trim().is_empty() {
                return Err(anyhow!(
                    "workflow template materialized file path cannot be empty"
                ));
            }
            match (source, contents) {
                (Some(_), Some(_)) => {
                    return Err(anyhow!(
                        "workflow template '{}' file '{}' cannot define both source and contents",
                        self.id,
                        path
                    ));
                }
                (None, None) => {
                    return Err(anyhow!(
                        "workflow template '{}' file '{}' must define source or contents",
                        self.id,
                        path
                    ));
                }
                _ => {}
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WorkflowTemplateParam {
    #[serde(default, rename = "type")]
    pub(crate) value_type: Option<String>,
    #[serde(default)]
    pub(crate) label: Option<String>,
    #[serde(default)]
    pub(crate) description: Option<String>,
    #[serde(default, rename = "default")]
    pub(crate) default_value: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum WorkflowTemplateEntrypoint {
    Prompt {
        id: String,
        path: String,
        #[serde(default)]
        hidden: bool,
        #[serde(default)]
        edit_path: Option<String>,
    },
    Flow {
        id: String,
        flow: String,
        #[serde(default)]
        hidden: bool,
        #[serde(default)]
        edit_path: Option<String>,
    },
}

impl WorkflowTemplateEntrypoint {
    fn id(&self) -> &str {
        match self {
            Self::Prompt { id, .. } | Self::Flow { id, .. } => id,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum WorkflowMaterialize {
    File {
        path: String,
        #[serde(default)]
        source: Option<String>,
        #[serde(default)]
        contents: Option<String>,
    },
    Dir {
        path: String,
    },
}

impl<R> RalphApp<R> {
    pub fn list_workflow_templates(&self) -> Result<Vec<WorkflowTemplateSummary>> {
        discover_workflow_templates(&self.project_dir).map(|templates| {
            templates
                .into_iter()
                .map(|template| WorkflowTemplateSummary {
                    id: template.definition.id,
                    name: template.definition.name,
                    description: template.definition.description,
                    source: template.source,
                })
                .collect()
        })
    }

    pub fn create_target_from_template(
        &self,
        target_id: &str,
        template_id: &str,
    ) -> Result<ralph_core::TargetSummary> {
        let template = find_workflow_template(&self.project_dir, template_id)?;
        let params = resolve_template_params(&template.definition, &BTreeMap::new())?;
        let paths = self.store.target_paths(target_id)?;
        if paths.dir.exists() {
            return Err(anyhow!("target '{target_id}' already exists"));
        }

        let targets_dir = paths
            .dir
            .parent()
            .ok_or_else(|| anyhow!("target directory has no parent"))?;
        fs::create_dir_all(targets_dir)
            .with_context(|| format!("failed to create {}", targets_dir))?;
        fs::create_dir_all(&paths.dir)
            .with_context(|| format!("failed to create target directory {}", paths.dir))?;

        materialize_template(
            &self.project_dir,
            &paths.dir,
            &template.manifest_ref,
            &template.definition,
            &params,
        )?;

        let config = build_target_config(target_id, &template, &params)?;
        self.store.write_target_config(&config)?;
        self.store.load_target(target_id)
    }
}

fn build_target_config(
    target_id: &str,
    template: &LoadedWorkflowTemplate,
    params: &BTreeMap<String, String>,
) -> Result<TargetConfig> {
    let entrypoints = template
        .definition
        .entrypoints
        .iter()
        .map(|entrypoint| match entrypoint {
            WorkflowTemplateEntrypoint::Prompt {
                id,
                path,
                hidden,
                edit_path,
            } => Ok(TargetEntrypoint::Prompt {
                id: render_params(id, params),
                path: resolve_artifact_reference(
                    Some(&template.manifest_ref),
                    &render_params(path, params),
                )?,
                hidden: *hidden,
                edit_path: edit_path
                    .as_deref()
                    .map(|path| render_params(path, params))
                    .map(|path| resolve_artifact_reference(Some(&template.manifest_ref), &path))
                    .transpose()?,
            }),
            WorkflowTemplateEntrypoint::Flow {
                id,
                flow,
                hidden,
                edit_path,
            } => Ok(TargetEntrypoint::Flow {
                id: render_params(id, params),
                flow: resolve_artifact_reference(
                    Some(&template.manifest_ref),
                    &render_params(flow, params),
                )?,
                params: params.clone(),
                hidden: *hidden,
                edit_path: edit_path
                    .as_deref()
                    .map(|path| render_params(path, params))
                    .map(|path| resolve_artifact_reference(Some(&template.manifest_ref), &path))
                    .transpose()?,
            }),
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(TargetConfig {
        id: target_id.to_owned(),
        scaffold: template.definition.scaffold,
        template: Some(template.definition.id.clone()),
        default_entrypoint: template.definition.default_entrypoint.clone().or_else(|| {
            entrypoints
                .first()
                .map(|entrypoint| entrypoint.id().to_owned())
        }),
        entrypoints,
        runtime: None,
        created_at: Some(current_unix_timestamp()),
        max_iterations: None,
        last_prompt: None,
        last_run_status: LastRunStatus::NeverRun,
    })
}

fn materialize_template(
    project_dir: &Utf8Path,
    target_dir: &Utf8Path,
    manifest_ref: &str,
    definition: &WorkflowTemplateDefinition,
    params: &BTreeMap<String, String>,
) -> Result<()> {
    for item in &definition.materialize {
        match item {
            WorkflowMaterialize::Dir { path } => {
                let path = target_dir.join(render_params(path, params));
                fs::create_dir_all(&path).with_context(|| format!("failed to create {}", path))?;
            }
            WorkflowMaterialize::File {
                path,
                source,
                contents,
            } => {
                let rendered_path = render_params(path, params);
                let path = target_dir.join(rendered_path);
                let rendered_contents = match (source, contents) {
                    (Some(source), None) => {
                        let rendered_source = render_params(source, params);
                        render_params(
                            &load_text_artifact(
                                project_dir,
                                target_dir,
                                &rendered_source,
                                Some(manifest_ref),
                            )?,
                            params,
                        )
                    }
                    (None, Some(contents)) => render_params(contents, params),
                    _ => unreachable!("validated materialize spec"),
                };
                atomic_write(&path, rendered_contents)
                    .with_context(|| format!("failed to write {path}"))?;
            }
        }
    }

    Ok(())
}

fn resolve_template_params(
    definition: &WorkflowTemplateDefinition,
    overrides: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    for key in overrides.keys() {
        if !definition.params.contains_key(key) {
            return Err(anyhow!(
                "workflow template '{}' does not define parameter '{}'",
                definition.id,
                key
            ));
        }
    }

    let mut params = BTreeMap::new();
    for (key, spec) in &definition.params {
        let value = overrides
            .get(key)
            .cloned()
            .or_else(|| spec.default_value.clone())
            .ok_or_else(|| {
                anyhow!(
                    "workflow template '{}' requires parameter '{}' to have a default value",
                    definition.id,
                    key
                )
            })?;
        params.insert(key.clone(), value);
    }
    Ok(params)
}

pub(crate) fn discover_workflow_templates(
    project_dir: &Utf8Path,
) -> Result<Vec<LoadedWorkflowTemplate>> {
    let mut templates = BTreeMap::new();

    for manifest_ref in BUILTIN_TEMPLATE_REFS {
        let template =
            load_workflow_template(project_dir, manifest_ref, WorkflowTemplateSource::Builtin)?;
        templates.insert(template.definition.id.clone(), template);
    }

    for template in discover_fs_workflow_templates(
        project_dir,
        WorkflowTemplateSource::User,
        user_workflows_root()?,
        "user://workflows",
    )? {
        templates.insert(template.definition.id.clone(), template);
    }

    for template in discover_fs_workflow_templates(
        project_dir,
        WorkflowTemplateSource::Project,
        Some(project_dir.join(".ralph/workflows")),
        "project://workflows",
    )? {
        templates.insert(template.definition.id.clone(), template);
    }

    let mut templates = templates.into_values().collect::<Vec<_>>();
    templates.sort_by(|left, right| {
        left.definition
            .name
            .to_ascii_lowercase()
            .cmp(&right.definition.name.to_ascii_lowercase())
            .then_with(|| left.definition.id.cmp(&right.definition.id))
    });
    Ok(templates)
}

pub(crate) fn find_workflow_template(
    project_dir: &Utf8Path,
    template_id: &str,
) -> Result<LoadedWorkflowTemplate> {
    discover_workflow_templates(project_dir)?
        .into_iter()
        .find(|template| template.definition.id == template_id)
        .ok_or_else(|| anyhow!("workflow template '{}' does not exist", template_id))
}

fn load_workflow_template(
    project_dir: &Utf8Path,
    manifest_ref: &str,
    source: WorkflowTemplateSource,
) -> Result<LoadedWorkflowTemplate> {
    let raw = load_text_artifact(project_dir, project_dir, manifest_ref, None)
        .with_context(|| format!("failed to load workflow template '{}'", manifest_ref))?;
    let definition: WorkflowTemplateDefinition = toml::from_str(&raw)
        .with_context(|| format!("failed to parse workflow template '{}'", manifest_ref))?;
    definition.validate()?;
    Ok(LoadedWorkflowTemplate {
        manifest_ref: manifest_ref.to_owned(),
        source,
        definition,
    })
}

fn discover_fs_workflow_templates(
    project_dir: &Utf8Path,
    source: WorkflowTemplateSource,
    root: Option<Utf8PathBuf>,
    root_ref: &str,
) -> Result<Vec<LoadedWorkflowTemplate>> {
    let Some(root) = root else {
        return Ok(Vec::new());
    };
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut templates = Vec::new();
    for entry in fs::read_dir(&root).with_context(|| format!("failed to read {}", root))? {
        let entry = entry?;
        let path = Utf8PathBuf::from_path_buf(entry.path())
            .map_err(|_| anyhow!("non-UTF8 workflow template path under {}", root))?;
        if !path.is_dir() {
            continue;
        }
        let Some(dir_name) = path.file_name() else {
            continue;
        };
        let manifest = path.join("workflow.toml");
        if !manifest.is_file() {
            continue;
        }
        let manifest_ref = format!("{root_ref}/{dir_name}/workflow.toml");
        templates.push(load_workflow_template(project_dir, &manifest_ref, source)?);
    }
    Ok(templates)
}

fn user_workflows_root() -> Result<Option<Utf8PathBuf>> {
    let Some(config_path) = ralph_core::AppConfig::user_config_path()? else {
        return Ok(None);
    };
    Ok(config_path.parent().map(|parent| parent.join("workflows")))
}

fn current_unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use anyhow::Result;
    use camino::Utf8PathBuf;

    use crate::RalphApp;

    use super::{WorkflowTemplateSource, discover_workflow_templates, find_workflow_template};

    #[test]
    fn builtin_templates_are_discoverable() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();

        let templates = discover_workflow_templates(&project_dir)?;
        let ids = templates
            .iter()
            .map(|template| template.definition.id.as_str())
            .collect::<Vec<_>>();

        assert!(ids.contains(&"single_prompt"));
        assert!(ids.contains(&"plan_build"));
        assert!(ids.contains(&"task_driven"));
        assert!(ids.contains(&"plan_driven"));
        Ok(())
    }

    #[test]
    fn user_templates_override_builtin_ids() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let config_root = project_dir.join(".test-config");
        fs::create_dir_all(config_root.join("ralph/workflows/task_driven"))?;
        fs::write(
            config_root.join("ralph/config.toml"),
            "selected_agent = \"codex\"\n",
        )?;
        fs::write(
            config_root.join("ralph/workflows/task_driven/workflow.toml"),
            "version = 1\nid = \"task_driven\"\nname = \"Custom Task\"\ndefault_entrypoint = \"main\"\n\n[[entrypoints]]\nid = \"main\"\nkind = \"prompt\"\npath = \"prompt_main.md\"\n\n[[materialize]]\nkind = \"file\"\npath = \"prompt_main.md\"\ncontents = \"# Prompt\\n\"\n",
        )?;

        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", config_root.as_str());
        }

        let template = find_workflow_template(&project_dir, "task_driven")?;
        assert_eq!(template.source, WorkflowTemplateSource::User);
        assert_eq!(template.definition.name, "Custom Task");
        Ok(())
    }

    #[test]
    fn app_can_create_target_from_project_workflow_template() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let workflow_dir = project_dir.join(".ralph/workflows/release_loop");
        fs::create_dir_all(workflow_dir.join("templates"))?;
        fs::write(
            workflow_dir.join("workflow.toml"),
            "version = 1\nid = \"release_loop\"\nname = \"Release Loop\"\ndefault_entrypoint = \"main\"\n\n[[entrypoints]]\nid = \"main\"\nkind = \"prompt\"\npath = \"prompt_main.md\"\nedit_path = \"prompt_main.md\"\n\n[[materialize]]\nkind = \"file\"\npath = \"prompt_main.md\"\nsource = \"self://templates/prompt_main.md\"\n",
        )?;
        fs::write(
            workflow_dir.join("templates/prompt_main.md"),
            "# Prompt\n\nShip it.\n",
        )?;

        let app = RalphApp::load(project_dir.clone())?;
        let summary = app.create_target_from_template("demo", "release_loop")?;

        assert_eq!(summary.template.as_deref(), Some("release_loop"));
        assert_eq!(summary.default_entrypoint.as_deref(), Some("main"));
        assert_eq!(
            fs::read_to_string(project_dir.join(".ralph/targets/demo/prompt_main.md"))?,
            "# Prompt\n\nShip it.\n"
        );
        Ok(())
    }
}
