use std::{
    env, fs,
    io::{self, IsTerminal, Read, Write},
    path::Path,
    process::{Command as ProcessCommand, Stdio},
};

use anyhow::{Context, Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};
use clap::{Args, Parser, Subcommand, ValueEnum};
use ralph_app::{ConsoleDelegate, RalphApp};
use ralph_core::{
    AppConfig, CliColorMode, CliOutputMode, CliPagerMode, CliPromptInputMode, CodingAgent,
    ConfigFileScope, ReviewData, RunControl, SpecSummary,
};
use ralph_tui::{run_tui, run_tui_scoped};
use serde::Serialize;
use tempfile::NamedTempFile;
use tracing_subscriber::{EnvFilter, fmt};

const ROOT_ABOUT: &str = "Durable agent workflow for repository planning and execution";
const ROOT_LONG_ABOUT: &str = "\
Ralph keeps planning and execution state on disk so work stays inspectable, resumable, and\
\nagent-independent.
\
\n\
\n`ralph` opens the full TUI.
\n`ralph <path-or-target>` opens the scoped TUI for a single spec.
\
\n\
\nUse the CLI when you want explicit workflow commands, inspectable output, setup tools, or\
\nscriptable configuration management.";
const ROOT_AFTER_HELP: &str = "\
Daily Workflow:
  ralph new [target]      Create a new target and run the planner
  ralph plan <target>     Revise an existing target from a new planning request
  ralph build <target>    Run builder iterations for a target
  ralph status [target]   Inspect all targets or summarize one target
  ralph show <target>     Print durable artifacts for one target
  ralph edit <target>     Edit the spec and automatically realign progress

Setup And Diagnostics:
  ralph init              Create project-local Ralph config
  ralph doctor            Validate config, tooling, and agent availability

Configuration:
  ralph config show       Render merged or scoped config
  ralph config get        Read one config value
  ralph config set        Write one config value
  ralph config edit       Open a config file in your editor
  ralph config path       Show config file locations

Agent Management:
  ralph agent list        Show supported agents and PATH detection
  ralph agent current     Show the effective agent and where it came from
  ralph agent set         Persist an agent in user or project config

Examples:
  ralph
  ralph docs/spec.md
  ralph new auth-refresh --prompt \"Add token rotation and failure recovery\"
  printf 'Tighten the delivery plan\\n' | ralph plan auth-refresh --stdin
  ralph build auth-refresh --agent codex
  ralph status
  ralph show auth-refresh --artifact progress
  ralph config set planner.program raijin --scope project";

const NEW_AFTER_HELP: &str = "\
Prompt input resolution:
  1. --prompt / --prompt-file / --stdin / --editor
  2. [cli].prompt_input from config
  3. auto fallback: editor on a TTY, stdin otherwise

Examples:
  ralph new --prompt \"Design a release checklist\"
  ralph new docs/release.md --editor";

const PLAN_AFTER_HELP: &str = "\
Use this when the target already exists and the user intent has changed.

Examples:
  ralph plan auth-refresh --prompt \"Include CLI migration notes\"
  cat request.txt | ralph plan auth-refresh --stdin";

const BUILD_AFTER_HELP: &str = "\
Examples:
  ralph build auth-refresh
  ralph build auth-refresh --builder-max-iterations 40

Persistent config:
  builder_max_iterations = 40";

const STATUS_AFTER_HELP: &str = "\
Examples:
  ralph status
  ralph status auth-refresh --output json";

const SHOW_AFTER_HELP: &str = "\
Examples:
  ralph show auth-refresh
  ralph show docs/spec.md --artifact feedback";

const EDIT_AFTER_HELP: &str = "\
If the spec changes, Ralph automatically runs the progress-revision planner pass afterward.

Examples:
  ralph edit auth-refresh
  ralph edit docs/spec.md --agent raijin

Persistent config:
  editor_override = \"nvim\"";

const INIT_AFTER_HELP: &str = "\
Examples:
  ralph init
  ralph init --agent codex --editor nvim --force

Persistent config keys:
  planning_max_iterations
  builder_max_iterations
  editor_override";

const DOCTOR_AFTER_HELP: &str = "\
Checks:
  - user and project config parse correctly
  - effective config loads
  - project state is writable
  - editor command resolves
  - supported agents are detectable on PATH";

const CONFIG_AFTER_HELP: &str = "\
Useful persistent keys:
  planning_max_iterations
  builder_max_iterations
  editor_override
  cli.output
  cli.pager
  cli.color
  cli.prompt_input

Examples:
  ralph config set planning_max_iterations 12 --scope project
  ralph config set builder_max_iterations 40 --scope project
  ralph config set editor_override nvim --scope user";

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum AgentArg {
    Opencode,
    Codex,
    Raijin,
}

