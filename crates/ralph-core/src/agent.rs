use std::{collections::BTreeMap, env, ffi::OsStr, path::Path};

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

    fn all() -> [Self; 3] {
        [Self::Opencode, Self::Codex, Self::Raijin]
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

fn detect_agents_in_path(path: Option<&OsStr>, pathext: Option<&OsStr>) -> Vec<CodingAgent> {
    CodingAgent::all()
        .into_iter()
        .filter(|agent| program_is_on_path(agent.default_program(), path, pathext))
        .collect()
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

#[cfg(test)]
mod tests {
    use super::{CodingAgent, PromptTransport, RunnerConfig};

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
}
