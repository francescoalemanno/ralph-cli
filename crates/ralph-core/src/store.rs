use std::{
    fs,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};

use crate::{
    EntrypointKind, LastRunStatus, PlanDrivenWorkflowState, PromptFile, ScaffoldId, TargetConfig,
    TargetEntrypoint, TargetFile, TargetFileContents, TargetPaths, TargetReview, TargetSummary,
    WorkflowMode, atomic_write, scaffold::materialize_target_scaffold,
};

pub(crate) const ARTIFACT_DIR_NAME: &str = ".ralph";
const TARGETS_DIR_NAME: &str = "targets";

#[derive(Debug, Clone)]
pub struct TargetStore {
    project_dir: Utf8PathBuf,
}

impl TargetStore {
    fn fallback_target_config(&self, target_id: &str) -> TargetConfig {
        TargetConfig {
            id: target_id.to_owned(),
            scaffold: None,
            default_entrypoint: None,
            entrypoints: Vec::new(),
            runtime: None,
            mode: None,
            workflow: None,
            inflight: None,
            created_at: None,
            max_iterations: None,
            last_prompt: None,
            last_run_status: LastRunStatus::NeverRun,
        }
    }

    fn new_target_config(&self, target_id: &str, scaffold: Option<ScaffoldId>) -> TargetConfig {
        let mode = match scaffold {
            Some(ScaffoldId::TaskDriven) => Some(WorkflowMode::TaskDriven),
            Some(ScaffoldId::PlanDriven) => Some(WorkflowMode::PlanDriven),
            _ => None,
        };
        TargetConfig {
            id: target_id.to_owned(),
            scaffold,
            default_entrypoint: default_entrypoint_for_scaffold(scaffold),
            entrypoints: default_entrypoints_for_scaffold(scaffold),
            runtime: None,
            mode,
            workflow: mode.map(workflow_state_for_mode),
            inflight: None,
            created_at: Some(current_unix_timestamp()),
            max_iterations: None,
            last_prompt: None,
            last_run_status: LastRunStatus::NeverRun,
        }
    }

    pub fn new(project_dir: impl Into<Utf8PathBuf>) -> Self {
        Self {
            project_dir: project_dir.into(),
        }
    }

    fn ralph_dir(&self) -> Utf8PathBuf {
        self.project_dir.join(ARTIFACT_DIR_NAME)
    }

    fn targets_dir(&self) -> Utf8PathBuf {
        self.ralph_dir().join(TARGETS_DIR_NAME)
    }

    fn ensure_targets_dir(&self) -> Result<()> {
        fs::create_dir_all(self.targets_dir())
            .with_context(|| format!("failed to create {}", self.targets_dir()))?;
        Ok(())
    }

    fn validate_target_id(&self, target_id: &str) -> Result<()> {
        let trimmed = target_id.trim();
        if trimmed.is_empty() {
            return Err(anyhow!("target id cannot be empty"));
        }
        if trimmed.contains('/') || trimmed.contains('\\') {
            return Err(anyhow!("target id cannot contain path separators"));
        }
        if trimmed.starts_with('.') {
            return Err(anyhow!("target id cannot start with '.'"));
        }
        Ok(())
    }

    fn target_dir(&self, target_id: &str) -> Utf8PathBuf {
        self.targets_dir().join(target_id)
    }

    pub fn target_paths(&self, target_id: &str) -> Result<TargetPaths> {
        self.validate_target_id(target_id)?;
        let dir = self.target_dir(target_id);
        Ok(TargetPaths {
            config_path: dir.join("target.toml"),
            dir,
        })
    }

    pub fn create_target(
        &self,
        target_id: &str,
        scaffold: Option<ScaffoldId>,
    ) -> Result<TargetSummary> {
        self.ensure_targets_dir()?;
        let paths = self.target_paths(target_id)?;
        if paths.dir.exists() {
            return Err(anyhow!("target '{target_id}' already exists"));
        }
        fs::create_dir_all(&paths.dir)
            .with_context(|| format!("failed to create target directory {}", paths.dir))?;

        let config = self.new_target_config(target_id, scaffold);
        self.write_target_config(&config)?;

        materialize_target_scaffold(&paths.dir, scaffold.unwrap_or(ScaffoldId::SinglePrompt))?;

        self.load_target(target_id)
    }

    pub fn delete_target(&self, target_id: &str) -> Result<()> {
        let paths = self.target_paths(target_id)?;
        if !paths.dir.exists() {
            return Ok(());
        }
        fs::remove_dir_all(&paths.dir)
            .with_context(|| format!("failed to remove target directory {}", paths.dir))
    }

