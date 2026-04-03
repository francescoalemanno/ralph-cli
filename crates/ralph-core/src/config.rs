use std::{collections::BTreeMap, env, ffi::OsStr, fs, path::Path};

use anyhow::{Context, Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};
use dirs::home_dir;
use serde::{Deserialize, Serialize};

use crate::{atomic_write, store::ARTIFACT_DIR_NAME};

const PROJECT_CONFIG_FILE_NAME: &str = "config.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CliColorMode {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CliPagerMode {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CliOutputMode {
    #[default]
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CliPromptInputMode {
    #[default]
    Auto,
    Stdin,
    Editor,
    Prompt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFileScope {
    User,
    Project,
}

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

    pub fn default_program(self) -> &'static str {
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
    pub shell_template: Option<String>,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self::for_agent(CodingAgent::default())
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
                shell_template: None,
            },
            CodingAgent::Raijin => Self {
                program: "raijin".to_owned(),
                args: Vec::new(),
                env: BTreeMap::new(),
                prompt_transport: PromptTransport::EnvVar,
                prompt_env_var: default_prompt_env_var(),
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
pub struct CliConfig {
    #[serde(default)]
    pub color: CliColorMode,
    #[serde(default)]
    pub pager: CliPagerMode,
    #[serde(default)]
    pub output: CliOutputMode,
    #[serde(default)]
    pub prompt_input: CliPromptInputMode,
}

impl Default for CliConfig {
    fn default() -> Self {
        Self {
            color: CliColorMode::Auto,
            pager: CliPagerMode::Auto,
            output: CliOutputMode::Text,
            prompt_input: CliPromptInputMode::Auto,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub runner: RunnerConfig,
    #[serde(default = "default_max_iterations")]
    pub max_iterations: usize,
    #[serde(default)]
    pub editor_override: Option<String>,
    #[serde(default)]
    pub theme: ThemeConfig,
    #[serde(default)]
    pub cli: CliConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            runner: RunnerConfig::default(),
            max_iterations: default_max_iterations(),
            editor_override: None,
            theme: ThemeConfig::default(),
            cli: CliConfig::default(),
        }
    }
}

impl AppConfig {
    pub fn load(project_dir: &Utf8Path) -> Result<Self> {
        let mut config = Self::default();

        if let Some(user_path) = user_config_path()?
            && user_path.exists()
        {
            config = merge_config(config, read_partial_config(&user_path)?);
        }

        let project_path = project_config_path(project_dir);
        if project_path.exists() {
            config = merge_config(config, read_partial_config(&project_path)?);
        }

        config.select_detected_coding_agent(&CodingAgent::detected());
        Ok(config)
    }

    pub fn coding_agent(&self) -> CodingAgent {
        self.runner.inferred_agent().unwrap_or_default()
    }

    pub fn set_coding_agent(&mut self, agent: CodingAgent) {
        self.runner = RunnerConfig::for_agent(agent);
    }

    pub fn select_detected_coding_agent(&mut self, detected: &[CodingAgent]) -> bool {
        let Some(current) = self.runner.inferred_agent() else {
            return false;
        };
        if detected.is_empty() || detected.contains(&current) {
            return false;
        }
        self.set_coding_agent(detected[0]);
        true
    }

    pub fn persist_project_coding_agent(project_dir: &Utf8Path, agent: CodingAgent) -> Result<()> {
        Self::persist_scoped_coding_agent(project_dir, ConfigFileScope::Project, agent)
    }

    pub fn persist_scoped_coding_agent(
        project_dir: &Utf8Path,
        scope: ConfigFileScope,
        agent: CodingAgent,
    ) -> Result<()> {
        let path = config_path_for_scope(project_dir, scope)?
            .ok_or_else(|| anyhow!("unable to resolve config path for scope"))?;
        let mut config = if path.exists() {
            let partial = read_partial_config(&path)?;
            merge_config(AppConfig::default(), partial)
        } else {
            AppConfig::default()
        };
        config.set_coding_agent(agent);
        write_config(&path, &config)
    }

    pub fn user_config_path() -> Result<Option<Utf8PathBuf>> {
        user_config_path()
    }

    pub fn project_config_path(project_dir: &Utf8Path) -> Utf8PathBuf {
        project_config_path(project_dir)
    }

    pub fn config_path_for_scope(
        project_dir: &Utf8Path,
        scope: ConfigFileScope,
    ) -> Result<Option<Utf8PathBuf>> {
        config_path_for_scope(project_dir, scope)
    }

    pub fn scoped_config_toml(
        project_dir: &Utf8Path,
        scope: ConfigFileScope,
    ) -> Result<Option<String>> {
        let Some(path) = config_path_for_scope(project_dir, scope)? else {
            return Ok(None);
        };
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(fs::read_to_string(&path).with_context(|| {
            format!("failed to read config at {path}")
        })?))
    }

    pub fn effective_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).context("failed to serialize effective config")
    }

    pub fn validate_scoped_config(project_dir: &Utf8Path, scope: ConfigFileScope) -> Result<()> {
        if let Some(path) = config_path_for_scope(project_dir, scope)?
            && path.exists()
        {
            read_partial_config(&path)?;
        }
        Ok(())
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
struct PartialCliConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    color: Option<CliColorMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pager: Option<CliPagerMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<CliOutputMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_input: Option<CliPromptInputMode>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct PartialAppConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    runner: Option<PartialRunnerConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_iterations: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    editor_override: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    theme: Option<PartialThemeConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cli: Option<PartialCliConfig>,
}

