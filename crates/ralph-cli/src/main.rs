mod cli;
mod output;

use std::{
    collections::BTreeMap,
    env, fs,
    io::{self, IsTerminal, Read},
    process::{Command, ExitCode},
};

use crate::{
    cli::{
        Cli, Commands, ConfigMutationArgs, ConfigShowArgs, ConfigViewArg, EditArgs, GetArgs,
        GuidedArgs, PayloadArgs, PlanShortcutArgs, RequestArgs, RunArgs, RuntimeArgs, ShowArgs,
        SignalArgs, render_workflow_help,
    },
    output::{
        CliRunHeader, print_run_header, print_workflow_definition, print_workflow_list,
        print_workflow_run,
    },
};
use anyhow::{Context, Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};
use ralph_app::{
    ConsoleDelegate, RalphApp, WorkflowRequestInput, WorkflowRunInput, edit_file, prompt_nonempty,
    prompt_yes_no,
};
use ralph_core::{
    AgentEventRecord, AppConfig, ConfigFileScope, HOST_CHANNEL_ID, LastRunStatus, MAIN_CHANNEL_ID,
    PLANNING_PLAN_FILE_EVENT, RUNTIME_DIR_NAME, agent_events_wal_path,
    append_agent_event_to_wal_path, current_unix_timestamp_ms,
    latest_agent_event_body_from_wal_in_channel, validate_agent_event,
};
use tracing_subscriber::{EnvFilter, fmt};

const SPECIAL_WORKFLOW_PLAN_PLACEHOLDER: &str = "<unavailable, ignore>";

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
    apply_config_mutations(&project_dir, &cli.config_mutations)?;

    match cli.command {
        Some(command) => run_command(project_dir, command).await,
        None => Ok(()),
    }
}

async fn run_command(project_dir: Utf8PathBuf, command: Commands) -> Result<()> {
    match command {
        Commands::Guided(args) => run_guided_command(project_dir, args).await,
        Commands::TasksOnly(args) => run_tasks_only(project_dir, args).await,
        Commands::ReviewOnly(args) => run_review_only(project_dir, args).await,
        Commands::FinalizeOnly(args) => run_finalize_only(project_dir, args).await,
        Commands::Workflow(args) => run_workflow_command(project_dir, args).await,
        Commands::Signal(args) => run_signal(args),
        Commands::Payload(args) => run_payload(args),
        Commands::Get(args) => run_get(args),
        Commands::Workflows => {
            let app = RalphApp::load(project_dir)?;
            print_workflow_list(app.list_workflows()?);
            Ok(())
        }
        Commands::ShowWorkflow(args) => run_show_workflow(project_dir, args),
        Commands::EditWorkflow(args) => run_edit_workflow(project_dir, args),
        Commands::ShowConfig(args) => run_show_config(project_dir, args),
    }
}

async fn run_guided_command(project_dir: Utf8PathBuf, args: GuidedArgs) -> Result<()> {
    let plan_summary = run_workflow_with_input(
        project_dir.clone(),
        &args.runtime,
        "plan",
        WorkflowRunInput {
            request: resolve_cli_request_input(&args.request_args, Some("Plan description: "))?,
            options: BTreeMap::new(),
        },
    )
    .await?;

    if plan_summary.status != LastRunStatus::Completed || !args.build_after_plan {
        return Ok(());
    }

    let Some(plan_file) = planning_plan_file(&plan_summary)? else {
        return Ok(());
    };

    if prompt_yes_no(&format!("Build this plan now? [{}] ", plan_file), false)? {
        let task_summary = run_special_workflow(
            project_dir.clone(),
            &args.runtime,
            "task",
            Some(plan_file.clone()),
        )
        .await?;
        if task_summary.status == LastRunStatus::Completed {
            let _ =
                run_special_workflow(project_dir, &args.runtime, "review", Some(plan_file)).await?;
        }
    }

    Ok(())
}

async fn run_tasks_only(project_dir: Utf8PathBuf, args: PlanShortcutArgs) -> Result<()> {
    run_special_workflow(project_dir, &args.runtime, "task", Some(args.plan_file))
        .await
        .map(|_| ())
}

