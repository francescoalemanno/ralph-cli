mod cli;
mod output;

use std::{
    env, fs,
    io::{self, IsTerminal, Read},
    process::{Command, ExitCode},
};

use crate::{
    cli::{
        AgentCommands, Cli, Commands, ConfigCommands, ConfigViewArg, GetArgs, InitArgs,
        RequestArgs, render_run_workflow_help,
    },
    output::{
        AgentCurrentRow, CliRunHeader, agent_list_rows, print_agent_current, print_agent_list,
        print_run_header, print_workflow_definition, print_workflow_list, print_workflow_run,
    },
};
use anyhow::{Context, Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};
use ralph_app::{ConsoleDelegate, RalphApp, WorkflowRequestInput, WorkflowRunInput};
use ralph_core::{
    AppConfig, ConfigFileScope, atomic_write, latest_agent_event_body_from_wal_in_channel,
    load_workflow, seed_builtin_workflows_if_missing,
};
use ralph_tui::{
    TuiLaunchOptions, TuiPreloadedRequest, TuiRequestSource, edit_file, run_tui_with_options,
};
use serde::Serialize;
use tracing_subscriber::{EnvFilter, fmt};

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
    let project_dir = resolve_project_dir(cli.project_dir.clone())?;

    run_command(project_dir, cli.command).await
}

fn build_tui_launch_options(
    project_dir: &Utf8Path,
    args: &cli::RunArgs,
) -> Result<TuiLaunchOptions> {
    let workflow = load_workflow(&args.workflow)
        .with_context(|| format!("failed to load workflow '{}'", args.workflow))?;
    let argv = args.request_args.argv_text();
    let provided = args.request_args.provided_count();
    if workflow.uses_request_token() && provided == 0 {
        return Err(anyhow!(
            "opening the runner TUI requires both a workflow and a request; use `ralph run <workflow-id> \"your request\"` or `ralph run <workflow-id> --file REQ.md`"
        ));
    }
    if provided > 1 {
        return Err(anyhow!(
            "opening the runner TUI accepts at most one preloaded request source; use argv text or `--file`, not both"
        ));
    }

    let preloaded_request = match (argv, args.request_args.request_file.clone()) {
        (Some(text), None) => Some(TuiPreloadedRequest {
            source: TuiRequestSource::Argv,
            text,
            file_path: None,
        }),
        (None, Some(path)) => {
            let resolved = resolve_project_relative_path(project_dir, &path);
            let text = fs::read_to_string(resolved.as_std_path())
                .with_context(|| format!("failed to read request file {}", resolved))?;
            Some(TuiPreloadedRequest {
                source: TuiRequestSource::File,
                text,
                file_path: Some(path),
            })
        }
        (None, None) => None,
        _ => {
            return Err(anyhow!(
                "opening the runner TUI accepts at most one preloaded request source; use argv text or `--file`, not both"
            ));
        }
    };

    Ok(TuiLaunchOptions {
        preset_workflow: Some(args.workflow.clone()),
        preloaded_request,
        workflow_options: args.workflow_options.clone(),
    })
}

async fn run_command(project_dir: Utf8PathBuf, command: Commands) -> Result<()> {
    match command {
        Commands::Run(args) => run_run_command(project_dir, args).await,
        Commands::Get(args) => run_get(args),
        Commands::Ls => {
            let app = RalphApp::load(project_dir)?;
            print_workflow_list(app.list_workflows()?);
            Ok(())
        }
        Commands::Show(args) => {
            let app = RalphApp::load(project_dir)?;
            let workflow = app.load_workflow(&args.workflow_id)?;
            print_workflow_definition(&workflow)
        }
        Commands::Edit(args) => {
            let app = RalphApp::load(project_dir)?;
            let path = app.resolve_workflow_edit_path(&args.workflow_id)?;
            edit_file(&path, app.config().editor_override.as_deref(), &app.config().theme)
        }
        Commands::Agent(command) => run_agent_command(project_dir, command),
        Commands::Config(command) => run_config_command(project_dir, command),
        Commands::Init(args) => run_init(project_dir, args),
        Commands::Doctor => run_doctor(project_dir),
    }
}

