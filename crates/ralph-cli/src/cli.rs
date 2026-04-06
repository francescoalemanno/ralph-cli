use camino::Utf8PathBuf;
use clap::{Args, Parser, Subcommand, ValueEnum};
use ralph_app::RalphApp;
use ralph_core::ConfigFileScope;

const ROOT_ABOUT: &str = "Workflow runner for Ralph";
const ROOT_LONG_ABOUT: &str = "\
Ralph runs request-driven workflows from the workflow registry.
\
\n\
\n`ralph run <workflow-id> <request>` opens the workflow runner TUI.
\nThe TUI requires both a workflow id and a request provided as argv text or `--file`.
\n`ralph run --cli <workflow-id> <request>` runs a workflow in CLI mode.
\nCLI mode also accepts the request from stdin when input is piped.
\
\n\
\nUse the CLI when you want workflow execution, workflow inspection, setup tools, or\
\nscriptable configuration management.";
const PROJECT_DIR_HELP: &str = "Run the command against this project directory";
const WORKFLOW_HELP: &str = "Workflow ID from the registry";
const REQUEST_FILE_HELP: &str = "Read the request from a file";
const REQUEST_HELP: &str = "Provide the request as argv text";
const RUN_CLI_HELP: &str = "Run in CLI mode instead of opening the TUI";
const RUN_AGENT_HELP: &str = "Override the configured coding agent for this run";
const RUN_MAX_ITERATIONS_HELP: &str = "Override the configured workflow iteration limit";
const EMIT_EVENT_HELP: &str = "Event name to append to the current run WAL";
const EMIT_BODY_HELP: &str = "Optional event body text";
const CONFIG_SCOPE_WRITE_HELP: &str = "Config scope to update";
const CONFIG_SCOPE_VIEW_HELP: &str = "Config view to render";
const INIT_AGENT_HELP: &str = "Persist this agent as the project default";
const INIT_EDITOR_HELP: &str = "Persist this editor command as the project default";
const INIT_MAX_ITERATIONS_HELP: &str =
    "Persist this workflow iteration limit as the project default";
const FORCE_HELP: &str = "Overwrite an existing project config file";
const RUN_AFTER_HELP: &str = "\
Examples:
  ralph run task-based \"fix the failing tests\"
  ralph run task-based --file REQ.md
  cat REQ.md | ralph run --cli task-based";
const EMIT_LONG_ABOUT: &str = "\
Emit an agent event into the current Ralph run WAL.

This command only works inside a Ralph agent run.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum WritableConfigScopeArg {
    User,
    Project,
}

impl From<WritableConfigScopeArg> for ConfigFileScope {
    fn from(value: WritableConfigScopeArg) -> Self {
        match value {
            WritableConfigScopeArg::User => ConfigFileScope::User,
            WritableConfigScopeArg::Project => ConfigFileScope::Project,
        }
    }
}

#[derive(Debug, Clone, Args, Default)]
pub(crate) struct RuntimeArgs {
    #[arg(long, value_name = "ID", help = RUN_AGENT_HELP)]
    pub(crate) agent: Option<String>,
    #[arg(long, value_name = "N", help = RUN_MAX_ITERATIONS_HELP)]
    pub(crate) max_iterations: Option<usize>,
}

impl RuntimeArgs {
    pub(crate) fn apply_to<R>(&self, app: &mut RalphApp<R>) -> anyhow::Result<()> {
        if let Some(agent) = &self.agent {
            app.set_agent(agent)?;
        }
        if let Some(max_iterations) = self.max_iterations {
            app.config_mut().max_iterations = max_iterations;
        }
        Ok(())
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "ralph",
    about = ROOT_ABOUT,
    long_about = ROOT_LONG_ABOUT,
    arg_required_else_help = true,
    subcommand_required = true
)]
pub(crate) struct Cli {
    #[arg(long, global = true, value_name = "PATH", help = PROJECT_DIR_HELP)]
    pub(crate) project_dir: Option<Utf8PathBuf>,
    #[command(subcommand)]
    pub(crate) command: Commands,
}

#[derive(Debug, Clone, Subcommand)]
pub(crate) enum Commands {
    #[command(about = "Run a workflow; opens the TUI by default", after_help = RUN_AFTER_HELP)]
    Run(RunArgs),
    #[command(
        about = "Emit an agent event into the current Ralph run WAL",
        long_about = EMIT_LONG_ABOUT
    )]
    Emit(EmitArgs),
    #[command(about = "List available workflows", visible_alias = "workflows")]
    Ls,
    #[command(about = "Print a workflow definition")]
    Show(ShowArgs),
    #[command(about = "Edit a workflow definition in your configured editor")]
    Edit(EditArgs),
    #[command(subcommand, about = "Inspect and manage coding agents")]
    Agent(AgentCommands),
    #[command(subcommand, about = "Inspect project and user config")]
    Config(ConfigCommands),
    #[command(about = "Create or overwrite the project config")]
    Init(InitArgs),
    #[command(about = "Validate config and detected agents")]
    Doctor,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct RunArgs {
    #[arg(long, action = clap::ArgAction::SetTrue, help = RUN_CLI_HELP)]
    pub(crate) cli: bool,
    #[command(flatten)]
    pub(crate) runtime: RuntimeArgs,
    #[arg(value_name = "WORKFLOW_ID", help = WORKFLOW_HELP)]
    pub(crate) workflow: String,
    #[command(flatten)]
    pub(crate) request_args: RequestArgs,
}

