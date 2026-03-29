use std::fs;

use anyhow::{Context, Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};

use crate::{
    ClarificationAnswer, ClarificationRequest, ReviewData, SpecPaths, SpecSummary, WorkflowState,
    append_persisted_done_marker, generate_slug,
};

const RECENT_USER_FEEDBACK_START: &str = "<RECENT-USER-FEEDBACK>";
const RECENT_USER_FEEDBACK_END: &str = "</RECENT-USER-FEEDBACK>";
const OLDER_USER_FEEDBACK_START: &str = "<OLDER-USER-FEEDBACK>";
const OLDER_USER_FEEDBACK_END: &str = "</OLDER-USER-FEEDBACK>";

#[derive(Debug, Clone)]
pub struct ArtifactStore {
    project_dir: Utf8PathBuf,
}

impl ArtifactStore {
    pub fn new(project_dir: impl Into<Utf8PathBuf>) -> Self {
        Self {
            project_dir: project_dir.into(),
        }
    }

    pub fn project_dir(&self) -> &Utf8Path {
        &self.project_dir
    }

    pub fn ralph_dir(&self) -> Utf8PathBuf {
        self.project_dir.join(".ralph")
    }

    pub fn ensure_ralph_dir(&self) -> Result<()> {
        fs::create_dir_all(self.ralph_dir())
            .with_context(|| format!("failed to create {}", self.ralph_dir()))?;
        Ok(())
    }

    pub fn allocate_spec_pair(&self) -> Result<SpecPaths> {
        self.ensure_ralph_dir()?;
        for _ in 0..128 {
            let slug = generate_slug();
            let spec_path = self.ralph_dir().join(format!("spec-{slug}.md"));
            if !spec_path.exists() {
                let progress_path = self.ralph_dir().join(format!("progress-{slug}.txt"));
                let feedback_path = self.ralph_dir().join(format!("feedback-{slug}.txt"));
                return Ok(SpecPaths {
                    spec_path,
                    progress_path,
                    feedback_path,
                });
            }
        }

        Err(anyhow!("failed to allocate a unique spec slug"))
    }

    pub fn resolve_target(&self, target: &str) -> Result<SpecPaths> {
        let target = target.trim();
        if target.is_empty() {
            return Err(anyhow!("spec target cannot be empty"));
        }

        let is_path_like = target.contains(std::path::MAIN_SEPARATOR)
            || target.contains('/')
            || target.ends_with(".md")
            || target.ends_with(".txt");

        let spec_path = if is_path_like {
            let raw = Utf8PathBuf::from(target);
            if raw.is_absolute() {
                raw
            } else {
                self.project_dir.join(raw)
            }
        } else {
            self.ralph_dir().join(format!("spec-{target}.md"))
        };

        Ok(SpecPaths {
            progress_path: Self::derive_progress_path(&spec_path)?,
            feedback_path: Self::derive_feedback_path(&spec_path)?,
            spec_path,
        })
    }

    pub fn derive_progress_path(spec_path: &Utf8Path) -> Result<Utf8PathBuf> {
        let file_name = spec_path
            .file_name()
            .ok_or_else(|| anyhow!("spec path must point to a file"))?;

        if let Some(slug) = file_name
            .strip_prefix("spec-")
            .and_then(|rest| rest.strip_suffix(".md"))
        {
            return Ok(spec_path.with_file_name(format!("progress-{slug}.txt")));
        }

        let stem = spec_path
            .file_stem()
            .ok_or_else(|| anyhow!("spec file must have a valid stem"))?;
        Ok(spec_path.with_file_name(format!("{stem}.progress.txt")))
    }

    pub fn derive_feedback_path(spec_path: &Utf8Path) -> Result<Utf8PathBuf> {
        let file_name = spec_path
            .file_name()
            .ok_or_else(|| anyhow!("spec path must point to a file"))?;

        if let Some(slug) = file_name
            .strip_prefix("spec-")
            .and_then(|rest| rest.strip_suffix(".md"))
        {
            return Ok(spec_path.with_file_name(format!("feedback-{slug}.txt")));
        }

        let stem = spec_path
            .file_stem()
            .ok_or_else(|| anyhow!("spec file must have a valid stem"))?;
        Ok(spec_path.with_file_name(format!("{stem}.feedback.txt")))
    }

    pub fn read_spec(&self, path: &Utf8Path) -> Result<String> {
        self.read_optional(path)
    }

    pub fn read_progress(&self, path: &Utf8Path) -> Result<String> {
        self.read_optional(path)
    }