async fn run_run_command(project_dir: Utf8PathBuf, args: cli::RunArgs) -> Result<()> {
    let result = if args.cli {
        run_cli_workflow(project_dir, &args).await
    } else if !io::stdin().is_terminal() {
        Err(anyhow!(
            "stdin preloading is not supported in TUI mode; use `ralph run --cli <workflow-id>` or pass the request as argv text or `--file`"
        ))
    } else {
        run_tui_workflow(project_dir, &args)
    };

    result.map_err(|error| maybe_with_run_help(&args.workflow, error))
}

async fn run_cli_workflow(project_dir: Utf8PathBuf, args: &cli::RunArgs) -> Result<()> {
    let mut app = RalphApp::load(project_dir)?;
    let input = resolve_workflow_run_input(args)?;
    args.runtime.apply_to(&mut app)?;
    let workflow = app.load_workflow(&args.workflow)?;
    let request_preview = resolve_request_preview(app.project_dir(), &workflow, &input.request)?;
    let agent = app
        .config()
        .agent_definition(app.agent_id())
        .ok_or_else(|| anyhow!("agent '{}' is not defined", app.agent_id()))?;
    print_run_header(&app.config().theme, &CliRunHeader {
        version: env!("CARGO_PKG_VERSION"),
        workflow_id: workflow.workflow_id.clone(),
        workflow_title: workflow.title.clone(),
        entrypoint: workflow.entrypoint.clone(),
        agent: format!("{} ({})", app.agent_name(), app.agent_id()),
        runner: agent.runner.command_preview(),
        project_dir: app.project_dir().to_string(),
        branch: git_branch(app.project_dir()),
        request_source: describe_request_source(&workflow, &input.request),
        request_preview,
        max_iterations: app.config().max_iterations,
        session_timeout_secs: agent.runner.session_timeout_secs,
        idle_timeout_secs: agent.runner.idle_timeout_secs,
        workflow_options: input
            .options
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
        artifact_root: app
            .project_dir()
            .join(".ralph")
            .join("runs")
            .join(&args.workflow)
            .to_string(),
    });
    let mut delegate = ConsoleDelegate::default();
    let summary = app
        .run_workflow(&args.workflow, input, &mut delegate)
        .await?;
    print_workflow_run(&app.config().theme, &summary);
    Ok(())
}

fn run_tui_workflow(project_dir: Utf8PathBuf, args: &cli::RunArgs) -> Result<()> {
    let launch = build_tui_launch_options(&project_dir, args)?;
    let mut app = RalphApp::load(project_dir)?;
    args.runtime.apply_to(&mut app)?;
    run_tui_with_options(app, launch)
}

fn maybe_with_run_help(workflow_id: &str, error: anyhow::Error) -> anyhow::Error {
    if is_run_usage_error(&error) {
        with_run_help(workflow_id, error)
    } else {
        error
    }
}

fn with_run_help(workflow_id: &str, error: anyhow::Error) -> anyhow::Error {
    let message = format!("{error:#}");
    match render_run_workflow_help(workflow_id) {
        Ok(help) => anyhow!("{}\n\n{}", message.trim_end(), help.trim_end()),
        Err(_) => anyhow!("{message}"),
    }
}

fn is_run_usage_error(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}");
    [
        "opening the runner TUI requires both a workflow and a request",
        "opening the runner TUI accepts at most one preloaded request source",
        "stdin preloading is not supported in TUI mode",
        "provide the workflow request in exactly one runtime form",
        "does not accept argv requests",
        "does not accept stdin requests",
        "does not accept --request-file",
        "requires a request via argv, stdin, or --request-file",
        "requires option '--",
        "failed to read request file ",
        "failed to read workflow request file ",
        "agent '",
    ]
    .iter()
    .any(|pattern| message.contains(pattern))
}

