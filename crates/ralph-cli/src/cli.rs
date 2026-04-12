use std::{collections::BTreeMap, ffi::OsString};

use anyhow::{Context, Result, anyhow};
use camino::Utf8PathBuf;
use clap::{
    Arg, ArgAction, ArgMatches, Command,
    error::{Error, ErrorKind},
    parser::ValueSource,
};
use ralph_app::RalphApp;
use ralph_core::{list_all_workflows, load_workflow, workflow_option_flag};

const ROOT_ABOUT: &str = "Guided planner and workflow runner for Ralph";
const ROOT_LONG_ABOUT: &str = "\
Ralph starts in guided mode by default.
\
\n\
\n`ralph` asks for a plan description, runs the `plan` workflow interactively,\
\nthen can continue into `task` and `review`.
\n`ralph --plan[=DESCRIPTION]` creates a plan interactively and stops after the plan file is written.
\n`ralph -t <PLAN_FILE>` runs only the `task` workflow.
\n`ralph -r [PLAN_FILE]` runs only the `review` workflow.
\n`ralph -f [PLAN_FILE]` runs only the `finalize` workflow.
\
\n\
\nUse `ralph w <workflow-id> ...` when you want the lower-level workflow runner,\
\nworkflow inspection, or scriptable configuration changes.";
const PROJECT_DIR_HELP: &str = "Run the command against this project directory";
const REQUEST_FILE_HELP: &str = "Read the request from a file";
const REQUEST_HELP: &str = "Provide the request as argv text";
const RUNTIME_AGENT_HELP: &str = "Override the configured coding agent for this invocation";
const RUNTIME_MAX_ITERATIONS_HELP: &str = "Override the configured workflow iteration limit";
const RUNTIME_SESSION_TIMEOUT_HELP: &str =
    "Kill the agent after a fixed duration like 30m, 5m, or 45s";
const RUNTIME_IDLE_TIMEOUT_HELP: &str =
    "Kill the agent after this much time with no output like 5m, 30s, or 1h";
const GUIDED_PLAN_HELP: &str =
    "Create a plan interactively and stop after the plan file is written.";
const GUIDED_TASKS_ONLY_HELP: &str =
    "Execute the plan tasks only, then stop without running review or finalize.";
const GUIDED_REVIEW_HELP: &str = "Run the full review pipeline only, skipping task execution.";
const GUIDED_FINALIZE_HELP: &str =
    "Run the finalize workflow only, skipping task execution and review.";
const WORKFLOWS_HELP: &str = "List available workflows";
const SHOW_WORKFLOW_HELP: &str = "Print a workflow definition";
const EDIT_WORKFLOW_HELP: &str = "Edit a workflow definition in your configured editor";
const SHOW_CONFIG_HELP: &str = "Print user, project, or effective config";
const SET_PROJECT_AGENT_HELP: &str = "Persist the default agent for this project";
const SET_USER_AGENT_HELP: &str = "Persist the default agent for the user config";
const RUN_AFTER_HELP: &str = "\
Examples:
  ralph --agent claude w default \"fix the failing tests\"
  ralph --file REQ.md w default
  cat REQ.md | ralph w bare";
const GET_LONG_ABOUT: &str = "\
Print the most recent payload stored for an event in the current Ralph run WAL.

Without `--channel`, the lookup scans all channels in the current run.

This command only works inside a Ralph agent run.";
const SIGNAL_LONG_ABOUT: &str = "\
Append a signal event with no body into the current Ralph run WAL.

The event is written to the current Ralph channel from `RALPH_CHANNEL_ID`.

This command only works inside a Ralph agent run.";
const PAYLOAD_LONG_ABOUT: &str = "\
Append a payload event with a body into the current Ralph run WAL.

The event is written to the current Ralph channel from `RALPH_CHANNEL_ID`.

This command only works inside a Ralph agent run.";
const SIGNAL_EVENT_HELP: &str = "Event name to append to the current Ralph run WAL";
const PAYLOAD_EVENT_HELP: &str =
    "Event name whose payload should be appended to the current Ralph run WAL";
const PAYLOAD_BODY_HELP: &str = "Payload body to append to the current Ralph run WAL";
const GET_EVENT_HELP: &str = "Event name whose latest payload should be printed";
const GET_CHANNEL_HELP: &str = "Optional channel ID to filter the event lookup";

