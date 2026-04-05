use std::{
    fs,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};

use crate::{
    LastRunStatus, PromptFile, ScaffoldId, TargetConfig, TargetFile, TargetFileContents,
    TargetPaths, TargetReview, TargetSummary, atomic_write, scaffold::materialize_target_scaffold,
};

pub(crate) const ARTIFACT_DIR_NAME: &str = ".ralph";
const TARGETS_DIR_NAME: &str = "targets";

#[derive(Debug, Clone)]
pub struct TargetStore {
    project_dir: Utf8PathBuf,
}

impl TargetStore {
    fn new_target_config(&self, target_id: &str, scaffold: Option<ScaffoldId>) -> TargetConfig {
        TargetConfig {
            id: target_id.to_owned(),
            scaffold,
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
            id: config.id.clone(),
            dir: paths.dir,
            prompt_files,
            files,
            scaffold: config.scaffold,
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
        self.read_target_config_internal(target_id)
    }

    fn read_target_config_internal(&self, target_id: &str) -> Result<TargetConfig> {
        let paths = self.target_paths(target_id)?;
        let raw = fs::read_to_string(&paths.config_path)
            .with_context(|| format!("failed to read target config {}", paths.config_path))?;
        let mut config: TargetConfig = toml::from_str(&raw)
            .with_context(|| format!("failed to parse target config {}", paths.config_path))?;
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

    fn list_target_files(&self, target_id: &str) -> Result<Vec<TargetFile>> {
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
                is_prompt: is_prompt_file_name(&name),
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

#[cfg(test)]
mod tests {
    use super::{TargetStore, is_prompt_file_name};
    use crate::{LastRunStatus, ScaffoldId};

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
        assert!(summary.files.iter().any(|file| file.name == "progress.txt"));
        assert!(
            summary
                .files
                .iter()
                .find(|file| file.name == "progress.txt")
                .is_some_and(|file| !file.is_prompt)
        );
        assert!(summary.created_at.is_some());
    }

    #[test]
    fn list_targets_rejects_invalid_target_config() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = camino::Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let store = TargetStore::new(project_dir.clone());
        let target_dir = project_dir.join(".ralph/targets/awesome-feat-X");
        std::fs::create_dir_all(&target_dir).unwrap();
        std::fs::write(target_dir.join("notes.md"), "# Notes\n").unwrap();
        std::fs::write(target_dir.join("target.toml"), "scaffold = \"playbook\"\n").unwrap();

        let error = store.list_targets().unwrap_err().to_string();

        assert!(error.contains("failed to parse target config"));
    }

    #[test]
    fn load_target_rejects_invalid_target_config() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = camino::Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let store = TargetStore::new(project_dir.clone());
        let target_dir = project_dir.join(".ralph/targets/demo");
        std::fs::create_dir_all(&target_dir).unwrap();
        std::fs::write(target_dir.join("notes.md"), "# Notes\n").unwrap();
        std::fs::write(target_dir.join("target.toml"), "scaffold = \"playbook\"\n").unwrap();

        let error = store.load_target("demo").unwrap_err().to_string();

        assert!(error.contains("failed to parse target config"));
    }

    #[test]
    fn load_target_rejects_missing_target_config() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = camino::Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let store = TargetStore::new(project_dir.clone());
        let target_dir = project_dir.join(".ralph/targets/demo");
        std::fs::create_dir_all(&target_dir).unwrap();
        std::fs::write(target_dir.join("notes.md"), "# Notes\n").unwrap();

        let error = store.load_target("demo").unwrap_err().to_string();

        assert!(error.contains("failed to read target config"));
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
    fn single_prompt_scaffold_keeps_progress_outside_the_prompt_file() {
        let temp = tempfile::tempdir().unwrap();
        let store = TargetStore::new(
            camino::Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap(),
        );

        let summary = store
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
        let progress =
            std::fs::read_to_string(store.target_paths("demo").unwrap().dir.join("progress.txt"))
                .unwrap();

        assert!(prompt.contains("# Requests (not sorted by priority)"));
        assert!(
            prompt.contains(
                "1a. Study the existing source code before deciding something is missing."
            )
        );
        assert!(prompt.contains("1b. Study `{ralph-env:TARGET_DIR}/progress.txt`."));
        assert!(
            prompt.contains(
                "2. Execute the single most high leverage remaining item in \"Requests\"."
            )
        );
        assert!(
            prompt.contains(
                "3. Update `{ralph-env:TARGET_DIR}/progress.txt` with completed work and new findings when that keeps the next loop grounded."
            )
        );
        assert!(prompt.contains("4. Stop."));
        assert_eq!(
            summary
                .prompt_files
                .iter()
                .map(|prompt| prompt.name.as_str())
                .collect::<Vec<_>>(),
            vec!["prompt_main.md"]
        );
        assert!(progress.contains("Completed work:"));
        assert!(progress.contains("Next candidate work:"));
    }
}