async fn run_review_only(
    project_dir: Utf8PathBuf,
    args: cli::OptionalPlanShortcutArgs,
) -> Result<()> {
    run_special_workflow(project_dir, &args.runtime, "review", args.plan_file)
        .await
        .map(|_| ())
}

async fn run_finalize_only(
    project_dir: Utf8PathBuf,
    args: cli::OptionalPlanShortcutArgs,
) -> Result<()> {
    run_special_workflow(project_dir, &args.runtime, "finalize", args.plan_file)
        .await
        .map(|_| ())
}

async fn run_special_workflow(
    project_dir: Utf8PathBuf,
    runtime: &RuntimeArgs,
    workflow_id: &str,
    plan_file: Option<String>,
) -> Result<ralph_core::WorkflowRunSummary> {
    let plan_file = resolve_special_workflow_plan_file(&project_dir, workflow_id, plan_file)?;
    let options = plan_file
        .into_iter()
        .map(|plan_file| ("plan-file".to_owned(), plan_file))
        .collect();
    run_workflow_with_input(
        project_dir,
        runtime,
        workflow_id,
        WorkflowRunInput {
            options,
            ..Default::default()
        },
    )
    .await
}

async fn run_workflow_command(project_dir: Utf8PathBuf, args: RunArgs) -> Result<()> {
    run_cli_workflow(project_dir, &args)
        .await
        .map(|_| ())
        .map_err(|error| maybe_with_run_help(&args.workflow, error))
}

async fn run_cli_workflow(
    project_dir: Utf8PathBuf,
    args: &cli::RunArgs,
) -> Result<ralph_core::WorkflowRunSummary> {
    let input = resolve_workflow_run_input(args)?;
    run_workflow_with_input(project_dir, &args.runtime, &args.workflow, input).await
}

async fn run_workflow_with_input(
    project_dir: Utf8PathBuf,
    runtime: &RuntimeArgs,
    workflow_id: &str,
    input: WorkflowRunInput,
) -> Result<ralph_core::WorkflowRunSummary> {
    let mut app = RalphApp::load(project_dir)?;
    runtime.apply_to(&mut app)?;
    let workflow = app.load_workflow(workflow_id)?;
    let request_preview = resolve_request_preview(app.project_dir(), &workflow, &input.request)?;
    let agent = app
        .config()
        .agent_definition(app.agent_id())
        .ok_or_else(|| anyhow!("agent '{}' is not defined", app.agent_id()))?;
    print_run_header(
        &app.config().theme,
        &CliRunHeader {
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
                .join(workflow_id)
                .to_string(),
        },
    );
    let mut delegate = ConsoleDelegate::new(&app.config().theme);
    let summary = app.run_workflow(workflow_id, input, &mut delegate).await?;
    print_workflow_run(&app.config().theme, &summary);
    Ok(summary)
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
    match render_workflow_help(workflow_id) {
        Ok(help) => anyhow!("{}\n\n{}", message.trim_end(), help.trim_end()),
        Err(_) => anyhow!("{message}"),
    }
}

fn is_run_usage_error(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}");
    [
        "provide the workflow request in exactly one runtime form",
        "does not accept argv requests",
        "does not accept stdin requests",
        "does not accept --file",
        "requires a request via argv, stdin, or --file",
        "requires option '--",
        "failed to read request file ",
        "failed to read workflow request file ",
        "agent '",
    ]
    .iter()
    .any(|pattern| message.contains(pattern))
}

fn apply_config_mutations(project_dir: &Utf8Path, mutations: &ConfigMutationArgs) -> Result<()> {
    if let Some(agent) = &mutations.set_user_agent {
        AppConfig::persist_scoped_coding_agent(project_dir, ConfigFileScope::User, agent)?;
    }
    if let Some(agent) = &mutations.set_project_agent {
        AppConfig::persist_scoped_coding_agent(project_dir, ConfigFileScope::Project, agent)?;
    }
    Ok(())
}