const PROJECT_DIR_ARG: &str = "project_dir";
const GUIDED_REQUEST_ARG: &str = "guided_request";
const AGENT_ARG: &str = "agent";
const MAX_ITERATIONS_ARG: &str = "max_iterations";
const SESSION_TIMEOUT_ARG: &str = "session_timeout";
const IDLE_TIMEOUT_ARG: &str = "idle_timeout";
const REQUEST_FILE_ARG: &str = "request_file";
const REQUEST_ARG: &str = "request";
const EVENT_ARG: &str = "event";
const BODY_ARG: &str = "body";
const CHANNEL_ARG: &str = "channel";
const PLAN_ARG: &str = "plan";
const TASKS_ONLY_ARG: &str = "tasks_only";
const REVIEW_ARG: &str = "review";
const FINALIZE_ARG: &str = "finalize";
const WORKFLOWS_ARG: &str = "workflows";
const SHOW_WORKFLOW_ARG: &str = "show_workflow";
const EDIT_WORKFLOW_ARG: &str = "edit_workflow";
const SHOW_CONFIG_ARG: &str = "show_config";
const SET_PROJECT_AGENT_ARG: &str = "set_project_agent";
const SET_USER_AGENT_ARG: &str = "set_user_agent";
const INTERNAL_EVENT_COMMAND_NAMES: &[&str] = &["get", "payload", "signal"];

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

#[derive(Debug, Clone, Default)]
pub(crate) struct ConfigMutationArgs {
    pub(crate) set_project_agent: Option<String>,
    pub(crate) set_user_agent: Option<String>,
}

impl ConfigMutationArgs {
    pub(crate) fn is_empty(&self) -> bool {
        self.set_project_agent.is_none() && self.set_user_agent.is_none()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Cli {
    pub(crate) project_dir: Option<Utf8PathBuf>,
    pub(crate) config_mutations: ConfigMutationArgs,
    pub(crate) command: Option<Commands>,
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
        Self::try_parse_from_with_internal_commands(args, internal_event_commands_enabled())
    }

    fn try_parse_from_with_internal_commands<I, T>(
        args: I,
        internal_event_commands_enabled: bool,
    ) -> std::result::Result<Self, Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let mut command = build_cli_command_with_internal_commands(internal_event_commands_enabled)
            .map_err(|error| Error::raw(ErrorKind::InvalidValue, error.to_string()))?;
        let matches = command.try_get_matches_from_mut(args)?;
        Self::from_matches(&matches, internal_event_commands_enabled)
            .map_err(|error| Error::raw(ErrorKind::InvalidValue, error.to_string()))
    }