fn read_partial_config(path: &Utf8Path) -> Result<PartialAppConfig> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file at {path}"))?;
    toml::from_str(&raw).with_context(|| format!("failed to parse config file at {path}"))
}

fn write_config(path: &Utf8Path, config: &AppConfig) -> Result<()> {
    let raw = toml::to_string_pretty(config).context("failed to serialize config")?;
    atomic_write(path, raw).with_context(|| format!("failed to write config at {path}"))
}

fn project_config_path(project_dir: &Utf8Path) -> Utf8PathBuf {
    project_dir
        .join(ARTIFACT_DIR_NAME)
        .join(PROJECT_CONFIG_FILE_NAME)
}

fn config_path_for_scope(
    project_dir: &Utf8Path,
    scope: ConfigFileScope,
) -> Result<Option<Utf8PathBuf>> {
    match scope {
        ConfigFileScope::User => user_config_path(),
        ConfigFileScope::Project => Ok(Some(project_config_path(project_dir))),
    }
}

fn user_config_path() -> Result<Option<Utf8PathBuf>> {
    let Some(home) = home_dir() else {
        return Ok(None);
    };
    let path = home
        .join(".config")
        .join("ralph")
        .join(PROJECT_CONFIG_FILE_NAME);
    Utf8PathBuf::from_path_buf(path)
        .map(Some)
        .map_err(|_| anyhow!("user config path is not valid UTF-8"))
}

fn merge_config(mut base: AppConfig, partial: PartialAppConfig) -> AppConfig {
    if let Some(runner) = partial.runner {
        base.runner = merge_runner(base.runner, runner);
    }
    if let Some(max_iterations) = partial.max_iterations {
        base.max_iterations = max_iterations;
    }
    if let Some(editor_override) = partial.editor_override {
        base.editor_override = Some(editor_override);
    }
    if let Some(theme) = partial.theme {
        if let Some(value) = theme.accent_color {
            base.theme.accent_color = value;
        }
        if let Some(value) = theme.success_color {
            base.theme.success_color = value;
        }
        if let Some(value) = theme.warning_color {
            base.theme.warning_color = value;
        }
    }
    if let Some(cli) = partial.cli {
        if let Some(value) = cli.color {
            base.cli.color = value;
        }
        if let Some(value) = cli.pager {
            base.cli.pager = value;
        }
        if let Some(value) = cli.output {
            base.cli.output = value;
        }
        if let Some(value) = cli.prompt_input {
            base.cli.prompt_input = value;
        }
    }
    base
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
    if let Some(shell_template) = partial.shell_template {
        runner.shell_template = Some(shell_template);
    }
    runner
}

fn detect_agents_in_path(path: Option<&OsStr>, pathext: Option<&OsStr>) -> Vec<CodingAgent> {
    CodingAgent::all()
        .into_iter()
        .filter(|agent| program_is_on_path(agent.default_program(), path, pathext))
        .collect()
}

impl CodingAgent {
    fn all() -> [Self; 3] {
        [Self::Opencode, Self::Codex, Self::Raijin]
    }
}