fn run_agent_command(project_dir: Utf8PathBuf, command: AgentCommands) -> Result<()> {
    match command {
        AgentCommands::List => {
            let app = RalphApp::load(project_dir)?;
            let rows = agent_list_rows(app.all_agents());
            print_agent_list(&rows);
            Ok(())
        }
        AgentCommands::Current => {
            let app = RalphApp::load(project_dir.clone())?;
            let row = AgentCurrentRow {
                effective_agent: format!("{} ({})", app.agent_name(), app.agent_id()),
                project_dir: project_dir.to_string(),
            };
            print_agent_current(&row);
            Ok(())
        }
        AgentCommands::Set(args) => {
            AppConfig::persist_scoped_coding_agent(&project_dir, args.scope.into(), &args.agent)
        }
    }
}

fn run_config_command(project_dir: Utf8PathBuf, command: ConfigCommands) -> Result<()> {
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
            println!(
                "user={}\nproject={}",
                user.unwrap_or_else(|| "<unavailable>".to_owned()),
                project
            );
            Ok(())
        }
    }
}

fn run_get(args: GetArgs) -> Result<()> {
    let event = args.event.trim().to_owned();
    if event.is_empty() {
        return Err(anyhow!("event name cannot be empty"));
    }

    let wal_path = env_utf8_path("RALPH_WAL_PATH")?;
    let body =
        latest_agent_event_body_from_wal_in_channel(&wal_path, &event, args.channel.as_deref())?
            .ok_or_else(|| anyhow!("no event with name {event}"))?;
    println!("{body}");
    Ok(())
}

fn required_env(key: &str) -> Result<String> {
    env::var(key)
        .map_err(|_| anyhow!("missing {key}; this command only works inside a Ralph agent run"))
}

fn env_utf8_path(key: &str) -> Result<Utf8PathBuf> {
    #[cfg(test)]
    if let Some(path) = test_support::env_path_override(key) {
        return Ok(path);
    }

    Ok(Utf8PathBuf::from(required_env(key)?))
}

fn resolve_workflow_run_input(args: &cli::RunArgs) -> Result<WorkflowRunInput> {
    Ok(WorkflowRunInput {
        request: build_request_input(&args.request_args)?,
        options: args.workflow_options.clone(),
    })
}

fn build_request_input(request_args: &RequestArgs) -> Result<WorkflowRequestInput> {
    let stdin = read_stdin_if_piped()?;

    Ok(WorkflowRequestInput {
        argv: request_args.argv_text(),
        stdin,
        request_file: request_args.request_file.clone(),
    })
}

fn read_stdin_if_piped() -> Result<Option<String>> {
    if io::stdin().is_terminal() {
        return Ok(None);
    }

    let mut buffer = String::new();
    io::stdin()
        .read_to_string(&mut buffer)
        .context("failed to read stdin request")?;
    Ok(Some(buffer))
}

fn resolve_project_relative_path(project_dir: &Utf8Path, path: &Utf8Path) -> Utf8PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_dir.join(path)
    }
}

fn describe_request_source(
    workflow: &ralph_core::WorkflowDefinition,
    request: &WorkflowRequestInput,
) -> String {
    if request.argv.is_some() {
        return "argv".to_owned();
    }
    if request.stdin.is_some() {
        return "stdin".to_owned();
    }
    if let Some(path) = &request.request_file {
        return format!("file {}", path);
    }
    match workflow.request.as_ref() {
        Some(definition) if definition.inline.is_some() => "workflow inline".to_owned(),
        Some(definition) => definition
            .file
            .as_ref()
            .map(|file| format!("workflow file {}", file.path))
            .unwrap_or_else(|| "none".to_owned()),
        None => "none".to_owned(),
    }
}

fn resolve_request_preview(
    project_dir: &Utf8Path,
    workflow: &ralph_core::WorkflowDefinition,
    request: &WorkflowRequestInput,
) -> Result<Option<String>> {
    if let Some(argv) = &request.argv {
        return Ok(Some(argv.clone()));
    }
    if let Some(stdin) = &request.stdin {
        return Ok(Some(stdin.clone()));
    }
    if let Some(path) = &request.request_file {
        let resolved = resolve_project_relative_path(project_dir, path);
        let contents = fs::read_to_string(resolved.as_std_path())
            .with_context(|| format!("failed to read request file {}", resolved))?;
        return Ok(Some(contents));
    }

    let Some(definition) = workflow.request.as_ref() else {
        return Ok(None);
    };
    if let Some(inline) = &definition.inline {
        return Ok(Some(inline.clone()));
    }
    if let Some(file) = &definition.file {
        let resolved = resolve_project_relative_path(project_dir, &file.path);
        let contents = fs::read_to_string(resolved.as_std_path())
            .with_context(|| format!("failed to read workflow request file {}", resolved))?;
        return Ok(Some(contents));
    }
    Ok(None)
}

