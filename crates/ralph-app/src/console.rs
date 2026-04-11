use std::{
    io::{self, BufRead, IsTerminal, Write},
    sync::OnceLock,
};

use anyhow::Result;
use async_trait::async_trait;
use termimad::MadSkin;

use crate::{
    PlanningAnswerSource, PlanningDraftDecision, PlanningDraftDecisionKind, PlanningDraftReview,
    PlanningQuestion, PlanningQuestionAnswer, RunDelegate, RunEvent, format_iteration_banner,
};

#[derive(Default)]
pub struct ConsoleDelegate;

#[async_trait]
impl RunDelegate for ConsoleDelegate {
    async fn on_event(&mut self, event: RunEvent) -> Result<()> {
        match event {
            RunEvent::IterationStarted {
                prompt_name,
                iteration,
                max_iterations,
            } => {
                println!();
                println!(
                    "{}",
                    format_iteration_banner(&prompt_name, iteration, max_iterations)
                );
            }
            RunEvent::Output(chunk) => {
                print!("{chunk}");
            }
            RunEvent::ParallelWorkerLaunched { channel_id, label } => {
                println!(
                    "{}",
                    format_parallel_event("queued", &channel_id, &label, None)
                );
            }
            RunEvent::ParallelWorkerStarted { channel_id, label } => {
                println!(
                    "{}",
                    format_parallel_event("running", &channel_id, &label, None)
                );
            }
            RunEvent::ParallelWorkerFinished {
                channel_id,
                label,
                exit_code,
            } => {
                println!(
                    "{}",
                    format_parallel_event("done", &channel_id, &label, Some(exit_code))
                );
            }
            RunEvent::Note(note) => {
                eprintln!("{}", format_note(&note));
            }
            RunEvent::Finished { status, summary } => {
                println!("\n{}", format_finish_line(status.label(), &summary));
            }
        }
        Ok(())
    }

    async fn answer_planning_question(
        &mut self,
        question: &PlanningQuestion,
    ) -> Result<PlanningQuestionAnswer> {
        let mut stdout = io::stdout().lock();
        writeln!(stdout)?;
        writeln!(stdout, "Planner question")?;
        writeln!(stdout, "{}", question.question)?;
        if let Some(context) = &question.context
            && !context.trim().is_empty()
        {
            writeln!(stdout)?;
            writeln!(stdout, "Context: {}", context.trim())?;
        }
        writeln!(stdout)?;
        for (index, option) in question.options.iter().enumerate() {
            writeln!(stdout, "  {}) {}", index + 1, option)?;
        }
        writeln!(
            stdout,
            "  {}) Other (type your own answer)",
            question.options.len() + 1
        )?;
        stdout.flush()?;
        drop(stdout);

        loop {
            let selection = prompt_line(&format!(
                "Enter number (1-{}): ",
                question.options.len() + 1
            ))?;
            let Ok(selected) = selection.parse::<usize>() else {
                eprintln!("invalid selection, enter a number");
                continue;
            };
            if selected == 0 || selected > question.options.len() + 1 {
                eprintln!(
                    "invalid selection, enter a number between 1 and {}",
                    question.options.len() + 1
                );
                continue;
            }
            if selected == question.options.len() + 1 {
                let answer = loop {
                    let line = prompt_line("Enter your answer: ")?;
                    if line.trim().is_empty() {
                        eprintln!("answer cannot be empty");
                        continue;
                    }
                    break line;
                };
                return Ok(PlanningQuestionAnswer {
                    answer,
                    source: PlanningAnswerSource::Custom,
                });
            }
            return Ok(PlanningQuestionAnswer {
                answer: question.options[selected - 1].clone(),
                source: PlanningAnswerSource::Option,
            });
        }
    }