#[derive(Debug, Clone, Args, Default)]
pub(crate) struct RequestArgs {
    #[arg(long = "file", value_name = "FILE", help = REQUEST_FILE_HELP)]
    pub(crate) request_file: Option<Utf8PathBuf>,
    #[arg(
        value_name = "REQUEST",
        trailing_var_arg = true,
        allow_hyphen_values = true,
        help = REQUEST_HELP
    )]
    pub(crate) request: Vec<String>,
}

impl RequestArgs {
    pub(crate) fn argv_text(&self) -> Option<String> {
        if self.request.is_empty() {
            None
        } else {
            Some(self.request.join(" "))
        }
    }

    pub(crate) fn provided_count(&self) -> usize {
        usize::from(!self.request.is_empty()) + usize::from(self.request_file.is_some())
    }
}

#[derive(Debug, Clone, Args)]
pub(crate) struct EmitArgs {
    #[arg(value_name = "EVENT", help = EMIT_EVENT_HELP)]
    pub(crate) event: String,
    #[arg(
        value_name = "BODY",
        trailing_var_arg = true,
        allow_hyphen_values = true,
        help = EMIT_BODY_HELP
    )]
    pub(crate) body: Vec<String>,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct ShowArgs {
    #[arg(value_name = "WORKFLOW_ID", help = WORKFLOW_HELP)]
    pub(crate) workflow_id: String,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct EditArgs {
    #[arg(value_name = "WORKFLOW_ID", help = WORKFLOW_HELP)]
    pub(crate) workflow_id: String,
}

#[derive(Debug, Clone, Subcommand)]
pub(crate) enum AgentCommands {
    #[command(about = "List supported agents and whether they are detected on PATH")]
    List,
    #[command(about = "Show the effective coding agent")]
    Current,
    #[command(about = "Persist the default coding agent in config")]
    Set(AgentSetArgs),
}

#[derive(Debug, Clone, Args)]
pub(crate) struct AgentSetArgs {
    #[arg(value_name = "ID", help = "Supported agent ID to persist")]
    pub(crate) agent: String,
    #[arg(
        long,
        value_enum,
        default_value_t = WritableConfigScopeArg::Project,
        help = CONFIG_SCOPE_WRITE_HELP
    )]
    pub(crate) scope: WritableConfigScopeArg,
}

#[derive(Debug, Clone, Subcommand)]
pub(crate) enum ConfigCommands {
    #[command(about = "Print user, project, or effective config")]
    Show(ConfigShowArgs),
    #[command(about = "Print config file paths")]
    Path,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct ConfigShowArgs {
    #[arg(
        long,
        value_enum,
        default_value_t = ConfigViewArg::Effective,
        help = CONFIG_SCOPE_VIEW_HELP
    )]
    pub(crate) scope: ConfigViewArg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ConfigViewArg {
    User,
    Project,
    Effective,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct InitArgs {
    #[arg(long, value_name = "ID", help = INIT_AGENT_HELP)]
    pub(crate) agent: Option<String>,
    #[arg(long, value_name = "CMD", help = INIT_EDITOR_HELP)]
    pub(crate) editor: Option<String>,
    #[arg(long, value_name = "N", help = INIT_MAX_ITERATIONS_HELP)]
    pub(crate) max_iterations: Option<usize>,
    #[arg(long, action = clap::ArgAction::SetTrue, help = FORCE_HELP)]
    pub(crate) force: bool,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, Commands};

    #[test]
    fn root_cli_requires_a_subcommand() {
        assert!(Cli::try_parse_from(["ralph"]).is_err());
    }

    #[test]
    fn run_subcommand_parses_positional_workflow_and_request() {
        let cli = Cli::try_parse_from(["ralph", "run", "task-based", "fix", "tests"]).unwrap();

        let Commands::Run(args) = cli.command else {
            panic!("expected run subcommand");
        };
        assert_eq!(args.workflow, "task-based");
        assert_eq!(
            args.request_args.request,
            vec!["fix".to_owned(), "tests".to_owned()]
        );
    }

    #[test]
    fn run_subcommand_accepts_global_runtime_flags() {
        let cli = Cli::try_parse_from([
            "ralph",
            "run",
            "--agent",
            "claude",
            "task-based",
            "ship",
            "it",
        ])
        .unwrap();

        let Commands::Run(args) = cli.command else {
            panic!("expected run subcommand");
        };
        assert_eq!(args.workflow, "task-based");
        assert_eq!(args.runtime.agent.as_deref(), Some("claude"));
        assert_eq!(
            args.request_args.request,
            vec!["ship".to_owned(), "it".to_owned()]
        );
    }

    #[test]
    fn run_subcommand_parses_request_file() {
        let cli = Cli::try_parse_from(["ralph", "run", "task-based", "--file", "REQ.md"]).unwrap();

        let Commands::Run(args) = cli.command else {
            panic!("expected run subcommand");
        };
        assert_eq!(args.workflow, "task-based");
        assert_eq!(
            args.request_args.request_file,
            Some(camino::Utf8PathBuf::from("REQ.md"))
        );
    }

    #[test]
    fn run_subcommand_parses_cli_flag() {
        let cli =
            Cli::try_parse_from(["ralph", "run", "--cli", "task-based", "fix", "tests"]).unwrap();

        let Commands::Run(args) = cli.command else {
            panic!("expected run subcommand");
        };
        assert!(args.cli);
        assert_eq!(args.workflow, "task-based");
    }

    #[test]
    fn non_run_subcommands_reject_runtime_overrides() {
        assert!(Cli::try_parse_from(["ralph", "ls", "--agent", "claude"]).is_err());
        assert!(
            Cli::try_parse_from(["ralph", "show", "--max-iterations", "3", "task-based"]).is_err()
        );
    }
}
