use std::{collections::BTreeMap, ffi::OsString};

use anyhow::{Context, Result, anyhow};
use camino::Utf8PathBuf;
use clap::{
    Arg, ArgAction, ArgMatches, Command,
    error::{Error, ErrorKind},
};
use ralph_app::RalphApp;
use ralph_core::{ConfigFileScope, list_all_workflows, load_workflow, workflow_option_flag};

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
const RUN_SESSION_TIMEOUT_HELP: &str =
    "Kill the agent after a fixed duration like 30m, 5m, or 45s";
const RUN_IDLE_TIMEOUT_HELP: &str =
    "Kill the agent after this much time with no output like 5m, 30s, or 1h";
const GET_EVENT_HELP: &str = "Event name whose latest payload should be printed";
const GET_CHANNEL_HELP: &str = "Optional channel ID to filter the event lookup";
const CONFIG_SCOPE_WRITE_HELP: &str = "Config scope to update";
const CONFIG_SCOPE_VIEW_HELP: &str = "Config view to render";
const INIT_AGENT_HELP: &str = "Persist this agent as the project default";
const INIT_EDITOR_HELP: &str = "Persist this editor command as the project default";
const INIT_MAX_ITERATIONS_HELP: &str =
    "Persist this workflow iteration limit as the project default";
const FORCE_HELP: &str = "Overwrite an existing project config file";
const RUN_AFTER_HELP: &str = "\
Examples:
  ralph run default \"fix the failing tests\"
  ralph run default --file REQ.md
  cat REQ.md | ralph run --cli bare";
const GET_LONG_ABOUT: &str = "\
Print the most recent payload stored for an event in the current Ralph run WAL.

Without `--channel`, the lookup scans all channels in the current run.

This command only works inside a Ralph agent run.";

const PROJECT_DIR_ARG: &str = "project_dir";
const CLI_ARG: &str = "cli";
const AGENT_ARG: &str = "agent";
const MAX_ITERATIONS_ARG: &str = "max_iterations";
const SESSION_TIMEOUT_ARG: &str = "session_timeout";
const IDLE_TIMEOUT_ARG: &str = "idle_timeout";
const REQUEST_FILE_ARG: &str = "request_file";
const REQUEST_ARG: &str = "request";
const EVENT_ARG: &str = "event";
const CHANNEL_ARG: &str = "channel";
const WORKFLOW_ID_ARG: &str = "workflow_id";
const SCOPE_ARG: &str = "scope";
const EDITOR_ARG: &str = "editor";
const FORCE_ARG: &str = "force";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WritableConfigScopeArg {
    User,
    Project,
}

impl WritableConfigScopeArg {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "user" => Ok(Self::User),
            "project" => Ok(Self::Project),
            _ => Err(anyhow!(
                "invalid config scope '{}'; expected 'user' or 'project'",
                value
            )),
        }
    }
}

