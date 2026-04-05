use std::fs;

use anyhow::{Context, Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};
use dirs::home_dir;
use serde::{Deserialize, Serialize};

use crate::{AgentConfig, atomic_write, builtin_agents, store::ARTIFACT_DIR_NAME};

const PROJECT_CONFIG_FILE_NAME: &str = "config.toml";

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
pub struct AppConfig {
    #[serde(default = "default_default_agent")]
    pub default_agent: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default = "builtin_agents")]
    pub agents: Vec<AgentConfig>,
    #[serde(default = "default_max_iterations")]
    pub max_iterations: usize,
    #[serde(default)]
    pub editor_override: Option<String>,
    #[serde(default)]
    pub theme: ThemeConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            default_agent: default_default_agent(),
            agent: None,
            agents: builtin_agents(),
            max_iterations: default_max_iterations(),
            editor_override: None,
            theme: ThemeConfig::default(),
        }
    }
}

impl AppConfig {
    pub fn load(project_dir: &Utf8Path) -> Result<Self> {
        seed_user_config_if_missing()?;

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

        config.validate()?;
        Ok(config)
    }

    pub fn agent_id(&self) -> &str {
        self.agent.as_deref().unwrap_or(&self.default_agent)
    }

    pub fn agent_name(&self) -> String {
        self.agent_definition(self.agent_id())
            .map(|agent| agent.name.clone())
            .unwrap_or_else(|| self.agent_id().to_owned())
    }

    pub fn set_agent(&mut self, agent_id: impl Into<String>) {
        self.agent = Some(agent_id.into());
    }

    pub fn agent_definition(&self, agent_id: &str) -> Option<&AgentConfig> {
        self.agents.iter().find(|agent| agent.id == agent_id)
    }

    pub fn all_agents(&self) -> &[AgentConfig] {
        &self.agents
    }

    pub fn available_agents(&self) -> Vec<&AgentConfig> {
        self.agents
            .iter()
            .filter(|agent| agent.is_available())
            .collect()
    }

    pub fn persist_scoped_coding_agent(
        project_dir: &Utf8Path,
        scope: ConfigFileScope,
        agent_id: &str,
    ) -> Result<()> {
        let config = Self::load(project_dir)?;
        if config.agent_definition(agent_id).is_none() {
            return Err(anyhow!("agent '{}' is not defined", agent_id));
        }

        let path = config_path_for_scope(project_dir, scope)?
            .ok_or_else(|| anyhow!("unable to resolve config path for scope"))?;
        let mut partial = if path.exists() {
            read_partial_config(&path)?
        } else {
            PartialAppConfig::default()
        };
        match scope {
            ConfigFileScope::User => partial.default_agent = Some(agent_id.to_owned()),
            ConfigFileScope::Project => partial.agent = Some(agent_id.to_owned()),
        }
        write_partial_config(&path, &partial)
    }

    pub fn user_config_path() -> Result<Option<Utf8PathBuf>> {
        user_config_path()
    }

    pub fn project_config_path(project_dir: &Utf8Path) -> Utf8PathBuf {
        project_config_path(project_dir)
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
            let partial = read_partial_config(&path)?;
            let merged = merge_config(AppConfig::default(), partial);
            merged.validate()?;
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if self.agents.is_empty() {
            return Err(anyhow!("config defines no agents"));
        }

        let mut seen = std::collections::BTreeSet::new();
        for agent in &self.agents {
            if agent.id.trim().is_empty() {
                return Err(anyhow!("agent id cannot be empty"));
            }
            if !seen.insert(agent.id.clone()) {
                return Err(anyhow!("duplicate agent id '{}'", agent.id));
            }
        }

        if self.agent_definition(&self.default_agent).is_none() {
            return Err(anyhow!(
                "default_agent '{}' is not defined in agents",
                self.default_agent
            ));
        }
        if let Some(agent) = &self.agent
            && self.agent_definition(agent).is_none()
        {
            return Err(anyhow!("agent '{}' is not defined in agents", agent));
        }

        Ok(())
    }
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
    default_agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agents: Option<Vec<AgentConfig>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_iterations: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    editor_override: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    theme: Option<PartialThemeConfig>,
}

fn read_partial_config(path: &Utf8Path) -> Result<PartialAppConfig> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file at {path}"))?;
    toml::from_str(&raw).with_context(|| {
        if raw.contains("[runner]") {
            format!(
                "failed to parse config file at {path}; the legacy [runner] schema is no longer supported, regenerate this config with default_agent/agent and [[agents]] entries"
            )
        } else {
            format!("failed to parse config file at {path}")
        }
    })
}

fn write_partial_config(path: &Utf8Path, config: &PartialAppConfig) -> Result<()> {
    let raw = toml::to_string_pretty(config).context("failed to serialize config")?;
    atomic_write(path, raw).with_context(|| format!("failed to write config at {path}"))
}