fn run_show_workflow(project_dir: Utf8PathBuf, args: ShowArgs) -> Result<()> {
    let app = RalphApp::load(project_dir)?;
    let workflow = app.load_workflow(&args.workflow_id)?;
    print_workflow_definition(&workflow)
}

fn run_edit_workflow(project_dir: Utf8PathBuf, args: EditArgs) -> Result<()> {
    let app = RalphApp::load(project_dir)?;
    let path = app.resolve_workflow_edit_path(&args.workflow_id)?;
    edit_file(&path, app.config().editor_override.as_deref())
}

fn run_show_config(project_dir: Utf8PathBuf, args: ConfigShowArgs) -> Result<()> {
    let app = RalphApp::load(project_dir.clone())?;
    let raw = match args.scope {
        ConfigViewArg::User => AppConfig::scoped_config_toml(&project_dir, ConfigFileScope::User)?
            .unwrap_or_else(|| "<missing>".to_owned()),
        ConfigViewArg::Project => {
            AppConfig::scoped_config_toml(&project_dir, ConfigFileScope::Project)?
                .unwrap_or_else(|| "<missing>".to_owned())
        }
        ConfigViewArg::Effective => app.config().effective_toml()?,
    };
    println!("{raw}");
    Ok(())
}

#[derive(Debug, Clone)]
struct EmitEventContext {
    wal_path: Utf8PathBuf,
    run_dir: Utf8PathBuf,
    run_id: String,
    channel_id: String,
    project_dir: Utf8PathBuf,
    prompt_path: Utf8PathBuf,
    prompt_name: String,
}

impl EmitEventContext {
    fn from_env() -> Result<Self> {
        let wal_path = env_utf8_path("RALPH_WAL_PATH")?;
        let run_dir = run_dir_from_wal_path(&wal_path)?;
        let run_id = run_dir
            .file_name()
            .filter(|name| !name.is_empty())
            .map(ToOwned::to_owned)
            .ok_or_else(|| anyhow!("could not determine run id from RALPH_WAL_PATH"))?;
        let channel_id = required_env("RALPH_CHANNEL_ID")?;
        let channel_id = channel_id.trim();
        if channel_id.is_empty() {
            return Err(anyhow!(
                "RALPH_CHANNEL_ID cannot be empty; this command only works inside a Ralph agent run"
            ));
        }
        let project_dir = project_dir_from_run_dir(&run_dir)?;
        let prompt_path = env_utf8_path("RALPH_PROMPT_PATH")?;
        let prompt_name = prompt_path
            .file_stem()
            .filter(|name| !name.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| "workflow".to_owned());

        Ok(Self {
            wal_path,
            run_dir,
            run_id,
            channel_id: channel_id.to_owned(),
            project_dir,
            prompt_path,
            prompt_name,
        })
    }
}

fn run_signal(args: SignalArgs) -> Result<()> {
    let context = EmitEventContext::from_env()?;
    append_event(&context, &args.event, "")?;
    Ok(())
}

