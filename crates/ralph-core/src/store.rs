use std::{
    fs,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};

use crate::{
    GoalDrivenWorkflowState, LastRunStatus, PromptFile, ScaffoldId, TargetConfig, TargetFile,
    TargetFileContents, TargetPaths, TargetReview, TargetSummary, WorkflowMode, atomic_write,
    generate_slug,
};

pub const ARTIFACT_DIR_NAME: &str = ".ralph";
pub const TARGETS_DIR_NAME: &str = "targets";

#[derive(Debug, Clone)]
pub struct TargetStore {
    project_dir: Utf8PathBuf,
}

impl TargetStore {
    fn fallback_target_config(&self, target_id: &str) -> TargetConfig {
        TargetConfig {
            id: target_id.to_owned(),
            scaffold: None,
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
            Some(ScaffoldId::TaskBased) => Some(WorkflowMode::TaskBased),
            Some(ScaffoldId::GoalDriven) => Some(WorkflowMode::GoalDriven),
            _ => None,
        };
        TargetConfig {
            id: target_id.to_owned(),
            scaffold,
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

    pub fn project_dir(&self) -> &Utf8Path {
        &self.project_dir
    }

    pub fn ralph_dir(&self) -> Utf8PathBuf {
        self.project_dir.join(ARTIFACT_DIR_NAME)
    }

    pub fn targets_dir(&self) -> Utf8PathBuf {
        self.ralph_dir().join(TARGETS_DIR_NAME)
    }

    pub fn ensure_targets_dir(&self) -> Result<()> {
        fs::create_dir_all(self.targets_dir())
            .with_context(|| format!("failed to create {}", self.targets_dir()))?;
        Ok(())
    }

    pub fn allocate_target_id(&self) -> Result<String> {
        self.ensure_targets_dir()?;
        for _ in 0..128 {
            let id = generate_slug();
            if !self.target_dir(&id).exists() {
                return Ok(id);
            }
        }
        Err(anyhow!("failed to allocate a unique target id"))
    }

    pub fn validate_target_id(&self, target_id: &str) -> Result<()> {
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

    pub fn target_dir(&self, target_id: &str) -> Utf8PathBuf {
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

        match scaffold.unwrap_or(ScaffoldId::SinglePrompt) {
            ScaffoldId::PlanBuild => {
                self.write_target_file(target_id, "0_plan.md", &plan_build_plan_prompt())?;
                self.write_target_file(target_id, "1_build.md", &plan_build_build_prompt())?;
            }
            ScaffoldId::TaskBased => {
                self.write_target_file(target_id, "GOAL.md", &goal_driven_goal_template())?;
                self.write_target_file(
                    target_id,
                    "progress.toml",
                    &task_based_progress_seed_template(),
                )?;
            }
            ScaffoldId::GoalDriven => {
                self.write_target_file(target_id, "GOAL.md", &goal_driven_goal_template())?;
                fs::create_dir_all(paths.dir.join("specs"))
                    .with_context(|| format!("failed to create {}", paths.dir.join("specs")))?;
            }
            ScaffoldId::SinglePrompt => {
                self.write_target_file(target_id, "prompt_main.md", &single_prompt_template())?;
            }
        }

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
        let config = self.read_target_config(target_id)?;
        let files = self.list_target_files(target_id)?;
        let prompt_files = files
            .iter()
            .filter(|file| file.is_prompt)
            .map(|file| PromptFile {
                name: file.name.clone(),
                path: file.path.clone(),
            })
            .collect::<Vec<_>>();

        Ok(TargetSummary {
            id: config.id,
            dir: paths.dir,
            prompt_files,
            files,
            scaffold: config.scaffold,
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
        let mut config: TargetConfig =
            toml::from_str(&raw).unwrap_or_else(|_| self.fallback_target_config(target_id));
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

    pub fn write_target_file(&self, target_id: &str, name: &str, contents: &str) -> Result<()> {
        let paths = self.target_paths(target_id)?;
        self.write_file(&paths.dir.join(name), contents)
    }

    pub fn read_named_target_file(&self, target_id: &str, name: &str) -> Result<String> {
        let paths = self.target_paths(target_id)?;
        self.read_file(&paths.dir.join(name))
    }

    pub fn list_prompt_files(&self, target_id: &str) -> Result<Vec<PromptFile>> {
        let mut prompts = self
            .list_target_files(target_id)?
            .into_iter()
            .filter(|file| file.is_prompt)
            .map(|file| PromptFile {
                name: file.name,
                path: file.path,
            })
            .collect::<Vec<_>>();
        prompts.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(prompts)
    }

    pub fn list_target_files(&self, target_id: &str) -> Result<Vec<TargetFile>> {
        let paths = self.target_paths(target_id)?;
        let config = self.read_target_config(target_id)?;
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
                is_prompt: is_target_prompt_file(&config, &name),
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

    fn write_file(&self, path: &Utf8Path, contents: &str) -> Result<()> {
        atomic_write(path, contents).with_context(|| format!("failed to write {path}"))
    }
}

pub fn is_prompt_file_name(name: &str) -> bool {
    name.ends_with(".md")
}

pub fn bare_prompt_template(scaffold: ScaffoldId) -> String {
    match scaffold {
        ScaffoldId::PlanBuild => plan_build_plan_prompt(),
        ScaffoldId::TaskBased => goal_driven_goal_template(),
        ScaffoldId::GoalDriven => goal_driven_goal_template(),
        ScaffoldId::SinglePrompt => single_prompt_template(),
    }
}

fn current_unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn plan_build_plan_prompt() -> String {
    r#"0a. Study `specs/*`.
0b. Study `IMPLEMENTATION_PLAN.md` if present in the repository root.
0c. Study the codebase areas that appear to hold shared utilities, core modules, or reusable components.
0d. Study the existing source code before deciding something is missing.

1. Identify missing, incomplete, inconsistent, or unverified work by comparing `specs/*`, `IMPLEMENTATION_PLAN.md`, and the existing source code.
2. Update `IMPLEMENTATION_PLAN.md` in the repository root as a prioritized bullet list of remaining work.
3. If specifications are missing or inconsistent, update `specs/*` conservatively and reflect the resulting work in `IMPLEMENTATION_PLAN.md`.
4. Plan only. Do not implement anything.
5. Prefer shared, consolidated solutions in the codebase over ad hoc duplication.
6. If `IMPLEMENTATION_PLAN.md` is already up to date and sufficient for the next build loop, leave it unchanged.

ULTIMATE GOAL - We want to achieve:
[project-specific goal].

Consider missing elements and plan accordingly. If an element is missing, search first to confirm it does not already exist, then, if needed, author the specification at `specs/FILENAME.md`.

{"ralph":"watch","path":"IMPLEMENTATION_PLAN.md"}
"#
    .to_owned()
}

fn plan_build_build_prompt() -> String {
    r#"0a. Study `specs/*`.
0b. Study `IMPLEMENTATION_PLAN.md` if present in the repository root.
0c. Study the existing source code before deciding something is missing.
1. Choose the highest-priority open item from `IMPLEMENTATION_PLAN.md`.
2. Implement only that highest-priority item completely against the specifications. Do not leave placeholders or partial implementations behind.
3. Run the checks relevant to the code you changed.
4. Update `IMPLEMENTATION_PLAN.md` in the repository root with completed work and new findings.
5. Update `AGENTS.md` only when you learn durable operational guidance about running or debugging the project.
6. If you find no work left to do in `IMPLEMENTATION_PLAN.md` and/or `specs/*`, leave `IMPLEMENTATION_PLAN.md` unchanged.

{"ralph":"watch","path":"IMPLEMENTATION_PLAN.md"}
"#
    .to_owned()
}

fn single_prompt_template() -> String {
    "# Requests (not sorted by priority)\n- A\n- B\n- C\n\n# Execution policy\n1. Read {ralph-env:TARGET_DIR}/progress.txt.\n2. Execute the single most high leverage item in \"Requests\".\n3. If an item was executed, update progress in {ralph-env:TARGET_DIR}/progress.txt with the notions about the executed item; else if no item was left to execute, do not change progress.\n4. Stop\n\n{\"ralph\":\"watch\",\"path\":\"{ralph-env:TARGET_DIR}/progress.txt\"}\n"
        .to_owned()
}

fn goal_driven_goal_template() -> String {
    "# Goal\n\nCapture the desired outcome here.\n\n- Requests\n- Constraints\n- Observations\n- Acceptance notes\n"
        .to_owned()
}

fn task_based_progress_seed_template() -> String {
    r#"version = 1

[[items]]
description = "Planning phase"
steps = [
    "Revise progress.toml into a clear ordered list of concrete tasks derived from your studies",
    "Do not start other items",
    "Stop after updating progress.toml"
]
completed = false
"#
    .to_owned()
}

fn is_target_prompt_file(config: &TargetConfig, name: &str) -> bool {
    match config.mode {
        Some(WorkflowMode::TaskBased) => false,
        Some(WorkflowMode::GoalDriven) => false,
        None => is_prompt_file_name(name),
    }
}

fn workflow_state_for_mode(mode: WorkflowMode) -> GoalDrivenWorkflowState {
    let mut workflow = GoalDrivenWorkflowState::default();
    workflow.phase = match mode {
        WorkflowMode::TaskBased => crate::GoalDrivenPhase::Build,
        WorkflowMode::GoalDriven => crate::GoalDrivenPhase::Plan,
    };
    workflow
}

#[cfg(test)]
mod tests {
    use super::{TargetStore, is_prompt_file_name};
    use crate::{GoalDrivenPhase, LastRunStatus, ScaffoldId, WorkflowMode};

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

        let plan_prompt = store.read_named_target_file("demo", "0_plan.md").unwrap();
        let build_prompt = store.read_named_target_file("demo", "1_build.md").unwrap();
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
    fn goal_driven_scaffold_creates_goal_file_and_state() {
        let temp = tempfile::tempdir().unwrap();
        let store = TargetStore::new(
            camino::Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap(),
        );

        let summary = store
            .create_target("demo", Some(ScaffoldId::GoalDriven))
            .unwrap();

        assert!(summary.prompt_files.is_empty());
        assert_eq!(summary.mode, Some(WorkflowMode::GoalDriven));
        assert!(summary.files.iter().any(|file| file.name == "GOAL.md"));
        assert!(summary.files.iter().all(|file| !file.is_prompt));
        assert!(store.target_dir("demo").join("specs").is_dir());

        let config = store.read_target_config("demo").unwrap();
        assert_eq!(config.mode, Some(WorkflowMode::GoalDriven));
        assert_eq!(
            config.workflow.as_ref().map(|workflow| workflow.phase),
            Some(GoalDrivenPhase::Plan)
        );
    }

    #[test]
    fn task_based_scaffold_creates_goal_and_progress_files() {
        let temp = tempfile::tempdir().unwrap();
        let store = TargetStore::new(
            camino::Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap(),
        );

        let summary = store
            .create_target("demo", Some(ScaffoldId::TaskBased))
            .unwrap();

        assert!(summary.prompt_files.is_empty());
        assert_eq!(summary.mode, Some(WorkflowMode::TaskBased));
        assert!(summary.files.iter().any(|file| file.name == "GOAL.md"));
        assert!(
            summary
                .files
                .iter()
                .any(|file| file.name == "progress.toml")
        );
        assert!(summary.files.iter().all(|file| !file.is_prompt));

        let config = store.read_target_config("demo").unwrap();
        assert_eq!(config.mode, Some(WorkflowMode::TaskBased));
        assert_eq!(
            config.workflow.as_ref().map(|workflow| workflow.phase),
            Some(GoalDrivenPhase::Build)
        );
        let progress = store
            .read_named_target_file("demo", "progress.toml")
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
        let prompt = store
            .read_named_target_file("demo", "prompt_main.md")
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
