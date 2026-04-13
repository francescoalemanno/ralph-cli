use std::{
    collections::BTreeMap,
    path::{Component, Path},
};

use serde::{Deserialize, Serialize};
use which::{which, which_global};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PromptInput {
    Argv,
    #[default]
    Stdin,
    Env,
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
    #[default]
    Opencode,
    Codex,
    Claude,
    Droid,
    Gemini,
    Pi,
    Raijin,
}

impl CodingAgent {
    pub fn detected() -> Vec<Self> {
        detect_agents_on_path()
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
                runner: RunnerConfig {
                    mode: CommandMode::Exec,
                    program: Some("opencode".to_owned()),
                    args: vec![
                        "run".to_owned(),
                        "--thinking".to_owned(),
                        "--format".to_owned(),
                        "json".to_owned(),
                    ],
                    command: None,
                    prompt_input: PromptInput::Stdin,
                    prompt_env_var: default_prompt_env_var(),
                    env: BTreeMap::from([(
                        "OPENCODE_CONFIG_CONTENT".to_owned(),
                        r#"{"$schema":"https://opencode.ai/config.json","permission":"allow","lsp":false}"#
                            .to_owned(),
                    )]),
                    session_timeout_secs: None,
                    idle_timeout_secs: None,
                },
            },
            Self::Codex => AgentConfig {
                id: self.id().to_owned(),
                name: self.label().to_owned(),
                builtin: true,
                hidden: false,
                runner: RunnerConfig {
                    mode: CommandMode::Exec,
                    program: Some("codex".to_owned()),
                    args: vec![
                        "exec".to_owned(),
                        "--dangerously-bypass-approvals-and-sandbox".to_owned(),
                        "--ephemeral".to_owned(),
                        "--json".to_owned(),
                    ],
                    command: None,
                    prompt_input: PromptInput::Stdin,
                    prompt_env_var: default_prompt_env_var(),
                    env: BTreeMap::new(),
                    session_timeout_secs: None,
                    idle_timeout_secs: None,
                },
            },
            Self::Claude => AgentConfig {
                id: self.id().to_owned(),
                name: self.label().to_owned(),
                builtin: true,
                hidden: false,
                runner: RunnerConfig {
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
                    session_timeout_secs: None,
                    idle_timeout_secs: None,
                },
            },
            Self::Droid => AgentConfig {
                id: self.id().to_owned(),
                name: self.label().to_owned(),
                builtin: true,
                hidden: false,
                runner: RunnerConfig {
                    mode: CommandMode::Exec,
                    program: Some("droid".to_owned()),
                    args: vec![
                        "exec".to_owned(),
                        droid_skip_permissions_flag(),
                        "{prompt}".to_owned(),
                    ],
                    command: None,
                    prompt_input: PromptInput::Argv,
                    prompt_env_var: default_prompt_env_var(),
                    env: BTreeMap::new(),
                    session_timeout_secs: None,
                    idle_timeout_secs: None,
                },
            },
            Self::Raijin => AgentConfig {
                id: self.id().to_owned(),
                name: self.label().to_owned(),
                builtin: true,
                hidden: false,
                runner: RunnerConfig {
                    mode: CommandMode::Exec,
                    program: Some("raijin".to_owned()),
                    args: vec![
                        "-ephemeral".to_owned(),
                        "-no-echo".to_owned(),
                        "-no-thinking".to_owned(),
                        "{prompt}".to_owned(),
                    ],
                    command: None,
                    prompt_input: PromptInput::Argv,
                    prompt_env_var: default_prompt_env_var(),
                    env: BTreeMap::new(),
                    session_timeout_secs: None,
                    idle_timeout_secs: None,
                },
            },
            Self::Gemini => AgentConfig {
                id: self.id().to_owned(),
                name: self.label().to_owned(),
                builtin: true,
                hidden: false,
                runner: RunnerConfig {
                    mode: CommandMode::Exec,
                    program: Some("gemini".to_owned()),
                    args: vec!["-y".to_owned(), "-p".to_owned(), "{prompt}".to_owned()],
                    command: None,
                    prompt_input: PromptInput::Argv,
                    prompt_env_var: default_prompt_env_var(),
                    env: BTreeMap::new(),
                    session_timeout_secs: None,
                    idle_timeout_secs: None,
                },
            },
            Self::Pi => AgentConfig {
                id: self.id().to_owned(),
                name: self.label().to_owned(),
                builtin: true,
                hidden: false,
                runner: RunnerConfig {
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
                    session_timeout_secs: None,
                    idle_timeout_secs: None,
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
    #[serde(default)]
    pub session_timeout_secs: Option<u64>,
    #[serde(default)]
    pub idle_timeout_secs: Option<u64>,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        CodingAgent::default().definition().runner
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
            CommandMode::Exec => self.program.as_deref().is_some_and(executable_is_available),
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
    pub runner: RunnerConfig,
}

impl AgentConfig {
    pub fn is_available(&self) -> bool {
        self.runner.is_available()
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
        session_timeout_secs: None,
        idle_timeout_secs: None,
    };
    AgentConfig {
        id: "__test_shell".to_owned(),
        name: "Test Shell".to_owned(),
        builtin: true,
        hidden: true,
        runner,
    }
}

fn detect_agents_on_path() -> Vec<CodingAgent> {
    CodingAgent::all()
        .into_iter()
        .filter(|agent| executable_is_available(agent.default_program()))
        .collect()
}

fn executable_is_available(program: &str) -> bool {
    if is_bare_program_name(program) {
        which_global(program).is_ok()
    } else {
        which(program).is_ok()
    }
}

fn is_bare_program_name(program: &str) -> bool {
    let mut components = Path::new(program).components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

fn default_prompt_env_var() -> String {
    "PROMPT".to_owned()
}

fn droid_skip_permissions_flag() -> String {
    format!("--skip-permissions-{}{}", "un", "safe")
}

#[cfg(test)]
mod tests {
    use super::{
        CodingAgent, CommandMode, PromptInput, RunnerConfig, builtin_agents,
        droid_skip_permissions_flag,
    };

    fn exec_runner(program: &str) -> RunnerConfig {
        RunnerConfig {
            mode: CommandMode::Exec,
            program: Some(program.to_owned()),
            args: Vec::new(),
            command: None,
            prompt_input: PromptInput::Stdin,
            prompt_env_var: "PROMPT".to_owned(),
            env: Default::default(),
            session_timeout_secs: None,
            idle_timeout_secs: None,
        }
    }

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
        assert_eq!(agent.runner.mode, CommandMode::Shell);
        assert_eq!(agent.runner.command.as_deref(), Some("{prompt}"));
    }

    #[test]
    fn exec_runner_accepts_existing_absolute_program_paths() {
        let current_exe = std::env::current_exe().unwrap();
        assert!(exec_runner(&current_exe.to_string_lossy()).is_available());
    }

    #[test]
    fn exec_runner_rejects_missing_programs() {
        assert!(!exec_runner("__missing_agent__").is_available());
    }

    #[test]
    fn codex_builtin_uses_stdin_exec_mode() {
        let codex = CodingAgent::Codex.definition();
        assert_eq!(codex.runner.mode, CommandMode::Exec);
        assert_eq!(codex.runner.prompt_input, PromptInput::Stdin);
        assert_eq!(
            codex.runner.args,
            vec![
                "exec",
                "--dangerously-bypass-approvals-and-sandbox",
                "--ephemeral",
                "--json",
            ]
        );
    }

    #[test]
    fn opencode_builtin_carries_permission_env() {
        let opencode = CodingAgent::Opencode.definition();
        assert!(opencode.runner.env.contains_key("OPENCODE_CONFIG_CONTENT"));
    }

    #[test]
    fn opencode_builtin_commands_match_expected_shapes() {
        let opencode = CodingAgent::Opencode.definition();
        assert_eq!(opencode.runner.prompt_input, PromptInput::Stdin);
        assert_eq!(
            opencode.runner.args,
            vec!["run", "--thinking", "--format", "json"]
        );
    }

    #[test]
    fn raijin_builtin_commands_match_expected_shapes() {
        let raijin = CodingAgent::Raijin.definition();
        assert_eq!(raijin.runner.prompt_input, PromptInput::Argv);
        assert_eq!(
            raijin.runner.args,
            vec!["-ephemeral", "-no-echo", "-no-thinking", "{prompt}"]
        );
    }

    #[test]
    fn gemini_builtin_commands_match_expected_shapes() {
        let gemini = CodingAgent::Gemini.definition();
        assert_eq!(gemini.runner.prompt_input, PromptInput::Argv);
        assert_eq!(gemini.runner.args, vec!["-y", "-p", "{prompt}"]);
    }

    #[test]
    fn pi_builtin_commands_match_expected_shapes() {
        let pi = CodingAgent::Pi.definition();
        assert_eq!(pi.runner.prompt_input, PromptInput::Argv);
        assert_eq!(pi.runner.args, vec!["--no-session", "-p", "{prompt}"]);
    }

    #[test]
    fn claude_builtin_commands_match_expected_shapes() {
        let claude = CodingAgent::Claude.definition();
        assert_eq!(claude.runner.prompt_input, PromptInput::Argv);
        assert_eq!(
            claude.runner.args,
            vec![
                "--dangerously-skip-permissions",
                "--allow-dangerously-skip-permissions",
                "-p",
                "{prompt}",
            ]
        );
    }

    #[test]
    fn droid_builtin_commands_match_expected_shapes() {
        let droid = CodingAgent::Droid.definition();
        assert_eq!(droid.runner.prompt_input, PromptInput::Argv);
        assert_eq!(
            droid.runner.args,
            vec![
                "exec".to_owned(),
                droid_skip_permissions_flag(),
                "{prompt}".to_owned(),
            ]
        );
    }
}