fn program_is_on_path(program: &str, path: Option<&OsStr>, pathext: Option<&OsStr>) -> bool {
    let Some(path) = path else {
        return false;
    };
    let extensions = executable_extensions(pathext);
    env::split_paths(path).any(|dir| {
        if extensions.is_empty() {
            dir.join(program).is_file()
        } else {
            extensions
                .iter()
                .any(|extension| dir.join(format!("{program}{extension}")).is_file())
        }
    })
}

fn executable_extensions(pathext: Option<&OsStr>) -> Vec<String> {
    if cfg!(windows) {
        pathext
            .and_then(|value| value.to_str())
            .map(|value| {
                value
                    .split(';')
                    .filter(|part| !part.is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| vec![".exe".to_owned(), ".cmd".to_owned(), ".bat".to_owned()])
    } else {
        Vec::new()
    }
}

fn normalized_program_name(program: &str) -> Option<String> {
    let file_name = Path::new(program).file_name()?.to_string_lossy();
    Some(file_name.trim_end_matches(".exe").to_ascii_lowercase())
}

fn default_prompt_env_var() -> String {
    "PROMPT".to_owned()
}

fn default_max_iterations() -> usize {
    40
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{AppConfig, CodingAgent, PromptTransport, RunnerConfig, merge_runner};

    #[test]
    fn agent_presets_preserve_commands() {
        let opencode = RunnerConfig::for_agent(CodingAgent::Opencode);
        assert_eq!(opencode.program, "opencode");
        assert_eq!(
            opencode.args,
            vec!["run", "--format", "default", "--thinking"]
        );

        let codex = RunnerConfig::for_agent(CodingAgent::Codex);
        assert_eq!(codex.program, "codex");
        assert_eq!(
            codex.args,
            vec![
                "exec",
                "--dangerously-bypass-approvals-and-sandbox",
                "--ephemeral"
            ]
        );

        let raijin = RunnerConfig::for_agent(CodingAgent::Raijin);
        assert_eq!(raijin.program, "raijin");
        assert_eq!(raijin.prompt_transport, PromptTransport::EnvVar);
        assert_eq!(
            raijin.shell_template.as_deref(),
            Some(r#"raijin -ephemeral "$PROMPT""#)
        );
    }

    #[test]
    fn app_config_switches_runner_agent() {
        let mut config = AppConfig::default();
        config.set_coding_agent(CodingAgent::Codex);
        assert_eq!(config.runner.program, "codex");
    }

    #[test]
    fn detected_agent_fallback_still_switches_known_built_in_runner() {
        let mut config = AppConfig::default();
        assert!(config.select_detected_coding_agent(&[CodingAgent::Codex]));
        assert_eq!(config.runner.program, "codex");
    }

    #[test]
    fn detected_agent_fallback_preserves_unknown_custom_runner() {
        let mut config = AppConfig {
            runner: RunnerConfig {
                program: "custom-runner".to_owned(),
                args: vec!["--json".to_owned()],
                env: BTreeMap::from([("CUSTOM_ENV".to_owned(), "1".to_owned())]),
                prompt_transport: PromptTransport::TempFile,
                prompt_env_var: "CUSTOM_PROMPT".to_owned(),
                shell_template: Some("custom-runner {prompt_file}".to_owned()),
            },
            ..Default::default()
        };

        assert!(!config.select_detected_coding_agent(&[CodingAgent::Codex]));
        assert_eq!(config.runner.program, "custom-runner");
        assert_eq!(config.runner.args, vec!["--json"]);
        assert_eq!(
            config.runner.env.get("CUSTOM_ENV").map(String::as_str),
            Some("1")
        );
        assert_eq!(config.runner.prompt_transport, PromptTransport::TempFile);
        assert_eq!(config.runner.prompt_env_var, "CUSTOM_PROMPT");
        assert_eq!(
            config.runner.shell_template.as_deref(),
            Some("custom-runner {prompt_file}")
        );
    }

    #[test]
    fn merge_runner_overrides_selected_fields() {
        let merged = merge_runner(
            RunnerConfig::default(),
            super::PartialRunnerConfig {
                env: Some(BTreeMap::from([("A".to_owned(), "B".to_owned())])),
                ..Default::default()
            },
        );
        assert_eq!(merged.env.get("A").map(String::as_str), Some("B"));
    }
}
