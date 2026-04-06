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
\
\n\
\nUse the CLI when you want workflow execution, workflow inspection, setup tools, or\
\nscriptable configuration management.";
const WORKFLOW_HELP: &str = "Select the workflow";
const REQUEST_FILE_HELP: &str = "Provide the request from a file";
const REQUEST_HELP: &str = "Provide the request from argv text";

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
    #[arg(long, global = true, value_name = "ID")]
    pub(crate) agent: Option<String>,
    #[arg(long, global = true, value_name = "N")]
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
    #[arg(long, global = true, value_name = "PATH")]
    pub(crate) project_dir: Option<Utf8PathBuf>,
    #[command(flatten)]
    pub(crate) runtime: RuntimeArgs,
    #[command(subcommand)]
    pub(crate) command: Commands,
}

#[derive(Debug, Clone, Subcommand)]
pub(crate) enum Commands {
    #[command(about = "Open the workflow runner TUI, or use --cli for CLI mode")]
    Run(RunArgs),
    #[command(about = "Emit an agent event into the current Ralph run WAL")]
    Emit(EmitArgs),
    #[command(about = "List workflows", visible_alias = "workflows")]
    Ls,
    #[command(about = "Show a workflow definition")]
    Show(ShowArgs),
    #[command(about = "Edit a workflow definition")]
    Edit(EditArgs),
    #[command(subcommand, about = "Inspect and manage supported coding agents")]
    Agent(AgentCommands),
    #[command(subcommand, about = "Inspect project and user config")]
    Config(ConfigCommands),
    #[command(about = "Create or overwrite the project config")]
    Init(InitArgs),
    #[command(about = "Validate config and agent detection")]
    Doctor,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct RunArgs {
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub(crate) cli: bool,
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
    #[arg(value_name = "EVENT")]
    pub(crate) event: String,
    #[arg(
        value_name = "BODY",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    pub(crate) body: Vec<String>,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct ShowArgs {
    #[arg(value_name = "WORKFLOW_ID")]
    pub(crate) workflow_id: String,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct EditArgs {
    #[arg(value_name = "WORKFLOW_ID")]
    pub(crate) workflow_id: String,
}

#[derive(Debug, Clone, Subcommand)]
pub(crate) enum AgentCommands {
    #[command(about = "List supported agents and whether they are detected on PATH")]
    List,
    #[command(about = "Show the effective agent")]
    Current,
    #[command(about = "Persist a supported agent into user or project config")]
    Set(AgentSetArgs),
}

#[derive(Debug, Clone, Args)]
pub(crate) struct AgentSetArgs {
    #[arg(value_name = "ID")]
    pub(crate) agent: String,
    #[arg(long, value_enum, default_value_t = WritableConfigScopeArg::Project)]
    pub(crate) scope: WritableConfigScopeArg,
}

#[derive(Debug, Clone, Subcommand)]
pub(crate) enum ConfigCommands {
    #[command(about = "Render user, project, or effective config")]
    Show(ConfigShowArgs),
    #[command(about = "Show config file paths")]
    Path,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct ConfigShowArgs {
    #[arg(long, value_enum, default_value_t = ConfigViewArg::Effective)]
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
    #[arg(long, value_name = "ID")]
    pub(crate) agent: Option<String>,
    #[arg(long, value_name = "CMD")]
    pub(crate) editor: Option<String>,
    #[arg(long, value_name = "N")]
    pub(crate) max_iterations: Option<usize>,
    #[arg(long, action = clap::ArgAction::SetTrue)]
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
        assert_eq!(cli.runtime.agent.as_deref(), Some("claude"));
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
}