    fn from_matches(matches: &ArgMatches, internal_event_commands_enabled: bool) -> Result<Self> {
        let project_dir = matches.get_one::<Utf8PathBuf>(PROJECT_DIR_ARG).cloned();
        let runtime = parse_runtime_args(matches);
        let runtime_flags_present = arg_present(matches, AGENT_ARG)
            || arg_present(matches, MAX_ITERATIONS_ARG)
            || arg_present(matches, SESSION_TIMEOUT_ARG)
            || arg_present(matches, IDLE_TIMEOUT_ARG);
        let config_mutations = ConfigMutationArgs {
            set_project_agent: matches.get_one::<String>(SET_PROJECT_AGENT_ARG).cloned(),
            set_user_agent: matches.get_one::<String>(SET_USER_AGENT_ARG).cloned(),
        };

        let has_plan = arg_present(matches, PLAN_ARG);
        let has_tasks_only = arg_present(matches, TASKS_ONLY_ARG);
        let has_review = arg_present(matches, REVIEW_ARG);
        let has_finalize = arg_present(matches, FINALIZE_ARG);
        let has_workflows = arg_present(matches, WORKFLOWS_ARG);
        let has_show_workflow = arg_present(matches, SHOW_WORKFLOW_ARG);
        let has_edit_workflow = arg_present(matches, EDIT_WORKFLOW_ARG);
        let has_show_config = arg_present(matches, SHOW_CONFIG_ARG);

        let mut primary_actions = Vec::new();
        if has_plan {
            primary_actions.push("--plan");
        }
        if has_tasks_only {
            primary_actions.push("--tasks-only");
        }
        if has_review {
            primary_actions.push("--review");
        }
        if has_finalize {
            primary_actions.push("--finalize");
        }
        if has_workflows {
            primary_actions.push("--workflows");
        }
        if has_show_workflow {
            primary_actions.push("--show-workflow");
        }
        if has_edit_workflow {
            primary_actions.push("--edit-workflow");
        }
        if has_show_config {
            primary_actions.push("--show-config");
        }
        if let Some((name, _)) = matches.subcommand() {
            primary_actions.push(match name {
                "w" => "w",
                "get" => "get",
                "signal" => "signal",
                "payload" => "payload",
                _ => name,
            });
        }

        if primary_actions.len() > 1 {
            return Err(anyhow!(
                "multiple primary actions cannot be combined: {}",
                primary_actions.join(", ")
            ));
        }

        let command = match matches.subcommand() {
            Some(("w", submatches)) => Some(Commands::Workflow(parse_workflow_args(
                matches, submatches,
            )?)),
            Some(("signal", submatches)) => {
                ensure_runtime_flags_absent(runtime_flags_present, "signal")?;
                ensure_request_file_absent(matches, "signal")?;
                ensure_guided_request_absent(matches, "signal")?;
                ensure_config_mutations_absent(&config_mutations, "signal")?;
                Some(Commands::Signal(parse_signal_args(submatches)?))
            }
            Some(("payload", submatches)) => {
                ensure_runtime_flags_absent(runtime_flags_present, "payload")?;
                ensure_request_file_absent(matches, "payload")?;
                ensure_guided_request_absent(matches, "payload")?;
                ensure_config_mutations_absent(&config_mutations, "payload")?;
                Some(Commands::Payload(parse_payload_args(submatches)?))
            }
            Some(("get", submatches)) => {
                ensure_runtime_flags_absent(runtime_flags_present, "get")?;
                ensure_request_file_absent(matches, "get")?;
                ensure_guided_request_absent(matches, "get")?;
                ensure_config_mutations_absent(&config_mutations, "get")?;
                Some(Commands::Get(parse_get_args(submatches)?))
            }
            Some((name, _)) => return Err(anyhow!("unsupported subcommand '{}'", name)),
            None if has_plan => Some(Commands::Guided(GuidedArgs {
                runtime,
                request_args: parse_guided_request_args(
                    matches,
                    true,
                    internal_event_commands_enabled,
                )?,
                build_after_plan: false,
            })),
            None if has_tasks_only => {
                ensure_request_file_absent(matches, "--tasks-only")?;
                ensure_guided_request_absent(matches, "--tasks-only")?;
                Some(Commands::TasksOnly(PlanShortcutArgs {
                    runtime,
                    plan_file: required_string_result(matches, TASKS_ONLY_ARG)?,
                }))
            }
            None if has_review => {
                ensure_request_file_absent(matches, "--review")?;
                ensure_guided_request_absent(matches, "--review")?;
                Some(Commands::ReviewOnly(OptionalPlanShortcutArgs {
                    runtime,
                    plan_file: normalize_optional_value(matches.get_one::<String>(REVIEW_ARG)),
                }))
            }
            None if has_finalize => {
                ensure_request_file_absent(matches, "--finalize")?;
                ensure_guided_request_absent(matches, "--finalize")?;
                Some(Commands::FinalizeOnly(OptionalPlanShortcutArgs {
                    runtime,
                    plan_file: normalize_optional_value(matches.get_one::<String>(FINALIZE_ARG)),
                }))
            }
            None if has_workflows => {
                ensure_runtime_flags_absent(runtime_flags_present, "--workflows")?;
                ensure_request_file_absent(matches, "--workflows")?;
                ensure_guided_request_absent(matches, "--workflows")?;
                Some(Commands::Workflows)
            }
            None if has_show_workflow => {
                ensure_runtime_flags_absent(runtime_flags_present, "--show-workflow")?;
                ensure_request_file_absent(matches, "--show-workflow")?;
                ensure_guided_request_absent(matches, "--show-workflow")?;
                Some(Commands::ShowWorkflow(parse_show_workflow_args(matches)?))
            }
            None if has_edit_workflow => {
                ensure_runtime_flags_absent(runtime_flags_present, "--edit-workflow")?;
                ensure_request_file_absent(matches, "--edit-workflow")?;
                ensure_guided_request_absent(matches, "--edit-workflow")?;
                Some(Commands::EditWorkflow(parse_edit_workflow_args(matches)?))
            }
            None if has_show_config => {
                ensure_runtime_flags_absent(runtime_flags_present, "--show-config")?;
                ensure_request_file_absent(matches, "--show-config")?;
                ensure_guided_request_absent(matches, "--show-config")?;
                Some(Commands::ShowConfig(parse_show_config_args(matches)?))
            }
            None if config_mutations.is_empty() => Some(Commands::Guided(GuidedArgs {
                runtime,
                request_args: parse_guided_request_args(
                    matches,
                    false,
                    internal_event_commands_enabled,
                )?,
                build_after_plan: true,
            })),
            None => {
                ensure_runtime_flags_absent(runtime_flags_present, "config mutation flags")?;
                ensure_request_file_absent(matches, "config mutation flags")?;
                ensure_guided_request_absent(matches, "config mutation flags")?;
                None
            }
        };

        Ok(Self {
            project_dir,
            config_mutations,
            command,
        })
    }
}

