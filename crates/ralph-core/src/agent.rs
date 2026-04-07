use std::{collections::BTreeMap, env, ffi::OsStr, path::Path};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PromptInput {
    Argv,
    #[default]
    Stdin,
    Env,
    File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CommandMode {
    #[default]
    Exec,
    Shell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CodingAgent {
    Opencode,
    #[default]
    Codex,
    Claude,
    Droid,
    Gemini,
    Pi,
    Raijin,
}

impl CodingAgent {
    pub fn detected() -> Vec<Self> {
        let path = env::var_os("PATH");
        let pathext = env::var_os("PATHEXT");
        detect_agents_in_path(path.as_deref(), pathext.as_deref())
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Opencode => "opencode",
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Droid => "droid",
            Self::Gemini => "gemini",
            Self::Pi => "pi",
            Self::Raijin => "raijin",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Opencode => "OpenCode",
            Self::Codex => "Codex",
            Self::Claude => "Claude Code",
            Self::Droid => "Droid",
            Self::Gemini => "Gemini CLI",
            Self::Pi => "Pi Coding",
            Self::Raijin => "Raijin",
        }
    }

    pub fn default_program(self) -> &'static str {
        match self {
            Self::Opencode => "opencode",
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Droid => "droid",
            Self::Gemini => "gemini",
            Self::Pi => "pi",
            Self::Raijin => "raijin",
        }
    }

    pub fn definition(self) -> AgentConfig {
        match self {
            Self::Opencode => AgentConfig {
                id: self.id().to_owned(),
                name: self.label().to_owned(),
                builtin: true,
                hidden: false,
                non_interactive: RunnerConfig {
                    mode: CommandMode::Exec,
                    program: Some("opencode".to_owned()),
                    args: vec![
                        "run".to_owned(),
                        "--format".to_owned(),
                        "default".to_owned(),
                        "--thinking".to_owned(),
                    ],
                    command: None,
                    prompt_input: PromptInput::Stdin,
                    prompt_env_var: default_prompt_env_var(),
                    env: BTreeMap::from([(
                        "OPENCODE_CONFIG_CONTENT".to_owned(),
                        r#"{"$schema":"https://opencode.ai/config.json","permission":"allow","lsp":false}"#
                            .to_owned(),
                    )]),
                },
                interactive: RunnerConfig {
                    mode: CommandMode::Exec,
                    program: Some("opencode".to_owned()),
                    args: vec![
                        "{project_dir}".to_owned(),
                        "--prompt".to_owned(),
                        "{prompt}".to_owned(),
                    ],
                    command: None,
                    prompt_input: PromptInput::Argv,
                    prompt_env_var: default_prompt_env_var(),
                    env: BTreeMap::from([(
                        "OPENCODE_CONFIG_CONTENT".to_owned(),
                        r#"{"$schema":"https://opencode.ai/config.json","permission":"allow","lsp":false}"#
                            .to_owned(),
                    )]),
                },
            },
            Self::Codex => AgentConfig {
                id: self.id().to_owned(),
                name: self.label().to_owned(),
                builtin: true,
                hidden: false,
                non_interactive: RunnerConfig {
                    mode: CommandMode::Exec,
                    program: Some("codex".to_owned()),
                    args: vec![
                        "exec".to_owned(),
                        "--dangerously-bypass-approvals-and-sandbox".to_owned(),
                        "--ephemeral".to_owned(),
                    ],
                    command: None,
                    prompt_input: PromptInput::Stdin,
                    prompt_env_var: default_prompt_env_var(),
                    env: BTreeMap::new(),
                },
                interactive: RunnerConfig {
                    mode: CommandMode::Exec,
                    program: Some("codex".to_owned()),
                    args: vec!["{prompt}".to_owned()],
                    command: None,
                    prompt_input: PromptInput::Argv,
                    prompt_env_var: default_prompt_env_var(),
                    env: BTreeMap::new(),
                },
            },
            Self::Claude => AgentConfig {
                id: self.id().to_owned(),
                name: self.label().to_owned(),
                builtin: true,
                hidden: false,
                non_interactive: RunnerConfig {
                    mode: CommandMode::Exec,
                    program: Some("claude".to_owned()),
                    args: vec![
                        "--dangerously-skip-permissions".to_owned(),
                        "--allow-dangerously-skip-permissions".to_owned(),
                        "-p".to_owned(),
                        "{prompt}".to_owned(),
                    ],
                    command: None,
                    prompt_input: PromptInput::Argv,
                    prompt_env_var: default_prompt_env_var(),
                    env: BTreeMap::new(),
                },
                interactive: RunnerConfig {
                    mode: CommandMode::Exec,
                    program: Some("claude".to_owned()),
                    args: vec![
                        "--dangerously-skip-permissions".to_owned(),
                        "--allow-dangerously-skip-permissions".to_owned(),
                        "{prompt}".to_owned(),
                    ],
                    command: None,
                    prompt_input: PromptInput::Argv,
                    prompt_env_var: default_prompt_env_var(),
                    env: BTreeMap::new(),
                },
            },
            Self::Droid => AgentConfig {
                id: self.id().to_owned(),
                name: self.label().to_owned(),
                builtin: true,
                hidden: false,
                non_interactive: RunnerConfig {
                    mode: CommandMode::Exec,
                    program: Some("droid".to_owned()),
                    args: vec![
                        "exec".to_owned(),
                        "--skip-permissions-unsafe".to_owned(),
                        "{prompt}".to_owned(),
                    ],
                    command: None,
                    prompt_input: PromptInput::Argv,
                    prompt_env_var: default_prompt_env_var(),
                    env: BTreeMap::new(),
                },
                interactive: RunnerConfig {
                    mode: CommandMode::Exec,
                    program: Some("droid".to_owned()),
                    args: vec!["{prompt}".to_owned()],
                    command: None,
                    prompt_input: PromptInput::Argv,
                    prompt_env_var: default_prompt_env_var(),
                    env: BTreeMap::new(),
                },
            },
            Self::Raijin => AgentConfig {
                id: self.id().to_owned(),
                name: self.label().to_owned(),
                builtin: true,
                hidden: false,
                non_interactive: RunnerConfig {
                    mode: CommandMode::Exec,
                    program: Some("raijin".to_owned()),
                    args: vec!["-ephemeral".to_owned(), "{prompt}".to_owned()],
                    command: None,
                    prompt_input: PromptInput::Argv,
                    prompt_env_var: default_prompt_env_var(),
                    env: BTreeMap::new(),
                },
                interactive: RunnerConfig {
                    mode: CommandMode::Exec,
                    program: Some("raijin".to_owned()),
                    args: vec!["-new".to_owned(), "{prompt}".to_owned()],
                    command: None,
                    prompt_input: PromptInput::Argv,
                    prompt_env_var: default_prompt_env_var(),
                    env: BTreeMap::new(),
                },
            },
            Self::Gemini => AgentConfig {
                id: self.id().to_owned(),
                name: self.label().to_owned(),
                builtin: true,
                hidden: false,
                non_interactive: RunnerConfig {
                    mode: CommandMode::Exec,
                    program: Some("gemini".to_owned()),
                    args: vec!["-y".to_owned(), "-p".to_owned(), "{prompt}".to_owned()],
                    command: None,
                    prompt_input: PromptInput::Argv,
                    prompt_env_var: default_prompt_env_var(),
                    env: BTreeMap::new(),
                },
                interactive: RunnerConfig {
                    mode: CommandMode::Exec,
                    program: Some("gemini".to_owned()),
                    args: vec!["-y".to_owned(), "-i".to_owned(), "{prompt}".to_owned()],
                    command: None,
                    prompt_input: PromptInput::Argv,
                    prompt_env_var: default_prompt_env_var(),
                    env: BTreeMap::new(),
                },
            },
            Self::Pi => AgentConfig {
                id: self.id().to_owned(),
                name: self.label().to_owned(),
                builtin: true,
                hidden: false,
                non_interactive: RunnerConfig {
                    mode: CommandMode::Exec,
                    program: Some("pi".to_owned()),
                    args: vec![
                        "--no-session".to_owned(),
                        "-p".to_owned(),
                        "{prompt}".to_owned(),
                    ],
                    command: None,
                    prompt_input: PromptInput::Argv,
                    prompt_env_var: default_prompt_env_var(),
                    env: BTreeMap::new(),
                },
                interactive: RunnerConfig {
                    mode: CommandMode::Exec,
                    program: Some("pi".to_owned()),
                    args: vec!["--no-session".to_owned(), "{prompt}".to_owned()],
                    command: None,
                    prompt_input: PromptInput::Argv,
                    prompt_env_var: default_prompt_env_var(),
                    env: BTreeMap::new(),
                },
            },
        }
    }

    fn all() -> [Self; 7] {
        [
            Self::Opencode,
            Self::Codex,
            Self::Claude,
            Self::Droid,
            Self::Gemini,
            Self::Pi,
            Self::Raijin,
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerConfig {
    #[serde(default)]
    pub mode: CommandMode,
    #[serde(default)]
    pub program: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub prompt_input: PromptInput,
    #[serde(default = "default_prompt_env_var")]
    pub prompt_env_var: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        CodingAgent::default().definition().non_interactive
    }
}

impl RunnerConfig {
    pub fn command_preview(&self) -> String {
        match self.mode {
            CommandMode::Exec => {
                let mut pieces = Vec::new();
                if let Some(program) = &self.program {
                    pieces.push(program.clone());
                }
                pieces.extend(self.args.clone());
                pieces.join(" ")
            }
            CommandMode::Shell => self.command.clone().unwrap_or_default(),
        }
    }

    pub fn is_available(&self) -> bool {
        match self.mode {
            CommandMode::Shell => true,
            CommandMode::Exec => self.program.as_deref().is_some_and(|program| {
                if Path::new(program).is_absolute() || program.contains(std::path::MAIN_SEPARATOR) {
                    return Path::new(program).is_file();
                }
                program_is_on_path(
                    program,
                    env::var_os("PATH").as_deref(),
                    env::var_os("PATHEXT").as_deref(),
                )
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentConfig {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub builtin: bool,
    #[serde(default)]
    pub hidden: bool,
    pub non_interactive: RunnerConfig,
    pub interactive: RunnerConfig,
}

impl AgentConfig {
    pub fn is_available(&self) -> bool {
        self.non_interactive.is_available() || self.interactive.is_available()
    }
}

pub fn builtin_agents() -> Vec<AgentConfig> {
    let mut agents = CodingAgent::all()
        .into_iter()
        .map(CodingAgent::definition)
        .collect::<Vec<_>>();
    agents.push(test_shell_agent_definition());
    agents
}

fn test_shell_agent_definition() -> AgentConfig {
    let runner = RunnerConfig {
        mode: CommandMode::Shell,
        program: None,
        args: Vec::new(),
        command: Some("{prompt}".to_owned()),
        prompt_input: PromptInput::Argv,
        prompt_env_var: default_prompt_env_var(),
        env: BTreeMap::new(),
    };
    AgentConfig {
        id: "__test_shell".to_owned(),
        name: "Test Shell".to_owned(),
        builtin: true,
        hidden: true,
        non_interactive: runner.clone(),
        interactive: runner,
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

fn default_prompt_env_var() -> String {
    "PROMPT".to_owned()
}

#[cfg(test)]
mod tests {
    use super::{CodingAgent, CommandMode, PromptInput, builtin_agents};

    #[test]
    fn builtin_agent_definitions_are_seeded() {
        let builtin_ids = builtin_agents()
            .into_iter()
            .map(|agent| agent.id)
            .collect::<Vec<_>>();
        assert_eq!(
            builtin_ids,
            vec![
                "opencode",
                "codex",
                "claude",
                "droid",
                "gemini",
                "pi",
                "raijin",
                "__test_shell",
            ]
        );
    }

    #[test]
    fn hidden_test_shell_builtin_executes_prompts_verbatim_in_shell_mode() {
        let agent = builtin_agents()
            .into_iter()
            .find(|agent| agent.id == "__test_shell")
            .expect("hidden test shell builtin must exist");

        assert!(agent.hidden);
        assert_eq!(agent.non_interactive.mode, CommandMode::Shell);
        assert_eq!(agent.non_interactive.command.as_deref(), Some("{prompt}"));
        assert_eq!(agent.interactive.mode, CommandMode::Shell);
        assert_eq!(agent.interactive.command.as_deref(), Some("{prompt}"));
    }

    #[test]
    fn codex_builtin_uses_stdin_noninteractive_and_argv_interactive() {
        let codex = CodingAgent::Codex.definition();
        assert_eq!(codex.non_interactive.mode, CommandMode::Exec);
        assert_eq!(codex.non_interactive.prompt_input, PromptInput::Stdin);
        assert_eq!(
            codex.non_interactive.args,
            vec![
                "exec",
                "--dangerously-bypass-approvals-and-sandbox",
                "--ephemeral",
            ]
        );
        assert_eq!(codex.interactive.prompt_input, PromptInput::Argv);
        assert_eq!(codex.interactive.args, vec!["{prompt}"]);
    }

    #[test]
    fn opencode_builtin_carries_permission_env() {
        let opencode = CodingAgent::Opencode.definition();
        assert!(
            opencode
                .non_interactive
                .env
                .contains_key("OPENCODE_CONFIG_CONTENT")
        );
        assert!(
            opencode
                .interactive
                .env
                .contains_key("OPENCODE_CONFIG_CONTENT")
        );
    }

    #[test]
    fn opencode_builtin_commands_match_expected_shapes() {
        let opencode = CodingAgent::Opencode.definition();
        assert_eq!(opencode.non_interactive.prompt_input, PromptInput::Stdin);
        assert_eq!(
            opencode.non_interactive.args,
            vec!["run", "--format", "default", "--thinking"]
        );
        assert_eq!(opencode.interactive.prompt_input, PromptInput::Argv);
        assert_eq!(
            opencode.interactive.args,
            vec!["{project_dir}", "--prompt", "{prompt}"]
        );
    }

    #[test]
    fn raijin_builtin_commands_match_expected_shapes() {
        let raijin = CodingAgent::Raijin.definition();
        assert_eq!(raijin.non_interactive.prompt_input, PromptInput::Argv);
        assert_eq!(raijin.non_interactive.args, vec!["-ephemeral", "{prompt}"]);
        assert_eq!(raijin.interactive.prompt_input, PromptInput::Argv);
        assert_eq!(raijin.interactive.args, vec!["-new", "{prompt}"]);
    }

    #[test]
    fn gemini_builtin_commands_match_expected_shapes() {
        let gemini = CodingAgent::Gemini.definition();
        assert_eq!(gemini.non_interactive.prompt_input, PromptInput::Argv);
        assert_eq!(gemini.non_interactive.args, vec!["-y", "-p", "{prompt}"]);
        assert_eq!(gemini.interactive.prompt_input, PromptInput::Argv);
        assert_eq!(gemini.interactive.args, vec!["-y", "-i", "{prompt}"]);
    }

    #[test]
    fn pi_builtin_commands_match_expected_shapes() {
        let pi = CodingAgent::Pi.definition();
        assert_eq!(pi.non_interactive.prompt_input, PromptInput::Argv);
        assert_eq!(
            pi.non_interactive.args,
            vec!["--no-session", "-p", "{prompt}"]
        );
        assert_eq!(pi.interactive.prompt_input, PromptInput::Argv);
        assert_eq!(pi.interactive.args, vec!["--no-session", "{prompt}"]);
    }

    #[test]
    fn claude_builtin_commands_match_expected_shapes() {
        let claude = CodingAgent::Claude.definition();
        assert_eq!(claude.non_interactive.prompt_input, PromptInput::Argv);
        assert_eq!(
            claude.non_interactive.args,
            vec![
                "--dangerously-skip-permissions",
                "--allow-dangerously-skip-permissions",
                "-p",
                "{prompt}",
            ]
        );
        assert_eq!(claude.interactive.prompt_input, PromptInput::Argv);
        assert_eq!(
            claude.interactive.args,
            vec![
                "--dangerously-skip-permissions",
                "--allow-dangerously-skip-permissions",
                "{prompt}",
            ]
        );
    }

    #[test]
    fn droid_builtin_commands_match_expected_shapes() {
        let droid = CodingAgent::Droid.definition();
        assert_eq!(droid.non_interactive.prompt_input, PromptInput::Argv);
        assert_eq!(
            droid.non_interactive.args,
            vec!["exec", "--skip-permissions-unsafe", "{prompt}"]
        );
        assert_eq!(droid.interactive.prompt_input, PromptInput::Argv);
        assert_eq!(droid.interactive.args, vec!["{prompt}"]);
    }
}