impl From<AgentArg> for CodingAgent {
    fn from(value: AgentArg) -> Self {
        match value {
            AgentArg::Opencode => CodingAgent::Opencode,
            AgentArg::Codex => CodingAgent::Codex,
            AgentArg::Raijin => CodingAgent::Raijin,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OutputArg {
    Text,
    Json,
}

impl From<OutputArg> for CliOutputMode {
    fn from(value: OutputArg) -> Self {
        match value {
            OutputArg::Text => CliOutputMode::Text,
            OutputArg::Json => CliOutputMode::Json,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ColorArg {
    Auto,
    Always,
    Never,
}

impl From<ColorArg> for CliColorMode {
    fn from(value: ColorArg) -> Self {
        match value {
            ColorArg::Auto => CliColorMode::Auto,
            ColorArg::Always => CliColorMode::Always,
            ColorArg::Never => CliColorMode::Never,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum PagerArg {
    Auto,
    Always,
    Never,
}

impl From<PagerArg> for CliPagerMode {
    fn from(value: PagerArg) -> Self {
        match value {
            PagerArg::Auto => CliPagerMode::Auto,
            PagerArg::Always => CliPagerMode::Always,
            PagerArg::Never => CliPagerMode::Never,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ArtifactArg {
    Spec,
    Progress,
    Feedback,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ConfigScopeArg {
    User,
    Project,
    Effective,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum WritableConfigScopeArg {
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

#[derive(Debug, Clone, Copy)]
struct InspectPrefs {
    output: CliOutputMode,
    pager: CliPagerMode,
    color: CliColorMode,
}

#[derive(Debug, Clone, Default)]
struct WorkflowRuntimeOverrides {
    agent: Option<CodingAgent>,
    planning_max_iterations: Option<usize>,
    builder_max_iterations: Option<usize>,
    editor_override: Option<String>,
}

impl WorkflowRuntimeOverrides {
    fn apply_to_config(&self, config: &mut AppConfig) {
        if let Some(agent) = self.agent {
            config.set_coding_agent(agent);
        }
        if let Some(iterations) = self.planning_max_iterations {
            config.planning_max_iterations = iterations;
        }
        if let Some(iterations) = self.builder_max_iterations {
            config.builder_max_iterations = iterations;
        }
        if let Some(editor) = &self.editor_override {
            config.editor_override = Some(editor.clone());
        }
    }
}

#[derive(Debug, Clone, Args, Default)]
struct PlannerRuntimeArgs {
    #[arg(
        long,
        value_enum,
        help = "Temporarily use a different supported agent for this workflow command."
    )]
    agent: Option<AgentArg>,
    #[arg(
        long,
        value_name = "N",
        help = "Temporarily override planner iteration limit for this workflow command. Persistent config key: planning_max_iterations."
    )]
    planning_max_iterations: Option<usize>,
}

impl PlannerRuntimeArgs {
    fn overrides(&self) -> WorkflowRuntimeOverrides {
        WorkflowRuntimeOverrides {
            agent: self.agent.map(Into::into),
            planning_max_iterations: self.planning_max_iterations,
            ..WorkflowRuntimeOverrides::default()
        }
    }
}

#[derive(Debug, Clone, Args, Default)]
struct BuilderRuntimeArgs {
    #[arg(
        long,
        value_enum,
        help = "Temporarily use a different supported agent for this workflow command."
    )]
    agent: Option<AgentArg>,
    #[arg(
        long,
        value_name = "N",
        help = "Temporarily override builder iteration limit for this workflow command. Persistent config key: builder_max_iterations."
    )]
    builder_max_iterations: Option<usize>,
}

impl BuilderRuntimeArgs {
    fn overrides(&self) -> WorkflowRuntimeOverrides {
        WorkflowRuntimeOverrides {
            agent: self.agent.map(Into::into),
            builder_max_iterations: self.builder_max_iterations,
            ..WorkflowRuntimeOverrides::default()
        }
    }
}

#[derive(Debug, Clone, Args, Default)]
struct EditorCommandArg {
    #[arg(
        long = "editor-command",
        value_name = "CMD",
        help = "Temporarily override the editor command used by Ralph. Persistent config key: editor_override."
    )]
    editor_command: Option<String>,
}

impl EditorCommandArg {
    fn editor_override(&self) -> Option<String> {
        self.editor_command.clone()
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "ralph",
    about = ROOT_ABOUT,
    long_about = ROOT_LONG_ABOUT,
    after_help = ROOT_AFTER_HELP
)]
struct Cli {
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        help = "Use a different project directory for this invocation."
    )]
    project_dir: Option<Utf8PathBuf>,
    #[arg(
        long,
        global = true,
        value_enum,
        help = "Override inspect output mode for this invocation."
    )]
    output: Option<OutputArg>,
    #[arg(
        long,
        global = true,
        value_enum,
        help = "Override CLI color preference for this invocation."
    )]
    color: Option<ColorArg>,
    #[arg(
        long,
        global = true,
        value_enum,
        help = "Override CLI pager preference for this invocation."
    )]
    pager: Option<PagerArg>,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Clone, Subcommand)]
enum Commands {
    #[command(about = "Create a new target and run the planner", after_help = NEW_AFTER_HELP)]
    New(NewArgs),
    #[command(about = "Revise an existing target from a new planning request", after_help = PLAN_AFTER_HELP)]
    Plan(PlanArgs),
    #[command(about = "Run builder iterations for a target", after_help = BUILD_AFTER_HELP)]
    Build(BuildArgs),
    #[command(about = "Summarize all targets or inspect one target", after_help = STATUS_AFTER_HELP)]
    Status(StatusArgs),
    #[command(about = "Print durable artifacts for one target", after_help = SHOW_AFTER_HELP)]
    Show(ShowArgs),
    #[command(about = "Edit a spec and realign progress if it changed", after_help = EDIT_AFTER_HELP)]
    Edit(EditArgs),
    #[command(subcommand, about = "Inspect and manage supported coding agents")]
    Agent(AgentCommands),
    #[command(subcommand, about = "Inspect and manage Ralph configuration")]
    Config(ConfigCommands),
    #[command(about = "Create project-local Ralph config", after_help = INIT_AFTER_HELP)]
    Init(InitArgs),
    #[command(about = "Validate config, tooling, and agent availability", after_help = DOCTOR_AFTER_HELP)]
    Doctor,
}

#[derive(Debug, Clone, Args)]
struct NewArgs {
    #[arg(
        value_name = "TARGET",
        help = "Named target or explicit spec path to create."
    )]
    target: Option<String>,
    #[command(flatten)]
    prompt: PromptInputArgs,
    #[command(flatten)]
    runtime: PlannerRuntimeArgs,
    #[command(flatten)]
    editor_command: EditorCommandArg,
}

#[derive(Debug, Clone, Args)]
struct PlanArgs {
    #[arg(
        value_name = "TARGET",
        help = "Named target or explicit spec path to revise."
    )]
    target: String,
    #[command(flatten)]
    prompt: PromptInputArgs,
    #[command(flatten)]
    runtime: PlannerRuntimeArgs,
    #[command(flatten)]
    editor_command: EditorCommandArg,
}

#[derive(Debug, Clone, Args)]
struct BuildArgs {
    #[arg(value_name = "TARGET", help = "Named target or explicit spec path.")]
    target: String,
    #[command(flatten)]
    runtime: BuilderRuntimeArgs,
}

#[derive(Debug, Clone, Args)]
struct EditArgs {
    #[arg(value_name = "TARGET", help = "Named target or explicit spec path.")]
    target: String,
    #[command(flatten)]
    runtime: PlannerRuntimeArgs,
    #[command(flatten)]
    editor_command: EditorCommandArg,
}

