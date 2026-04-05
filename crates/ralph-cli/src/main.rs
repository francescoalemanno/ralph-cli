mod cli;
mod fake_agent;
mod output;

use std::{env, fs, process::ExitCode};

use crate::{
    cli::{AgentCommands, Cli, Commands, ConfigCommands, ConfigViewArg, InitArgs, OutputArg},
    output::{
        AgentCurrentRow, agent_list_rows, print_agent_current, print_agent_list, print_bare_file,
        print_json_or_text, print_prompt_file_row, print_target_list, print_target_review,
        print_target_summary,
    },
};
use anyhow::{Context, Result, anyhow};
use camino::Utf8PathBuf;
use clap::Parser;
use ralph_app::{ConsoleDelegate, RalphApp};
use ralph_core::{AppConfig, ConfigFileScope, ScaffoldId, atomic_write, bare_prompt_template};
use ralph_tui::{edit_file, run_tui, run_tui_scoped};
use serde::Serialize;
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Debug, Clone, PartialEq, Eq)]
enum TargetMode<'a> {
    Target(&'a str),
    BarePrompt(Utf8PathBuf),
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
            match resolve_target_mode(args.target.as_deref(), args.prompt.as_deref())? {
                TargetMode::Target(target) => {
                    let scaffold: ScaffoldId = args.scaffold.into();
                    let summary = app.create_target(target, Some(scaffold))?;
                    if args.edit {
                        let prompt_path = summary
                            .prompt_files
                            .first()
                            .map(|prompt| prompt.path.clone())
                            .ok_or_else(|| {
                                anyhow!("target '{}' has no runnable prompt files", summary.id)
                            })?;
                        edit_file(&prompt_path, app.config().editor_override.as_deref())?;
                    }
                    print_target_summary(output, &summary)
                }
                TargetMode::BarePrompt(prompt_path) => {
                    let scaffold: ScaffoldId = args.scaffold.into();
                    create_bare_prompt_file(&prompt_path, scaffold)?;
                    if args.edit {
                        edit_file(&prompt_path, app.config().editor_override.as_deref())?;
                    }
                    print_prompt_file_row(
                        output,
                        prompt_path.to_string(),
                        Some(scaffold.as_str().to_owned()),
                        None,
                    )
                }
            }
        }
        Commands::Run(args) => {
            let mut app = RalphApp::load(project_dir)?;
            args.runtime.apply_to(&mut app)?;
            let mut delegate = ConsoleDelegate;
            match resolve_target_mode(args.target.as_deref(), args.prompt.as_deref())? {
                TargetMode::Target(target) => {
                    let summary = app
                        .run_target_with_control(
                            target,
                            args.prompt.as_deref(),
                            ralph_core::RunControl::new(),
                            &mut delegate,
                        )
                        .await?;
                    print_target_summary(output, &summary)
                }
                TargetMode::BarePrompt(prompt_path) => {
                    let status = app.run_prompt_file(&prompt_path, &mut delegate).await?;
                    print_prompt_file_row(
                        output,
                        prompt_path.to_string(),
                        None,
                        Some(status.label().to_owned()),
                    )
                }
            }
        }
        Commands::FakeAgent(args) => fake_agent::run(args.command),
        Commands::Ls => {
            let app = RalphApp::load(project_dir)?;
            print_target_list(output, app.list_targets()?)
        }
        Commands::Show(args) => {
            match resolve_target_mode(args.target.as_deref(), args.prompt.as_deref())? {
                TargetMode::Target(target) => {
                    let app = RalphApp::load(project_dir)?;
                    let review = app.review_target(target)?;
                    print_target_review(output, &review, args.file.as_deref())
                }
                TargetMode::BarePrompt(prompt_path) => print_bare_file(output, &prompt_path),
            }
        }
        Commands::Edit(args) => {
            let app = RalphApp::load(project_dir)?;
            match resolve_target_mode(args.target.as_deref(), args.prompt.as_deref())? {
                TargetMode::Target(target) => {
                    let prompt_path =
                        app.resolve_target_edit_path(target, args.prompt.as_deref())?;
                    edit_file(&prompt_path, app.config().editor_override.as_deref())
                }
                TargetMode::BarePrompt(prompt_path) => {
                    edit_file(&prompt_path, app.config().editor_override.as_deref())
                }
            }
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
            let app = RalphApp::load(project_dir)?;
            let rows = agent_list_rows(app.all_agents());
            print_agent_list(output, &rows)
        }
        AgentCommands::Current => {
            let app = RalphApp::load(project_dir.clone())?;
            let row = AgentCurrentRow {
                effective_agent: format!("{} ({})", app.agent_name(), app.agent_id()),
                project_dir: project_dir.to_string(),
            };
            print_agent_current(output, &row)
        }
        AgentCommands::Set(args) => {
            AppConfig::persist_scoped_coding_agent(&project_dir, args.scope.into(), &args.agent)
        }
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

    let config = AppConfig::load(&project_dir)?;
    if let Some(agent) = args.agent.as_deref()
        && config.agent_definition(agent).is_none()
    {
        return Err(anyhow!("agent '{}' is not defined", agent));
    }

    #[derive(Serialize)]
    struct ProjectConfigFile {
        #[serde(skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        editor_override: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_iterations: Option<usize>,
    }

    let project_config = ProjectConfigFile {
        agent: args.agent,
        editor_override: args.editor,
        max_iterations: args.max_iterations,
    };

    atomic_write(&path, toml::to_string_pretty(&project_config)?)
        .with_context(|| format!("failed to write config at {path}"))?;
    println!("{path}");
    Ok(())
}

