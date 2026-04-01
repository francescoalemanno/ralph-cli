use std::fs;

use anyhow::{Context, Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};

use crate::{
    LastRunStatus, PromptFile, ScaffoldId, TargetConfig, TargetFile, TargetFileContents,
    TargetPaths, TargetReview, TargetSummary, generate_slug,
};

pub const ARTIFACT_DIR_NAME: &str = ".ralph";
pub const TARGETS_DIR_NAME: &str = "targets";

#[derive(Debug, Clone)]
pub struct TargetStore {
    project_dir: Utf8PathBuf,
}

impl TargetStore {
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

        let config = TargetConfig {
            id: target_id.to_owned(),
            scaffold,
            max_iterations: None,
            last_prompt: None,
            last_run_status: LastRunStatus::NeverRun,
        };
        self.write_target_config(&config)?;

        match scaffold.unwrap_or(ScaffoldId::Playbook) {
            ScaffoldId::Blank => {
                self.write_target_file(
                    target_id,
                    "prompt_main.md",
                    "# Prompt\n\nDescribe the work for this target.\n",
                )?;
            }
            ScaffoldId::Playbook => {
                self.write_target_file(target_id, "playbook_plan.md", &playbook_plan_prompt())?;
                self.write_target_file(target_id, "playbook_build.md", &playbook_build_prompt())?;
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
        summaries.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(summaries)
    }

    pub fn read_target_config(&self, target_id: &str) -> Result<TargetConfig> {
        let paths = self.target_paths(target_id)?;
        let raw = fs::read_to_string(&paths.config_path)
            .with_context(|| format!("failed to read target config {}", paths.config_path))?;
        toml::from_str(&raw)
            .with_context(|| format!("failed to parse target config {}", paths.config_path))
    }

    pub fn write_target_config(&self, config: &TargetConfig) -> Result<()> {
        let paths = self.target_paths(&config.id)?;
        if let Some(parent) = paths.config_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create target parent {}", parent))?;
        }
        let raw = toml::to_string_pretty(config).context("failed to serialize target config")?;
        fs::write(&paths.config_path, raw)
            .with_context(|| format!("failed to write target config {}", paths.config_path))
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

    fn write_file(&self, path: &Utf8Path, contents: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create parent directory for {path}"))?;
        }
        fs::write(path, contents).with_context(|| format!("failed to write {path}"))
    }
}

pub fn is_prompt_file_name(name: &str) -> bool {
    if !name.ends_with(".md") || name.starts_with('.') {
        return false;
    }
    let Some(stem) = name.strip_suffix(".md") else {
        return false;
    };
    let Some((slug, rest)) = stem.split_once('_') else {
        return false;
    };
    !slug.is_empty()
        && !rest.is_empty()
        && stem
            .chars()
            .all(|ch| matches!(ch, 'a'..='z' | '0'..='9' | '_' | '-'))
}

fn playbook_plan_prompt() -> String {
    r#"0a. Study `specs/*`.
0b. Study `IMPLEMENTATION_PLAN.md` if present in the repository root.
0c. Study `src/lib/*` if present to learn shared utilities and components.
0d. Study the existing source code before deciding something is missing.

1. Identify missing, incomplete, inconsistent, or unverified work by comparing `specs/*`, `IMPLEMENTATION_PLAN.md`, and the existing source code.
2. Update `IMPLEMENTATION_PLAN.md` in the repository root as a prioritized bullet list of remaining work.
3. If specifications are missing or inconsistent, update `specs/*` conservatively and reflect the resulting work in `IMPLEMENTATION_PLAN.md`.
4. Plan only. Do not implement anything.
5. Treat `src/lib` as the project's shared library and prefer shared, consolidated solutions over ad hoc duplication.
6. If `IMPLEMENTATION_PLAN.md` is already up to date and sufficient for the next build loop, leave it unchanged.

ULTIMATE GOAL - We want to achieve:
[project-specific goal].

Consider missing elements and plan accordingly. If an element is missing, search first to confirm it does not already exist, then, if needed, author the specification at `specs/FILENAME.md`.
"#
    .to_owned()
}

fn playbook_build_prompt() -> String {
    r#"0a. Study `specs/*`.
0b. Study `IMPLEMENTATION_PLAN.md` if present in the repository root.
0c. Study the existing source code before deciding something is missing.
1. Choose the highest-priority open item from `IMPLEMENTATION_PLAN.md`.
2. Implement only that highest-priority item completely against the specifications. Do not leave placeholders or partial implementations behind.
3. Run the checks relevant to the code you changed.
4. Update `IMPLEMENTATION_PLAN.md` in the repository root with completed work and new findings.
5. Update `AGENTS.md` only when you learn durable operational guidance about running or debugging the project.
6. If you find no work left to do in `IMPLEMENTATION_PLAN.md` and/or `specs/*`, leave `IMPLEMENTATION_PLAN.md` unchanged.
"#
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::{TargetStore, is_prompt_file_name};
    use crate::{LastRunStatus, ScaffoldId};

    #[test]
    fn prompt_file_discovery_ignores_uppercase_docs() {
        assert!(is_prompt_file_name("playbook_plan.md"));
        assert!(is_prompt_file_name("prompt_main.md"));
        assert!(!is_prompt_file_name("IMPLEMENTATION_PLAN.md"));
        assert!(!is_prompt_file_name("AGENTS.md"));
    }

    #[test]
    fn playbook_scaffold_creates_both_prompts() {
        let temp = tempfile::tempdir().unwrap();
        let store = TargetStore::new(
            camino::Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap(),
        );

        let summary = store
            .create_target("demo", Some(ScaffoldId::Playbook))
            .unwrap();
        let prompt_names = summary
            .prompt_files
            .iter()
            .map(|prompt| prompt.name.as_str())
            .collect::<Vec<_>>();

        assert_eq!(prompt_names, vec!["playbook_build.md", "playbook_plan.md"]);
        assert!(
            !summary
                .files
                .iter()
                .any(|file| file.name == "IMPLEMENTATION_PLAN.md")
        );
        assert_eq!(summary.last_run_status, LastRunStatus::NeverRun);

        let plan_prompt = store
            .read_named_target_file("demo", "playbook_plan.md")
            .unwrap();
        let build_prompt = store
            .read_named_target_file("demo", "playbook_build.md")
            .unwrap();
        assert!(
            plan_prompt.contains("ULTIMATE GOAL - We want to achieve:\n[project-specific goal].")
        );
        assert!(!plan_prompt.contains("Update [project-specific goal] placeholder below."));
        assert!(!build_prompt.contains("[project-specific goal]"));
    }

    #[test]
    fn create_target_defaults_to_playbook_scaffold() {
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

        assert_eq!(prompt_names, vec!["playbook_build.md", "playbook_plan.md"]);
    }
}