#[derive(Debug, Clone, Args)]
struct StatusArgs {
    #[arg(
        value_name = "TARGET",
        help = "Optional target to summarize instead of listing every spec."
    )]
    target: Option<String>,
}

#[derive(Debug, Clone, Args)]
struct ShowArgs {
    #[arg(value_name = "TARGET", help = "Named target or explicit spec path.")]
    target: String,
    #[arg(long, value_enum, default_value_t = ArtifactArg::All, help = "Select which durable artifact to print.")]
    artifact: ArtifactArg,
}

#[derive(Debug, Clone, Args, Default)]
struct PromptInputArgs {
    #[arg(long, value_name = "TEXT", conflicts_with_all = ["prompt_file", "stdin", "editor"], help = "Use an inline planning request.")]
    prompt: Option<String>,
    #[arg(long, value_name = "PATH", conflicts_with_all = ["prompt", "stdin", "editor"], help = "Read the planning request from a file.")]
    prompt_file: Option<Utf8PathBuf>,
    #[arg(long, action = clap::ArgAction::SetTrue, conflicts_with_all = ["prompt", "prompt_file", "editor"], help = "Read the planning request from stdin.")]
    stdin: bool,
    #[arg(long, action = clap::ArgAction::SetTrue, conflicts_with_all = ["prompt", "prompt_file", "stdin"], help = "Open the planning request in your editor.")]
    editor: bool,
}

#[derive(Debug, Clone, Subcommand)]
enum AgentCommands {
    #[command(about = "List supported agents and whether they are detected on PATH")]
    List,
    #[command(about = "Show the effective agent and where it comes from")]
    Current,
    #[command(about = "Persist a supported agent into user or project config")]
    Set(AgentSetArgs),
}

#[derive(Debug, Clone, Args)]
struct AgentSetArgs {
    #[arg(value_enum, help = "Supported agent preset to persist.")]
    agent: AgentArg,
    #[arg(long, value_enum, default_value_t = WritableConfigScopeArg::Project, help = "Config scope to update.")]
    scope: WritableConfigScopeArg,
}

#[derive(Debug, Clone, Subcommand)]
enum ConfigCommands {
    #[command(about = "Render Ralph config from user, project, or effective scope", after_help = CONFIG_AFTER_HELP)]
    Show(ConfigShowArgs),
    #[command(about = "Read one config value from user, project, or effective scope", after_help = CONFIG_AFTER_HELP)]
    Get(ConfigGetArgs),
    #[command(about = "Set one config value in user or project config", after_help = CONFIG_AFTER_HELP)]
    Set(ConfigSetArgs),
    #[command(about = "Open a config file in your editor", after_help = CONFIG_AFTER_HELP)]
    Edit(ConfigEditArgs),
    #[command(about = "Show config file paths", after_help = CONFIG_AFTER_HELP)]
    Path(ConfigPathArgs),
}

#[derive(Debug, Clone, Args)]
struct ConfigShowArgs {
    #[arg(long, value_enum, default_value_t = ConfigScopeArg::Effective, help = "Which config view to render.")]
    scope: ConfigScopeArg,
}

#[derive(Debug, Clone, Args)]
struct ConfigGetArgs {
    #[arg(
        value_name = "KEY",
        help = "Dotted config key such as planner.program or cli.output."
    )]
    key: String,
    #[arg(long, value_enum, default_value_t = ConfigScopeArg::Effective, help = "Which config view to read from.")]
    scope: ConfigScopeArg,
}

#[derive(Debug, Clone, Args)]
struct ConfigSetArgs {
    #[arg(
        value_name = "KEY",
        help = "Dotted config key such as planner.program or cli.output."
    )]
    key: String,
    #[arg(
        value_name = "VALUE",
        help = "Value to write. TOML syntax is accepted; unparseable values are stored as strings."
    )]
    value: String,
    #[arg(long, value_enum, default_value_t = WritableConfigScopeArg::Project, help = "Which config file to modify.")]
    scope: WritableConfigScopeArg,
}

#[derive(Debug, Clone, Args)]
struct ConfigEditArgs {
    #[arg(long, value_enum, default_value_t = WritableConfigScopeArg::Project, help = "Which config file to open.")]
    scope: WritableConfigScopeArg,
}

#[derive(Debug, Clone, Args)]
struct ConfigPathArgs {
    #[arg(long, value_enum, default_value_t = WritableConfigScopeArg::Project, help = "Which config path to print.")]
    scope: WritableConfigScopeArg,
}

#[derive(Debug, Clone, Args)]
struct InitArgs {
    #[arg(
        long,
        value_enum,
        help = "Persist a supported agent into the new project config."
    )]
    agent: Option<AgentArg>,
    #[arg(
        long,
        value_name = "CMD",
        help = "Persist an editor override into the new project config."
    )]
    editor: Option<String>,
    #[arg(
        long,
        value_name = "N",
        help = "Persist planner iteration limit into the new project config."
    )]
    planning_max_iterations: Option<usize>,
    #[arg(
        long,
        value_name = "N",
        help = "Persist builder iteration limit into the new project config."
    )]
    builder_max_iterations: Option<usize>,
    #[arg(long, action = clap::ArgAction::SetTrue, help = "Overwrite an existing project config.")]
    force: bool,
}

#[derive(Debug, Clone, Serialize)]
struct StatusEntry {
    spec_path: String,
    progress_path: String,
    feedback_path: String,
    state: String,
    spec_preview: String,
    progress_preview: String,
    feedback_preview: String,
}

#[derive(Debug, Clone, Serialize)]
struct AgentStatus {
    name: String,
    label: String,
    command: String,
    detected: bool,
}

#[derive(Debug, Clone, Serialize)]
struct AgentCurrent {
    effective: String,
    source: String,
    configured_user: Option<String>,
    configured_project: Option<String>,
    detected: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct DoctorCheck {
    name: String,
    ok: bool,
    detail: String,
}

#[derive(Debug, Clone, Serialize)]
struct DoctorReport {
    ok: bool,
    checks: Vec<DoctorCheck>,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    if let Some(target) = direct_tui_target_argument() {
        let project_dir = current_project_dir()?;
        let app = RalphApp::load(project_dir)?;
        if io::stdin().is_terminal() && io::stdout().is_terminal() {
            run_tui_scoped(app, &target)?;
            return Ok(());
        }
        return Err(anyhow!("interactive TUI requires a TTY"));
    }

