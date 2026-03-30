use std::{
    env,
    io::{self, IsTerminal, Read},
};

use anyhow::{Context, Result, anyhow};
use camino::Utf8PathBuf;
use clap::{Parser, Subcommand};
use ralph_app::{ConsoleDelegate, RalphApp};
use ralph_core::RunControl;
use ralph_tui::{run_tui, run_tui_scoped};
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Debug, Parser)]
#[command(
    name = "ralph",
    about = "Durable planning and execution workflow for repositories"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Tui,
    List,
    Review {
        target: String,
    },
    Run {
        target: String,
    },
    Revise {
        target: String,
        #[arg(long)]
        prompt: Option<String>,
    },
    Edit {
        target: String,
    },
    Create {
        #[arg(long)]
        prompt: Option<String>,
    },
    Replan {
        target: String,
        #[arg(long)]
        prompt: Option<String>,
    },
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
    let project_dir = current_project_dir()?;
    let app = RalphApp::load(project_dir)?;

    match cli.command {
        None => {
            if io::stdin().is_terminal() && io::stdout().is_terminal() {
                run_tui(app)?;
            } else {
                return Err(anyhow!("interactive TUI requires a TTY"));
            }
        }
        Some(Commands::Tui) => run_tui(app)?,
        Some(Commands::List) => {
            for spec in app.list_specs()? {
                println!(
                    "{}\n  state: {:?}\n  progress: {}\n",
                    spec.spec_path, spec.state, spec.progress_path
                );
            }
        }
        Some(Commands::Review { target }) => {
            let review = app.review_target(&target)?;
            println!("spec: {}", review.spec_path);
            println!("progress: {}", review.progress_path);
            println!("state: {:?}\n", review.state);
            println!("--- spec ---\n{}", review.spec_contents);
            println!("--- progress ---\n{}", review.progress_contents);
        }
        Some(Commands::Run { target }) => {
            let control = install_ctrl_c_handler();
            let mut delegate = ConsoleDelegate;
            app.run_target_with_control(&target, control, &mut delegate)
                .await?;
        }
        Some(Commands::Revise { target, prompt }) => {
            let prompt = planning_request(prompt)?;
            let control = install_ctrl_c_handler();
            let mut delegate = ConsoleDelegate;
            app.revise_target_with_control(&target, &prompt, control, &mut delegate)
                .await?;
        }
        Some(Commands::Edit { target }) => app.edit_target(&target)?,
        Some(Commands::Create { prompt }) => {
            let prompt = planning_request(prompt)?;
            let control = install_ctrl_c_handler();
            let mut delegate = ConsoleDelegate;
            app.create_new_with_control(&prompt, control, &mut delegate)
                .await?;
        }
        Some(Commands::Replan { target, prompt }) => {
            let prompt = planning_request(prompt)?;
            let control = install_ctrl_c_handler();
            let mut delegate = ConsoleDelegate;
            app.replan_target_with_control(&target, &prompt, control, &mut delegate)
                .await?;
        }
    }

    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = fmt().with_env_filter(filter).with_target(false).try_init();
}

fn current_project_dir() -> Result<Utf8PathBuf> {
    let cwd = env::current_dir().context("failed to get current directory")?;
    Utf8PathBuf::from_path_buf(cwd).map_err(|_| anyhow!("current directory is not valid UTF-8"))
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
        "tui" | "list" | "review" | "run" | "revise" | "edit" | "create" | "replan"
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

fn planning_request(prompt: Option<String>) -> Result<String> {
    if let Some(prompt) = prompt {
        if prompt.trim().is_empty() {
            return Err(anyhow!("planning request cannot be empty"));
        }
        return Ok(prompt);
    }

    if !io::stdin().is_terminal() {
        let mut buffer = String::new();
        io::stdin()
            .read_to_string(&mut buffer)
            .context("failed to read planning request from stdin")?;
        if buffer.trim().is_empty() {
            return Err(anyhow!("planning request cannot be empty"));
        }
        return Ok(buffer);
    }

    println!("Enter the planning request, then press Ctrl-D:");
    let mut buffer = String::new();
    io::stdin()
        .read_to_string(&mut buffer)
        .context("failed to read planning request from stdin")?;
    if buffer.trim().is_empty() {
        return Err(anyhow!("planning request cannot be empty"));
    }
    Ok(buffer)
}
