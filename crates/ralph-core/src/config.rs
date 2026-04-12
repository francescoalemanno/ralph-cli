use std::fs;

use anyhow::{Context, Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};
use dirs::home_dir;
use serde::{Deserialize, Serialize};

use crate::{AgentConfig, ThemeMode, atomic_write, builtin_agents};

pub const ARTIFACT_DIR_NAME: &str = ".ralph";
const PROJECT_CONFIG_FILE_NAME: &str = "config.toml";
const RALPH_CONFIG_HOME_ENV: &str = "RALPH_CONFIG_HOME";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFileScope {
    User,
    Project,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeConfig {
    #[serde(default)]
    pub mode: ThemeMode,
    #[serde(default = "default_accent_color")]
    pub accent_color: String,
    #[serde(default = "default_success_color")]
    pub success_color: String,
    #[serde(default = "default_warning_color")]
    pub warning_color: String,
    #[serde(default = "default_error_color")]
    pub error_color: String,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            mode: ThemeMode::Auto,
            accent_color: default_accent_color(),
            success_color: default_success_color(),
            warning_color: default_warning_color(),
            error_color: default_error_color(),
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
            editor_override: None,
            theme: ThemeConfig::default(),
        }
    }
}

impl AppConfig {
    pub fn load(project_dir: &Utf8Path) -> Result<Self> {
        seed_user_config_if_missing()?;

        let mut config = Self::default();

        if let Some(user_path) = Self::user_config_path()?
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

    pub fn configured_agent_id(&self) -> &str {
        self.agent.as_deref().unwrap_or(&self.default_agent)
    }

    pub fn agent_id(&self) -> &str {
        self.effective_agent_definition()
            .map(|agent| agent.id.as_str())
            .unwrap_or_else(|| self.configured_agent_id())
    }

    pub fn agent_name(&self) -> String {
        self.effective_agent_definition()
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
        prioritized_agents(
            self.agents
                .iter()
                .enumerate()
                .filter(|(_, agent)| !agent.hidden && agent.is_available()),
        )
    }

    fn effective_agent_definition(&self) -> Option<&AgentConfig> {
        let configured = self.configured_agent_id();
        if let Some(agent) = self.agent_definition(configured)
            && agent.is_available()
        {
            return Some(agent);
        }

        self.available_agents().into_iter().next()
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
        global_config_dir().map(|path| Some(path.join(PROJECT_CONFIG_FILE_NAME)))
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
    mode: Option<ThemeMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    accent_color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    success_color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    warning_color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_color: Option<String>,
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
    let path = global_config_dir()?.join(PROJECT_CONFIG_FILE_NAME);
    if path.exists() {
        refresh_seeded_user_config_if_needed(&path)?;
        return Ok(());
    }
    let raw = toml::to_string_pretty(&AppConfig::default())
        .context("failed to serialize default user config")?;
    atomic_write(&path, raw).with_context(|| format!("failed to seed user config at {path}"))
}

fn refresh_seeded_user_config_if_needed(path: &Utf8Path) -> Result<()> {
    let partial = read_partial_config(path)?;
    let Some(agents) = partial.agents.as_ref() else {
        return Ok(());
    };
    if !is_builtin_registry_snapshot(agents) {
        return Ok(());
    }

    let current_builtins = builtin_agents();
    if agents == current_builtins.as_slice() {
        return Ok(());
    }

    let mut updated = partial;
    updated.agents = Some(current_builtins);
    write_partial_config(path, &updated)
}

fn is_builtin_registry_snapshot(agents: &[AgentConfig]) -> bool {
    if agents.is_empty() {
        return false;
    }

    let current_builtins = builtin_agents();
    let current_by_id = current_builtins
        .iter()
        .map(|agent| (agent.id.as_str(), agent))
        .collect::<std::collections::BTreeMap<_, _>>();

    agents.iter().all(|agent| {
        agent.builtin
            && current_by_id
                .get(agent.id.as_str())
                .is_some_and(|builtin| builtin == &agent)
    })
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
        ConfigFileScope::User => AppConfig::user_config_path(),
        ConfigFileScope::Project => Ok(Some(project_config_path(project_dir))),
    }
}

pub fn global_config_dir() -> Result<Utf8PathBuf> {
    if let Some(path) = global_config_dir_override_value() {
        return Ok(path);
    }

    if let Some(path) = std::env::var_os(RALPH_CONFIG_HOME_ENV) {
        return Utf8PathBuf::from_path_buf(path.into())
            .map_err(|_| anyhow!("{RALPH_CONFIG_HOME_ENV} is not valid UTF-8"));
    }

    canonical_global_config_dir()
}

fn global_config_dir_override_value() -> Option<Utf8PathBuf> {
    global_config_dir_override()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn global_config_dir_override() -> &'static std::sync::RwLock<Option<Utf8PathBuf>> {
    use std::sync::{OnceLock, RwLock};

    static GLOBAL_CONFIG_DIR_OVERRIDE: OnceLock<RwLock<Option<Utf8PathBuf>>> = OnceLock::new();
    GLOBAL_CONFIG_DIR_OVERRIDE.get_or_init(|| RwLock::new(None))
}

fn global_config_dir_override_lock() -> &'static std::sync::Mutex<()> {
    use std::sync::{Mutex, OnceLock};