    let cli = Cli::parse();
    let project_dir = cli
        .project_dir
        .clone()
        .map(Ok)
        .unwrap_or_else(current_project_dir)?;
    let command = cli.command.clone();

    match command {
        None => {
            let app = load_workflow_app(&project_dir, &WorkflowRuntimeOverrides::default())?;
            if io::stdin().is_terminal() && io::stdout().is_terminal() {
                run_tui(app)?;
                Ok(())
            } else {
                Err(anyhow!("interactive TUI requires a TTY"))
            }
        }
        Some(Commands::New(args)) => cmd_new(&cli, &project_dir, args).await,
        Some(Commands::Plan(args)) => cmd_plan(&cli, &project_dir, args).await,
        Some(Commands::Build(args)) => cmd_build(&cli, &project_dir, args).await,
        Some(Commands::Status(args)) => cmd_status(&cli, &project_dir, args),
        Some(Commands::Show(args)) => cmd_show(&cli, &project_dir, args),
        Some(Commands::Edit(args)) => cmd_edit(&cli, &project_dir, args).await,
        Some(Commands::Agent(command)) => cmd_agent(&cli, &project_dir, command),
        Some(Commands::Config(command)) => cmd_config(&cli, &project_dir, command),
        Some(Commands::Init(args)) => cmd_init(&project_dir, args),
        Some(Commands::Doctor) => cmd_doctor(&cli, &project_dir),
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = fmt().with_env_filter(filter).with_target(false).try_init();
}

fn current_project_dir() -> Result<Utf8PathBuf> {
    let cwd = env::current_dir().context("failed to get current directory")?;
    Utf8PathBuf::from_path_buf(cwd).map_err(|_| anyhow!("current directory is not valid UTF-8"))
}

fn load_workflow_app(
    project_dir: &Utf8Path,
    overrides: &WorkflowRuntimeOverrides,
) -> Result<RalphApp> {
    let mut app = RalphApp::load(project_dir.to_path_buf())?;
    overrides.apply_to_config(app.config_mut());
    Ok(app)
}

fn inspect_prefs(cli: &Cli, config: &AppConfig) -> InspectPrefs {
    InspectPrefs {
        output: cli.output.map(Into::into).unwrap_or(config.cli.output),
        pager: cli.pager.map(Into::into).unwrap_or(config.cli.pager),
        color: cli.color.map(Into::into).unwrap_or(config.cli.color),
    }
}

async fn cmd_new(_cli: &Cli, project_dir: &Utf8Path, args: NewArgs) -> Result<()> {
    let mut overrides = args.runtime.overrides();
    overrides.editor_override = args.editor_command.editor_override();
    let app = load_workflow_app(project_dir, &overrides)?;
    let request = load_planning_request(&args.prompt, app.config())?;
    let control = install_ctrl_c_handler();
    let mut delegate = ConsoleDelegate;

    if let Some(target) = args.target {
        let paths = app.resolve_target(&target)?;
        if paths.spec_path.exists() || paths.progress_path.exists() || paths.feedback_path.exists()
        {
            return Err(anyhow!(
                "target already exists at {}; use `ralph plan {target}` to revise it",
                paths.spec_path
            ));
        }
        app.revise_target_with_control(&target, &request, control, &mut delegate)
            .await?;
    } else {
        app.create_new_with_control(&request, control, &mut delegate)
            .await?;
    }
    Ok(())
}

async fn cmd_plan(_cli: &Cli, project_dir: &Utf8Path, args: PlanArgs) -> Result<()> {
    let mut overrides = args.runtime.overrides();
    overrides.editor_override = args.editor_command.editor_override();
    let app = load_workflow_app(project_dir, &overrides)?;
    let request = load_planning_request(&args.prompt, app.config())?;
    let control = install_ctrl_c_handler();
    let mut delegate = ConsoleDelegate;
    app.revise_target_with_control(&args.target, &request, control, &mut delegate)
        .await?;
    Ok(())
}

async fn cmd_build(_cli: &Cli, project_dir: &Utf8Path, args: BuildArgs) -> Result<()> {
    let overrides = args.runtime.overrides();
    let app = load_workflow_app(project_dir, &overrides)?;
    let control = install_ctrl_c_handler();
    let mut delegate = ConsoleDelegate;
    app.run_target_with_control(&args.target, control, &mut delegate)
        .await?;
    Ok(())
}

fn cmd_status(cli: &Cli, project_dir: &Utf8Path, args: StatusArgs) -> Result<()> {
    let app = RalphApp::load(project_dir.to_path_buf())?;
    let prefs = inspect_prefs(cli, app.config());
    match args.target {
        Some(target) => {
            let review = app.review_target(&target)?;
            let entry = status_entry_from_review(&review);
            emit_text_or_json(&render_status_entry(&entry), &entry, &prefs)
        }
        None => {
            let specs = app.list_specs()?;
            let entries = specs
                .into_iter()
                .map(status_entry_from_summary)
                .collect::<Vec<_>>();
            emit_text_or_json(&render_status_table(&entries), &entries, &prefs)
        }
    }
}

fn cmd_show(cli: &Cli, project_dir: &Utf8Path, args: ShowArgs) -> Result<()> {
    let app = RalphApp::load(project_dir.to_path_buf())?;
    let prefs = inspect_prefs(cli, app.config());
    let review = app.review_target(&args.target)?;
    let text = render_artifacts(&review, args.artifact);
    emit_text_or_json(&text, &review, &prefs)
}

async fn cmd_edit(_cli: &Cli, project_dir: &Utf8Path, args: EditArgs) -> Result<()> {
    let mut overrides = args.runtime.overrides();
    overrides.editor_override = args.editor_command.editor_override();
    let app = load_workflow_app(project_dir, &overrides)?;
    let session = app.begin_spec_edit(&args.target)?;
    app.edit_spec_session(&session)?;
    let Some(request) = app.finish_spec_edit(session)? else {
        println!("Spec unchanged; progress was not revised.");
        return Ok(());
    };

    let control = install_ctrl_c_handler();
    let mut delegate = ConsoleDelegate;
    app.revise_progress_after_spec_edit_with_control(request, control, &mut delegate)
        .await?;
    Ok(())
}

fn cmd_agent(cli: &Cli, project_dir: &Utf8Path, command: AgentCommands) -> Result<()> {
    match command {
        AgentCommands::List => {
            let config = AppConfig::load(project_dir)?;
            let prefs = inspect_prefs(cli, &config);
            let detected = CodingAgent::detected();
            let statuses = [
                CodingAgent::Opencode,
                CodingAgent::Codex,
                CodingAgent::Raijin,
            ]
            .into_iter()
            .map(|agent| AgentStatus {
                name: agent_name(agent).to_owned(),
                label: agent.label().to_owned(),
                command: agent_program(agent).to_owned(),
                detected: detected.contains(&agent),
            })
            .collect::<Vec<_>>();
            emit_text_or_json(&render_agent_list(&statuses), &statuses, &prefs)
        }
        AgentCommands::Current => {
            let config = AppConfig::load(project_dir)?;
            let prefs = inspect_prefs(cli, &config);
            let detected = CodingAgent::detected();
            let configured_project = AppConfig::configured_coding_agent_for_scope(
                project_dir,
                ConfigFileScope::Project,
            )?;
            let configured_user =
                AppConfig::configured_coding_agent_for_scope(project_dir, ConfigFileScope::User)?;
            let effective = config.coding_agent();
            let source = if configured_project.is_some() {
                if configured_project == Some(effective) {
                    "project".to_owned()
                } else {
                    "detected fallback from project config".to_owned()
                }
            } else if configured_user.is_some() {
                if configured_user == Some(effective) {
                    "user".to_owned()
                } else {
                    "detected fallback from user config".to_owned()
                }
            } else if effective != AppConfig::default().coding_agent() {
                "detected fallback".to_owned()
            } else {
                "default".to_owned()
            };
            let current = AgentCurrent {
                effective: agent_name(effective).to_owned(),
                source,
                configured_user: configured_user.map(|agent| agent_name(agent).to_owned()),
                configured_project: configured_project.map(|agent| agent_name(agent).to_owned()),
                detected: detected
                    .into_iter()
                    .map(|agent| agent_name(agent).to_owned())
                    .collect(),
            };
            emit_text_or_json(&render_agent_current(&current), &current, &prefs)
        }
        AgentCommands::Set(args) => {
            let scope: ConfigFileScope = args.scope.into();
            AppConfig::persist_scoped_coding_agent(project_dir, scope, args.agent.into())?;
            let path = AppConfig::config_path_for_scope(project_dir, scope)?
                .ok_or_else(|| anyhow!("unable to resolve config path"))?;
            println!(
                "Set {} in {} config ({path})",
                agent_name(args.agent.into()),
                match scope {
                    ConfigFileScope::User => "user",
                    ConfigFileScope::Project => "project",
                }
            );
            Ok(())
        }
    }
}

fn cmd_config(cli: &Cli, project_dir: &Utf8Path, command: ConfigCommands) -> Result<()> {
    match command {
        ConfigCommands::Show(args) => {
            let (text, json_value) = match args.scope {
                ConfigScopeArg::Effective => {
                    let config = AppConfig::load(project_dir)?;
                    let text = config.effective_toml()?;
                    let json =
                        serde_json::to_value(&config).context("failed to serialize config")?;
                    (text, json)
                }
                ConfigScopeArg::User => {
                    let text = AppConfig::scoped_config_toml(project_dir, ConfigFileScope::User)?
                        .unwrap_or_else(|| "# no user config found\n".to_owned());
                    let json = AppConfig::read_scoped_config(project_dir, ConfigFileScope::User)?
                        .map(serde_json::to_value)
                        .transpose()
                        .context("failed to serialize config")?
                        .unwrap_or(serde_json::Value::Null);
                    (text, json)
                }
                ConfigScopeArg::Project => {
                    let text =
                        AppConfig::scoped_config_toml(project_dir, ConfigFileScope::Project)?
                            .unwrap_or_else(|| "# no project config found\n".to_owned());
                    let json =
                        AppConfig::read_scoped_config(project_dir, ConfigFileScope::Project)?
                            .map(serde_json::to_value)
                            .transpose()
                            .context("failed to serialize config")?
                            .unwrap_or(serde_json::Value::Null);
                    (text, json)
                }
            };
            let prefs = inspect_prefs(
                cli,
                &AppConfig::load(project_dir).unwrap_or_else(|_| AppConfig::default()),
            );
            emit_text_or_json(&text, &json_value, &prefs)
        }
        ConfigCommands::Get(args) => {
            let value = match args.scope {
                ConfigScopeArg::Effective => {
                    let config = AppConfig::load(project_dir)?;
                    let document = toml::Value::try_from(config)
                        .context("failed to convert effective config to TOML")?;
                    lookup_value_by_key(&document, &args.key).cloned()
                }
                ConfigScopeArg::User => {
                    AppConfig::scoped_config_value(project_dir, ConfigFileScope::User, &args.key)?
                }
                ConfigScopeArg::Project => AppConfig::scoped_config_value(
                    project_dir,
                    ConfigFileScope::Project,
                    &args.key,
                )?,
            };
            let prefs = inspect_prefs(
                cli,
                &AppConfig::load(project_dir).unwrap_or_else(|_| AppConfig::default()),
            );
            let value = value.unwrap_or(toml::Value::String("<unset>".to_owned()));
            emit_text_or_json(&value.to_string(), &value, &prefs)
        }
        ConfigCommands::Set(args) => {
            let scope: ConfigFileScope = args.scope.into();
            let value = parse_config_value(&args.value)?;
            AppConfig::set_scoped_config_value(project_dir, scope, &args.key, value)?;
            let path = AppConfig::config_path_for_scope(project_dir, scope)?
                .ok_or_else(|| anyhow!("unable to resolve config path"))?;
            println!("Updated `{}` in {path}", args.key);
            Ok(())
        }
        ConfigCommands::Edit(args) => {
            let scope: ConfigFileScope = args.scope.into();
            let path = AppConfig::config_path_for_scope(project_dir, scope)?
                .ok_or_else(|| anyhow!("unable to resolve config path"))?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create config directory at {parent}"))?;
            }
            if !path.exists() {
                fs::write(&path, "")
                    .with_context(|| format!("failed to create config file at {path}"))?;
            }
            let editor = preferred_editor(
                AppConfig::load(project_dir)
                    .ok()
                    .and_then(|config| config.editor_override.clone())
                    .as_deref(),
            );
            run_editor(editor.as_ref(), &path)?;
            AppConfig::validate_scoped_config(project_dir, scope)?;
            println!("Validated {path}");
            Ok(())
        }
        ConfigCommands::Path(args) => {
            let scope: ConfigFileScope = args.scope.into();
            let path = AppConfig::config_path_for_scope(project_dir, scope)?
                .ok_or_else(|| anyhow!("unable to resolve config path"))?;
            println!("{path}");
            Ok(())
        }
    }
}

fn cmd_init(project_dir: &Utf8Path, args: InitArgs) -> Result<()> {
    let path = AppConfig::project_config_path(project_dir);
    if path.exists() && !args.force {
        return Err(anyhow!(
            "project config already exists at {path}; rerun with --force to overwrite it"
        ));
    }

    let mut config = AppConfig::default();
    if let Some(agent) = args.agent.map(Into::into) {
        config.set_coding_agent(agent);
    } else {
        config.select_detected_coding_agent(&CodingAgent::detected());
    }
    if let Some(editor) = args.editor {
        config.editor_override = Some(editor);
    }
    if let Some(iterations) = args.planning_max_iterations {
        config.planning_max_iterations = iterations;
    }
    if let Some(iterations) = args.builder_max_iterations {
        config.builder_max_iterations = iterations;
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create config directory at {parent}"))?;
    }
    let rendered = config.effective_toml()?;
    fs::write(&path, rendered)
        .with_context(|| format!("failed to write project config at {path}"))?;
    println!("Initialized project config at {path}");
    Ok(())
}

fn cmd_doctor(cli: &Cli, project_dir: &Utf8Path) -> Result<()> {
    let config = AppConfig::load(project_dir).unwrap_or_else(|_| AppConfig::default());
    let prefs = inspect_prefs(cli, &config);
    let checks = build_doctor_checks(project_dir);
    let report = DoctorReport {
        ok: checks.iter().all(|check| check.ok),
        checks,
    };
    emit_text_or_json(&render_doctor_report(&report, prefs.color), &report, &prefs)?;
    if report.ok {
        Ok(())
    } else {
        std::process::exit(1);
    }
}

fn build_doctor_checks(project_dir: &Utf8Path) -> Vec<DoctorCheck> {
    let mut checks = Vec::new();

    checks.push(check_result(
        "user config",
        AppConfig::validate_scoped_config(project_dir, ConfigFileScope::User),
        "user config parses correctly",
    ));
    checks.push(check_result(
        "project config",
        AppConfig::validate_scoped_config(project_dir, ConfigFileScope::Project),
        "project config parses correctly",
    ));
    checks.push(check_result(
        "effective config",
        AppConfig::load(project_dir).map(|_| ()),
        "effective config loads successfully",
    ));

    let artifact_dir = project_dir.join(".ralph");
    checks.push(match probe_writable_project_state(&artifact_dir) {
        Ok(()) => DoctorCheck {
            name: "project state".to_owned(),
            ok: true,
            detail: format!("project artifacts are writable at {artifact_dir}"),
        },
        Err(error) => DoctorCheck {
            name: "project state".to_owned(),
            ok: false,
            detail: error.to_string(),
        },
    });

    let editor = preferred_editor(
        AppConfig::load(project_dir)
            .ok()
            .and_then(|config| config.editor_override.clone())
            .as_deref(),
    );
    checks.push(DoctorCheck {
        name: "editor".to_owned(),
        ok: command_available(editor.as_ref()),
        detail: if command_available(editor.as_ref()) {
            format!("editor command `{}` is available", editor)
        } else {
            format!("editor command `{}` is not available", editor)
        },
    });

    let detected = CodingAgent::detected();
    for agent in [
        CodingAgent::Opencode,
        CodingAgent::Codex,
        CodingAgent::Raijin,
    ] {
        checks.push(DoctorCheck {
            name: format!("agent: {}", agent_name(agent)),
            ok: detected.contains(&agent),
            detail: if detected.contains(&agent) {
                format!("{} detected on PATH", agent.label())
            } else {
                format!("{} not found on PATH", agent.label())
            },
        });
    }

    checks
}

fn check_result(name: &str, result: Result<()>, success_detail: &str) -> DoctorCheck {
    match result {
        Ok(()) => DoctorCheck {
            name: name.to_owned(),
            ok: true,
            detail: success_detail.to_owned(),
        },
        Err(error) => DoctorCheck {
            name: name.to_owned(),
            ok: false,
            detail: error.to_string(),
        },
    }
}

fn probe_writable_project_state(artifact_dir: &Utf8Path) -> Result<()> {
    fs::create_dir_all(artifact_dir)
        .with_context(|| format!("failed to create artifact directory at {artifact_dir}"))?;
    let probe = artifact_dir.join(".doctor-write-test");
    fs::write(&probe, "ok").with_context(|| format!("failed to write probe file at {probe}"))?;
    fs::remove_file(&probe).with_context(|| format!("failed to remove probe file at {probe}"))
}

fn load_planning_request(args: &PromptInputArgs, config: &AppConfig) -> Result<String> {
    if let Some(prompt) = &args.prompt {
        return non_empty_prompt(prompt.to_owned());
    }
    if let Some(path) = &args.prompt_file {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read planning request from {path}"))?;
        return non_empty_prompt(contents);
    }
    if args.stdin {
        return read_prompt_from_stdin();
    }
    if args.editor {
        return read_prompt_from_editor(config);
    }

    match config.cli.prompt_input {
        CliPromptInputMode::Stdin => read_prompt_from_stdin(),
        CliPromptInputMode::Editor => read_prompt_from_editor(config),
        CliPromptInputMode::Prompt => read_prompt_interactively(),
        CliPromptInputMode::Auto => {
            if io::stdin().is_terminal() {
                read_prompt_from_editor(config)
            } else {
                read_prompt_from_stdin()
            }
        }
    }
}

fn read_prompt_from_stdin() -> Result<String> {
    let mut buffer = String::new();
    io::stdin()
        .read_to_string(&mut buffer)
        .context("failed to read planning request from stdin")?;
    non_empty_prompt(buffer)
}

fn read_prompt_interactively() -> Result<String> {
    if !io::stdin().is_terminal() {
        return Err(anyhow!(
            "interactive prompt input requires a TTY; use --stdin or --prompt instead"
        ));
    }

    println!("Enter the planning request, then press Ctrl-D:");
    let mut buffer = String::new();
    io::stdin()
        .read_to_string(&mut buffer)
        .context("failed to read planning request from stdin")?;
    non_empty_prompt(buffer)
}

fn read_prompt_from_editor(config: &AppConfig) -> Result<String> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(anyhow!(
            "editor prompt input requires a TTY; use --stdin or --prompt instead"
        ));
    }

    let mut temp = NamedTempFile::new().context("failed to create temporary prompt file")?;
    writeln!(
        temp,
        "Describe the planning request here, then save and close the editor.\n"
    )
    .context("failed to seed temporary prompt file")?;
    let path = Utf8PathBuf::from_path_buf(temp.path().to_path_buf())
        .map_err(|_| anyhow!("temporary prompt path is not valid UTF-8"))?;
    let editor = preferred_editor(config.editor_override.as_deref());
    run_editor(editor.as_ref(), &path)?;
    let contents = fs::read_to_string(&path)
        .with_context(|| format!("failed to read temporary file {path}"))?;
    non_empty_prompt(contents)
}