fn git_branch(project_dir: &Utf8Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(project_dir.as_str())
        .arg("rev-parse")
        .arg("--abbrev-ref")
        .arg("HEAD")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if branch.is_empty() {
        None
    } else {
        Some(branch)
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
    seed_builtin_workflows_if_missing()?;
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
pub(crate) mod test_support {
    use std::{
        fs,
        sync::{Mutex, OnceLock, RwLock},
    };

    use camino::Utf8PathBuf;
    use ralph_core::ScopedGlobalConfigDirOverride;

    fn wal_path_override() -> &'static RwLock<Option<Utf8PathBuf>> {
        static WAL_PATH_OVERRIDE: OnceLock<RwLock<Option<Utf8PathBuf>>> = OnceLock::new();
        WAL_PATH_OVERRIDE.get_or_init(|| RwLock::new(None))
    }

    fn wal_path_override_lock() -> &'static Mutex<()> {
        static WAL_PATH_OVERRIDE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        WAL_PATH_OVERRIDE_LOCK.get_or_init(|| Mutex::new(()))
    }

    pub(crate) fn env_path_override(key: &str) -> Option<Utf8PathBuf> {
        match key {
            "RALPH_WAL_PATH" => wal_path_override()
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
            _ => None,
        }
    }

    pub(crate) struct ScopedWalPathOverride {
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl ScopedWalPathOverride {
        pub(crate) fn new(path: Utf8PathBuf) -> Self {
            let guard = wal_path_override_lock()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *wal_path_override()
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(path);
            Self { _guard: guard }
        }
    }

    impl Drop for ScopedWalPathOverride {
        fn drop(&mut self) {
            *wal_path_override()
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        }
    }

    pub(crate) fn with_test_workflow_home(test: impl FnOnce()) {
        let temp = tempfile::tempdir().unwrap();
        let config_home = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let _config_home: ScopedGlobalConfigDirOverride =
            ralph_core::scoped_global_config_dir_override(config_home.clone());
        fs::create_dir_all(config_home.join("workflows").as_std_path()).unwrap();
        fs::write(
            config_home.join("workflows/fixture-flow.yml").as_std_path(),
            r#"
version: 1
workflow_id: fixture-flow
title: Fixture Flow
entrypoint: main
options:
  state-file:
    default: state.txt
request:
  runtime:
    argv: true
    file_flag: true
prompts:
  main:
    title: Main
    fallback-route: no-route-error
    prompt: |
      state={ralph-option:state-file}
      request={ralph-request}
"#,
        )
        .unwrap();
        test();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{RunArgs, RuntimeArgs};
    use crate::test_support::{ScopedWalPathOverride, with_test_workflow_home};
    use std::fs;

    #[test]
    fn get_returns_latest_event_body_from_wal_path() {
        let temp = tempfile::tempdir().unwrap();
        let run_dir = Utf8PathBuf::from_path_buf(temp.path().join("run")).unwrap();
        fs::create_dir_all(run_dir.as_std_path()).unwrap();
        ralph_core::append_agent_event(
            &run_dir,
            &ralph_core::AgentEventRecord {
                v: 1,
                ts_unix_ms: 1,
                run_id: "run-1".to_owned(),
                channel_id: ralph_core::MAIN_CHANNEL_ID.to_owned(),
                event: "handoff".to_owned(),
                body: "first".to_owned(),
                project_dir: Utf8PathBuf::from("/tmp/project"),
                run_dir: run_dir.clone(),
                prompt_path: Utf8PathBuf::from("/tmp/workflow.yml"),
                prompt_name: "alpha".to_owned(),
                pid: 1,
            },
        )
        .unwrap();
        ralph_core::append_agent_event(
            &run_dir,
            &ralph_core::AgentEventRecord {
                v: 1,
                ts_unix_ms: 2,
                run_id: "run-1".to_owned(),
                channel_id: ralph_core::MAIN_CHANNEL_ID.to_owned(),
                event: "handoff".to_owned(),
                body: "second".to_owned(),
                project_dir: Utf8PathBuf::from("/tmp/project"),
                run_dir: run_dir.clone(),
                prompt_path: Utf8PathBuf::from("/tmp/workflow.yml"),
                prompt_name: "beta".to_owned(),
                pid: 1,
            },
        )
        .unwrap();

        let _wal_path = ScopedWalPathOverride::new(ralph_core::agent_events_wal_path(&run_dir));
        let result = latest_agent_event_body_from_wal_in_channel(
            &ralph_core::agent_events_wal_path(&run_dir),
            "handoff",
            None,
        )
        .unwrap();

        assert_eq!(result.as_deref(), Some("second"));
    }

    #[test]
    fn tui_launch_options_require_request_for_workflows_using_request_token() {
        with_test_workflow_home(|| {
            let project_dir = Utf8Path::new("/tmp/project");
            let error = build_tui_launch_options(
                project_dir,
                &RunArgs {
                    cli: false,
                    runtime: RuntimeArgs::default(),
                    workflow: "fixture-flow".to_owned(),
                    workflow_options: Default::default(),
                    request_args: RequestArgs::default(),
                },
            )
            .unwrap_err()
            .to_string();

            assert!(
                error.contains("opening the runner TUI requires both a workflow and a request")
            );
            assert!(error.contains("ralph run <workflow-id>"));
        });
    }

    #[test]
    fn tui_launch_options_allow_missing_request_for_workflows_without_request_token() {
        with_test_workflow_home(|| {
            let project_dir = Utf8Path::new("/tmp/project");
            let launch = build_tui_launch_options(
                project_dir,
                &RunArgs {
                    cli: false,
                    runtime: RuntimeArgs::default(),
                    workflow: "test-workflow".to_owned(),
                    workflow_options: Default::default(),
                    request_args: RequestArgs::default(),
                },
            )
            .unwrap();

            assert_eq!(launch.preset_workflow.as_deref(), Some("test-workflow"));
            assert!(launch.preloaded_request.is_none());
        });
    }

    #[test]
    fn tui_launch_options_preserve_positional_workflow_and_argv_request() {
        with_test_workflow_home(|| {
            let project_dir = Utf8Path::new("/tmp/project");
            let launch = build_tui_launch_options(
                project_dir,
                &RunArgs {
                    cli: false,
                    runtime: RuntimeArgs::default(),
                    workflow: "fixture-flow".to_owned(),
                    workflow_options: Default::default(),
                    request_args: RequestArgs {
                        request_file: None,
                        request: vec!["fix".to_owned(), "tests".to_owned()],
                    },
                },
            )
            .unwrap();

            assert_eq!(launch.preset_workflow.as_deref(), Some("fixture-flow"));
            let preload = launch.preloaded_request.expect("preloaded request");
            assert_eq!(preload.source, TuiRequestSource::Argv);
            assert_eq!(preload.text, "fix tests");
            assert!(preload.file_path.is_none());
        });
    }

    #[test]
    fn run_usage_errors_include_workflow_help() {
        with_test_workflow_home(|| {
            let error = maybe_with_run_help(
                "fixture-flow",
                anyhow!(
                    "opening the runner TUI requires both a workflow and a request; use `ralph run <workflow-id> \"your request\"` or `ralph run <workflow-id> --file REQ.md`"
                ),
            );
            let rendered = format!("{error:#}");

            assert!(
                rendered.contains("opening the runner TUI requires both a workflow and a request")
            );
            assert!(rendered.contains("Usage:"));
            assert!(rendered.contains("ralph run fixture-flow"));
            assert!(rendered.contains("--statefile"));
        });
    }
}
