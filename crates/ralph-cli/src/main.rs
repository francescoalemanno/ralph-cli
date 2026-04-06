mod cli;
mod output;

use std::{
    env, fs,
    io::{self, IsTerminal, Read},
    process::ExitCode,
};

use crate::{
    cli::{
        AgentCommands, Cli, Commands, ConfigCommands, ConfigViewArg, EmitArgs, InitArgs,
        RequestArgs, RuntimeArgs,
    },
    output::{
        AgentCurrentRow, agent_list_rows, print_agent_current, print_agent_list,
        print_emitted_event, print_workflow_definition, print_workflow_list, print_workflow_run,
    },
};
use anyhow::{Context, Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};
use clap::Parser;
use ralph_app::{ConsoleDelegate, RalphApp, WorkflowRequestInput};
use ralph_core::{
    AgentEventRecord, AppConfig, ConfigFileScope, append_agent_event, atomic_write,
    load_workflow_from_path, seed_builtin_workflows_if_missing,
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

    run_command(project_dir, cli.runtime, cli.command).await
}

fn build_tui_launch_options(
    project_dir: &Utf8Path,
    args: &cli::RunArgs,
) -> Result<TuiLaunchOptions> {
    let argv = args.request_args.argv_text();
    let provided = args.request_args.provided_count();
    if provided != 1 {
        return Err(anyhow!(
            "opening the runner TUI requires both a workflow and a request; use `ralph run <workflow-id> \"your request\"` or `ralph run <workflow-id> --file REQ.md`"
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
                "opening the runner TUI requires both a workflow and a request; use `ralph run <workflow-id> \"your request\"` or `ralph run <workflow-id> --file REQ.md`"
            ));
        }
    };

    Ok(TuiLaunchOptions {
        preset_workflow: Some(args.workflow.clone()),
        preloaded_request,
    })
}

async fn run_command(
    project_dir: Utf8PathBuf,
    runtime: RuntimeArgs,
    command: Commands,
) -> Result<()> {
    match command {
        Commands::Run(args) => {
            if args.cli {
                let mut app = RalphApp::load(project_dir)?;
                runtime.apply_to(&mut app)?;
                let mut delegate = ConsoleDelegate;
                let summary = app
                    .run_workflow(
                        &args.workflow,
                        resolve_workflow_request_input(&args)?,
                        &mut delegate,
                    )
                    .await?;
                print_workflow_run(&summary);
                Ok(())
            } else {
                if !io::stdin().is_terminal() {
                    return Err(anyhow!(
                        "stdin preloading is not supported in TUI mode; use `ralph run --cli <workflow-id>` or pass the request as argv text or `--file`"
                    ));
                }
                let launch = build_tui_launch_options(&project_dir, &args)?;
                let mut app = RalphApp::load(project_dir)?;
                runtime.apply_to(&mut app)?;
                run_tui_with_options(app, launch)
            }
        }
        Commands::Emit(args) => run_emit(args),
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
            edit_file(&path, app.config().editor_override.as_deref())
        }
        Commands::Agent(command) => run_agent_command(project_dir, command),
        Commands::Config(command) => run_config_command(project_dir, command),
        Commands::Init(args) => run_init(project_dir, args),
        Commands::Doctor => run_doctor(project_dir),
    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct EmitContext {
    run_id: String,
    project_dir: Utf8PathBuf,
    run_dir: Utf8PathBuf,
    prompt_path: Utf8PathBuf,
    prompt_name: String,
}

fn run_emit(args: EmitArgs) -> Result<()> {
    let event = args.event.trim().to_owned();
    if event.is_empty() {
        return Err(anyhow!("event name cannot be empty"));
    }

    let body = args.body.join(" ");
    let context = emit_context_from_env()?;
    validate_emit_args(&event, &body, &context)?;
    let wal_run_dir = context.run_dir.clone();

    append_agent_event(
        &wal_run_dir,
        &AgentEventRecord {
            v: 1,
            ts_unix_ms: current_unix_timestamp_ms(),
            run_id: context.run_id,
            event: event.clone(),
            body,
            project_dir: context.project_dir,
            run_dir: context.run_dir,
            prompt_path: context.prompt_path,
            prompt_name: context.prompt_name,
            pid: std::process::id(),
        },
    )?;

    print_emitted_event(&event);
    Ok(())
}

fn emit_context_from_env() -> Result<EmitContext> {
    Ok(EmitContext {
        run_id: required_env("RALPH_RUN_ID")?,
        project_dir: env_utf8_path("RALPH_PROJECT_DIR")?,
        run_dir: env_utf8_path("RALPH_RUN_DIR")?,
        prompt_path: env_utf8_path("RALPH_PROMPT_PATH")?,
        prompt_name: required_env("RALPH_PROMPT_NAME")?,
    })
}

fn validate_emit_args(event: &str, body: &str, context: &EmitContext) -> Result<()> {
    if !event.starts_with("loop-") {
        return Ok(());
    }

    match event {
        "loop-continue" | "loop-stop:ok" | "loop-stop:error" => Ok(()),
        "loop-route" => validate_loop_route_body(body, context),
        _ => Err(anyhow!(unsupported_loop_event_message(event))),
    }
}

fn validate_loop_route_body(body: &str, context: &EmitContext) -> Result<()> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Err(anyhow!(invalid_route_message(
            trimmed,
            &available_routes(context)?
        )));
    }
    if trimmed.contains('/') || trimmed.contains('\\') {
        return Err(anyhow!(invalid_route_message(
            trimmed,
            &available_routes(context)?
        )));
    }
    let routes = available_routes(context)?;
    if routes.iter().any(|route| route == trimmed) {
        return Ok(());
    }
    Err(anyhow!(invalid_route_message(trimmed, &routes)))
}