    pub fn read_feedback(&self, path: &Utf8Path) -> Result<String> {
        self.read_optional(path)
    }

    pub fn write_spec(&self, path: &Utf8Path, contents: &str) -> Result<()> {
        self.write_file(path, contents)
    }

    pub fn write_progress(&self, path: &Utf8Path, contents: &str) -> Result<()> {
        self.write_file(path, contents)
    }

    pub fn write_feedback(&self, path: &Utf8Path, contents: &str) -> Result<()> {
        self.write_file(path, contents)
    }

    pub fn write_auxiliary(&self, path: &Utf8Path, contents: &str) -> Result<()> {
        self.write_file(path, contents)
    }

    pub fn append_controller_note(&self, path: &Utf8Path, note: &str) -> Result<()> {
        let existing = self.read_optional(path)?;
        let note_block = format!("Controller note:\n{}\n", note.trim());
        let next = if existing.trim().is_empty() {
            note_block
        } else {
            format!("{}\n{}", existing.trim_end(), note_block)
        };
        self.write_file(path, &next)
    }

    pub fn persist_done_marker(&self, progress_path: &Utf8Path) -> Result<()> {
        let contents = self.read_optional(progress_path)?;
        self.write_file(progress_path, &append_persisted_done_marker(&contents))
    }

    pub fn default_feedback_contents() -> String {
        feedback_file_contents("None.", "None.")
    }

    pub fn append_feedback_clarification(
        &self,
        feedback_path: &Utf8Path,
        request: &ClarificationRequest,
        answer: &ClarificationAnswer,
    ) -> Result<()> {
        let existing = self.read_optional(feedback_path)?;
        let (recent, older) = parse_feedback_sections(&existing);
        let next_recent = format_feedback_exchange(request, answer);
        let next_older = merge_older_feedback(recent.as_deref(), older.as_deref());
        self.write_file(
            feedback_path,
            &feedback_file_contents(&next_recent, &next_older),
        )
    }

    pub fn delete_pair(&self, paths: &SpecPaths) -> Result<()> {
        if paths.spec_path.exists() {
            fs::remove_file(&paths.spec_path)
                .with_context(|| format!("failed to remove {}", paths.spec_path))?;
        }
        if paths.progress_path.exists() {
            fs::remove_file(&paths.progress_path)
                .with_context(|| format!("failed to remove {}", paths.progress_path))?;
        }
        if paths.feedback_path.exists() {
            fs::remove_file(&paths.feedback_path)
                .with_context(|| format!("failed to remove {}", paths.feedback_path))?;
        }
        Ok(())
    }

    pub fn review(&self, paths: &SpecPaths) -> Result<ReviewData> {
        let spec_contents = self.read_spec(&paths.spec_path)?;
        let progress_contents = self.read_progress(&paths.progress_path)?;
        let feedback_contents = self.read_feedback(&paths.feedback_path)?;
        let state = self.state_for_paths(paths)?;
        Ok(ReviewData {
            spec_path: paths.spec_path.clone(),
            progress_path: paths.progress_path.clone(),
            feedback_path: paths.feedback_path.clone(),
            spec_contents,
            progress_contents,
            feedback_contents,
            state,
        })
    }