    pub fn load_target(&self, target_id: &str) -> Result<TargetSummary> {
        let paths = self.target_paths(target_id)?;
        if !paths.dir.exists() {
            return Err(anyhow!("target '{target_id}' does not exist"));
        }
        let config = self.read_target_config_for_discovery(target_id)?;
        let files = self.list_target_files_with_config(target_id, &config)?;
        let prompt_files = files
            .iter()
            .filter(|file| file.is_prompt)
            .map(|file| PromptFile {
                name: file.name.clone(),
                path: file.path.clone(),
            })
            .collect::<Vec<_>>();

        Ok(TargetSummary {
            id: config.id.clone(),
            dir: paths.dir,
            prompt_files,
            files,
            scaffold: config.scaffold,
            default_entrypoint: resolved_default_entrypoint(&config),
            flow_entrypoints: resolved_flow_entrypoints(&config),
            mode: config.mode,
            created_at: config.created_at,
            last_prompt: config.last_prompt,
            last_run_status: config.last_run_status,
        })
    }

    pub fn review_target(&self, target_id: &str) -> Result<TargetReview> {
        let summary = self.load_target(target_id)?;
        let files = summary
            .files
            .iter()
            .map(|file| {
                Ok(TargetFileContents {
                    name: file.name.clone(),
                    path: file.path.clone(),
                    contents: self.read_file(&file.path)?,
                    is_prompt: file.is_prompt,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(TargetReview { summary, files })
    }

    pub fn list_targets(&self) -> Result<Vec<TargetSummary>> {
        self.ensure_targets_dir()?;
        let mut summaries = Vec::new();
        for entry in fs::read_dir(self.targets_dir())
            .with_context(|| format!("failed to read {}", self.targets_dir()))?
        {
            let entry = entry?;
            let path = Utf8PathBuf::from_path_buf(entry.path())
                .map_err(|_| anyhow!("non-UTF8 target path under {}", self.targets_dir()))?;
            if !path.is_dir() {
                continue;
            }
            if let Some(target_id) = path.file_name() {
                summaries.push(self.load_target(target_id)?);
            }
        }
        summaries.sort_by(|left, right| {
            right
                .created_at
                .unwrap_or(0)
                .cmp(&left.created_at.unwrap_or(0))
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(summaries)
    }

    pub fn read_target_config(&self, target_id: &str) -> Result<TargetConfig> {
        self.read_target_config_internal(target_id, true)
    }

    fn read_target_config_for_discovery(&self, target_id: &str) -> Result<TargetConfig> {
        self.read_target_config_internal(target_id, false)
    }

    fn read_target_config_internal(
        &self,
        target_id: &str,
        strict_parse: bool,
    ) -> Result<TargetConfig> {
        let paths = self.target_paths(target_id)?;
        let raw = match fs::read_to_string(&paths.config_path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(self.fallback_target_config(target_id));
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to read target config {}", paths.config_path)
                });
            }
        };
        let mut config = match toml::from_str(&raw) {
            Ok(config) => config,
            Err(_) if !strict_parse => self.fallback_target_config(target_id),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to parse target config {}", paths.config_path)
                });
            }
        };
        config.id = target_id.to_owned();
        Ok(config)
    }

    pub fn write_target_config(&self, config: &TargetConfig) -> Result<()> {
        let paths = self.target_paths(&config.id)?;
        let raw = toml::to_string_pretty(config).context("failed to serialize target config")?;
        atomic_write(&paths.config_path, raw)
            .with_context(|| format!("failed to write target config {}", paths.config_path))?;
        Ok(())
    }

    pub fn set_last_run(
        &self,
        target_id: &str,
        prompt_name: &str,
        status: LastRunStatus,
    ) -> Result<()> {
        let mut config = self.read_target_config(target_id)?;
        config.last_prompt = Some(prompt_name.to_owned());
        config.last_run_status = status;
        self.write_target_config(&config)
    }

    fn list_target_files_with_config(
        &self,
        target_id: &str,
        config: &TargetConfig,
    ) -> Result<Vec<TargetFile>> {
        let paths = self.target_paths(target_id)?;
        let mut files = Vec::new();
        for entry in fs::read_dir(&paths.dir)
            .with_context(|| format!("failed to read target directory {}", paths.dir))?
        {
            let entry = entry?;
            let path = Utf8PathBuf::from_path_buf(entry.path())
                .map_err(|_| anyhow!("non-UTF8 target file under {}", paths.dir))?;
            if !path.is_file() {
                continue;
            }
            let name = path
                .file_name()
                .ok_or_else(|| anyhow!("target file missing file name"))?
                .to_owned();
            files.push(TargetFile {
                is_prompt: is_target_prompt_file(config, &name),
                name,
                path,
            });
        }
        files.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(files)
    }