fn run_payload(args: PayloadArgs) -> Result<()> {
    let context = EmitEventContext::from_env()?;
    append_event(&context, &args.event, &args.body)?;
    Ok(())
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

fn append_event(context: &EmitEventContext, event: &str, body: &str) -> Result<()> {
    let event = event.trim();
    if event.is_empty() {
        return Err(anyhow!("event name cannot be empty"));
    }
    if context.channel_id != MAIN_CHANNEL_ID && event.starts_with("loop-") {
        return Err(anyhow!(
            "parallel worker '{}' cannot emit loop-control event '{}'",
            context.channel_id,
            event
        ));
    }

    validate_agent_event(event, body, Some(&context.prompt_path))?;
    append_agent_event_to_wal_path(
        &context.wal_path,
        &AgentEventRecord {
            v: 1,
            ts_unix_ms: current_unix_timestamp_ms(),
            run_id: context.run_id.clone(),
            channel_id: context.channel_id.clone(),
            event: event.to_owned(),
            body: body.to_owned(),
            project_dir: context.project_dir.clone(),
            run_dir: context.run_dir.clone(),
            prompt_path: context.prompt_path.clone(),
            prompt_name: context.prompt_name.clone(),
            pid: std::process::id(),
        },
    )?;
    Ok(())
}

fn run_dir_from_wal_path(wal_path: &Utf8Path) -> Result<Utf8PathBuf> {
    let runtime_dir = wal_path
        .parent()
        .ok_or_else(|| anyhow!("invalid RALPH_WAL_PATH; missing runtime directory"))?;
    if runtime_dir.file_name() != Some(RUNTIME_DIR_NAME) {
        return Err(anyhow!(
            "invalid RALPH_WAL_PATH; expected parent directory '{}'",
            RUNTIME_DIR_NAME
        ));
    }
    runtime_dir
        .parent()
        .map(Utf8Path::to_path_buf)
        .ok_or_else(|| anyhow!("invalid RALPH_WAL_PATH; missing run directory"))
}

fn project_dir_from_run_dir(run_dir: &Utf8Path) -> Result<Utf8PathBuf> {
    let workflow_dir = run_dir
        .parent()
        .ok_or_else(|| anyhow!("invalid Ralph run directory; missing workflow directory"))?;
    let runs_dir = workflow_dir
        .parent()
        .ok_or_else(|| anyhow!("invalid Ralph run directory; missing runs directory"))?;
    if runs_dir.file_name() != Some("runs") {
        return Err(anyhow!(
            "invalid Ralph run directory; expected parent directory 'runs'"
        ));
    }
    let ralph_dir = runs_dir
        .parent()
        .ok_or_else(|| anyhow!("invalid Ralph run directory; missing .ralph directory"))?;
    if ralph_dir.file_name() != Some(".ralph") {
        return Err(anyhow!(
            "invalid Ralph run directory; expected parent directory '.ralph'"
        ));
    }
    ralph_dir
        .parent()
        .map(Utf8Path::to_path_buf)
        .ok_or_else(|| anyhow!("invalid Ralph run directory; missing project directory"))
}

fn planning_plan_file(summary: &ralph_core::WorkflowRunSummary) -> Result<Option<String>> {
    let wal_path = agent_events_wal_path(&summary.run_dir);
    latest_agent_event_body_from_wal_in_channel(
        &wal_path,
        PLANNING_PLAN_FILE_EVENT,
        Some(HOST_CHANNEL_ID),
    )
}

fn resolve_special_workflow_plan_file(
    project_dir: &Utf8Path,
    workflow_id: &str,
    plan_file: Option<String>,
) -> Result<Option<String>> {
    if plan_file.is_some() || !matches!(workflow_id, "review" | "finalize") {
        return Ok(plan_file);
    }

    latest_planning_plan_file(project_dir)?.map_or_else(
        // Review/finalize prompts always interpolate `{ralph-option:plan-file}`.
        // When the shortcut is used without a plan, keep the workflow runnable by
        // injecting a sentinel string that tells the agent to ignore the missing plan.
        || Ok(Some(SPECIAL_WORKFLOW_PLAN_PLACEHOLDER.to_owned())),
        |plan_file| Ok(Some(plan_file)),
    )
}

fn latest_planning_plan_file(project_dir: &Utf8Path) -> Result<Option<String>> {
    let plan_runs_dir = project_dir.join(".ralph").join("runs").join("plan");
    if !plan_runs_dir.exists() {
        return Ok(None);
    }

    let mut run_dirs = fs::read_dir(plan_runs_dir.as_std_path())
        .with_context(|| format!("failed to read {}", plan_runs_dir))?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| Utf8PathBuf::from_path_buf(entry.path()).ok())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    run_dirs.sort_by_key(|path| planning_run_timestamp(path));
    run_dirs.reverse();

    for run_dir in run_dirs {
        let wal_path = agent_events_wal_path(&run_dir);
        if let Some(plan_file) = latest_agent_event_body_from_wal_in_channel(
            &wal_path,
            PLANNING_PLAN_FILE_EVENT,
            Some(HOST_CHANNEL_ID),
        )? {
            return Ok(Some(plan_file));
        }
    }

    Ok(None)
}

