use std::{collections::BTreeMap, fs};

use anyhow::{Context, Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};
use dirs::home_dir;
use serde::{Deserialize, Serialize};

use crate::{CodingAgent, PromptTransport, RunnerConfig, atomic_write, store::ARTIFACT_DIR_NAME};

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
        self.runner = self.runner.with_agent_preserving_env(agent);
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
        base.editor_override = normalize_optional_string(editor_override);
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
        runner.shell_template = normalize_optional_string(shell_template);
    }
    runner
}

fn normalize_optional_string(value: String) -> Option<String> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
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
    use std::fs;

    use camino::Utf8PathBuf;

    use super::{
        AppConfig, ConfigFileScope, PartialAppConfig, PartialRunnerConfig, merge_config,
        merge_runner,
    };
    use crate::{CodingAgent, PromptTransport, RunnerConfig};

    #[test]
    fn app_config_switches_runner_agent() {
        let mut config = AppConfig::default();
        config.set_coding_agent(CodingAgent::Codex);
        assert_eq!(config.runner.program, "codex");
    }

    #[test]
    fn switching_agent_preserves_runner_env_overrides() {
        let mut config = AppConfig {
            runner: RunnerConfig {
                env: BTreeMap::from([
                    ("OPENAI_API_KEY".to_owned(), "test-key".to_owned()),
                    ("RUST_LOG".to_owned(), "debug".to_owned()),
                ]),
                ..RunnerConfig::for_agent(CodingAgent::Opencode)
            },
            ..Default::default()
        };

        config.set_coding_agent(CodingAgent::Codex);

        assert_eq!(config.runner.program, "codex");
        assert_eq!(
            config.runner.args,
            vec![
                "exec",
                "--dangerously-bypass-approvals-and-sandbox",
                "--ephemeral"
            ]
        );
        assert_eq!(
            config.runner.env.get("OPENAI_API_KEY").map(String::as_str),
            Some("test-key")
        );
        assert_eq!(
            config.runner.env.get("RUST_LOG").map(String::as_str),
            Some("debug")
        );
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
            PartialRunnerConfig {
                env: Some(BTreeMap::from([("A".to_owned(), "B".to_owned())])),
                ..Default::default()
            },
        );
        assert_eq!(merged.env.get("A").map(String::as_str), Some("B"));
    }

    #[test]
    fn persisted_agent_switch_keeps_project_runner_env() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let config_dir = project_dir.join(".ralph");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("config.toml"),
            r#"[runner]
program = "opencode"
args = ["run"]

[runner.env]
OPENAI_API_KEY = "test-key"
RUST_LOG = "debug"
"#,
        )
        .unwrap();

        AppConfig::persist_scoped_coding_agent(
            &project_dir,
            ConfigFileScope::Project,
            CodingAgent::Codex,
        )
        .unwrap();

        let config = AppConfig::load(&project_dir).unwrap();
        assert_eq!(config.runner.program, "codex");
        assert_eq!(
            config.runner.env.get("OPENAI_API_KEY").map(String::as_str),
            Some("test-key")
        );
        assert_eq!(
            config.runner.env.get("RUST_LOG").map(String::as_str),
            Some("debug")
        );
    }

    #[test]
    fn blank_project_editor_override_clears_inherited_user_value() {
        let merged = merge_config(
            AppConfig {
                editor_override: Some("nvim".to_owned()),
                ..Default::default()
            },
            PartialAppConfig {
                editor_override: Some("   ".to_owned()),
                ..Default::default()
            },
        );

        assert_eq!(merged.editor_override, None);
    }

    #[test]
    fn blank_project_shell_template_clears_inherited_user_value() {
        let merged = merge_runner(
            RunnerConfig {
                program: "custom-runner".to_owned(),
                args: vec!["--json".to_owned()],
                env: BTreeMap::new(),
                prompt_transport: PromptTransport::TempFile,
                prompt_env_var: "PROMPT".to_owned(),
                shell_template: Some("custom-runner {prompt_file}".to_owned()),
            },
            PartialRunnerConfig {
                program: Some("codex".to_owned()),
                args: Some(vec![
                    "exec".to_owned(),
                    "--dangerously-bypass-approvals-and-sandbox".to_owned(),
                    "--ephemeral".to_owned(),
                ]),
                shell_template: Some("".to_owned()),
                ..Default::default()
            },
        );

        assert_eq!(merged.program, "codex");
        assert_eq!(
            merged.args,
            vec![
                "exec",
                "--dangerously-bypass-approvals-and-sandbox",
                "--ephemeral"
            ]
        );
        assert_eq!(merged.shell_template, None);
    }
}