    pub fn read_file(&self, path: &Utf8Path) -> Result<String> {
        fs::read_to_string(path).with_context(|| format!("failed to read {path}"))
    }
}

fn is_prompt_file_name(name: &str) -> bool {
    name.ends_with(".md")
}

fn current_unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn is_target_prompt_file(config: &TargetConfig, name: &str) -> bool {
    if config.entrypoints.iter().any(
        |entrypoint| matches!(entrypoint, TargetEntrypoint::Prompt { path, .. } if path == name),
    ) {
        return true;
    }

    if config.uses_hidden_workflow() && config.entrypoints.is_empty() {
        return false;
    }

    match config.mode {
        Some(WorkflowMode::TaskDriven) => false,
        Some(WorkflowMode::PlanDriven) => false,
        None => is_prompt_file_name(name),
    }
}

fn default_entrypoint_for_scaffold(scaffold: Option<ScaffoldId>) -> Option<String> {
    match scaffold {
        Some(ScaffoldId::TaskDriven | ScaffoldId::PlanDriven) => Some("main".to_owned()),
        _ => None,
    }
}

fn default_entrypoints_for_scaffold(scaffold: Option<ScaffoldId>) -> Vec<TargetEntrypoint> {
    match scaffold {
        Some(ScaffoldId::PlanDriven) => vec![TargetEntrypoint::Flow {
            id: "main".to_owned(),
            flow: "builtin://flows/plan_driven.toml".to_owned(),
            params: std::collections::BTreeMap::from([
                ("goal_file".to_owned(), "GOAL.md".to_owned()),
                ("derived_file".to_owned(), "plan.toml".to_owned()),
                ("specs_dir".to_owned(), "specs".to_owned()),
                ("journal_file".to_owned(), "journal.txt".to_owned()),
                ("archive_prefix".to_owned(), "goal_rebuild".to_owned()),
            ]),
            hidden: false,
            edit_path: Some("GOAL.md".to_owned()),
        }],
        Some(ScaffoldId::TaskDriven) => vec![TargetEntrypoint::Flow {
            id: "main".to_owned(),
            flow: "builtin://flows/task_driven.toml".to_owned(),
            params: std::collections::BTreeMap::from([
                ("goal_file".to_owned(), "GOAL.md".to_owned()),
                ("derived_file".to_owned(), "progress.toml".to_owned()),
                ("journal_file".to_owned(), "journal.txt".to_owned()),
                ("archive_prefix".to_owned(), "task_rebuild".to_owned()),
            ]),
            hidden: false,
            edit_path: Some("GOAL.md".to_owned()),
        }],
        _ => Vec::new(),
    }
}

fn resolved_default_entrypoint(config: &TargetConfig) -> Option<String> {
    if let Some(default_entrypoint) = &config.default_entrypoint {
        return Some(default_entrypoint.clone());
    }

    if let Some(entrypoint) = config.entrypoints.first() {
        return Some(entrypoint.id().to_owned());
    }

    if config.mode.is_some() {
        return Some("main".to_owned());
    }

    None
}

fn resolved_flow_entrypoints(config: &TargetConfig) -> Vec<String> {
    if !config.entrypoints.is_empty() {
        return config
            .entrypoints
            .iter()
            .filter(|entrypoint| entrypoint.kind() == EntrypointKind::Flow && !entrypoint.hidden())
            .map(|entrypoint| entrypoint.id().to_owned())
            .collect();
    }

    match config.mode {
        Some(WorkflowMode::TaskDriven | WorkflowMode::PlanDriven) => vec!["main".to_owned()],
        None => Vec::new(),
    }
}

fn workflow_state_for_mode(mode: WorkflowMode) -> PlanDrivenWorkflowState {
    PlanDrivenWorkflowState {
        phase: match mode {
            WorkflowMode::TaskDriven => crate::PlanDrivenPhase::Build,
            WorkflowMode::PlanDriven => crate::PlanDrivenPhase::Plan,
        },
        ..PlanDrivenWorkflowState::default()
    }
}

#[cfg(test)]
mod tests {
    use super::{TargetStore, is_prompt_file_name};
    use crate::{LastRunStatus, PlanDrivenPhase, ScaffoldId, WorkflowMode};

    #[test]
    fn prompt_file_discovery_accepts_any_target_local_md_file() {
        assert!(is_prompt_file_name("0_plan.md"));
        assert!(is_prompt_file_name("notes.md"));
        assert!(is_prompt_file_name("review.md"));
        assert!(is_prompt_file_name(".notes.md"));
        assert!(!is_prompt_file_name("README.MD"));
        assert!(!is_prompt_file_name("target.toml"));
    }