impl From<WritableConfigScopeArg> for ConfigFileScope {
    fn from(value: WritableConfigScopeArg) -> Self {
        match value {
            WritableConfigScopeArg::User => ConfigFileScope::User,
            WritableConfigScopeArg::Project => ConfigFileScope::Project,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RuntimeArgs {
    pub(crate) agent: Option<String>,
    pub(crate) max_iterations: Option<usize>,
    pub(crate) session_timeout_secs: Option<u64>,
    pub(crate) idle_timeout_secs: Option<u64>,
}

impl RuntimeArgs {
    pub(crate) fn apply_to<R>(&self, app: &mut RalphApp<R>) -> anyhow::Result<()> {
        if let Some(agent) = &self.agent {
            app.set_agent(agent)?;
        }
        if let Some(max_iterations) = self.max_iterations {
            app.config_mut().max_iterations = max_iterations;
        }
        if self.session_timeout_secs.is_some() || self.idle_timeout_secs.is_some() {
            let agent_id = app.agent_id().to_owned();
            let agent = app
                .config_mut()
                .agents
                .iter_mut()
                .find(|agent| agent.id == agent_id)
                .ok_or_else(|| anyhow!("agent '{}' is not defined", agent_id))?;
            if let Some(session_timeout_secs) = self.session_timeout_secs {
                agent.runner.session_timeout_secs = Some(session_timeout_secs);
            }
            if let Some(idle_timeout_secs) = self.idle_timeout_secs {
                agent.runner.idle_timeout_secs = Some(idle_timeout_secs);
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Cli {
    pub(crate) project_dir: Option<Utf8PathBuf>,
    pub(crate) command: Commands,
}

impl Cli {
    pub(crate) fn parse() -> Self {
        match Self::try_parse_from(std::env::args_os()) {
            Ok(cli) => cli,
            Err(error) => error.exit(),
        }
    }

    pub(crate) fn try_parse_from<I, T>(args: I) -> std::result::Result<Self, Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let mut command = build_cli_command()
            .map_err(|error| Error::raw(ErrorKind::InvalidValue, error.to_string()))?;
        let matches = command.try_get_matches_from_mut(args)?;
        Self::from_matches(&matches)
            .map_err(|error| Error::raw(ErrorKind::InvalidValue, error.to_string()))
    }

    fn from_matches(matches: &ArgMatches) -> Result<Self> {
        let project_dir = matches.get_one::<Utf8PathBuf>(PROJECT_DIR_ARG).cloned();
        let command = match matches.subcommand() {
            Some(("run", submatches)) => Commands::Run(parse_run_args(submatches)?),
            Some(("get", submatches)) => Commands::Get(parse_get_args(submatches)?),
            Some(("ls", _)) => Commands::Ls,
            Some(("show", submatches)) => Commands::Show(parse_show_args(submatches)?),
            Some(("edit", submatches)) => Commands::Edit(parse_edit_args(submatches)?),
            Some(("agent", submatches)) => Commands::Agent(parse_agent_command(submatches)?),
            Some(("config", submatches)) => Commands::Config(parse_config_command(submatches)?),
            Some(("init", submatches)) => Commands::Init(parse_init_args(submatches)),
            Some(("doctor", _)) => Commands::Doctor,
            Some((name, _)) => return Err(anyhow!("unsupported subcommand '{}'", name)),
            None => return Err(anyhow!("a subcommand is required")),
        };

        Ok(Self {
            project_dir,
            command,
        })
    }
}

pub(crate) fn render_run_workflow_help(workflow_id: &str) -> Result<String> {
    let mut command = build_cli_command()?;
    match command.try_get_matches_from_mut([
        OsString::from("ralph"),
        OsString::from("run"),
        OsString::from(workflow_id),
        OsString::from("--help"),
    ]) {
        Err(error) if error.kind() == ErrorKind::DisplayHelp => Ok(error.to_string()),
        Err(error) => Err(anyhow!(
            "failed to render help for workflow '{}': {}",
            workflow_id,
            error
        )),
        Ok(_) => Err(anyhow!(
            "failed to render help for workflow '{}': help exited successfully",
            workflow_id
        )),
    }
}

#[derive(Debug, Clone)]
pub(crate) enum Commands {
    Run(RunArgs),
    Get(GetArgs),
    Ls,
    Show(ShowArgs),
    Edit(EditArgs),
    Agent(AgentCommands),
    Config(ConfigCommands),
    Init(InitArgs),
    Doctor,
}

#[derive(Debug, Clone)]
pub(crate) struct RunArgs {
    pub(crate) cli: bool,
    pub(crate) runtime: RuntimeArgs,
    pub(crate) workflow: String,
    pub(crate) workflow_options: BTreeMap<String, String>,
    pub(crate) request_args: RequestArgs,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RequestArgs {
    pub(crate) request_file: Option<Utf8PathBuf>,
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

#[derive(Debug, Clone)]
pub(crate) struct GetArgs {
    pub(crate) event: String,
    pub(crate) channel: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ShowArgs {
    pub(crate) workflow_id: String,
}

#[derive(Debug, Clone)]
pub(crate) struct EditArgs {
    pub(crate) workflow_id: String,
}

#[derive(Debug, Clone)]
pub(crate) enum AgentCommands {
    List,
    Current,
    Set(AgentSetArgs),
}

#[derive(Debug, Clone)]
pub(crate) struct AgentSetArgs {
    pub(crate) agent: String,
    pub(crate) scope: WritableConfigScopeArg,
}

#[derive(Debug, Clone)]
pub(crate) enum ConfigCommands {
    Show(ConfigShowArgs),
    Path,
}

#[derive(Debug, Clone)]
pub(crate) struct ConfigShowArgs {
    pub(crate) scope: ConfigViewArg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigViewArg {
    User,
    Project,
    Effective,
}

impl ConfigViewArg {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "user" => Ok(Self::User),
            "project" => Ok(Self::Project),
            "effective" => Ok(Self::Effective),
            _ => Err(anyhow!(
                "invalid config view '{}'; expected 'user', 'project', or 'effective'",
                value
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct InitArgs {
    pub(crate) agent: Option<String>,
    pub(crate) editor: Option<String>,
    pub(crate) max_iterations: Option<usize>,
    pub(crate) force: bool,
}

fn build_cli_command() -> Result<Command> {
    Ok(Command::new("ralph")
        .about(ROOT_ABOUT)
        .long_about(ROOT_LONG_ABOUT)
        .arg_required_else_help(true)
        .subcommand_required(true)
        .arg(
            Arg::new(PROJECT_DIR_ARG)
                .long("project-dir")
                .global(true)
                .value_name("PATH")
                .value_parser(clap::value_parser!(Utf8PathBuf))
                .help(PROJECT_DIR_HELP),
        )
        .subcommand(build_run_command()?)
        .subcommand(
            Command::new("get")
                .about("Print the latest payload for an event in the current Ralph run WAL")
                .long_about(GET_LONG_ABOUT)
                .arg_required_else_help(true)
                .arg(
                    Arg::new(CHANNEL_ARG)
                        .long("channel")
                        .value_name("CHANNEL")
                        .help(GET_CHANNEL_HELP),
                )
                .arg(
                    Arg::new(EVENT_ARG)
                        .value_name("EVENT")
                        .required(true)
                        .help(GET_EVENT_HELP),
                ),
        )
        .subcommand(
            Command::new("ls")
                .about("List available workflows")
                .visible_alias("workflows"),
        )
        .subcommand(
            Command::new("show")
                .about("Print a workflow definition")
                .arg_required_else_help(true)
                .arg(
                    Arg::new(WORKFLOW_ID_ARG)
                        .value_name("WORKFLOW_ID")
                        .required(true)
                        .help(WORKFLOW_HELP),
                ),
        )
        .subcommand(
            Command::new("edit")
                .about("Edit a workflow definition in your configured editor")
                .arg_required_else_help(true)
                .arg(
                    Arg::new(WORKFLOW_ID_ARG)
                        .value_name("WORKFLOW_ID")
                        .required(true)
                        .help(WORKFLOW_HELP),
                ),
        )
        .subcommand(
            Command::new("agent")
                .about("Inspect and manage coding agents")
                .subcommand_required(true)
                .arg_required_else_help(true)
                .subcommand(
                    Command::new("list")
                        .about("List supported agents and whether they are detected on PATH"),
                )
                .subcommand(Command::new("current").about("Show the effective coding agent"))
                .subcommand(
                    Command::new("set")
                        .about("Persist the default coding agent in config")
                        .arg_required_else_help(true)
                        .arg(
                            Arg::new(AGENT_ARG)
                                .value_name("ID")
                                .required(true)
                                .help("Supported agent ID to persist"),
                        )
                        .arg(
                            Arg::new(SCOPE_ARG)
                                .long("scope")
                                .value_name("SCOPE")
                                .default_value("project")
                                .value_parser(["user", "project"])
                                .help(CONFIG_SCOPE_WRITE_HELP),
                        ),
                ),
        )
        .subcommand(
            Command::new("config")
                .about("Inspect project and user config")
                .subcommand_required(true)
                .arg_required_else_help(true)
                .subcommand(
                    Command::new("show")
                        .about("Print user, project, or effective config")
                        .arg(
                            Arg::new(SCOPE_ARG)
                                .long("scope")
                                .value_name("SCOPE")
                                .default_value("effective")
                                .value_parser(["user", "project", "effective"])
                                .help(CONFIG_SCOPE_VIEW_HELP),
                        ),
                )
                .subcommand(Command::new("path").about("Print config file paths")),
        )
        .subcommand(
            Command::new("init")
                .about("Create or overwrite the project config")
                .arg(
                    Arg::new(AGENT_ARG)
                        .long("agent")
                        .value_name("ID")
                        .help(INIT_AGENT_HELP),
                )
                .arg(
                    Arg::new(EDITOR_ARG)
                        .long("editor")
                        .value_name("CMD")
                        .help(INIT_EDITOR_HELP),
                )
                .arg(
                    Arg::new(MAX_ITERATIONS_ARG)
                        .long("max-iterations")
                        .value_name("N")
                        .value_parser(clap::value_parser!(usize))
                        .help(INIT_MAX_ITERATIONS_HELP),
                )
                .arg(
                    Arg::new(FORCE_ARG)
                        .long("force")
                        .action(ArgAction::SetTrue)
                        .help(FORCE_HELP),
                ),
        )
        .subcommand(Command::new("doctor").about("Validate config and detected agents")))
}

fn build_run_command() -> Result<Command> {
    let mut command = Command::new("run")
        .about("Run a workflow; opens the TUI by default")
        .after_help(RUN_AFTER_HELP)
        .arg_required_else_help(true)
        .subcommand_required(true)
        .subcommand_help_heading("Workflows")
        .arg(
            Arg::new(CLI_ARG)
                .long("cli")
                .global(true)
                .action(ArgAction::SetTrue)
                .help(RUN_CLI_HELP),
        )
        .arg(
            Arg::new(AGENT_ARG)
                .long("agent")
                .global(true)
                .value_name("ID")
                .help(RUN_AGENT_HELP),
        )
        .arg(
            Arg::new(MAX_ITERATIONS_ARG)
                .long("max-iterations")
                .global(true)
                .value_name("N")
                .value_parser(clap::value_parser!(usize))
                .help(RUN_MAX_ITERATIONS_HELP),
        )
        .arg(
            Arg::new(SESSION_TIMEOUT_ARG)
                .long("session-timeout")
                .global(true)
                .value_name("DURATION")
                .default_value("1h")
                .value_parser(clap::builder::ValueParser::new(parse_timeout_duration))
                .help(RUN_SESSION_TIMEOUT_HELP),
        )
        .arg(
            Arg::new(IDLE_TIMEOUT_ARG)
                .long("idle-timeout")
                .global(true)
                .value_name("DURATION")
                .default_value("10m")
                .value_parser(clap::builder::ValueParser::new(parse_timeout_duration))
                .help(RUN_IDLE_TIMEOUT_HELP),
        );

    for workflow in list_all_workflows()? {
        let definition = load_workflow(&workflow.workflow_id)
            .with_context(|| format!("failed to load workflow '{}'", workflow.workflow_id))?;
        command = command.subcommand(build_workflow_run_command(&definition)?);
    }

    Ok(command)
}

fn build_workflow_run_command(workflow: &ralph_core::WorkflowDefinition) -> Result<Command> {
    let about = if workflow.description.trim().is_empty() {
        workflow.title.clone()
    } else {
        workflow.description.clone()
    };

    let mut command = Command::new(leak(workflow.workflow_id.clone()))
        .about(leak(about))
        .hide(workflow.hidden);

    for option_id in workflow.option_ids() {
        let definition = workflow
            .option(option_id)
            .expect("option ids are sourced from the workflow");
        let flag = workflow_option_flag(option_id)?;
        let mut arg = Arg::new(leak(option_id.to_owned()))
            .long(leak(flag))
            .action(ArgAction::Set)
            .value_name(leak(
                definition
                    .value_name
                    .clone()
                    .unwrap_or_else(|| "VALUE".to_owned()),
            ));
        if !definition.help.trim().is_empty() {
            arg = arg.help(leak(definition.help.clone()));
        }
        if let Some(default) = &definition.default {
            arg = arg.default_value(leak(default.clone()));
        } else {
            arg = arg.required(true);
        }
        command = command.arg(arg);
    }

    Ok(command
        .arg(
            Arg::new(REQUEST_FILE_ARG)
                .long("file")
                .value_name("FILE")
                .value_parser(clap::value_parser!(Utf8PathBuf))
                .help(REQUEST_FILE_HELP),
        )
        .arg(
            Arg::new(REQUEST_ARG)
                .value_name("REQUEST")
                .trailing_var_arg(true)
                .allow_hyphen_values(true)
                .num_args(1..)
                .help(REQUEST_HELP),
        ))
}

fn parse_run_args(matches: &ArgMatches) -> Result<RunArgs> {
    let (workflow, workflow_matches) = matches
        .subcommand()
        .ok_or_else(|| anyhow!("a workflow id is required"))?;
    let definition = load_workflow(workflow)
        .with_context(|| format!("failed to load workflow '{}'", workflow))?;

    let workflow_options = definition
        .option_ids()
        .into_iter()
        .filter_map(|option_id| {
            workflow_matches
                .get_one::<String>(option_id)
                .map(|value| (option_id.to_owned(), value.to_owned()))
        })
        .collect::<BTreeMap<_, _>>();

    Ok(RunArgs {
        cli: workflow_matches.get_flag(CLI_ARG),
        runtime: RuntimeArgs {
            agent: workflow_matches.get_one::<String>(AGENT_ARG).cloned(),
            max_iterations: workflow_matches
                .get_one::<usize>(MAX_ITERATIONS_ARG)
                .copied(),
            session_timeout_secs: workflow_matches
                .get_one::<u64>(SESSION_TIMEOUT_ARG)
                .copied(),
            idle_timeout_secs: workflow_matches.get_one::<u64>(IDLE_TIMEOUT_ARG).copied(),
        },
        workflow: workflow.to_owned(),
        workflow_options,
        request_args: RequestArgs {
            request_file: workflow_matches
                .get_one::<Utf8PathBuf>(REQUEST_FILE_ARG)
                .cloned(),
            request: workflow_matches
                .get_many::<String>(REQUEST_ARG)
                .map(|values| values.cloned().collect())
                .unwrap_or_default(),
        },
    })
}

fn parse_get_args(matches: &ArgMatches) -> Result<GetArgs> {
    Ok(GetArgs {
        event: required_string_result(matches, EVENT_ARG)?,
        channel: matches.get_one::<String>(CHANNEL_ARG).cloned(),
    })
}

fn parse_show_args(matches: &ArgMatches) -> Result<ShowArgs> {
    Ok(ShowArgs {
        workflow_id: required_string_result(matches, WORKFLOW_ID_ARG)?,
    })
}

fn parse_edit_args(matches: &ArgMatches) -> Result<EditArgs> {
    Ok(EditArgs {
        workflow_id: required_string_result(matches, WORKFLOW_ID_ARG)?,
    })
}

fn parse_agent_command(matches: &ArgMatches) -> Result<AgentCommands> {
    match matches.subcommand() {
        Some(("list", _)) => Ok(AgentCommands::List),
        Some(("current", _)) => Ok(AgentCommands::Current),
        Some(("set", submatches)) => Ok(AgentCommands::Set(AgentSetArgs {
            agent: required_string_result(submatches, AGENT_ARG)?,
            scope: WritableConfigScopeArg::parse(
                submatches
                    .get_one::<String>(SCOPE_ARG)
                    .map(String::as_str)
                    .unwrap_or("project"),
            )?,
        })),
        Some((name, _)) => Err(anyhow!("unsupported agent subcommand '{}'", name)),
        None => Err(anyhow!("an agent subcommand is required")),
    }
}

fn parse_config_command(matches: &ArgMatches) -> Result<ConfigCommands> {
    match matches.subcommand() {
        Some(("show", submatches)) => Ok(ConfigCommands::Show(ConfigShowArgs {
            scope: ConfigViewArg::parse(
                submatches
                    .get_one::<String>(SCOPE_ARG)
                    .map(String::as_str)
                    .unwrap_or("effective"),
            )?,
        })),
        Some(("path", _)) => Ok(ConfigCommands::Path),
        Some((name, _)) => Err(anyhow!("unsupported config subcommand '{}'", name)),
        None => Err(anyhow!("a config subcommand is required")),
    }
}

fn parse_init_args(matches: &ArgMatches) -> InitArgs {
    InitArgs {
        agent: matches.get_one::<String>(AGENT_ARG).cloned(),
        editor: matches.get_one::<String>(EDITOR_ARG).cloned(),
        max_iterations: matches.get_one::<usize>(MAX_ITERATIONS_ARG).copied(),
        force: matches.get_flag(FORCE_ARG),
    }
}

fn required_string_result(matches: &ArgMatches, id: &str) -> Result<String> {
    matches
        .get_one::<String>(id)
        .cloned()
        .ok_or_else(|| anyhow!("missing required argument '{}'", id))
}

fn parse_timeout_duration(value: &str) -> Result<u64> {
    if value.len() < 2 {
        return Err(anyhow!(
            "invalid duration '{}'; expected [integer][h|m|s]",
            value
        ));
    }
    let (number, unit) = value.split_at(value.len() - 1);
    let amount = number.parse::<u64>().map_err(|_| {
        anyhow!(
            "invalid duration '{}'; expected [integer][h|m|s]",
            value
        )
    })?;
    if amount == 0 {
        return Err(anyhow!("invalid duration '{}'; value must be > 0", value));
    }
    match unit {
        "h" => Ok(amount.saturating_mul(60 * 60)),
        "m" => Ok(amount.saturating_mul(60)),
        "s" => Ok(amount),
        _ => Err(anyhow!(
            "invalid duration '{}'; expected [integer][h|m|s]",
            value
        )),
    }
}

fn leak(value: String) -> &'static str {
    Box::leak(value.into_boxed_str())
}

#[cfg(test)]
mod tests {
    use clap::error::ErrorKind;

    use super::{Cli, Commands, build_cli_command, parse_timeout_duration};
    use crate::test_support::with_test_workflow_home;

    #[test]
    fn root_cli_requires_a_subcommand() {
        with_test_workflow_home(|| {
            let error = Cli::try_parse_from(["ralph"]).unwrap_err();
            assert_eq!(
                error.kind(),
                ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
            );
        });
    }

    #[test]
    fn run_subcommand_parses_positional_workflow_and_request() {
        with_test_workflow_home(|| {
            let cli =
                Cli::try_parse_from(["ralph", "run", "fixture-flow", "fix", "tests"]).unwrap();

            let Commands::Run(args) = cli.command else {
                panic!("expected run subcommand");
            };
            assert_eq!(args.workflow, "fixture-flow");
            assert_eq!(args.runtime.session_timeout_secs, Some(60 * 60));
            assert_eq!(args.runtime.idle_timeout_secs, Some(10 * 60));
            assert_eq!(
                args.request_args.request,
                vec!["fix".to_owned(), "tests".to_owned()]
            );
        });
    }

    #[test]
    fn run_subcommand_accepts_global_runtime_flags() {
        with_test_workflow_home(|| {
            let cli = Cli::try_parse_from([
                "ralph",
                "run",
                "--agent",
                "claude",
                "--session-timeout",
                "30m",
                "--idle-timeout",
                "5m",
                "fixture-flow",
                "ship",
                "it",
            ])
            .unwrap();

            let Commands::Run(args) = cli.command else {
                panic!("expected run subcommand");
            };
            assert_eq!(args.workflow, "fixture-flow");
            assert_eq!(args.runtime.agent.as_deref(), Some("claude"));
            assert_eq!(args.runtime.session_timeout_secs, Some(30 * 60));
            assert_eq!(args.runtime.idle_timeout_secs, Some(5 * 60));
            assert_eq!(
                args.request_args.request,
                vec!["ship".to_owned(), "it".to_owned()]
            );
        });
    }

    #[test]
    fn run_subcommand_parses_request_file() {
        with_test_workflow_home(|| {
            let cli =
                Cli::try_parse_from(["ralph", "run", "fixture-flow", "--file", "REQ.md"]).unwrap();

            let Commands::Run(args) = cli.command else {
                panic!("expected run subcommand");
            };
            assert_eq!(args.workflow, "fixture-flow");
            assert_eq!(
                args.request_args.request_file,
                Some(camino::Utf8PathBuf::from("REQ.md"))
            );
        });
    }

    #[test]
    fn run_subcommand_parses_cli_flag() {
        with_test_workflow_home(|| {
            let cli =
                Cli::try_parse_from(["ralph", "run", "--cli", "fixture-flow", "fix", "tests"])
                    .unwrap();

            let Commands::Run(args) = cli.command else {
                panic!("expected run subcommand");
            };
            assert!(args.cli);
            assert_eq!(args.workflow, "fixture-flow");
            assert_eq!(args.runtime.session_timeout_secs, Some(60 * 60));
            assert_eq!(args.runtime.idle_timeout_secs, Some(10 * 60));
        });
    }

    #[test]
    fn run_subcommand_parses_workflow_specific_options() {
        with_test_workflow_home(|| {
            let cli = Cli::try_parse_from([
                "ralph",
                "run",
                "fixture-flow",
                "--statefile",
                "snapshot.md",
                "fix",
                "tests",
            ])
            .unwrap();

            let Commands::Run(args) = cli.command else {
                panic!("expected run subcommand");
            };
            assert_eq!(
                args.workflow_options.get("state-file").map(String::as_str),
                Some("snapshot.md")
            );
        });
    }

    #[test]
    fn workflow_help_includes_declared_options() {
        with_test_workflow_home(|| {
            let error =
                Cli::try_parse_from(["ralph", "run", "fixture-flow", "--help"]).unwrap_err();
            let rendered = error.to_string();

            assert_eq!(error.kind(), ErrorKind::DisplayHelp);
            assert!(rendered.contains("--statefile"));
            assert!(rendered.contains("state.txt"));
            assert!(rendered.contains("--session-timeout <DURATION>"));
            assert!(rendered.contains("[default: 1h]"));
            assert!(rendered.contains("--idle-timeout <DURATION>"));
            assert!(rendered.contains("[default: 10m]"));
        });
    }

    #[test]
    fn hidden_workflows_stay_out_of_help_but_remain_invocable_by_id() {
        with_test_workflow_home(|| {
            let mut command = build_cli_command().unwrap();
            let error = command
                .try_get_matches_from_mut(["ralph", "run", "--help"])
                .unwrap_err();
            let rendered = error.to_string();

            assert_eq!(error.kind(), ErrorKind::DisplayHelp);
            assert!(rendered.contains("plan"));
            assert!(rendered.contains("ipr"));
            assert!(!rendered.contains("test-workflow"));

            let cli = Cli::try_parse_from(["ralph", "run", "test-workflow"]).unwrap();
            let Commands::Run(args) = cli.command else {
                panic!("expected run subcommand");
            };
            assert_eq!(args.workflow, "test-workflow");
            assert!(args.request_args.request.is_empty());
        });
    }

    #[test]
    fn non_run_subcommands_reject_runtime_overrides() {
        with_test_workflow_home(|| {
            assert!(Cli::try_parse_from(["ralph", "ls", "--agent", "claude"]).is_err());
            assert!(
                Cli::try_parse_from(["ralph", "show", "--max-iterations", "3", "fixture-flow"])
                    .is_err()
            );
            assert!(
                Cli::try_parse_from(["ralph", "show", "--session-timeout", "3m", "fixture-flow"])
                    .is_err()
            );
        });
    }

    #[test]
    fn parses_timeout_durations_in_seconds_minutes_and_hours() {
        assert_eq!(parse_timeout_duration("45s").unwrap(), 45);
        assert_eq!(parse_timeout_duration("5m").unwrap(), 300);
        assert_eq!(parse_timeout_duration("2h").unwrap(), 7200);
    }

    #[test]
    fn rejects_invalid_timeout_durations() {
        for value in ["0s", "30", "1d", "ms", "1h30m"] {
            assert!(parse_timeout_duration(value).is_err(), "{value} should fail");
        }
    }

    #[test]
    fn get_subcommand_parses_event_name() {
        with_test_workflow_home(|| {
            let cli = Cli::try_parse_from(["ralph", "get", "handoff"]).unwrap();

            let Commands::Get(args) = cli.command else {
                panic!("expected get subcommand");
            };
            assert_eq!(args.event, "handoff");
            assert_eq!(args.channel, None);
        });
    }

    #[test]
    fn get_subcommand_parses_optional_channel_filter() {
        with_test_workflow_home(|| {
            let cli = Cli::try_parse_from(["ralph", "get", "--channel", "QT", "handoff"]).unwrap();

            let Commands::Get(args) = cli.command else {
                panic!("expected get subcommand");
            };
            assert_eq!(args.event, "handoff");
            assert_eq!(args.channel.as_deref(), Some("QT"));
        });
    }

    #[test]
    fn get_without_event_surfaces_help_instead_of_panicking() {
        with_test_workflow_home(|| {
            let error = Cli::try_parse_from(["ralph", "get"]).unwrap_err();
            let rendered = error.to_string();

            assert_eq!(
                error.kind(),
                ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
            );
            assert!(
                rendered
                    .contains("Print the latest payload for an event in the current Ralph run WAL")
            );
            assert!(rendered.contains("Usage:"));
            assert!(rendered.contains("ralph get"));
            assert!(rendered.contains("<EVENT>"));
        });
    }
}
