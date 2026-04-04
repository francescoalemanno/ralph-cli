use camino::Utf8PathBuf;
use clap::{Args, Parser, Subcommand, ValueEnum};
use ralph_app::RalphApp;
use ralph_core::{ConfigFileScope, ScaffoldId};

const ROOT_ABOUT: &str = "Minimal Ralph loop for repository targets";
const ROOT_LONG_ABOUT: &str = "\
Ralph stores target prompts on disk and runs a bare iteration loop.
\
\n\
\n`ralph` opens the full TUI.
\n`ralph <target>` opens the TUI focused on one target.
\
\n\
\nUse the CLI when you want explicit target management, file inspection, setup tools, or\
\nscriptable configuration management.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum OutputArg {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ScaffoldArg {
    SinglePrompt,
    PlanBuild,
    TaskDriven,
    PlanDriven,
}

impl From<ScaffoldArg> for ScaffoldId {
    fn from(value: ScaffoldArg) -> Self {
        match value {
            ScaffoldArg::SinglePrompt => ScaffoldId::SinglePrompt,
            ScaffoldArg::PlanBuild => ScaffoldId::PlanBuild,
            ScaffoldArg::TaskDriven => ScaffoldId::TaskDriven,
            ScaffoldArg::PlanDriven => ScaffoldId::PlanDriven,
        }
    }
}

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
    #[arg(long, value_name = "ID")]
    pub(crate) agent: Option<String>,
    #[arg(long, value_name = "N")]
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
#[command(name = "ralph", about = ROOT_ABOUT, long_about = ROOT_LONG_ABOUT)]
pub(crate) struct Cli {
    #[arg(long, global = true, value_name = "PATH")]
    pub(crate) project_dir: Option<Utf8PathBuf>,
    #[arg(long, global = true, value_enum, default_value_t = OutputArg::Text)]
    pub(crate) output: OutputArg,
    #[arg(value_name = "TARGET")]
    pub(crate) target: Option<String>,
    #[command(subcommand)]
    pub(crate) command: Option<Commands>,
}

#[derive(Debug, Clone, Subcommand)]
pub(crate) enum Commands {
    #[command(about = "Create a new target")]
    New(NewArgs),
    #[command(about = "Run a target workflow or selected prompt loop")]
    Run(RunArgs),
    #[command(about = "Launch the interactive workflow creator against the user config")]
    WorkflowCreator,
    #[command(about = "List targets", visible_alias = "status")]
    Ls,
    #[command(about = "Show target files")]
    Show(ShowArgs),
    #[command(about = "Edit a target prompt or workflow input")]
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
pub(crate) struct NewArgs {
    #[arg(value_name = "TARGET")]
    pub(crate) target: Option<String>,
    #[arg(long, value_enum, default_value_t = ScaffoldArg::SinglePrompt)]
    pub(crate) scaffold: ScaffoldArg,
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub(crate) edit: bool,
    #[arg(long, value_name = "FILE")]
    pub(crate) prompt: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct RunArgs {
    #[arg(value_name = "TARGET")]
    pub(crate) target: Option<String>,
    #[arg(long, value_name = "FILE")]
    pub(crate) prompt: Option<String>,
    #[arg(long, value_name = "ID")]
    pub(crate) entrypoint: Option<String>,
    #[arg(long, value_name = "ID")]
    pub(crate) action: Option<String>,
    #[command(flatten)]
    pub(crate) runtime: RuntimeArgs,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct ShowArgs {
    #[arg(value_name = "TARGET")]
    pub(crate) target: Option<String>,
    #[arg(long, value_name = "FILE")]
    pub(crate) prompt: Option<String>,
    #[arg(long, value_name = "FILE")]
    pub(crate) file: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct EditArgs {
    #[arg(value_name = "TARGET")]
    pub(crate) target: Option<String>,
    #[arg(long, value_name = "FILE")]
    pub(crate) prompt: Option<String>,
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