    #[test]
    fn plan_build_scaffold_creates_both_prompts() {
        let temp = tempfile::tempdir().unwrap();
        let store = TargetStore::new(
            camino::Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap(),
        );

        let summary = store
            .create_target("demo", Some(ScaffoldId::PlanBuild))
            .unwrap();
        let prompt_names = summary
            .prompt_files
            .iter()
            .map(|prompt| prompt.name.as_str())
            .collect::<Vec<_>>();

        assert_eq!(prompt_names, vec!["0_plan.md", "1_build.md"]);
        assert!(
            !summary
                .files
                .iter()
                .any(|file| file.name == "IMPLEMENTATION_PLAN.md")
        );
        assert_eq!(summary.last_run_status, LastRunStatus::NeverRun);

        let plan_prompt =
            std::fs::read_to_string(store.target_paths("demo").unwrap().dir.join("0_plan.md"))
                .unwrap();
        let build_prompt =
            std::fs::read_to_string(store.target_paths("demo").unwrap().dir.join("1_build.md"))
                .unwrap();
        assert!(
            plan_prompt.contains("ULTIMATE GOAL - We want to achieve:\n[project-specific goal].")
        );
        assert!(!plan_prompt.contains("Update [project-specific goal] placeholder below."));
        assert!(!build_prompt.contains("[project-specific goal]"));
    }

    #[test]
    fn create_target_defaults_to_single_prompt_scaffold() {
        let temp = tempfile::tempdir().unwrap();
        let store = TargetStore::new(
            camino::Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap(),
        );

        let summary = store.create_target("demo", None).unwrap();
        let prompt_names = summary
            .prompt_files
            .iter()
            .map(|prompt| prompt.name.as_str())
            .collect::<Vec<_>>();

        assert_eq!(prompt_names, vec!["prompt_main.md"]);
        assert!(summary.created_at.is_some());
    }

    #[test]
    fn plan_driven_scaffold_creates_goal_file_and_state() {
        let temp = tempfile::tempdir().unwrap();
        let store = TargetStore::new(
            camino::Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap(),
        );

        let summary = store
            .create_target("demo", Some(ScaffoldId::PlanDriven))
            .unwrap();

        assert!(summary.prompt_files.is_empty());
        assert_eq!(summary.mode, Some(WorkflowMode::PlanDriven));
        assert!(summary.files.iter().any(|file| file.name == "GOAL.md"));
        assert!(summary.files.iter().all(|file| !file.is_prompt));
        assert!(
            store
                .target_paths("demo")
                .unwrap()
                .dir
                .join("specs")
                .is_dir()
        );

        let config = store.read_target_config("demo").unwrap();
        assert_eq!(config.mode, Some(WorkflowMode::PlanDriven));
        assert_eq!(
            config.workflow.as_ref().map(|workflow| workflow.phase),
            Some(PlanDrivenPhase::Plan)
        );
    }

    #[test]
    fn task_driven_scaffold_creates_goal_and_progress_files() {
        let temp = tempfile::tempdir().unwrap();
        let store = TargetStore::new(
            camino::Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap(),
        );

        let summary = store
            .create_target("demo", Some(ScaffoldId::TaskDriven))
            .unwrap();

        assert!(summary.prompt_files.is_empty());
        assert_eq!(summary.mode, Some(WorkflowMode::TaskDriven));
        assert!(summary.files.iter().any(|file| file.name == "GOAL.md"));
        assert!(
            summary
                .files
                .iter()
                .any(|file| file.name == "progress.toml")
        );
        assert!(summary.files.iter().all(|file| !file.is_prompt));

        let config = store.read_target_config("demo").unwrap();
        assert_eq!(config.mode, Some(WorkflowMode::TaskDriven));
        assert_eq!(
            config.workflow.as_ref().map(|workflow| workflow.phase),
            Some(PlanDrivenPhase::Build)
        );
        let progress = std::fs::read_to_string(
            store
                .target_paths("demo")
                .unwrap()
                .dir
                .join("progress.toml"),
        )
        .unwrap();
        assert!(progress.contains("description = \"Planning phase\""));
    }

    #[test]
    fn target_directories_are_discovered_even_without_valid_target_config() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = camino::Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let store = TargetStore::new(project_dir.clone());
        let target_dir = project_dir.join(".ralph/targets/awesome-feat-X");
        std::fs::create_dir_all(&target_dir).unwrap();
        std::fs::write(target_dir.join("notes.md"), "# Notes\n").unwrap();
        std::fs::write(target_dir.join("target.toml"), "scaffold = \"playbook\"\n").unwrap();