    async fn review_planning_draft(
        &mut self,
        draft: &PlanningDraftReview,
    ) -> Result<PlanningDraftDecision> {
        let mut stdout = io::stdout().lock();
        writeln!(stdout)?;
        writeln!(stdout, "Plan draft")?;
        writeln!(stdout, "Target: {}", draft.target_path)?;
        writeln!(stdout, "--------------------")?;
        render_markdown_draft(&mut stdout, &draft.draft)?;
        writeln!(stdout, "--------------------")?;
        writeln!(stdout)?;
        writeln!(stdout, "  1) Accept")?;
        writeln!(stdout, "  2) Revise")?;
        writeln!(stdout, "  3) Reject")?;
        stdout.flush()?;
        drop(stdout);

        loop {
            match prompt_line("Enter number (1-3): ")?.as_str() {
                "1" => {
                    return Ok(PlanningDraftDecision {
                        kind: PlanningDraftDecisionKind::Accept,
                        feedback: None,
                    });
                }
                "2" => {
                    let feedback = loop {
                        let line = prompt_line("Enter revision feedback: ")?;
                        if line.trim().is_empty() {
                            eprintln!("revision feedback cannot be empty");
                            continue;
                        }
                        break line;
                    };
                    return Ok(PlanningDraftDecision {
                        kind: PlanningDraftDecisionKind::Revise,
                        feedback: Some(feedback),
                    });
                }
                "3" => {
                    return Ok(PlanningDraftDecision {
                        kind: PlanningDraftDecisionKind::Reject,
                        feedback: None,
                    });
                }
                _ => {
                    eprintln!("invalid selection, enter 1, 2, or 3");
                }
            }
        }
    }
}

fn prompt_line(prompt: &str) -> Result<String> {
    let mut stdout = io::stdout().lock();
    write!(stdout, "{prompt}")?;
    stdout.flush()?;
    drop(stdout);

    let stdin = io::stdin();
    let mut input = String::new();
    stdin.lock().read_line(&mut input)?;
    Ok(input.trim().to_owned())
}

fn render_markdown_draft(stdout: &mut impl Write, markdown: &str) -> Result<()> {
    if io::stdout().is_terminal() {
        write!(stdout, "{}", planning_draft_skin().term_text(markdown))?;
        if !markdown.ends_with('\n') {
            writeln!(stdout)?;
        }
        return Ok(());
    }

    writeln!(stdout, "{markdown}")?;
    if !markdown.ends_with('\n') {
        writeln!(stdout)?;
    }
    Ok(())
}

fn planning_draft_skin() -> &'static MadSkin {
    static SKIN: OnceLock<MadSkin> = OnceLock::new();
    SKIN.get_or_init(MadSkin::default)
}

fn format_parallel_event(
    kind: &str,
    channel_id: &str,
    label: &str,
    exit_code: Option<i32>,
) -> String {
    const ANSI_CYAN: &str = "\x1b[36m";
    const ANSI_GREEN: &str = "\x1b[32m";
    const ANSI_RED: &str = "\x1b[31m";
    const ANSI_DIM: &str = "\x1b[2m";
    const ANSI_RESET: &str = "\x1b[0m";

    let color = match (kind, exit_code) {
        ("done", Some(0)) => ANSI_GREEN,
        ("done", Some(_)) => ANSI_RED,
        ("running", _) => ANSI_CYAN,
        _ => ANSI_DIM,
    };

    match exit_code {
        Some(exit_code) => {
            format!("{color}[parallel:{channel_id}] {kind} {label} (exit={exit_code}){ANSI_RESET}")
        }
        None => format!("{color}[parallel:{channel_id}] {kind} {label}{ANSI_RESET}"),
    }
}

fn format_note(note: &str) -> String {
    const ANSI_YELLOW: &str = "\x1b[33m";
    const ANSI_RESET: &str = "\x1b[0m";

    format!("{ANSI_YELLOW}! {note}{ANSI_RESET}")
}

fn format_finish_line(status: &str, summary: &str) -> String {
    const ANSI_BOLD_GREEN: &str = "\x1b[1;32m";
    const ANSI_BOLD_RED: &str = "\x1b[1;31m";
    const ANSI_BOLD_YELLOW: &str = "\x1b[1;33m";
    const ANSI_RESET: &str = "\x1b[0m";

    let color = match status {
        "completed" => ANSI_BOLD_GREEN,
        "failed" => ANSI_BOLD_RED,
        _ => ANSI_BOLD_YELLOW,
    };

    format!("{color}{summary} ({status}){ANSI_RESET}")
}
