use std::{collections::BTreeMap, env, ffi::OsStr, fs, path::Path};

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use dirs::home_dir;
use serde::{Deserialize, Serialize};

use crate::store::ARTIFACT_DIR_NAME;

const PROJECT_CONFIG_FILE_NAME: &str = "config.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PromptTransport {
    #[default]
    Stdin,
    EnvVar,
    TempFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum QuestionSupportMode {
    Disabled,
    #[default]
    TextProtocol,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CodingAgent {
    #[default]
    Opencode,
    Codex,
    Raijin,
}

impl CodingAgent {
    pub fn detected() -> Vec<Self> {
        let path = env::var_os("PATH");
        let pathext = env::var_os("PATHEXT");
        detect_agents_in_path(path.as_deref(), pathext.as_deref())
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Opencode => "OpenCode",
            Self::Codex => "Codex",
            Self::Raijin => "Raijin",
        }
    }

    pub fn next_in(self, available: &[Self]) -> Self {
        if available.is_empty() {
            return self;
        }
        if let Some(index) = available.iter().position(|agent| *agent == self) {
            available[(index + 1) % available.len()]
        } else {
            available[0]
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Opencode => Self::Codex,
            Self::Codex => Self::Raijin,
            Self::Raijin => Self::Opencode,
        }
    }

    fn all() -> [Self; 3] {
        [Self::Opencode, Self::Codex, Self::Raijin]
    }

    fn default_program(self) -> &'static str {
        match self {
            Self::Opencode => "opencode",
            Self::Codex => "codex",
            Self::Raijin => "raijin",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerConfig {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub prompt_transport: PromptTransport,
    #[serde(default = "default_prompt_env_var")]
    pub prompt_env_var: String,
    #[serde(default)]
    pub question_support: QuestionSupportMode,
    #[serde(default)]
    pub shell_template: Option<String>,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self::for_agent(CodingAgent::Opencode)
    }
}

impl RunnerConfig {
    pub fn for_agent(agent: CodingAgent) -> Self {
        match agent {
            CodingAgent::Opencode => Self {
                program: "opencode".to_owned(),
                args: vec![
                    "run".to_owned(),
                    "--format".to_owned(),
                    "default".to_owned(),
                    "--thinking".to_owned(),
                ],
                env: BTreeMap::new(),
                prompt_transport: PromptTransport::Stdin,
                prompt_env_var: default_prompt_env_var(),
                question_support: QuestionSupportMode::TextProtocol,
                shell_template: None,
            },
            CodingAgent::Codex => Self {
                program: "codex".to_owned(),
                args: vec![
                    "exec".to_owned(),
                    "--dangerously-bypass-approvals-and-sandbox".to_owned(),
                    "--ephemeral".to_owned(),
                ],
                env: BTreeMap::new(),
                prompt_transport: PromptTransport::Stdin,
                prompt_env_var: default_prompt_env_var(),
                question_support: QuestionSupportMode::TextProtocol,
                shell_template: None,
            },
            CodingAgent::Raijin => Self {
                program: "raijin".to_owned(),
                args: Vec::new(),
                env: BTreeMap::new(),
                prompt_transport: PromptTransport::EnvVar,
                prompt_env_var: default_prompt_env_var(),
                question_support: QuestionSupportMode::TextProtocol,
                shell_template: Some(r#"raijin -ephemeral "$PROMPT""#.to_owned()),
            },
        }
    }

    pub fn inferred_agent(&self) -> Option<CodingAgent> {
        match normalized_program_name(&self.program).as_deref() {
            Some("opencode") => Some(CodingAgent::Opencode),
            Some("codex") => Some(CodingAgent::Codex),
            Some("raijin") => Some(CodingAgent::Raijin),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeConfig {
    #[serde(default = "default_accent_color")]
    pub accent_color: String,
    #[serde(default = "default_success_color")]
    pub success_color: String,
    #[serde(default = "default_warning_color")]
    pub warning_color: String,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            accent_color: default_accent_color(),
            success_color: default_success_color(),
            warning_color: default_warning_color(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub planner: RunnerConfig,
    #[serde(default)]
    pub builder: RunnerConfig,
    #[serde(default = "default_planning_iterations")]
    pub planning_max_iterations: usize,
    #[serde(default = "default_builder_iterations")]
    pub builder_max_iterations: usize,
    #[serde(default)]
    pub editor_override: Option<String>,
    #[serde(default)]
    pub theme: ThemeConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            planner: RunnerConfig::default(),
            builder: RunnerConfig::default(),
            planning_max_iterations: default_planning_iterations(),
            builder_max_iterations: default_builder_iterations(),
            editor_override: None,
            theme: ThemeConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct PartialRunnerConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    program: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    args: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    env: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_transport: Option<PromptTransport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_env_var: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    question_support: Option<QuestionSupportMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    shell_template: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct PartialThemeConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    accent_color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    success_color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    warning_color: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct PartialAppConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    planner: Option<PartialRunnerConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    builder: Option<PartialRunnerConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    planning_max_iterations: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    builder_max_iterations: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    editor_override: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    theme: Option<PartialThemeConfig>,
}

impl AppConfig {
    pub fn load(project_dir: &Utf8Path) -> Result<Self> {
        let mut config = Self::default();

        if let Some(user_path) = user_config_path()?
            && user_path.exists()
        {
            let partial = read_partial_config(&user_path)?;
            config = merge_config(config, partial);
        }

        let project_path = project_config_path(project_dir);
        if project_path.exists() {
            let partial = read_partial_config(&project_path)?;
            config = merge_config(config, partial);
        }

        Ok(config)
    }

    pub fn coding_agent(&self) -> CodingAgent {
        self.builder
            .inferred_agent()
            .or_else(|| self.planner.inferred_agent())
            .unwrap_or_default()
    }

    pub fn set_coding_agent(&mut self, agent: CodingAgent) {
        self.planner = RunnerConfig::for_agent(agent);
        self.builder = RunnerConfig::for_agent(agent);
    }

    pub fn persist_project_coding_agent(project_dir: &Utf8Path, agent: CodingAgent) -> Result<()> {
        let project_path = project_config_path(project_dir);
        let mut partial = if project_path.exists() {
            read_partial_config(&project_path)?
        } else {
            PartialAppConfig::default()
        };
        let runner = PartialRunnerConfig::from(RunnerConfig::for_agent(agent));
        partial.planner = Some(runner.clone());
        partial.builder = Some(runner);

        let rendered =
            toml::to_string_pretty(&partial).context("failed to serialize project config")?;
        if let Some(parent) = project_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create project config directory at {parent}")
            })?;
        }
        fs::write(&project_path, rendered)
            .with_context(|| format!("failed to write project config at {project_path}"))?;
        Ok(())
    }
}

fn read_partial_config(path: &Utf8Path) -> Result<PartialAppConfig> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file at {path}"))?;
    toml::from_str(&raw).with_context(|| format!("failed to parse config file at {path}"))
}

fn project_config_path(project_dir: &Utf8Path) -> Utf8PathBuf {
    project_dir
        .join(ARTIFACT_DIR_NAME)
        .join(PROJECT_CONFIG_FILE_NAME)
}

fn merge_config(mut config: AppConfig, partial: PartialAppConfig) -> AppConfig {
    if let Some(planner) = partial.planner {
        config.planner = merge_runner(config.planner, planner);
    }
    if let Some(builder) = partial.builder {
        config.builder = merge_runner(config.builder, builder);
    }
    if let Some(iterations) = partial.planning_max_iterations {
        config.planning_max_iterations = iterations;
    }
    if let Some(iterations) = partial.builder_max_iterations {
        config.builder_max_iterations = iterations;
    }
    if let Some(editor_override) = partial.editor_override {
        config.editor_override = Some(editor_override);
    }
    if let Some(theme) = partial.theme {
        if let Some(color) = theme.accent_color {
            config.theme.accent_color = color;
        }
        if let Some(color) = theme.success_color {
            config.theme.success_color = color;
        }
        if let Some(color) = theme.warning_color {
            config.theme.warning_color = color;
        }
    }
    config
}

fn merge_runner(mut runner: RunnerConfig, partial: PartialRunnerConfig) -> RunnerConfig {
    if let Some(program) = partial.program {
        runner.program = program;
    }
    if let Some(args) = partial.args {
        runner.args = args;
    }
    if let Some(env) = partial.env {
        runner.env = env;
    }
    if let Some(prompt_transport) = partial.prompt_transport {
        runner.prompt_transport = prompt_transport;
    }
    if let Some(prompt_env_var) = partial.prompt_env_var {
        runner.prompt_env_var = prompt_env_var;
    }
    if let Some(question_support) = partial.question_support {
        runner.question_support = question_support;
    }
    if let Some(shell_template) = partial.shell_template {
        runner.shell_template = Some(shell_template);
    }
    runner
}

impl From<RunnerConfig> for PartialRunnerConfig {
    fn from(runner: RunnerConfig) -> Self {
        Self {
            program: Some(runner.program),
            args: Some(runner.args),
            env: Some(runner.env),
            prompt_transport: Some(runner.prompt_transport),
            prompt_env_var: Some(runner.prompt_env_var),
            question_support: Some(runner.question_support),
            shell_template: runner.shell_template,
        }
    }
}

fn user_config_path() -> Result<Option<Utf8PathBuf>> {
    let Some(home) = home_dir() else {
        return Ok(None);
    };

    let path = home.join(".config").join("ralph").join("config.toml");
    Utf8PathBuf::from_path_buf(path)
        .map(Some)
        .map_err(|_| anyhow::anyhow!("user config path is not valid UTF-8"))
}

fn default_prompt_env_var() -> String {
    "PROMPT".to_owned()
}

fn default_planning_iterations() -> usize {
    8
}

fn default_builder_iterations() -> usize {
    25
}

fn default_accent_color() -> String {
    "cyan".to_owned()
}

fn default_success_color() -> String {
    "green".to_owned()
}

fn default_warning_color() -> String {
    "yellow".to_owned()
}

fn normalized_program_name(program: &str) -> Option<String> {
    let name = program.rsplit(['/', '\\']).next().unwrap_or(program);
    let name = name.strip_suffix(".exe").unwrap_or(name);
    (!name.is_empty()).then(|| name.to_ascii_lowercase())
}

fn detect_agents_in_path(
    path_env: Option<&OsStr>,
    pathext_env: Option<&OsStr>,
) -> Vec<CodingAgent> {
    CodingAgent::all()
        .into_iter()
        .filter(|agent| program_exists_in_path(agent.default_program(), path_env, pathext_env))
        .collect()
}

fn program_exists_in_path(
    program: &str,
    path_env: Option<&OsStr>,
    pathext_env: Option<&OsStr>,
) -> bool {
    let program_path = Path::new(program);
    if program_path.components().count() > 1 {
        return program_path.is_file();
    }

    let Some(path_env) = path_env else {
        return false;
    };

    let candidate_names = executable_names(program, pathext_env);
    env::split_paths(path_env).any(|dir| {
        candidate_names
            .iter()
            .any(|candidate| dir.join(candidate).is_file())
    })
}

fn executable_names(program: &str, pathext_env: Option<&OsStr>) -> Vec<String> {
    if Path::new(program).extension().is_some() {
        return vec![program.to_owned()];
    }

    let mut names = vec![program.to_owned()];
    if let Some(pathext) = pathext_env.and_then(OsStr::to_str) {
        for ext in pathext.split(';').filter(|ext| !ext.is_empty()) {
            names.push(format!("{program}{ext}"));
            names.push(format!("{program}{}", ext.to_ascii_lowercase()));
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use std::fs;

    use camino::Utf8PathBuf;

    use crate::store::ARTIFACT_DIR_NAME;

    use super::{
        AppConfig, CodingAgent, PROJECT_CONFIG_FILE_NAME, PromptTransport, RunnerConfig,
        detect_agents_in_path,
    };

    #[test]
    fn codex_runner_preset_matches_exec_cli() {
        let config = RunnerConfig::for_agent(CodingAgent::Codex);
        assert_eq!(config.program, "codex");
        assert_eq!(
            config.args,
            vec![
                "exec",
                "--dangerously-bypass-approvals-and-sandbox",
                "--ephemeral",
            ]
        );
    }

    #[test]
    fn raijin_runner_preset_matches_ephemeral_prompt_cli() {
        let config = RunnerConfig::for_agent(CodingAgent::Raijin);
        assert_eq!(config.program, "raijin");
        assert!(config.args.is_empty());
        assert_eq!(config.prompt_transport, PromptTransport::EnvVar);
        assert_eq!(config.prompt_env_var, "PROMPT");
        assert_eq!(
            config.shell_template.as_deref(),
            Some(r#"raijin -ephemeral "$PROMPT""#)
        );
    }

    #[test]
    fn infers_known_agents_from_program_name() {
        assert_eq!(
            RunnerConfig {
                program: "/opt/homebrew/bin/codex".to_owned(),
                ..RunnerConfig::default()
            }
            .inferred_agent(),
            Some(CodingAgent::Codex)
        );
        assert_eq!(
            RunnerConfig {
                program: r"C:\\Tools\\opencode.exe".to_owned(),
                ..RunnerConfig::default()
            }
            .inferred_agent(),
            Some(CodingAgent::Opencode)
        );
        assert_eq!(
            RunnerConfig {
                program: "/usr/local/bin/raijin".to_owned(),
                ..RunnerConfig::default()
            }
            .inferred_agent(),
            Some(CodingAgent::Raijin)
        );
    }

    #[test]
    fn app_config_switches_planner_and_builder_together() {
        let mut config = AppConfig::default();
        config.set_coding_agent(CodingAgent::Codex);
        assert_eq!(config.coding_agent(), CodingAgent::Codex);
        assert_eq!(config.planner.program, "codex");
        assert_eq!(config.builder.program, "codex");
    }

    #[test]
    fn detects_only_agents_present_on_path() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("codex"), "").unwrap();
        fs::write(temp.path().join("raijin"), "").unwrap();

        let detected = detect_agents_in_path(Some(temp.path().as_os_str()), None);
        assert_eq!(detected, vec![CodingAgent::Codex, CodingAgent::Raijin]);
    }

    #[test]
    fn cycles_within_detected_agents() {
        let detected = vec![CodingAgent::Codex, CodingAgent::Raijin];
        assert_eq!(CodingAgent::Codex.next_in(&detected), CodingAgent::Raijin);
        assert_eq!(CodingAgent::Raijin.next_in(&detected), CodingAgent::Codex);
        assert_eq!(CodingAgent::Opencode.next_in(&detected), CodingAgent::Codex);
    }

    #[test]
    fn persists_selected_agent_into_project_config() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        fs::create_dir_all(project_dir.join(ARTIFACT_DIR_NAME)).unwrap();
        fs::write(
            project_dir
                .join(ARTIFACT_DIR_NAME)
                .join(PROJECT_CONFIG_FILE_NAME),
            r#"
planning_max_iterations = 9

[theme]
accent_color = "blue"
"#,
        )
        .unwrap();

        AppConfig::persist_project_coding_agent(&project_dir, CodingAgent::Raijin).unwrap();

        let raw = fs::read_to_string(
            project_dir
                .join(ARTIFACT_DIR_NAME)
                .join(PROJECT_CONFIG_FILE_NAME),
        )
        .unwrap();
        assert!(raw.contains("planning_max_iterations = 9"));
        assert!(raw.contains("accent_color = \"blue\""));
        assert!(raw.contains("program = \"raijin\""));
        assert!(raw.contains("prompt_transport = \"env_var\""));

        let config = AppConfig::load(&project_dir).unwrap();
        assert_eq!(config.coding_agent(), CodingAgent::Raijin);
    }

    #[test]
    fn project_config_path_uses_hidden_ralph_directory() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        AppConfig::persist_project_coding_agent(&project_dir, CodingAgent::Codex).unwrap();

        assert!(!project_dir.join("ralph.toml").is_file());
        assert!(
            project_dir
                .join(ARTIFACT_DIR_NAME)
                .join(PROJECT_CONFIG_FILE_NAME)
                .is_file()
        );
    }
}