pub(crate) fn render_workflow_help(workflow_id: &str) -> Result<String> {
    let mut command = build_cli_command()?;
    match command.try_get_matches_from_mut([
        OsString::from("ralph"),
        OsString::from("w"),
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
    Guided(GuidedArgs),
    TasksOnly(PlanShortcutArgs),
    ReviewOnly(OptionalPlanShortcutArgs),
    FinalizeOnly(OptionalPlanShortcutArgs),
    Workflow(RunArgs),
    Workflows,
    ShowWorkflow(ShowArgs),
    EditWorkflow(EditArgs),
    ShowConfig(ConfigShowArgs),
    Signal(SignalArgs),
    Payload(PayloadArgs),
    Get(GetArgs),
}

#[derive(Debug, Clone)]
pub(crate) struct GuidedArgs {
    pub(crate) runtime: RuntimeArgs,
    pub(crate) request_args: RequestArgs,
    pub(crate) build_after_plan: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct PlanShortcutArgs {
    pub(crate) runtime: RuntimeArgs,
    pub(crate) plan_file: String,
}

#[derive(Debug, Clone)]
pub(crate) struct OptionalPlanShortcutArgs {
    pub(crate) runtime: RuntimeArgs,
    pub(crate) plan_file: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct RunArgs {
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
}

#[derive(Debug, Clone)]
pub(crate) struct GetArgs {
    pub(crate) event: String,
    pub(crate) channel: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct SignalArgs {
    pub(crate) event: String,
}

#[derive(Debug, Clone)]
pub(crate) struct PayloadArgs {
    pub(crate) event: String,
    pub(crate) body: String,
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

fn build_cli_command() -> Result<Command> {
    build_cli_command_with_internal_commands(internal_event_commands_enabled())
}

fn build_cli_command_with_internal_commands(
    enable_internal_event_commands: bool,
) -> Result<Command> {
    let mut command = Command::new("ralph")
        .about(ROOT_ABOUT)
        .long_about(ROOT_LONG_ABOUT)
        .arg(
            Arg::new(PROJECT_DIR_ARG)
                .long("project-dir")
                .value_name("PATH")
                .value_parser(clap::value_parser!(Utf8PathBuf))
                .help(PROJECT_DIR_HELP),
        )
        .arg(
            Arg::new(AGENT_ARG)
                .long("agent")
                .value_name("ID")
                .help(RUNTIME_AGENT_HELP),
        )
        .arg(
            Arg::new(MAX_ITERATIONS_ARG)
                .long("max-iterations")
                .value_name("N")
                .value_parser(clap::value_parser!(usize))
                .help(RUNTIME_MAX_ITERATIONS_HELP),
        )
        .arg(
            Arg::new(SESSION_TIMEOUT_ARG)
                .long("session-timeout")
                .value_name("DURATION")
                .default_value("1h")
                .value_parser(clap::builder::ValueParser::new(parse_timeout_duration))
                .help(RUNTIME_SESSION_TIMEOUT_HELP),
        )
        .arg(
            Arg::new(IDLE_TIMEOUT_ARG)
                .long("idle-timeout")
                .value_name("DURATION")
                .default_value("10m")
                .value_parser(clap::builder::ValueParser::new(parse_timeout_duration))
                .help(RUNTIME_IDLE_TIMEOUT_HELP),
        )
        .arg(
            Arg::new(REQUEST_FILE_ARG)
                .long("file")
                .value_name("FILE")
                .value_parser(clap::value_parser!(Utf8PathBuf))
                .help(REQUEST_FILE_HELP),
        )
        .arg(
            Arg::new(GUIDED_REQUEST_ARG)
                .value_name("REQUEST")
                .allow_hyphen_values(true)
                .num_args(1..)
                .help(REQUEST_HELP),
        )
        .arg(
            Arg::new(PLAN_ARG)
                .long("plan")
                .value_name("DESCRIPTION")
                .num_args(0..=1)
                .require_equals(true)
                .default_missing_value("")
                .conflicts_with_all([TASKS_ONLY_ARG, REVIEW_ARG, FINALIZE_ARG])
                .help(GUIDED_PLAN_HELP),
        )
        .arg(
            Arg::new(TASKS_ONLY_ARG)
                .short('t')
                .long("tasks-only")
                .value_name("PLAN_FILE")
                .action(ArgAction::Set)
                .conflicts_with_all([PLAN_ARG, REVIEW_ARG, FINALIZE_ARG])
                .help(GUIDED_TASKS_ONLY_HELP),
        )
        .arg(
            Arg::new(REVIEW_ARG)
                .short('r')
                .long("review")
                .value_name("PLAN_FILE")
                .num_args(0..=1)
                .default_missing_value("")
                .conflicts_with_all([PLAN_ARG, TASKS_ONLY_ARG, FINALIZE_ARG])
                .help(GUIDED_REVIEW_HELP),
        )
        .arg(
            Arg::new(FINALIZE_ARG)
                .short('f')
                .long("finalize")
                .value_name("PLAN_FILE")
                .num_args(0..=1)
                .default_missing_value("")
                .conflicts_with_all([PLAN_ARG, TASKS_ONLY_ARG, REVIEW_ARG])
                .help(GUIDED_FINALIZE_HELP),
        )
        .arg(
            Arg::new(WORKFLOWS_ARG)
                .long("workflows")
                .action(ArgAction::SetTrue)
                .help(WORKFLOWS_HELP),
        )
        .arg(
            Arg::new(SHOW_WORKFLOW_ARG)
                .long("show-workflow")
                .value_name("WORKFLOW_ID")
                .help(SHOW_WORKFLOW_HELP),
        )
        .arg(
            Arg::new(EDIT_WORKFLOW_ARG)
                .long("edit-workflow")
                .value_name("WORKFLOW_ID")
                .help(EDIT_WORKFLOW_HELP),
        )
        .arg(
            Arg::new(SHOW_CONFIG_ARG)
                .long("show-config")
                .value_name("SCOPE")
                .num_args(0..=1)
                .require_equals(true)
                .default_missing_value("effective")
                .value_parser(["user", "project", "effective"])
                .help(SHOW_CONFIG_HELP),
        )
        .arg(
            Arg::new(SET_PROJECT_AGENT_ARG)
                .long("set-project-agent")
                .value_name("ID")
                .conflicts_with(SET_USER_AGENT_ARG)
                .help(SET_PROJECT_AGENT_HELP),
        )
        .arg(
            Arg::new(SET_USER_AGENT_ARG)
                .long("set-user-agent")
                .value_name("ID")
                .conflicts_with(SET_PROJECT_AGENT_ARG)
                .help(SET_USER_AGENT_HELP),
        )
        .subcommand(build_workflow_command()?);

    if enable_internal_event_commands {
        command = command
            .subcommand(
                Command::new("signal")
                    .hide(true)
                    .about("Append a signal event to the current Ralph run WAL")
                    .long_about(SIGNAL_LONG_ABOUT)
                    .arg_required_else_help(true)
                    .arg(
                        Arg::new(EVENT_ARG)
                            .value_name("EVENT")
                            .required(true)
                            .help(SIGNAL_EVENT_HELP),
                    ),
            )
            .subcommand(
                Command::new("payload")
                    .hide(true)
                    .about("Append a payload event to the current Ralph run WAL")
                    .long_about(PAYLOAD_LONG_ABOUT)
                    .arg_required_else_help(true)
                    .arg(
                        Arg::new(EVENT_ARG)
                            .value_name("EVENT")
                            .required(true)
                            .help(PAYLOAD_EVENT_HELP),
                    )
                    .arg(
                        Arg::new(BODY_ARG)
                            .value_name("BODY")
                            .required(true)
                            .allow_hyphen_values(true)
                            .help(PAYLOAD_BODY_HELP),
                    ),
            )
            .subcommand(
                Command::new("get")
                    .hide(true)
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
            );
    }

    Ok(command)
}

fn build_workflow_command() -> Result<Command> {
    let mut command = Command::new("w")
        .about("Run a workflow in the terminal")
        .visible_alias("workflow")
        .after_help(RUN_AFTER_HELP)
        .arg_required_else_help(true)
        .subcommand_required(true)
        .subcommand_help_heading("Workflows");

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

    Ok(command.arg(
        Arg::new(REQUEST_ARG)
            .value_name("REQUEST")
            .trailing_var_arg(true)
            .allow_hyphen_values(true)
            .num_args(1..)
            .help(REQUEST_HELP),
    ))
}

fn parse_runtime_args(matches: &ArgMatches) -> RuntimeArgs {
    RuntimeArgs {
        agent: matches.get_one::<String>(AGENT_ARG).cloned(),
        max_iterations: matches.get_one::<usize>(MAX_ITERATIONS_ARG).copied(),
        session_timeout_secs: matches.get_one::<u64>(SESSION_TIMEOUT_ARG).copied(),
        idle_timeout_secs: matches.get_one::<u64>(IDLE_TIMEOUT_ARG).copied(),
    }
}

fn parse_workflow_args(root_matches: &ArgMatches, matches: &ArgMatches) -> Result<RunArgs> {
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
        runtime: parse_runtime_args(root_matches),
        workflow: workflow.to_owned(),
        workflow_options,
        request_args: RequestArgs {
            request_file: root_matches
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

fn parse_signal_args(matches: &ArgMatches) -> Result<SignalArgs> {
    Ok(SignalArgs {
        event: required_string_result(matches, EVENT_ARG)?,
    })
}

fn parse_payload_args(matches: &ArgMatches) -> Result<PayloadArgs> {
    Ok(PayloadArgs {
        event: required_string_result(matches, EVENT_ARG)?,
        body: required_string_result(matches, BODY_ARG)?,
    })
}

fn parse_show_workflow_args(matches: &ArgMatches) -> Result<ShowArgs> {
    Ok(ShowArgs {
        workflow_id: required_string_result(matches, SHOW_WORKFLOW_ARG)?,
    })
}

fn parse_edit_workflow_args(matches: &ArgMatches) -> Result<EditArgs> {
    Ok(EditArgs {
        workflow_id: required_string_result(matches, EDIT_WORKFLOW_ARG)?,
    })
}

fn parse_show_config_args(matches: &ArgMatches) -> Result<ConfigShowArgs> {
    Ok(ConfigShowArgs {
        scope: ConfigViewArg::parse(
            matches
                .get_one::<String>(SHOW_CONFIG_ARG)
                .map(String::as_str)
                .unwrap_or("effective"),
        )?,
    })
}

fn parse_guided_request_args(
    matches: &ArgMatches,
    planning_flag_mode: bool,
    internal_event_commands_enabled: bool,
) -> Result<RequestArgs> {
    let guided_request = matches
        .get_many::<String>(GUIDED_REQUEST_ARG)
        .map(|values| values.cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    let plan_description = normalize_optional_value(matches.get_one::<String>(PLAN_ARG));

    if planning_flag_mode && plan_description.is_some() && !guided_request.is_empty() {
        return Err(anyhow!(
            "--plan=<DESCRIPTION> cannot be combined with positional request text"
        ));
    }
    if !internal_event_commands_enabled
        && guided_request
            .first()
            .is_some_and(|value| INTERNAL_EVENT_COMMAND_NAMES.contains(&value.as_str()))
    {
        let command = guided_request.first().expect("checked above");
        return Err(anyhow!(
            "'{command}' is reserved for internal Ralph agent communication and only works inside a Ralph agent run"
        ));
    }

    Ok(RequestArgs {
        request_file: matches.get_one::<Utf8PathBuf>(REQUEST_FILE_ARG).cloned(),
        request: match plan_description {
            Some(description) => vec![description],
            None => guided_request,
        },
    })
}

fn required_string_result(matches: &ArgMatches, id: &str) -> Result<String> {
    matches
        .get_one::<String>(id)
        .cloned()
        .ok_or_else(|| anyhow!("missing required argument '{}'", id))
}

fn ensure_runtime_flags_absent(runtime_flags_present: bool, context: &str) -> Result<()> {
    if !runtime_flags_present {
        Ok(())
    } else {
        Err(anyhow!(
            "{context} cannot be combined with runtime override flags"
        ))
    }
}

fn ensure_request_file_absent(matches: &ArgMatches, context: &str) -> Result<()> {
    if matches.contains_id(REQUEST_FILE_ARG) && arg_present(matches, REQUEST_FILE_ARG) {
        Err(anyhow!("{context} does not accept --file"))
    } else {
        Ok(())
    }
}

fn ensure_guided_request_absent(matches: &ArgMatches, context: &str) -> Result<()> {
    if matches.get_many::<String>(GUIDED_REQUEST_ARG).is_some() {
        Err(anyhow!("{context} does not accept positional request text"))
    } else {
        Ok(())
    }
}

fn ensure_config_mutations_absent(mutations: &ConfigMutationArgs, context: &str) -> Result<()> {
    if mutations.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(
            "{context} cannot be combined with config mutation flags"
        ))
    }
}

fn arg_present(matches: &ArgMatches, id: &str) -> bool {
    matches
        .value_source(id)
        .is_some_and(|source| source != ValueSource::DefaultValue)
}

fn normalize_optional_value(value: Option<&String>) -> Option<String> {
    value
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn parse_timeout_duration(value: &str) -> Result<u64> {
    if value.len() < 2 {
        return Err(anyhow!(
            "invalid duration '{}'; expected [integer][h|m|s]",
            value
        ));
    }
    let (number, unit) = value.split_at(value.len() - 1);
    let amount = number
        .parse::<u64>()
        .map_err(|_| anyhow!("invalid duration '{}'; expected [integer][h|m|s]", value))?;
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

fn internal_event_commands_enabled() -> bool {
    std::env::var_os("RALPH_WAL_PATH").is_some()
}

#[cfg(test)]
mod tests {
    use clap::error::ErrorKind;

    use super::{
        Cli, Commands, ConfigViewArg, build_cli_command_with_internal_commands,
        parse_timeout_duration,
    };
    use crate::test_support::with_test_workflow_home;

    #[test]
    fn root_cli_defaults_to_guided_mode() {
        with_test_workflow_home(|| {
            let cli = Cli::try_parse_from(["ralph"]).unwrap();

            let Some(Commands::Guided(args)) = cli.command else {
                panic!("expected guided command");
            };
            assert!(args.request_args.request.is_empty());
            assert!(args.request_args.request_file.is_none());
            assert!(args.build_after_plan);
        });
    }

    #[test]
    fn bare_guided_mode_accepts_request_file() {
        with_test_workflow_home(|| {
            let cli = Cli::try_parse_from(["ralph", "--file", "REQ.md"]).unwrap();

            let Some(Commands::Guided(args)) = cli.command else {
                panic!("expected guided command");
            };
            assert_eq!(
                args.request_args.request_file,
                Some(camino::Utf8PathBuf::from("REQ.md"))
            );
        });
    }

    #[test]
    fn bare_guided_mode_accepts_argv_request() {
        with_test_workflow_home(|| {
            let cli = Cli::try_parse_from(["ralph", "ship", "auth"]).unwrap();

            let Some(Commands::Guided(args)) = cli.command else {
                panic!("expected guided command");
            };
            assert_eq!(args.request_args.argv_text().as_deref(), Some("ship auth"));
            assert!(args.request_args.request_file.is_none());
        });
    }

    #[test]
    fn plan_flag_parses_optional_description() {
        with_test_workflow_home(|| {
            let cli = Cli::try_parse_from(["ralph", "--plan=ship auth"]).unwrap();

            let Some(Commands::Guided(args)) = cli.command else {
                panic!("expected guided command");
            };
            assert_eq!(args.request_args.argv_text().as_deref(), Some("ship auth"));
            assert!(!args.build_after_plan);
        });
    }

    #[test]
    fn plan_flag_rejects_duplicate_argv_request_text() {
        with_test_workflow_home(|| {
            let error =
                Cli::try_parse_from(["ralph", "--plan=ship auth", "and", "cache"]).unwrap_err();

            assert_eq!(error.kind(), ErrorKind::InvalidValue);
            assert!(
                error
                    .to_string()
                    .contains("cannot be combined with positional request text")
            );
        });
    }

    #[test]
    fn plan_flag_without_description_stops_after_planning() {
        with_test_workflow_home(|| {
            let cli = Cli::try_parse_from(["ralph", "--plan"]).unwrap();

            let Some(Commands::Guided(args)) = cli.command else {
                panic!("expected guided command");
            };
            assert!(args.request_args.argv_text().is_none());
            assert!(!args.build_after_plan);
        });
    }

    #[test]
    fn tasks_only_flag_requires_plan_file() {
        with_test_workflow_home(|| {
            let cli = Cli::try_parse_from(["ralph", "-t", "PLAN.md"]).unwrap();

            let Some(Commands::TasksOnly(args)) = cli.command else {
                panic!("expected tasks-only command");
            };
            assert_eq!(args.plan_file, "PLAN.md");

            let error = Cli::try_parse_from(["ralph", "-t"]).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidValue);
        });
    }

    #[test]
    fn review_and_finalize_flags_accept_optional_plan_file() {
        with_test_workflow_home(|| {
            let review = Cli::try_parse_from(["ralph", "-r"]).unwrap();
            let Some(Commands::ReviewOnly(args)) = review.command else {
                panic!("expected review-only command");
            };
            assert!(args.plan_file.is_none());

            let review = Cli::try_parse_from(["ralph", "-r", "PLAN.md"]).unwrap();
            let Some(Commands::ReviewOnly(args)) = review.command else {
                panic!("expected review-only command");
            };
            assert_eq!(args.plan_file.as_deref(), Some("PLAN.md"));

            let finalize = Cli::try_parse_from(["ralph", "-f"]).unwrap();
            let Some(Commands::FinalizeOnly(args)) = finalize.command else {
                panic!("expected finalize-only command");
            };
            assert!(args.plan_file.is_none());

            let finalize = Cli::try_parse_from(["ralph", "-f", "PLAN.md"]).unwrap();
            let Some(Commands::FinalizeOnly(args)) = finalize.command else {
                panic!("expected finalize-only command");
            };
            assert_eq!(args.plan_file.as_deref(), Some("PLAN.md"));
        });
    }

    #[test]
    fn multiple_primary_actions_are_rejected() {
        with_test_workflow_home(|| {
            let error = Cli::try_parse_from(["ralph", "--plan", "--workflows"]).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidValue);
            assert!(
                error
                    .to_string()
                    .contains("multiple primary actions cannot be combined")
            );
        });
    }

    #[test]
    fn workflow_subcommand_parses_positional_workflow_and_request() {
        with_test_workflow_home(|| {
            let cli = Cli::try_parse_from(["ralph", "w", "fixture-flow", "fix", "tests"]).unwrap();

            let Some(Commands::Workflow(args)) = cli.command else {
                panic!("expected workflow command");
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
    fn workflow_subcommand_accepts_root_runtime_flags() {
        with_test_workflow_home(|| {
            let cli = Cli::try_parse_from([
                "ralph",
                "--agent",
                "claude",
                "--session-timeout",
                "30m",
                "--idle-timeout",
                "5m",
                "w",
                "fixture-flow",
                "ship",
                "it",
            ])
            .unwrap();

            let Some(Commands::Workflow(args)) = cli.command else {
                panic!("expected workflow command");
            };
            assert_eq!(args.workflow, "fixture-flow");
            assert_eq!(args.runtime.agent.as_deref(), Some("claude"));
            assert_eq!(args.runtime.session_timeout_secs, Some(30 * 60));
            assert_eq!(args.runtime.idle_timeout_secs, Some(5 * 60));
        });
    }

    #[test]
    fn workflow_subcommand_parses_request_file_from_root() {
        with_test_workflow_home(|| {
            let cli =
                Cli::try_parse_from(["ralph", "--file", "REQ.md", "w", "fixture-flow"]).unwrap();

            let Some(Commands::Workflow(args)) = cli.command else {
                panic!("expected workflow command");
            };
            assert_eq!(
                args.request_args.request_file,
                Some(camino::Utf8PathBuf::from("REQ.md"))
            );
        });
    }

    #[test]
    fn workflow_subcommand_parses_workflow_specific_options() {
        with_test_workflow_home(|| {
            let cli = Cli::try_parse_from([
                "ralph",
                "w",
                "fixture-flow",
                "--statefile",
                "snapshot.md",
                "fix",
                "tests",
            ])
            .unwrap();

            let Some(Commands::Workflow(args)) = cli.command else {
                panic!("expected workflow command");
            };
            assert_eq!(
                args.workflow_options.get("state-file").map(String::as_str),
                Some("snapshot.md")
            );
        });
    }

    #[test]
    fn workflow_help_includes_declared_options_and_hides_root_runtime_flags() {
        with_test_workflow_home(|| {
            let error = Cli::try_parse_from(["ralph", "w", "fixture-flow", "--help"]).unwrap_err();
            let rendered = error.to_string();

            assert_eq!(error.kind(), ErrorKind::DisplayHelp);
            assert!(rendered.contains("--statefile"));
            assert!(rendered.contains("state.txt"));
            assert!(!rendered.contains("--session-timeout"));
            assert!(!rendered.contains("--idle-timeout"));
        });
    }

    #[test]
    fn hidden_workflows_stay_out_of_help_but_remain_invocable_by_id() {
        with_test_workflow_home(|| {
            let mut command = build_cli_command_with_internal_commands(false).unwrap();
            let error = command
                .try_get_matches_from_mut(["ralph", "w", "--help"])
                .unwrap_err();
            let rendered = error.to_string();

            assert_eq!(error.kind(), ErrorKind::DisplayHelp);
            assert!(rendered.contains("finalize"));
            assert!(rendered.contains("plan"));
            assert!(rendered.contains("review"));
            assert!(rendered.contains("task"));
            assert!(!rendered.contains("test-workflow"));

            let cli = Cli::try_parse_from(["ralph", "w", "test-workflow"]).unwrap();
            let Some(Commands::Workflow(args)) = cli.command else {
                panic!("expected workflow command");
            };
            assert_eq!(args.workflow, "test-workflow");
            assert!(args.request_args.request.is_empty());
        });
    }

    #[test]
    fn non_runnable_actions_reject_runtime_overrides() {
        with_test_workflow_home(|| {
            assert!(Cli::try_parse_from(["ralph", "--agent", "claude", "--workflows"]).is_err());
            assert!(
                Cli::try_parse_from([
                    "ralph",
                    "--max-iterations",
                    "3",
                    "--show-workflow",
                    "fixture-flow"
                ])
                .is_err()
            );
            assert!(
                Cli::try_parse_from(["ralph", "--session-timeout", "3m", "--show-config"]).is_err()
            );
        });
    }

    #[test]
    fn non_runnable_actions_reject_guided_request_text() {
        with_test_workflow_home(|| {
            let error = Cli::try_parse_from(["ralph", "--workflows", "ship auth"]).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidValue);
            assert!(
                error
                    .to_string()
                    .contains("--workflows does not accept positional request text")
            );
        });
    }

    #[test]
    fn config_mutation_flags_can_be_used_without_a_primary_action() {
        with_test_workflow_home(|| {
            let cli = Cli::try_parse_from(["ralph", "--set-project-agent", "claude"]).unwrap();
            assert!(cli.command.is_none());
            assert_eq!(
                cli.config_mutations.set_project_agent.as_deref(),
                Some("claude")
            );
        });
    }

    #[test]
    fn show_config_defaults_to_effective_scope() {
        with_test_workflow_home(|| {
            let cli = Cli::try_parse_from(["ralph", "--show-config"]).unwrap();
            let Some(Commands::ShowConfig(args)) = cli.command else {
                panic!("expected show-config command");
            };
            assert_eq!(args.scope, ConfigViewArg::Effective);
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
            assert!(
                parse_timeout_duration(value).is_err(),
                "{value} should fail"
            );
        }
    }

    #[test]
    fn internal_event_commands_are_hidden_from_help() {
        with_test_workflow_home(|| {
            let mut command = build_cli_command_with_internal_commands(true).unwrap();
            let error = command
                .try_get_matches_from_mut(["ralph", "--help"])
                .unwrap_err();
            let rendered = error.to_string();

            assert_eq!(error.kind(), ErrorKind::DisplayHelp);
            assert!(!rendered.contains("signal"));
            assert!(!rendered.contains("payload"));
            assert!(!rendered.contains("get"));
        });
    }

    #[test]
    fn internal_event_commands_parse_only_when_enabled() {
        with_test_workflow_home(|| {
            let error = Cli::try_parse_from(["ralph", "get", "handoff"]).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidValue);
            assert!(
                error
                    .to_string()
                    .contains("reserved for internal Ralph agent communication")
            );

            let cli = Cli::try_parse_from_with_internal_commands(
                ["ralph", "get", "--channel", "QT", "handoff"],
                true,
            )
            .unwrap();
            let Some(Commands::Get(args)) = cli.command else {
                panic!("expected get command");
            };
            assert_eq!(args.event, "handoff");
            assert_eq!(args.channel.as_deref(), Some("QT"));
        });
    }
}