        let targets = store.list_targets().unwrap();

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].id, "awesome-feat-X");
        assert_eq!(targets[0].created_at, None);
        assert_eq!(
            targets[0]
                .prompt_files
                .iter()
                .map(|prompt| prompt.name.as_str())
                .collect::<Vec<_>>(),
            vec!["notes.md"]
        );
    }

    #[test]
    fn load_target_tolerates_invalid_target_config_during_discovery() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = camino::Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let store = TargetStore::new(project_dir.clone());
        let target_dir = project_dir.join(".ralph/targets/demo");
        std::fs::create_dir_all(&target_dir).unwrap();
        std::fs::write(target_dir.join("notes.md"), "# Notes\n").unwrap();
        std::fs::write(target_dir.join("target.toml"), "scaffold = \"playbook\"\n").unwrap();

        let summary = store.load_target("demo").unwrap();

        assert_eq!(summary.id, "demo");
        assert_eq!(
            summary
                .prompt_files
                .iter()
                .map(|prompt| prompt.name.as_str())
                .collect::<Vec<_>>(),
            vec!["notes.md"]
        );
    }

    #[test]
    fn invalid_target_config_is_rejected_for_operational_reads() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = camino::Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let store = TargetStore::new(project_dir.clone());
        let target_dir = project_dir.join(".ralph/targets/demo");
        std::fs::create_dir_all(&target_dir).unwrap();
        std::fs::write(target_dir.join("target.toml"), "scaffold = \"playbook\"\n").unwrap();

        let error = store.read_target_config("demo").unwrap_err().to_string();

        assert!(error.contains("failed to parse target config"));
    }

    #[test]
    fn invalid_target_config_is_not_silently_overwritten_by_set_last_run() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = camino::Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let store = TargetStore::new(project_dir.clone());
        let target_dir = project_dir.join(".ralph/targets/demo");
        std::fs::create_dir_all(&target_dir).unwrap();
        let config_path = target_dir.join("target.toml");
        let invalid = "scaffold = \"playbook\"\n";
        std::fs::write(&config_path, invalid).unwrap();

        let error = store
            .set_last_run("demo", "prompt_main.md", LastRunStatus::Completed)
            .unwrap_err()
            .to_string();

        assert!(error.contains("failed to parse target config"));
        assert_eq!(std::fs::read_to_string(config_path).unwrap(), invalid);
    }

    #[test]
    fn newer_targets_sort_first() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = camino::Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let store = TargetStore::new(project_dir.clone());
        let old_dir = project_dir.join(".ralph/targets/older");
        let new_dir = project_dir.join(".ralph/targets/newer");
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::create_dir_all(&new_dir).unwrap();
        std::fs::write(old_dir.join("prompt.md"), "# Old\n").unwrap();
        std::fs::write(new_dir.join("prompt.md"), "# New\n").unwrap();
        std::fs::write(
            old_dir.join("target.toml"),
            "id = \"older\"\ncreated_at = 10\nlast_run_status = \"never_run\"\n",
        )
        .unwrap();
        std::fs::write(
            new_dir.join("target.toml"),
            "id = \"newer\"\ncreated_at = 20\nlast_run_status = \"never_run\"\n",
        )
        .unwrap();

        let targets = store.list_targets().unwrap();

        assert_eq!(
            targets
                .iter()
                .map(|target| target.id.as_str())
                .collect::<Vec<_>>(),
            vec!["newer", "older"]
        );
    }

    #[test]
    fn single_prompt_scaffold_uses_target_specific_progress_path() {
        let temp = tempfile::tempdir().unwrap();
        let store = TargetStore::new(
            camino::Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap(),
        );

        store
            .create_target("demo", Some(ScaffoldId::SinglePrompt))
            .unwrap();
        let prompt = std::fs::read_to_string(
            store
                .target_paths("demo")
                .unwrap()
                .dir
                .join("prompt_main.md"),
        )
        .unwrap();

        assert!(
            prompt
                .contains("{\"ralph\":\"watch\",\"path\":\"{ralph-env:TARGET_DIR}/progress.txt\"}")
        );
        assert!(prompt.contains("# Requests (not sorted by priority)"));
        assert!(prompt.contains("1. Read {ralph-env:TARGET_DIR}/progress.txt."));
        assert!(
            prompt.contains(
                "3. If an item was executed, update progress in {ralph-env:TARGET_DIR}/progress.txt with the notions about the executed item; else if no item was left to execute, do not change progress."
            )
        );
        assert!(prompt.contains("4. Stop"));
    }
}