fn non_empty_prompt(value: String) -> Result<String> {
    if value.trim().is_empty() {
        Err(anyhow!("planning request cannot be empty"))
    } else {
        Ok(value)
    }
}

fn preferred_editor(configured: Option<&str>) -> String {
    configured
        .map(str::to_owned)
        .or_else(|| env::var("VISUAL").ok())
        .or_else(|| env::var("EDITOR").ok())
        .unwrap_or_else(|| "vi".to_owned())
}

fn run_editor(editor: &str, path: &Utf8Path) -> Result<()> {
    let status = ProcessCommand::new(editor)
        .arg(path.as_std_path())
        .status()
        .with_context(|| format!("failed to launch editor `{editor}`"))?;
    if !status.success() {
        return Err(anyhow!("editor `{editor}` exited with status {status}"));
    }
    Ok(())
}

fn emit_text_or_json<T>(text: &str, json_value: &T, prefs: &InspectPrefs) -> Result<()>
where
    T: Serialize,
{
    match prefs.output {
        CliOutputMode::Text => emit_text(text, prefs),
        CliOutputMode::Json => {
            let rendered =
                serde_json::to_string_pretty(json_value).context("failed to serialize JSON")?;
            println!("{rendered}");
            Ok(())
        }
    }
}

fn emit_text(text: &str, prefs: &InspectPrefs) -> Result<()> {
    let mut rendered = text.to_owned();
    if !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    if should_page(text, prefs.pager) {
        page_text(&rendered)
    } else {
        print!("{rendered}");
        Ok(())
    }
}