fn seed_user_config_if_missing() -> Result<()> {
    let Some(path) = user_config_path()? else {
        return Ok(());
    };
    if path.exists() {
        return Ok(());
    }
    let raw = toml::to_string_pretty(&AppConfig::default())
        .context("failed to serialize default user config")?;
    atomic_write(&path, raw).with_context(|| format!("failed to seed user config at {path}"))
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
    let path = if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME") {
        std::path::PathBuf::from(config_home)
            .join("ralph")
            .join(PROJECT_CONFIG_FILE_NAME)
    } else {
        let Some(home) = home_dir() else {
            return Ok(None);
        };
        home.join(".config")
            .join("ralph")
            .join(PROJECT_CONFIG_FILE_NAME)
    };
    Utf8PathBuf::from_path_buf(path)
        .map(Some)
        .map_err(|_| anyhow!("user config path is not valid UTF-8"))
}

fn merge_config(mut base: AppConfig, partial: PartialAppConfig) -> AppConfig {
    if let Some(default_agent) = partial.default_agent {
        base.default_agent = default_agent;
    }
    if let Some(agent) = partial.agent {
        base.agent = normalize_optional_string(agent);
    }
    if let Some(agents) = partial.agents {
        base.agents = agents;
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
    base
}

fn normalize_optional_string(value: String) -> Option<String> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}

fn default_default_agent() -> String {
    "codex".to_owned()
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
    use std::fs;
    use std::sync::OnceLock;

    use camino::Utf8PathBuf;

    use super::{AppConfig, ConfigFileScope, PartialAppConfig, merge_config};
    use crate::AgentConfig;

    fn configure_test_user_config_home() {
        static TEST_CONFIG_HOME: OnceLock<Utf8PathBuf> = OnceLock::new();
        let path = TEST_CONFIG_HOME.get_or_init(|| {
            let path = Utf8PathBuf::from_path_buf(
                std::env::temp_dir().join(format!("ralph-test-config-{}", std::process::id())),
            )
            .unwrap();
            fs::create_dir_all(&path).unwrap();
            path
        });
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", path.as_str());
        }
    }

    #[test]
    fn app_config_defaults_to_seeded_agents() {
        let config = AppConfig::default();
        assert_eq!(config.default_agent, "codex");
        assert!(config.agent_definition("codex").is_some());
        assert!(config.agent_definition("claude").is_some());
        assert!(config.agent_definition("droid").is_some());
        assert!(config.agent_definition("gemini").is_some());
        assert!(config.agent_definition("opencode").is_some());
        assert!(config.agent_definition("raijin").is_some());
    }

    #[test]
    fn merge_config_can_override_project_agent() {
        let merged = merge_config(
            AppConfig::default(),
            PartialAppConfig {
                agent: Some("raijin".to_owned()),
                ..Default::default()
            },
        );
        assert_eq!(merged.agent_id(), "raijin");
    }

    #[test]
    fn persisted_agent_switch_updates_project_agent_only() {
        configure_test_user_config_home();
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();

        AppConfig::persist_scoped_coding_agent(&project_dir, ConfigFileScope::Project, "raijin")
            .unwrap();

        let raw = fs::read_to_string(project_dir.join(".ralph/config.toml")).unwrap();
        assert!(raw.contains("agent = \"raijin\""));
        assert!(!raw.contains("[[agents]]"));
    }

    #[test]
    fn user_config_is_seeded_with_builtin_agents() {
        configure_test_user_config_home();

        let user_path = AppConfig::user_config_path().unwrap().unwrap();
        if user_path.exists() {
            fs::remove_file(&user_path).unwrap();
        }

        let _ = AppConfig::load(Utf8PathBuf::from("/tmp/project").as_ref()).unwrap();
        let raw = fs::read_to_string(user_path).unwrap();
        assert!(raw.contains("[[agents]]"));
        assert!(raw.contains("id = \"codex\""));
        assert!(raw.contains("id = \"claude\""));
        assert!(raw.contains("id = \"droid\""));
        assert!(raw.contains("id = \"gemini\""));
        assert!(raw.contains("id = \"opencode\""));
        assert!(raw.contains("id = \"raijin\""));
    }

    #[test]
    fn effective_agent_name_comes_from_definition() {
        let config = AppConfig {
            agent: Some("raijin".to_owned()),
            ..AppConfig::default()
        };
        assert_eq!(config.agent_name(), "Raijin");
    }

    #[test]
    fn invalid_selected_agent_is_rejected() {
        let config = AppConfig {
            agent: Some("missing".to_owned()),
            ..AppConfig::default()
        };
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("missing"));
    }

    #[test]
    fn replacing_agents_replaces_registry() {
        let merged = merge_config(
            AppConfig::default(),
            PartialAppConfig {
                agents: Some(vec![AgentConfig {
                    id: "custom".to_owned(),
                    name: "Custom".to_owned(),
                    builtin: false,
                    non_interactive: crate::CodingAgent::Codex.definition().non_interactive,
                    interactive: crate::CodingAgent::Codex.definition().interactive,
                }]),
                default_agent: Some("custom".to_owned()),
                ..Default::default()
            },
        );
        assert_eq!(merged.agents.len(), 1);
        assert_eq!(merged.agent_id(), "custom");
    }
}