fn run_doctor(project_dir: Utf8PathBuf) -> Result<()> {
    AppConfig::validate_scoped_config(&project_dir, ConfigFileScope::User)?;
    AppConfig::validate_scoped_config(&project_dir, ConfigFileScope::Project)?;
    fs::create_dir_all(project_dir.join(".ralph"))
        .with_context(|| format!("failed to write under {}", project_dir))?;

    let app = RalphApp::load(project_dir)?;
    let available = app.available_agents();
    if available.is_empty() {
        println!("doctor: no supported agents detected on PATH");
    } else {
        println!(
            "doctor: detected agents: {}",
            available
                .iter()
                .map(|agent| agent.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    println!("doctor: ok");
    Ok(())
}

fn resolve_target_mode<'a>(
    target: Option<&'a str>,
    prompt: Option<&str>,
) -> Result<TargetMode<'a>> {
    match target {
        Some(target) => Ok(TargetMode::Target(target)),
        None => resolve_bare_prompt_path(prompt).map(TargetMode::BarePrompt),
    }
}

fn resolve_bare_prompt_path(prompt: Option<&str>) -> Result<Utf8PathBuf> {
    let prompt =
        prompt.ok_or_else(|| anyhow!("requires --prompt <file> when TARGET is omitted"))?;
    let path = Utf8PathBuf::from(prompt);
    if path.is_absolute() {
        return Ok(path);
    }
    let cwd = Utf8PathBuf::from_path_buf(env::current_dir().context("failed to read cwd")?)
        .map_err(|_| anyhow!("current directory is not valid UTF-8"))?;
    Ok(cwd.join(path))
}

fn create_bare_prompt_file(path: &camino::Utf8Path, scaffold: ScaffoldId) -> Result<()> {
    if path.exists() {
        return Err(anyhow!("prompt file already exists at {}", path));
    }
    atomic_write(path, bare_prompt_template(scaffold))
        .with_context(|| format!("failed to write prompt file {}", path))
}

fn resolve_project_dir(project_dir: Option<Utf8PathBuf>) -> Result<Utf8PathBuf> {
    match project_dir {
        Some(path) => Ok(path),
        None => Utf8PathBuf::from_path_buf(env::current_dir().context("failed to read cwd")?)
            .map_err(|_| anyhow!("current directory is not valid UTF-8")),
    }
}

fn init_tracing() {
    let _ = fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_mode_prefers_target_when_present() {
        let mode = resolve_target_mode(Some("demo"), Some("ignored.md")).unwrap();
        assert_eq!(mode, TargetMode::Target("demo"));
    }

    #[test]
    fn target_mode_requires_prompt_for_bare_mode() {
        let error = resolve_target_mode(None, None).unwrap_err().to_string();
        assert_eq!(error, "requires --prompt <file> when TARGET is omitted");
    }

    #[test]
    fn bare_prompt_paths_are_resolved_from_cwd() {
        let cwd = Utf8PathBuf::from_path_buf(env::current_dir().unwrap()).unwrap();
        let resolved = resolve_bare_prompt_path(Some("notes/prompt.md")).unwrap();
        assert_eq!(resolved, cwd.join("notes/prompt.md"));
    }
}