    pub fn list_specs(&self) -> Result<Vec<SpecSummary>> {
        self.ensure_ralph_dir()?;
        let mut specs = Vec::new();
        self.collect_spec_paths(&self.ralph_dir(), &mut specs)?;

        let mut summaries = specs
            .into_iter()
            .map(|spec_path| {
                let paths = SpecPaths {
                    progress_path: Self::derive_progress_path(&spec_path)?,
                    feedback_path: Self::derive_feedback_path(&spec_path)?,
                    spec_path,
                };
                let review = self.review(&paths)?;
                Ok(SpecSummary {
                    spec_path: review.spec_path,
                    progress_path: review.progress_path,
                    feedback_path: review.feedback_path,
                    state: review.state,
                    spec_preview: preview(&review.spec_contents),
                    progress_preview: preview(&review.progress_contents),
                    feedback_preview: preview(&review.feedback_contents),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        summaries.sort_by(|left, right| {
            let left_rank = if left.state == WorkflowState::Completed {
                1
            } else {
                0
            };
            let right_rank = if right.state == WorkflowState::Completed {
                1
            } else {
                0
            };
            left_rank
                .cmp(&right_rank)
                .then_with(|| left.spec_path.cmp(&right.spec_path))
        });

        Ok(summaries)
    }

    pub fn state_for_paths(&self, paths: &SpecPaths) -> Result<WorkflowState> {
        let spec = self.read_optional(&paths.spec_path)?;
        if spec.trim().is_empty() {
            return Ok(WorkflowState::Empty);
        }

        let progress = self.read_optional(&paths.progress_path)?;
        let final_non_empty = progress
            .lines()
            .rev()
            .map(str::trim)
            .find(|line| !line.is_empty());

        if matches!(final_non_empty, Some("<promise>DONE</promise>")) {
            Ok(WorkflowState::Completed)
        } else {
            Ok(WorkflowState::Planned)
        }
    }

    pub fn past_spec_path(&self, spec_path: &Utf8Path) -> Result<Utf8PathBuf> {
        Self::sibling_with_suffix(spec_path, ".past-spec.md")
    }

    pub fn spec_edit_diff_path(&self, spec_path: &Utf8Path) -> Result<Utf8PathBuf> {
        Self::sibling_with_suffix(spec_path, ".spec-edit.diff.txt")
    }

    fn collect_spec_paths(&self, dir: &Utf8Path, specs: &mut Vec<Utf8PathBuf>) -> Result<()> {
        if !dir.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(dir).with_context(|| format!("failed to read {dir}"))? {
            let entry = entry?;
            let path = Utf8PathBuf::from_path_buf(entry.path())
                .map_err(|_| anyhow!("non-UTF8 path found under {}", self.ralph_dir()))?;
            if path.is_dir() {
                self.collect_spec_paths(&path, specs)?;
                continue;
            }
            if path.extension() != Some("md") {
                continue;
            }
            if path
                .file_name()
                .is_some_and(|name| name.ends_with(".progress.md"))
            {
                continue;
            }
            specs.push(path);
        }
        Ok(())
    }

    fn read_optional(&self, path: &Utf8Path) -> Result<String> {
        if !path.exists() {
            return Ok(String::new());
        }
        fs::read_to_string(path).with_context(|| format!("failed to read {path}"))
    }

    fn write_file(&self, path: &Utf8Path, contents: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create parent directory for {path}"))?;
        }
        fs::write(path, contents).with_context(|| format!("failed to write {path}"))
    }

    fn sibling_with_suffix(path: &Utf8Path, suffix: &str) -> Result<Utf8PathBuf> {
        let stem = path
            .file_stem()
            .ok_or_else(|| anyhow!("path must have a valid stem"))?;
        Ok(path.with_file_name(format!("{stem}{suffix}")))
    }
}

fn parse_feedback_sections(contents: &str) -> (Option<String>, Option<String>) {
    (
        extract_feedback_section(
            contents,
            RECENT_USER_FEEDBACK_START,
            RECENT_USER_FEEDBACK_END,
        ),
        extract_feedback_section(contents, OLDER_USER_FEEDBACK_START, OLDER_USER_FEEDBACK_END),
    )
}

fn extract_feedback_section(contents: &str, start_tag: &str, end_tag: &str) -> Option<String> {
    let start = contents.find(start_tag)?;
    let offset = start + start_tag.len();
    let end = contents[offset..].find(end_tag)?;
    Some(contents[offset..offset + end].trim().to_owned())
}

fn merge_older_feedback(recent: Option<&str>, older: Option<&str>) -> String {
    let mut sections = Vec::new();
    if let Some(recent) = recent
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "None.")
    {
        sections.push(recent.to_owned());
    }
    if let Some(older) = older
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "None.")
    {
        sections.push(older.to_owned());
    }

    if sections.is_empty() {
        "None.".to_owned()
    } else {
        sections.join("\n\n")
    }
}

fn format_feedback_exchange(
    request: &ClarificationRequest,
    answer: &ClarificationAnswer,
) -> String {
    let mut lines = vec![format!("Q: {}", request.question.trim())];
    if !answer.used_option_selection && !request.options.is_empty() {
        lines.push("Options:".to_owned());
        lines.extend(request.options.iter().enumerate().map(|(index, option)| {
            format!(
                "{}. {} - {}",
                index + 1,
                option.label.trim(),
                option.description.trim()
            )
        }));
    }
    lines.push(format!("A: {}", answer.text.trim()));
    lines.join("\n")
}

fn feedback_file_contents(recent: &str, older: &str) -> String {
    format!(
        "{RECENT_USER_FEEDBACK_START}\n{recent}\n{RECENT_USER_FEEDBACK_END}\n\n{OLDER_USER_FEEDBACK_START}\n{older}\n{OLDER_USER_FEEDBACK_END}\n"
    )
}

fn preview(contents: &str) -> String {
    let lines = contents
        .lines()
        .skip_while(|line| line.trim().is_empty())
        .map(str::trim_end)
        .take(12)
        .collect::<Vec<_>>();

    if lines.is_empty() {
        "<empty>".to_owned()
    } else {
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_spec(suffix: &str) -> String {
        format!(
            "# Goal\nGoal {suffix}\n\n# User Requirements And Constraints\nRequirements {suffix}\n\n# Non-Goals\nNon-goals {suffix}\n\n# Proposed Design\nDesign {suffix}\n\n# Implementation Plan\nPlan {suffix}\n\n# Acceptance Criteria\nAcceptance {suffix}\n\n# Risks\nRisks {suffix}\n\n# Open Questions\nQuestions {suffix}\n"
        )
    }

    fn store() -> (TempDir, ArtifactStore) {
        let temp = TempDir::new().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let store = ArtifactStore::new(project_dir);
        store.ensure_ralph_dir().unwrap();
        (temp, store)
    }

    #[test]
    fn derives_default_progress_path() {
        let path = Utf8Path::new("/tmp/project/.ralph/spec-otter-thread-sage.md");
        let progress = ArtifactStore::derive_progress_path(path).unwrap();
        assert_eq!(
            progress,
            "/tmp/project/.ralph/progress-otter-thread-sage.txt"
        );
    }

    #[test]
    fn derives_custom_progress_path() {
        let path = Utf8Path::new("/tmp/project/.ralph/my-feature.md");
        let progress = ArtifactStore::derive_progress_path(path).unwrap();
        assert_eq!(progress, "/tmp/project/.ralph/my-feature.progress.txt");
    }

    #[test]
    fn derives_feedback_path() {
        let path = Utf8Path::new("/tmp/project/.ralph/spec-otter-thread-sage.md");
        let feedback = ArtifactStore::derive_feedback_path(path).unwrap();
        assert_eq!(
            feedback,
            "/tmp/project/.ralph/feedback-otter-thread-sage.txt"
        );
    }

    #[test]
    fn state_detection_handles_empty_planned_and_completed() {
        let (_temp, store) = store();
        let empty = store.resolve_target("alpha").unwrap();
        assert_eq!(store.state_for_paths(&empty).unwrap(), WorkflowState::Empty);

        store
            .write_spec(&empty.spec_path, &sample_spec("X"))
            .unwrap();
        assert_eq!(
            store.state_for_paths(&empty).unwrap(),
            WorkflowState::Planned
        );

        store
            .write_progress(&empty.progress_path, "Done\n<promise>DONE</promise>\n")
            .unwrap();
        assert_eq!(
            store.state_for_paths(&empty).unwrap(),
            WorkflowState::Completed
        );
    }

    #[test]
    fn feedback_rotation_keeps_recent_and_older_entries() {
        let (_temp, store) = store();
        let paths = store.resolve_target("alpha").unwrap();
        let request_one = ClarificationRequest {
            question: "Which db?".to_owned(),
            options: Vec::new(),
        };
        let answer_one = ClarificationAnswer {
            text: "Postgres".to_owned(),
            used_option_selection: false,
        };
        store
            .append_feedback_clarification(&paths.feedback_path, &request_one, &answer_one)
            .unwrap();

        let request_two = ClarificationRequest {
            question: "Which runtime?".to_owned(),
            options: Vec::new(),
        };
        let answer_two = ClarificationAnswer {
            text: "Tokio".to_owned(),
            used_option_selection: false,
        };
        store
            .append_feedback_clarification(&paths.feedback_path, &request_two, &answer_two)
            .unwrap();

        let feedback = store.read_feedback(&paths.feedback_path).unwrap();
        assert!(feedback.contains("<RECENT-USER-FEEDBACK>\nQ: Which runtime?\nA: Tokio"));
        assert!(feedback.contains("<OLDER-USER-FEEDBACK>\nQ: Which db?\nA: Postgres"));
    }

    #[test]
    fn free_form_feedback_persists_options() {
        let (_temp, store) = store();
        let paths = store.resolve_target("alpha").unwrap();
        let request = ClarificationRequest {
            question: "Which UI stack?".to_owned(),
            options: vec![
                crate::ClarificationOption {
                    label: "TUI polish".to_owned(),
                    description: "Keep ratatui and improve current screens".to_owned(),
                },
                crate::ClarificationOption {
                    label: "Rewrite".to_owned(),
                    description: "Switch to Bubble Tea".to_owned(),
                },
            ],
        };
        let answer = ClarificationAnswer {
            text: "I like option 2, but use Bubbletea instead of Tview".to_owned(),
            used_option_selection: false,
        };

        store
            .append_feedback_clarification(&paths.feedback_path, &request, &answer)
            .unwrap();

        let feedback = store.read_feedback(&paths.feedback_path).unwrap();
        assert!(feedback.contains("Q: Which UI stack?"));
        assert!(feedback.contains("Options:"));
        assert!(feedback.contains("1. TUI polish - Keep ratatui and improve current screens"));
        assert!(feedback.contains("2. Rewrite - Switch to Bubble Tea"));
        assert!(feedback.contains("A: I like option 2, but use Bubbletea instead of Tview"));
    }

    #[test]
    fn option_selection_feedback_omits_options() {
        let (_temp, store) = store();
        let paths = store.resolve_target("alpha").unwrap();
        let request = ClarificationRequest {
            question: "Which runtime?".to_owned(),
            options: vec![crate::ClarificationOption {
                label: "Tokio".to_owned(),
                description: "Async runtime".to_owned(),
            }],
        };
        let answer = ClarificationAnswer {
            text: "Tokio".to_owned(),
            used_option_selection: true,
        };

        store
            .append_feedback_clarification(&paths.feedback_path, &request, &answer)
            .unwrap();

        let feedback = store.read_feedback(&paths.feedback_path).unwrap();
        assert!(feedback.contains("Q: Which runtime?"));
        assert!(feedback.contains("A: Tokio"));
        assert!(!feedback.contains("Options:"));
    }

    #[test]
    fn sorts_active_specs_before_completed() {
        let (_temp, store) = store();
        let active = store.resolve_target("active").unwrap();
        store
            .write_spec(&active.spec_path, &sample_spec("active"))
            .unwrap();

        let completed = store.resolve_target("done").unwrap();
        store
            .write_spec(&completed.spec_path, &sample_spec("done"))
            .unwrap();
        store
            .write_progress(&completed.progress_path, "<promise>DONE</promise>\n")
            .unwrap();

        let specs = store.list_specs().unwrap();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].state, WorkflowState::Planned);
        assert_eq!(specs[1].state, WorkflowState::Completed);
    }

