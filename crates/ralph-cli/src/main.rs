use std::{env, fs, path::Path, process::ExitCode};

use anyhow::{Context, Result, anyhow};
use camino::Utf8PathBuf;
use clap::{Args, Parser, Subcommand, ValueEnum};
use ralph_app::{ConsoleDelegate, RalphApp};
use ralph_core::{
    AppConfig, CodingAgent, ConfigFileScope, ScaffoldId, TargetReview, TargetSummary,
};
use ralph_tui::{run_tui, run_tui_scoped};
use serde::Serialize;
use tracing_subscriber::{EnvFilter, fmt};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ScaffoldArg {
    Blank,
    Playbook,
}

impl From<ScaffoldArg> for ScaffoldId {
    fn from(value: ScaffoldArg) -> Self {
        match value {
            ScaffoldArg::Blank => ScaffoldId::Blank,
            ScaffoldArg::Playbook => ScaffoldId::Playbook,
        }
    }
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

#[derive(Debug, Clone, Args, Default)]
struct RuntimeArgs {
    #[arg(long, value_enum)]
    agent: Option<AgentArg>,
    #[arg(long, value_name = "N")]
    max_iterations: Option<usize>,
}

impl RuntimeArgs {
    fn apply_to(&self, app: &mut RalphApp) {
        if let Some(agent) = self.agent {
            app.set_coding_agent(agent.into());
        }
        if let Some(max_iterations) = self.max_iterations {
            app.config_mut().max_iterations = max_iterations;
        }
    }
}

#[derive(Debug, Parser)]
#[command(name = "ralph", about = ROOT_ABOUT, long_about = ROOT_LONG_ABOUT)]
struct Cli {
    #[arg(long, global = true, value_name = "PATH")]
    project_dir: Option<Utf8PathBuf>,
    #[arg(long, global = true, value_enum, default_value_t = OutputArg::Text)]
    output: OutputArg,
    #[arg(value_name = "TARGET")]
    target: Option<String>,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Clone, Subcommand)]