fn should_page(text: &str, pager: CliPagerMode) -> bool {
    match pager {
        CliPagerMode::Always => io::stdout().is_terminal(),
        CliPagerMode::Never => false,
        CliPagerMode::Auto => io::stdout().is_terminal() && text.lines().count() > 30,
    }
}

fn page_text(text: &str) -> Result<()> {
    #[cfg(unix)]
    {
        let pager = env::var("PAGER").unwrap_or_else(|_| "less -FRX".to_owned());
        let mut child = ProcessCommand::new("sh")
            .arg("-c")
            .arg(&pager)
            .stdin(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to launch pager `{pager}`"))?;
        child
            .stdin
            .as_mut()
            .context("failed to open pager stdin")?
            .write_all(text.as_bytes())
            .context("failed to write to pager")?;
        child.wait().context("failed to wait for pager")?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        print!("{text}");
        Ok(())
    }
}

fn status_entry_from_summary(summary: SpecSummary) -> StatusEntry {
    StatusEntry {
        spec_path: summary.spec_path.to_string(),
        progress_path: summary.progress_path.to_string(),
        feedback_path: summary.feedback_path.to_string(),
        state: workflow_state_label(summary.state).to_owned(),
        spec_preview: summary.spec_preview,
        progress_preview: summary.progress_preview,
        feedback_preview: summary.feedback_preview,
    }
}

fn status_entry_from_review(review: &ReviewData) -> StatusEntry {
    StatusEntry {
        spec_path: review.spec_path.to_string(),
        progress_path: review.progress_path.to_string(),
        feedback_path: review.feedback_path.to_string(),
        state: workflow_state_label(review.state).to_owned(),
        spec_preview: preview(&review.spec_contents),
        progress_preview: preview(&review.progress_contents),
        feedback_preview: preview(&review.feedback_contents),
    }
}

fn workflow_state_label(state: ralph_core::WorkflowState) -> &'static str {
    match state {
        ralph_core::WorkflowState::Empty => "empty",
        ralph_core::WorkflowState::Planned => "planned",
        ralph_core::WorkflowState::Completed => "completed",
    }
}

