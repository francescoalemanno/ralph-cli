use std::{collections::BTreeMap, fs};

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use dirs::config_dir;
use serde::{Deserialize, Serialize};

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
    NativeTool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CodingAgent {
    #[default]
    Opencode,
    Codex,
}

impl CodingAgent {
    pub fn label(self) -> &'static str {
        match self {
            Self::Opencode => "OpenCode",
            Self::Codex => "Codex",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Opencode => Self::Codex,
            Self::Codex => Self::Opencode,
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
        }
    }

    pub fn inferred_agent(&self) -> Option<CodingAgent> {
        match normalized_program_name(&self.program).as_deref() {
            Some("opencode") => Some(CodingAgent::Opencode),
            Some("codex") => Some(CodingAgent::Codex),
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

#[derive(Debug, Clone, Default, Deserialize)]
struct PartialRunnerConfig {
    program: Option<String>,
    args: Option<Vec<String>>,
    env: Option<BTreeMap<String, String>>,
    prompt_transport: Option<PromptTransport>,
    prompt_env_var: Option<String>,
    question_support: Option<QuestionSupportMode>,
    shell_template: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct PartialThemeConfig {
    accent_color: Option<String>,
    success_color: Option<String>,
    warning_color: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct PartialAppConfig {
    planner: Option<PartialRunnerConfig>,
    builder: Option<PartialRunnerConfig>,
    planning_max_iterations: Option<usize>,
    builder_max_iterations: Option<usize>,
    editor_override: Option<String>,
    theme: Option<PartialThemeConfig>,
}

impl AppConfig {
    pub fn load(project_dir: &Utf8Path) -> Result<Self> {
        let mut config = Self::default();

        if let Some(user_path) = user_config_path()? {
            if user_path.exists() {
                let partial = read_partial_config(&user_path)?;
                config = merge_config(config, partial);
            }
        }

        let project_path = project_dir.join("ralph.toml");
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
}

fn read_partial_config(path: &Utf8Path) -> Result<PartialAppConfig> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file at {path}"))?;
    toml::from_str(&raw).with_context(|| format!("failed to parse config file at {path}"))
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

fn user_config_path() -> Result<Option<Utf8PathBuf>> {
    let Some(base) = config_dir() else {
        return Ok(None);
    };

    let path = base.join("ralph").join("config.toml");
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

#[cfg(test)]
mod tests {
    use super::{AppConfig, CodingAgent, RunnerConfig};

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
    }

    #[test]
    fn app_config_switches_planner_and_builder_together() {
        let mut config = AppConfig::default();
        config.set_coding_agent(CodingAgent::Codex);
        assert_eq!(config.coding_agent(), CodingAgent::Codex);
        assert_eq!(config.planner.program, "codex");
        assert_eq!(config.builder.program, "codex");
    }
}