fn planning_run_timestamp(run_dir: &Utf8Path) -> u64 {
    run_dir
        .file_name()
        .and_then(|name| name.rsplit_once('-'))
        .and_then(|(_, timestamp)| timestamp.parse::<u64>().ok())
        .unwrap_or(0)
}

fn ensure_interactive_terminal(context: &str) -> Result<()> {
    if io::stdin().is_terminal() {
        return Ok(());
    }

    Err(anyhow!("{context} requires an interactive terminal"))
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
        request: resolve_cli_request_input(&args.request_args, None)?,
        options: args.workflow_options.clone(),
    })
}

fn resolve_cli_request_input(
    request_args: &RequestArgs,
    prompt_if_missing: Option<&str>,
) -> Result<WorkflowRequestInput> {
    resolve_cli_request_input_with_stdin(request_args, prompt_if_missing, read_stdin_if_piped()?)
}

fn resolve_cli_request_input_with_stdin(
    request_args: &RequestArgs,
    prompt_if_missing: Option<&str>,
    piped_stdin: Option<String>,
) -> Result<WorkflowRequestInput> {
    let mut request_input = WorkflowRequestInput {
        argv: request_args.argv_text(),
        stdin: piped_stdin,
        request_file: request_args.request_file.clone(),
    };

    if request_input.clone().into_source()?.is_some() {
        return Ok(request_input);
    }

    let Some(prompt) = prompt_if_missing else {
        return Ok(request_input);
    };

    ensure_interactive_terminal("guided planning")?;
    request_input.argv = Some(prompt_nonempty(prompt)?);
    Ok(request_input)
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
    fn append_event_writes_to_the_explicit_wal_path() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().join("project")).unwrap();
        let run_dir = project_dir.join(".ralph/runs/fixture-flow/run-1");
        let wal_path = ralph_core::agent_events_wal_path(&run_dir);
        fs::create_dir_all(run_dir.join(RUNTIME_DIR_NAME).as_std_path()).unwrap();

        let context = EmitEventContext {
            wal_path: wal_path.clone(),
            run_dir: run_dir.clone(),
            run_id: "run-1".to_owned(),
            channel_id: MAIN_CHANNEL_ID.to_owned(),
            project_dir: project_dir.clone(),
            prompt_path: Utf8PathBuf::from("/tmp/workflow.yml"),
            prompt_name: "fixture-flow".to_owned(),
        };

        append_event(&context, "handoff", "ready").unwrap();

        let wal = ralph_core::read_agent_events_since_path(&wal_path, 0).unwrap();
        assert_eq!(wal.records.len(), 1);
        assert_eq!(wal.records[0].event, "handoff");
        assert_eq!(wal.records[0].body, "ready");
        assert_eq!(wal.records[0].run_id, "run-1");
        assert_eq!(wal.records[0].channel_id, MAIN_CHANNEL_ID);
        assert_eq!(wal.records[0].project_dir, project_dir);
        assert_eq!(wal.records[0].run_dir, run_dir);
    }

    #[test]
    fn append_event_rejects_loop_control_from_parallel_channels() {
        let context = EmitEventContext {
            wal_path: Utf8PathBuf::from("/tmp/agent-events.wal.ndjson"),
            run_dir: Utf8PathBuf::from("/tmp/project/.ralph/runs/fixture-flow/run-1"),
            run_id: "run-1".to_owned(),
            channel_id: "QT".to_owned(),
            project_dir: Utf8PathBuf::from("/tmp/project"),
            prompt_path: Utf8PathBuf::from("/tmp/workflow.yml"),
            prompt_name: "fixture-flow".to_owned(),
        };

        let error = append_event(&context, "loop-stop:ok", "done")
            .unwrap_err()
            .to_string();
        assert!(error.contains("parallel worker 'QT' cannot emit loop-control event"));
    }

    #[test]
    fn run_usage_errors_include_workflow_help() {
        with_test_workflow_home(|| {
            let error = maybe_with_run_help(
                "fixture-flow",
                anyhow!("provide the workflow request in exactly one runtime form"),
            );
            let rendered = format!("{error:#}");

            assert!(rendered.contains("provide the workflow request in exactly one runtime form"));
            assert!(rendered.contains("Usage:"));
            assert!(rendered.contains("ralph w fixture-flow"));
            assert!(rendered.contains("--statefile"));
        });
    }

    #[test]
    fn guided_request_resolution_preserves_stdin_as_stdin() {
        let request_input = resolve_cli_request_input_with_stdin(
            &RequestArgs::default(),
            Some("Plan description: "),
            Some("ship auth".to_owned()),
        )
        .unwrap();

        assert!(request_input.argv.is_none());
        assert_eq!(request_input.stdin.as_deref(), Some("ship auth"));
    }

    #[test]
    fn guided_request_resolution_rejects_multiple_sources() {
        let error = resolve_cli_request_input_with_stdin(
            &RequestArgs {
                request_file: Some(Utf8PathBuf::from("REQ.md")),
                ..Default::default()
            },
            Some("Plan description: "),
            Some("ship auth".to_owned()),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("exactly one runtime form"));
    }

    #[test]
    fn request_resolution_preserves_argv_source() {
        let input = resolve_cli_request_input_with_stdin(
            &RequestArgs {
                request: vec!["ship auth".to_owned()],
                ..Default::default()
            },
            None,
            None,
        )
        .unwrap();

        assert_eq!(input.argv.as_deref(), Some("ship auth"));
        assert!(input.stdin.is_none());
    }

    #[test]
    fn review_shortcut_resolves_the_latest_planning_plan_file() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().join("project")).unwrap();
        let older_run_dir = project_dir.join(".ralph/runs/plan/42-100");
        let newer_run_dir = project_dir.join(".ralph/runs/plan/42-200");
        fs::create_dir_all(older_run_dir.as_std_path()).unwrap();
        fs::create_dir_all(newer_run_dir.as_std_path()).unwrap();

        for (run_dir, plan_file) in [
            (&older_run_dir, "docs/plans/older.md"),
            (&newer_run_dir, "docs/plans/newer.md"),
        ] {
            ralph_core::append_agent_event(
                run_dir,
                &ralph_core::AgentEventRecord {
                    v: 1,
                    ts_unix_ms: 1,
                    run_id: run_dir.file_name().unwrap().to_owned(),
                    channel_id: HOST_CHANNEL_ID.to_owned(),
                    event: PLANNING_PLAN_FILE_EVENT.to_owned(),
                    body: plan_file.to_owned(),
                    project_dir: project_dir.clone(),
                    run_dir: run_dir.to_path_buf(),
                    prompt_path: Utf8PathBuf::from("/tmp/plan.yml"),
                    prompt_name: "plan".to_owned(),
                    pid: 1,
                },
            )
            .unwrap();
        }

        assert_eq!(
            resolve_special_workflow_plan_file(&project_dir, "review", None)
                .unwrap()
                .as_deref(),
            Some("docs/plans/newer.md")
        );
    }

    #[test]
    fn review_shortcut_requires_a_plan_when_none_can_be_resolved() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().join("project")).unwrap();
        fs::create_dir_all(project_dir.as_std_path()).unwrap();

        assert_eq!(
            resolve_special_workflow_plan_file(&project_dir, "review", None)
                .unwrap()
                .as_deref(),
            Some(SPECIAL_WORKFLOW_PLAN_PLACEHOLDER)
        );
    }

    #[test]
    fn finalize_shortcut_uses_placeholder_when_no_plan_can_be_resolved() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().join("project")).unwrap();
        fs::create_dir_all(project_dir.as_std_path()).unwrap();

        assert_eq!(
            resolve_special_workflow_plan_file(&project_dir, "finalize", None)
                .unwrap()
                .as_deref(),
            Some(SPECIAL_WORKFLOW_PLAN_PLACEHOLDER)
        );
    }
}