fn preview(contents: &str) -> String {
    let lines = contents
        .lines()
        .skip_while(|line| line.trim().is_empty())
        .take(6)
        .collect::<Vec<_>>();
    if lines.is_empty() {
        "<empty>".to_owned()
    } else {
        lines.join("\n")
    }
}

fn render_status_table(entries: &[StatusEntry]) -> String {
    if entries.is_empty() {
        return "No specs found.".to_owned();
    }

    let spec_width = entries
        .iter()
        .map(|entry| entry.spec_path.len())
        .max()
        .unwrap_or(4)
        .clamp(4, 56);
    let mut lines = vec![format!(
        "{:<spec_width$}  {:<10}  {}",
        "SPEC",
        "STATE",
        "PROGRESS",
        spec_width = spec_width
    )];
    lines.push(format!(
        "{:-<spec_width$}  {:-<10}  {:-<20}",
        "",
        "",
        "",
        spec_width = spec_width
    ));
    for entry in entries {
        let progress = entry.progress_preview.lines().next().unwrap_or("<empty>");
        lines.push(format!(
            "{:<spec_width$}  {:<10}  {}",
            truncate(&entry.spec_path, spec_width),
            entry.state,
            progress,
            spec_width = spec_width
        ));
    }
    lines.join("\n")
}