    #[test]
    fn persists_done_marker() {
        let (_temp, store) = store();
        let paths = store.resolve_target("alpha").unwrap();
        store
            .write_progress(&paths.progress_path, "Task\n")
            .unwrap();
        store.persist_done_marker(&paths.progress_path).unwrap();
        assert_eq!(
            store.read_progress(&paths.progress_path).unwrap(),
            "Task\n<promise>DONE</promise>\n"
        );
    }

    #[test]
    fn appends_feedback_by_moving_recent_to_older() {
        let (_temp, store) = store();
        let paths = store.resolve_target("alpha").unwrap();
        store
            .write_feedback(
                &paths.feedback_path,
                &ArtifactStore::default_feedback_contents(),
            )
            .unwrap();

        let db_request = ClarificationRequest {
            question: "Which db?".to_owned(),
            options: Vec::new(),
        };
        let db_answer = ClarificationAnswer {
            text: "Postgres".to_owned(),
            used_option_selection: false,
        };
        store
            .append_feedback_clarification(&paths.feedback_path, &db_request, &db_answer)
            .unwrap();
        let runtime_request = ClarificationRequest {
            question: "Which runtime?".to_owned(),
            options: Vec::new(),
        };
        let runtime_answer = ClarificationAnswer {
            text: "Tokio".to_owned(),
            used_option_selection: false,
        };
        store
            .append_feedback_clarification(&paths.feedback_path, &runtime_request, &runtime_answer)
            .unwrap();

        let feedback = store.read_feedback(&paths.feedback_path).unwrap();
        assert!(feedback.contains("<RECENT-USER-FEEDBACK>\nQ: Which runtime?\nA: Tokio"));
        assert!(feedback.contains("<OLDER-USER-FEEDBACK>\nQ: Which db?\nA: Postgres"));
    }

    #[test]
    fn ignores_legacy_visible_ralph_directory() {
        let (_temp, store) = store();
        let legacy_dir = store.project_dir().join("ralph");
        fs::create_dir_all(&legacy_dir).unwrap();
        fs::write(legacy_dir.join("spec-legacy.md"), sample_spec("legacy")).unwrap();
        fs::write(legacy_dir.join("progress-legacy.txt"), "Task 1\n").unwrap();

        let specs = store.list_specs().unwrap();
        assert!(specs.is_empty());
    }
}