enum Commands {
    #[command(about = "Create a new target")]
    New(NewArgs),
    #[command(about = "Run a selected prompt loop for a target")]
    Run(RunArgs),
    #[command(about = "List targets", visible_alias = "status")]
    Ls,
    #[command(about = "Show target files")]
    Show(ShowArgs),
    #[command(about = "Edit a target prompt")]
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
struct NewArgs {
    #[arg(value_name = "TARGET")]
    target: String,
    #[arg(long, value_enum, default_value_t = ScaffoldArg::Playbook)]
    scaffold: ScaffoldArg,
    #[arg(long, action = clap::ArgAction::SetTrue)]
    edit: bool,
    #[arg(long, value_name = "FILE")]
    prompt: Option<String>,
}

#[derive(Debug, Clone, Args)]
struct RunArgs {
    #[arg(value_name = "TARGET")]
    target: String,
    #[arg(long, value_name = "FILE")]
    prompt: Option<String>,
    #[command(flatten)]
    runtime: RuntimeArgs,
}

#[derive(Debug, Clone, Args)]
struct ShowArgs {
    #[arg(value_name = "TARGET")]
    target: String,
    #[arg(long, value_name = "FILE")]
    file: Option<String>,
}

#[derive(Debug, Clone, Args)]
struct EditArgs {
    #[arg(value_name = "TARGET")]
    target: String,
    #[arg(long, value_name = "FILE")]
    prompt: Option<String>,
}

#[derive(Debug, Clone, Subcommand)]
enum AgentCommands {
    #[command(about = "List supported agents and whether they are detected on PATH")]
    List,
    #[command(about = "Show the effective agent")]
    Current,
    #[command(about = "Persist a supported agent into user or project config")]
    Set(AgentSetArgs),
}

#[derive(Debug, Clone, Args)]
struct AgentSetArgs {
    #[arg(value_enum)]
    agent: AgentArg,
    #[arg(long, value_enum, default_value_t = WritableConfigScopeArg::Project)]
    scope: WritableConfigScopeArg,
}

#[derive(Debug, Clone, Subcommand)]
enum ConfigCommands {
    #[command(about = "Render user, project, or effective config")]
    Show(ConfigShowArgs),
    #[command(about = "Show config file paths")]
    Path,
}

#[derive(Debug, Clone, Args)]
struct ConfigShowArgs {
    #[arg(long, value_enum, default_value_t = ConfigViewArg::Effective)]
    scope: ConfigViewArg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ConfigViewArg {
    User,
    Project,
    Effective,
}

#[derive(Debug, Clone, Args)]
struct InitArgs {
    #[arg(long, value_enum)]
    agent: Option<AgentArg>,
    #[arg(long, value_name = "CMD")]
    editor: Option<String>,
    #[arg(long, value_name = "N")]
    max_iterations: Option<usize>,
    #[arg(long, action = clap::ArgAction::SetTrue)]
    force: bool,
}

#[derive(Debug, Serialize)]
struct AgentListRow {
    agent: String,
    detected: bool,
    command: String,
}

#[derive(Debug, Serialize)]
struct AgentCurrentRow {
    effective_agent: String,
    project_dir: String,
}

#[derive(Debug, Serialize)]
struct TargetListRow {
    target: String,
    last_prompt: Option<String>,
    last_run_status: String,
    prompts: Vec<String>,
    scaffold: Option<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    if let Err(error) = try_main().await {
        eprintln!("{error:#}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

async fn try_main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    let project_dir = resolve_project_dir(cli.project_dir)?;

    match (cli.command, cli.target) {
        (None, None) => run_tui(RalphApp::load(project_dir)?),
        (None, Some(target)) => run_tui_scoped(RalphApp::load(project_dir)?, &target),
        (Some(command), _) => run_command(project_dir, cli.output, command).await,
    }
}

async fn run_command(project_dir: Utf8PathBuf, output: OutputArg, command: Commands) -> Result<()> {
    match command {
        Commands::New(args) => {
            let app = RalphApp::load(project_dir)?;
            let summary = app.create_target(&args.target, Some(args.scaffold.into()))?;
            if args.edit {
                let prompt = match args.prompt.as_deref() {
                    Some(name) => Some(name),
                    None if args.scaffold == ScaffoldArg::Playbook => Some("playbook_plan.md"),
                    None => None,
                };
                app.edit_prompt(&args.target, prompt)?;
            }
            print_summary(output, &summary)
        }
        Commands::Run(args) => {
            let mut app = RalphApp::load(project_dir)?;
            args.runtime.apply_to(&mut app);
            let mut delegate = ConsoleDelegate;
            let summary = app
                .run_target(&args.target, args.prompt.as_deref(), &mut delegate)
                .await?;
            print_summary(output, &summary)
        }
        Commands::Ls => {
            let app = RalphApp::load(project_dir)?;
            let rows = app
                .list_targets()?
                .into_iter()
                .map(target_row)
                .collect::<Vec<_>>();
            print_json_or_text(output, &rows, render_targets_text(&rows))
        }
        Commands::Show(args) => {
            let app = RalphApp::load(project_dir)?;
            let review = app.review_target(&args.target)?;
            print_show(output, &review, args.file.as_deref())
        }
        Commands::Edit(args) => {
            let app = RalphApp::load(project_dir)?;
            app.edit_prompt(&args.target, args.prompt.as_deref())
        }
        Commands::Agent(command) => run_agent_command(project_dir, output, command),
        Commands::Config(command) => run_config_command(project_dir, output, command),
        Commands::Init(args) => run_init(project_dir, args),
        Commands::Doctor => run_doctor(project_dir),
    }
}

fn run_agent_command(
    project_dir: Utf8PathBuf,
    output: OutputArg,
    command: AgentCommands,
) -> Result<()> {
    match command {
        AgentCommands::List => {
            let detected = CodingAgent::detected();
            let rows = [
                CodingAgent::Opencode,
                CodingAgent::Codex,
                CodingAgent::Raijin,
            ]
            .into_iter()
            .map(|agent| AgentListRow {
                agent: agent.label().to_owned(),
                detected: detected.contains(&agent),
                command: AppConfig::default().runner.for_agent_fallback(agent),
            })
            .collect::<Vec<_>>();
            let text = rows
                .iter()
                .map(|row| {
                    format!(
                        "{:<9} detected={} command={}",
                        row.agent, row.detected, row.command
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            print_json_or_text(output, &rows, text)
        }
        AgentCommands::Current => {
            let app = RalphApp::load(project_dir.clone())?;
            let row = AgentCurrentRow {
                effective_agent: app.coding_agent().label().to_owned(),
                project_dir: project_dir.to_string(),
            };
            let text = format!(
                "effective_agent={}\nproject_dir={}",
                row.effective_agent, row.project_dir
            );
            print_json_or_text(output, &row, text)
        }
        AgentCommands::Set(args) => AppConfig::persist_scoped_coding_agent(
            &project_dir,
            args.scope.into(),
            args.agent.into(),
        ),
    }
}

fn run_config_command(
    project_dir: Utf8PathBuf,
    output: OutputArg,
    command: ConfigCommands,
) -> Result<()> {
    match command {
        ConfigCommands::Show(args) => {
            let app = RalphApp::load(project_dir.clone())?;
            let raw = match args.scope {
                ConfigViewArg::User => {
                    AppConfig::scoped_config_toml(&project_dir, ConfigFileScope::User)?
                        .unwrap_or_else(|| "<missing>".to_owned())
                }
                ConfigViewArg::Project => {
                    AppConfig::scoped_config_toml(&project_dir, ConfigFileScope::Project)?
                        .unwrap_or_else(|| "<missing>".to_owned())
                }
                ConfigViewArg::Effective => app.config().effective_toml()?,
            };
            println!("{raw}");
            Ok(())
        }
        ConfigCommands::Path => {
            let user = AppConfig::user_config_path()?.map(|path| path.to_string());
            let project = AppConfig::project_config_path(&project_dir).to_string();
            let text = format!(
                "user={}\nproject={}",
                user.clone().unwrap_or_else(|| "<unavailable>".to_owned()),
                project
            );
            print_json_or_text(
                output,
                &serde_json::json!({ "user": user, "project": project }),
                text,
            )
        }
    }
}

fn run_init(project_dir: Utf8PathBuf, args: InitArgs) -> Result<()> {
    let path = AppConfig::project_config_path(&project_dir);
    if path.exists() && !args.force {
        return Err(anyhow!(
            "config already exists at {}; use --force to overwrite",
            path
        ));
    }

    let mut config = AppConfig::default();
    if let Some(agent) = args.agent {
        config.set_coding_agent(agent.into());
    }
    if let Some(editor) = args.editor {
        config.editor_override = Some(editor);
    }
    if let Some(max_iterations) = args.max_iterations {
        config.max_iterations = max_iterations;
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create config parent for {path}"))?;
    }
    fs::write(&path, toml::to_string_pretty(&config)?)
        .with_context(|| format!("failed to write config at {path}"))?;
    println!("{path}");
    Ok(())
}

fn run_doctor(project_dir: Utf8PathBuf) -> Result<()> {
    AppConfig::validate_scoped_config(&project_dir, ConfigFileScope::User)?;
    AppConfig::validate_scoped_config(&project_dir, ConfigFileScope::Project)?;
    let app = RalphApp::load(project_dir.clone())?;
    fs::create_dir_all(project_dir.join(".ralph"))
        .with_context(|| format!("failed to write under {}", project_dir))?;

    let editor = app
        .config()
        .editor_override
        .clone()
        .or_else(|| env::var("VISUAL").ok())
        .or_else(|| env::var("EDITOR").ok())
        .unwrap_or_else(|| "vi".to_owned());

    resolve_command(&editor).with_context(|| format!("editor command '{editor}' was not found"))?;

    let detected = CodingAgent::detected();
    if detected.is_empty() {
        println!("doctor: no supported agents detected on PATH");
    } else {
        println!(
            "doctor: detected agents: {}",
            detected
                .iter()
                .map(|agent| agent.label())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    println!("doctor: ok");
    Ok(())
}

fn print_summary(output: OutputArg, summary: &TargetSummary) -> Result<()> {
    let row = target_row(summary.clone());
    let text = render_targets_text(&[row]);
    print_json_or_text(output, summary, text)
}

fn print_show(output: OutputArg, review: &TargetReview, selected_file: Option<&str>) -> Result<()> {
    if let Some(file_name) = selected_file {
        let file = review
            .files
            .iter()
            .find(|file| file.name == file_name)
            .ok_or_else(|| {
                anyhow!(
                    "file '{file_name}' not found for target '{}'",
                    review.summary.id
                )
            })?;
        if matches!(output, OutputArg::Json) {
            println!("{}", serde_json::to_string_pretty(file)?);
        } else {
            println!("{}", file.contents);
        }
        return Ok(());
    }

    if matches!(output, OutputArg::Json) {
        println!("{}", serde_json::to_string_pretty(review)?);
        return Ok(());
    }

    for (index, file) in review.files.iter().enumerate() {
        if index > 0 {
            println!();
        }
        println!("## {}", file.name);
        println!("{}", file.contents);
    }
    Ok(())
}

fn print_json_or_text<T>(output: OutputArg, value: &T, text: String) -> Result<()>
where
    T: Serialize,
{
    match output {
        OutputArg::Text => {
            println!("{text}");
            Ok(())
        }
        OutputArg::Json => {
            println!("{}", serde_json::to_string_pretty(value)?);
            Ok(())
        }
    }
}

fn target_row(summary: TargetSummary) -> TargetListRow {
    TargetListRow {
        target: summary.id,
        last_prompt: summary.last_prompt,
        last_run_status: summary.last_run_status.label().to_owned(),
        prompts: summary
            .prompt_files
            .into_iter()
            .map(|prompt| prompt.name)
            .collect(),
        scaffold: summary
            .scaffold
            .map(|scaffold| scaffold.as_str().to_owned()),
    }
}

fn render_targets_text(rows: &[TargetListRow]) -> String {
    if rows.is_empty() {
        return "No targets.".to_owned();
    }
    rows.iter()
        .map(|row| {
            format!(
                "{} [{}] prompts={}{}",
                row.target,
                row.last_run_status,
                row.prompts.join(", "),
                row.scaffold
                    .as_ref()
                    .map(|scaffold| format!(" scaffold={scaffold}"))
                    .unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn resolve_project_dir(project_dir: Option<Utf8PathBuf>) -> Result<Utf8PathBuf> {
    match project_dir {
        Some(path) => Ok(path),
        None => Utf8PathBuf::from_path_buf(env::current_dir().context("failed to read cwd")?)
            .map_err(|_| anyhow!("current directory is not valid UTF-8")),
    }
}

fn resolve_command(command: &str) -> Result<()> {
    if command.contains(std::path::MAIN_SEPARATOR) || command.contains('/') {
        let path = Path::new(command);
        if path.is_file() {
            return Ok(());
        }
        return Err(anyhow!("command path does not exist"));
    }

    let path = env::var_os("PATH").ok_or_else(|| anyhow!("PATH is not set"))?;
    for directory in env::split_paths(&path) {
        if directory.join(command).is_file() {
            return Ok(());
        }
        if cfg!(windows) {
            for ext in [".exe", ".cmd", ".bat"] {
                if directory.join(format!("{command}{ext}")).is_file() {
                    return Ok(());
                }
            }
        }
    }
    Err(anyhow!("command not found on PATH"))
}

fn init_tracing() {
    let _ = fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .try_init();
}

trait RunnerConfigCommandExt {
    fn for_agent_fallback(self, agent: CodingAgent) -> String;
}

impl RunnerConfigCommandExt for ralph_core::RunnerConfig {
    fn for_agent_fallback(self, agent: CodingAgent) -> String {
        let config = ralph_core::RunnerConfig::for_agent(agent);
        let mut pieces = vec![config.program];
        pieces.extend(config.args);
        pieces.join(" ")
    }
}