fn render_status_entry(entry: &StatusEntry) -> String {
    format!(
        "Spec: {}\nState: {}\nProgress: {}\nFeedback: {}\n\nSpec preview:\n{}\n\nProgress preview:\n{}\n\nFeedback preview:\n{}",
        entry.spec_path,
        entry.state,
        entry.progress_path,
        entry.feedback_path,
        entry.spec_preview,
        entry.progress_preview,
        entry.feedback_preview
    )
}

fn render_artifacts(review: &ReviewData, artifact: ArtifactArg) -> String {
    match artifact {
        ArtifactArg::Spec => format!(
            "--- spec ({}) ---\n{}",
            review.spec_path, review.spec_contents
        ),
        ArtifactArg::Progress => format!(
            "--- progress ({}) ---\n{}",
            review.progress_path, review.progress_contents
        ),
        ArtifactArg::Feedback => format!(
            "--- feedback ({}) ---\n{}",
            review.feedback_path, review.feedback_contents
        ),
        ArtifactArg::All => format!(
            "State: {}\n\n--- spec ({}) ---\n{}\n\n--- progress ({}) ---\n{}\n\n--- feedback ({}) ---\n{}",
            workflow_state_label(review.state),
            review.spec_path,
            review.spec_contents,
            review.progress_path,
            review.progress_contents,
            review.feedback_path,
            review.feedback_contents
        ),
    }
}

fn render_agent_list(statuses: &[AgentStatus]) -> String {
    let mut lines = vec!["SUPPORTED AGENTS".to_owned(), "----------------".to_owned()];
    for status in statuses {
        lines.push(format!(
            "{} ({})\n  command: {}\n  detected: {}",
            status.label,
            status.name,
            status.command,
            if status.detected { "yes" } else { "no" }
        ));
    }
    lines.join("\n")
}

fn render_agent_current(current: &AgentCurrent) -> String {
    format!(
        "Effective agent: {}\nSource: {}\nConfigured project agent: {}\nConfigured user agent: {}\nDetected agents: {}",
        current.effective,
        current.source,
        current.configured_project.as_deref().unwrap_or("<unset>"),
        current.configured_user.as_deref().unwrap_or("<unset>"),
        if current.detected.is_empty() {
            "<none>".to_owned()
        } else {
            current.detected.join(", ")
        }
    )
}

fn render_doctor_report(report: &DoctorReport, _color: CliColorMode) -> String {
    let mut lines = vec![if report.ok {
        "Ralph doctor passed.".to_owned()
    } else {
        "Ralph doctor found issues.".to_owned()
    }];
    for check in &report.checks {
        lines.push(format!(
            "[{}] {}: {}",
            if check.ok { "ok" } else { "fail" },
            check.name,
            check.detail
        ));
    }
    lines.join("\n")
}

fn parse_config_value(input: &str) -> Result<toml::Value> {
    let snippet = format!("value = {input}");
    if let Ok(mut parsed) = snippet.parse::<toml::Table>() {
        return parsed
            .remove("value")
            .ok_or_else(|| anyhow!("failed to parse config value"));
    }
    Ok(toml::Value::String(input.to_owned()))
}

fn lookup_value_by_key<'a>(value: &'a toml::Value, dotted_key: &str) -> Option<&'a toml::Value> {
    let mut current = value;
    for segment in dotted_key.split('.').filter(|segment| !segment.is_empty()) {
        current = current.as_table()?.get(segment)?;
    }
    Some(current)
}

fn agent_name(agent: CodingAgent) -> &'static str {
    match agent {
        CodingAgent::Opencode => "opencode",
        CodingAgent::Codex => "codex",
        CodingAgent::Raijin => "raijin",
    }
}

fn agent_program(agent: CodingAgent) -> &'static str {
    match agent {
        CodingAgent::Opencode => "opencode",
        CodingAgent::Codex => "codex",
        CodingAgent::Raijin => "raijin",
    }
}

fn command_available(command: &str) -> bool {
    let path = Path::new(command);
    if path.components().count() > 1 {
        return path.is_file();
    }
    let Some(path_env) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&path_env).any(|dir| dir.join(command).is_file())
}

fn truncate(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_owned();
    }
    let keep = width.saturating_sub(1);
    let mut output = value.chars().take(keep).collect::<String>();
    output.push('…');
    output
}

fn direct_tui_target_argument() -> Option<String> {
    let mut args = env::args().skip(1);
    let first = args.next()?;
    if args.next().is_some() {
        return None;
    }
    if first.starts_with('-') || is_known_command(&first) {
        return None;
    }
    Some(first)
}

fn is_known_command(value: &str) -> bool {
    matches!(
        value,
        "new"
            | "plan"
            | "build"
            | "status"
            | "show"
            | "edit"
            | "agent"
            | "config"
            | "init"
            | "doctor"
            | "help"
    )
}

fn install_ctrl_c_handler() -> RunControl {
    let control = RunControl::new();
    let cancel = control.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cancel.cancel();
        }
    });
    control
}

#[cfg(test)]
mod tests {
    use super::{Cli, is_known_command};
    use clap::{CommandFactory, Parser};

    #[test]
    fn root_help_includes_new_workflow_commands() {
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("ralph new [target]"));
        assert!(help.contains("ralph doctor"));
        assert!(help.contains("ralph <path-or-target>"));
    }

    #[test]
    fn parser_accepts_new_command_tree() {
        let cli = Cli::parse_from(["ralph", "config", "set", "cli.output", "json"]);
        match cli.command {
            Some(super::Commands::Config(_)) => {}
            other => panic!("unexpected command parse result: {other:?}"),
        }
    }

    #[test]
    fn parser_accepts_new_without_editor_collision() {
        let cli = Cli::try_parse_from(["ralph", "new"]).expect("new command should parse");
        match cli.command {
            Some(super::Commands::New(_)) => {}
            other => panic!("unexpected command parse result: {other:?}"),
        }
    }

    #[test]
    fn known_commands_are_new_surface_only() {
        assert!(is_known_command("new"));
        assert!(is_known_command("doctor"));
        assert!(!is_known_command("run"));
        assert!(!is_known_command("review"));
    }

    #[test]
    fn unknown_single_argument_is_not_treated_as_a_command() {
        assert!(!is_known_command("docs/spec.md"));
    }
}