fn invalid_route_message(route: &str, routes: &[String]) -> String {
    if routes.is_empty() {
        format!("\"{route}\" is not a valid event body for `loop-route`.\nNo routes are available.")
    } else {
        format!(
            "\"{route}\" is not a valid event body for `loop-route`.\nChoose among the available routes:\n{}",
            routes.join("\n")
        )
    }
}

fn unsupported_loop_event_message(event: &str) -> String {
    format!(
        "`{event}` is not a supported loop event.\nChoose among:\nloop-continue\nloop-stop:ok\nloop-stop:error\nloop-route"
    )
}

fn required_env(key: &str) -> Result<String> {
    env::var(key)
        .map_err(|_| anyhow!("missing {key}; this command only works inside a Ralph agent run"))
}

fn env_utf8_path(key: &str) -> Result<Utf8PathBuf> {
    Ok(Utf8PathBuf::from(required_env(key)?))
}

fn current_unix_timestamp_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn resolve_workflow_request_input(args: &cli::RunArgs) -> Result<WorkflowRequestInput> {
    build_request_input(&args.request_args)
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

fn available_routes(context: &EmitContext) -> Result<Vec<String>> {
    let workflow = load_workflow_from_path(&context.prompt_path)?;
    Ok(workflow
        .prompt_ids()
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>())
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
mod tests {
    use super::*;
    use crate::cli::RunArgs;
    use std::fs;

    #[test]
    fn loop_route_validation_lists_available_routes() {
        let temp = tempfile::tempdir().unwrap();
        let workflow_path = Utf8PathBuf::from_path_buf(temp.path().join("plan-build.yml")).unwrap();
        fs::write(
            workflow_path.as_std_path(),
            r#"
version: 1
workflow_id: plan-build
title: Plan Build
entrypoint: plan
prompts:
  plan:
    title: Plan
    is_interactive: false
    fallback-route: no-route-error
    prompt: hello
  build:
    title: Build
    is_interactive: false
    fallback-route: no-route-error
    prompt: world
"#,
        )
        .unwrap();

        let context = EmitContext {
            run_id: "run-1".to_owned(),
            project_dir: Utf8PathBuf::from("/tmp/project"),
            run_dir: Utf8PathBuf::from("/tmp/project/.ralph/runs/plan-build/run-1"),
            prompt_path: workflow_path,
            prompt_name: "plan".to_owned(),
        };
        let error = validate_loop_route_body("broken", &context)
            .unwrap_err()
            .to_string();

        assert!(error.contains("\"broken\" is not a valid event body for `loop-route`."));
        assert!(error.contains("plan"));
        assert!(error.contains("build"));
    }

    #[test]
    fn loop_route_validation_accepts_yaml_prompt_ids() {
        let temp = tempfile::tempdir().unwrap();
        let run_dir = Utf8PathBuf::from_path_buf(temp.path().join("run")).unwrap();
        fs::create_dir_all(run_dir.as_std_path()).unwrap();
        let workflow_path = Utf8PathBuf::from_path_buf(temp.path().join("plan-build.yml")).unwrap();
        fs::write(
            workflow_path.as_std_path(),
            r#"
version: 1
workflow_id: plan-build
title: Plan Build
entrypoint: plan
prompts:
  plan:
    title: Plan
    is_interactive: false
    fallback-route: no-route-error
    prompt: hello
  build:
    title: Build
    is_interactive: false
    fallback-route: no-route-error
    prompt: world
"#,
        )
        .unwrap();

        let context = EmitContext {
            run_id: "run-1".to_owned(),
            project_dir: Utf8PathBuf::from("/tmp/project"),
            run_dir,
            prompt_path: workflow_path,
            prompt_name: "plan".to_owned(),
        };

        validate_loop_route_body("build", &context).unwrap();
    }

    #[test]
    fn unsupported_loop_events_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let workflow_path = Utf8PathBuf::from_path_buf(temp.path().join("plan-build.yml")).unwrap();
        fs::write(
            workflow_path.as_std_path(),
            r#"
version: 1
workflow_id: plan-build
title: Plan Build
entrypoint: plan
prompts:
  plan:
    title: Plan
    is_interactive: false
    fallback-route: no-route-error
    prompt: hello
"#,
        )
        .unwrap();

        let error = validate_emit_args(
            "loop-pause",
            "later",
            &EmitContext {
                run_id: "run-1".to_owned(),
                project_dir: Utf8PathBuf::from("/tmp/project"),
                run_dir: Utf8PathBuf::from("/tmp/project/.ralph/runs/plan-build/run-1"),
                prompt_path: workflow_path,
                prompt_name: "plan".to_owned(),
            },
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("`loop-pause` is not a supported loop event."));
        assert!(error.contains("loop-continue"));
        assert!(error.contains("loop-route"));
    }

    #[test]
    fn non_loop_events_are_allowed_without_validation() {
        let temp = tempfile::tempdir().unwrap();
        let workflow_path = Utf8PathBuf::from_path_buf(temp.path().join("plan-build.yml")).unwrap();
        fs::write(
            workflow_path.as_std_path(),
            r#"
version: 1
workflow_id: plan-build
title: Plan Build
entrypoint: plan
prompts:
  plan:
    title: Plan
    is_interactive: false
    fallback-route: no-route-error
    prompt: hello
"#,
        )
        .unwrap();

        validate_emit_args(
            "note",
            "free form",
            &EmitContext {
                run_id: "run-1".to_owned(),
                project_dir: Utf8PathBuf::from("/tmp/project"),
                run_dir: Utf8PathBuf::from("/tmp/project/.ralph/runs/plan-build/run-1"),
                prompt_path: workflow_path,
                prompt_name: "plan".to_owned(),
            },
        )
        .unwrap();
    }

    #[test]
    fn tui_launch_options_require_exactly_one_request_form() {
        let project_dir = Utf8Path::new("/tmp/project");
        let error = build_tui_launch_options(
            project_dir,
            &RunArgs {
                cli: false,
                workflow: "task-based".to_owned(),
                request_args: RequestArgs::default(),
            },
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("opening the runner TUI requires both a workflow and a request"));
        assert!(error.contains("ralph run <workflow-id>"));
    }

    #[test]
    fn tui_launch_options_preserve_positional_workflow_and_argv_request() {
        let project_dir = Utf8Path::new("/tmp/project");
        let launch = build_tui_launch_options(
            project_dir,
            &RunArgs {
                cli: false,
                workflow: "task-based".to_owned(),
                request_args: RequestArgs {
                    request_file: None,
                    request: vec!["fix".to_owned(), "tests".to_owned()],
                },
            },
        )
        .unwrap();

        assert_eq!(launch.preset_workflow.as_deref(), Some("task-based"));
        let preload = launch.preloaded_request.expect("preloaded request");
        assert_eq!(preload.source, TuiRequestSource::Argv);
        assert_eq!(preload.text, "fix tests");
        assert!(preload.file_path.is_none());
    }
}