    static GLOBAL_CONFIG_DIR_OVERRIDE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    GLOBAL_CONFIG_DIR_OVERRIDE_LOCK.get_or_init(|| Mutex::new(()))
}

fn canonical_global_config_dir() -> Result<Utf8PathBuf> {
    let Some(home) = home_dir() else {
        return Err(anyhow!(
            "failed to resolve the Ralph global config directory"
        ));
    };
    Utf8PathBuf::from_path_buf(home.join(".config").join("ralph"))
        .map_err(|_| anyhow!("Ralph global config directory is not valid UTF-8"))
}

#[doc(hidden)]
pub struct ScopedGlobalConfigDirOverride {
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl ScopedGlobalConfigDirOverride {
    pub fn new(path: Utf8PathBuf) -> Self {
        let guard = global_config_dir_override_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *global_config_dir_override()
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(path);
        Self { _guard: guard }
    }
}

impl Drop for ScopedGlobalConfigDirOverride {
    fn drop(&mut self) {
        *global_config_dir_override()
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }
}

#[doc(hidden)]
pub fn scoped_global_config_dir_override(path: Utf8PathBuf) -> ScopedGlobalConfigDirOverride {
    ScopedGlobalConfigDirOverride::new(path)
}

#[cfg(test)]
pub(crate) fn configure_test_global_config_home() -> (Utf8PathBuf, ScopedGlobalConfigDirOverride) {
    use std::sync::OnceLock;

    static TEST_CONFIG_HOME: OnceLock<Utf8PathBuf> = OnceLock::new();
    let path = TEST_CONFIG_HOME.get_or_init(|| {
        let path = Utf8PathBuf::from_path_buf(
            std::env::temp_dir().join(format!("ralph-test-config-{}", std::process::id())),
        )
        .unwrap();
        fs::create_dir_all(&path).unwrap();
        path
    });
    (
        path.clone(),
        scoped_global_config_dir_override(path.clone()),
    )
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
    if let Some(editor_override) = partial.editor_override {
        base.editor_override = normalize_optional_string(editor_override);
    }
    if let Some(theme) = partial.theme {
        if let Some(value) = theme.mode {
            base.theme.mode = value;
        }
        if let Some(value) = theme.accent_color {
            base.theme.accent_color = value;
        }
        if let Some(value) = theme.success_color {
            base.theme.success_color = value;
        }
        if let Some(value) = theme.warning_color {
            base.theme.warning_color = value;
        }
        if let Some(value) = theme.error_color {
            base.theme.error_color = value;
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

fn prioritized_agents<'a, I>(agents: I) -> Vec<&'a AgentConfig>
where
    I: IntoIterator<Item = (usize, &'a AgentConfig)>,
{
    let mut prioritized = agents.into_iter().collect::<Vec<_>>();
    prioritized.sort_by_key(|(index, agent)| (agent_priority_bucket(agent.id.as_str()), *index));
    prioritized.into_iter().map(|(_, agent)| agent).collect()
}

fn agent_priority_bucket(agent_id: &str) -> usize {
    match agent_id {
        "opencode" => 0,
        "raijin" => 1,
        _ => 2,
    }
}

fn default_default_agent() -> String {
    "opencode".to_owned()
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

fn default_error_color() -> String {
    "red".to_owned()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use camino::Utf8PathBuf;

    use super::{
        AppConfig, ConfigFileScope, PartialAppConfig, configure_test_global_config_home,
        merge_config, write_partial_config,
    };
    use crate::{AgentConfig, CommandMode, PromptInput, RunnerConfig};

    #[test]
    fn app_config_defaults_to_seeded_agents() {
        let config = AppConfig::default();
        assert_eq!(config.default_agent, "opencode");
        assert!(config.agent_definition("codex").is_some());
        assert!(config.agent_definition("claude").is_some());
        assert!(config.agent_definition("droid").is_some());
        assert!(config.agent_definition("gemini").is_some());
        assert!(config.agent_definition("opencode").is_some());
        assert!(config.agent_definition("pi").is_some());
        assert!(config.agent_definition("raijin").is_some());
        assert!(config.agent_definition("__test_shell").is_some());
    }

    #[test]
    fn available_agents_exclude_hidden_builtins() {
        let config = AppConfig::default();
        assert!(
            config
                .available_agents()
                .into_iter()
                .all(|agent| agent.id != "__test_shell")
        );
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
        assert_eq!(merged.configured_agent_id(), "raijin");
    }

    #[test]
    fn persisted_agent_switch_updates_project_agent_only() {
        let (_, _guard) = configure_test_global_config_home();
        if let Some(user_path) = AppConfig::user_config_path().unwrap()
            && user_path.exists()
        {
            fs::remove_file(&user_path).unwrap();
        }
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
        let (_, _guard) = configure_test_global_config_home();

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
        assert!(raw.contains("id = \"pi\""));
        assert!(raw.contains("id = \"raijin\""));
        assert!(!raw.contains("max_iterations"));
    }

    #[test]
    fn stale_seeded_user_config_is_refreshed_with_new_builtins() {
        let (_, _guard) = configure_test_global_config_home();
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();

        let user_path = AppConfig::user_config_path().unwrap().unwrap();
        let old_builtins = crate::builtin_agents()
            .into_iter()
            .filter(|agent| agent.id != "pi")
            .collect::<Vec<_>>();
        write_partial_config(
            &user_path,
            &PartialAppConfig {
                default_agent: Some("codex".to_owned()),
                agents: Some(old_builtins),
                ..Default::default()
            },
        )
        .unwrap();

        let config = AppConfig::load(&project_dir).unwrap();
        assert!(config.agent_definition("pi").is_some());

        let raw = fs::read_to_string(user_path).unwrap();
        assert!(raw.contains("id = \"pi\""));
    }

    #[test]
    fn custom_user_agent_registry_is_not_rewritten_with_builtins() {
        let (_, _guard) = configure_test_global_config_home();
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();

        let user_path = AppConfig::user_config_path().unwrap().unwrap();
        write_partial_config(
            &user_path,
            &PartialAppConfig {
                default_agent: Some("custom".to_owned()),
                agents: Some(vec![AgentConfig {
                    id: "custom".to_owned(),
                    name: "Custom".to_owned(),
                    builtin: false,
                    hidden: false,
                    runner: crate::CodingAgent::Codex.definition().runner,
                }]),
                ..Default::default()
            },
        )
        .unwrap();

        let config = AppConfig::load(&project_dir).unwrap();
        assert!(config.agent_definition("custom").is_some());
        assert!(config.agent_definition("pi").is_none());

        let raw = fs::read_to_string(user_path).unwrap();
        assert!(!raw.contains("id = \"pi\""));
    }

    #[test]
    fn effective_agent_name_comes_from_definition() {
        let config = AppConfig {
            default_agent: "raijin".to_owned(),
            agents: vec![shell_agent("raijin", "Raijin")],
            ..AppConfig::default()
        };
        assert_eq!(config.agent_name(), "Raijin");
    }

    #[test]
    fn effective_agent_prefers_prioritized_available_fallback() {
        let config = AppConfig {
            default_agent: "codex".to_owned(),
            agent: Some("codex".to_owned()),
            agents: vec![
                unavailable_agent("codex", "Codex"),
                shell_agent("claude", "Claude"),
                shell_agent("raijin", "Raijin"),
                shell_agent("opencode", "OpenCode"),
            ],
            ..AppConfig::default()
        };

        assert_eq!(config.configured_agent_id(), "codex");
        assert_eq!(config.agent_id(), "opencode");
        assert_eq!(config.agent_name(), "OpenCode");
    }

    #[test]
    fn available_agents_follow_priority_order() {
        let config = AppConfig {
            default_agent: "custom".to_owned(),
            agents: vec![
                shell_agent("custom", "Custom"),
                shell_agent("raijin", "Raijin"),
                shell_agent("opencode", "OpenCode"),
                shell_agent("claude", "Claude"),
            ],
            ..AppConfig::default()
        };

        let available = config
            .available_agents()
            .into_iter()
            .map(|agent| agent.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(available, vec!["opencode", "raijin", "custom", "claude"]);
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
                    hidden: false,
                    runner: crate::CodingAgent::Codex.definition().runner,
                }]),
                default_agent: Some("custom".to_owned()),
                ..Default::default()
            },
        );
        assert_eq!(merged.agents.len(), 1);
        assert_eq!(merged.configured_agent_id(), "custom");
    }

    #[test]
    fn canonical_global_config_dir_uses_dot_config() {
        let path = super::canonical_global_config_dir().unwrap();
        assert!(path.as_str().ends_with("/.config/ralph"));
    }

    #[test]
    fn effective_config_omits_workflow_iteration_limit() {
        let raw = AppConfig::default().effective_toml().unwrap();
        assert!(!raw.contains("max_iterations"));
    }

    fn shell_agent(id: &str, name: &str) -> AgentConfig {
        let runner = RunnerConfig {
            mode: CommandMode::Shell,
            program: None,
            args: Vec::new(),
            command: Some("echo ok".to_owned()),
            prompt_input: PromptInput::Argv,
            prompt_env_var: "PROMPT".to_owned(),
            env: Default::default(),
            session_timeout_secs: None,
            idle_timeout_secs: None,
        };
        AgentConfig {
            id: id.to_owned(),
            name: name.to_owned(),
            builtin: false,
            hidden: false,
            runner,
        }
    }

    fn unavailable_agent(id: &str, name: &str) -> AgentConfig {
        let runner = RunnerConfig {
            mode: CommandMode::Exec,
            program: Some("__missing_agent__".to_owned()),
            args: Vec::new(),
            command: None,
            prompt_input: PromptInput::Argv,
            prompt_env_var: "PROMPT".to_owned(),
            env: Default::default(),
            session_timeout_secs: None,
            idle_timeout_secs: None,
        };
        AgentConfig {
            id: id.to_owned(),
            name: name.to_owned(),
            builtin: false,
            hidden: false,
            runner,
        }
    }
}
