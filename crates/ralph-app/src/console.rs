use std::io::{self, BufRead, Write};

use anyhow::Result;
use async_trait::async_trait;

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
                println!(
                    "{}",
                    format_iteration_banner(&prompt_name, iteration, max_iterations)
                );
            }
            RunEvent::Output(chunk) => {
                print!("{chunk}");
            }
            RunEvent::ParallelWorkerLaunched { channel_id, label } => {
                println!("[parallel:{channel_id}] launched {label}");
            }
            RunEvent::ParallelWorkerStarted { channel_id, label } => {
                println!("[parallel:{channel_id}] started {label}");
            }
            RunEvent::ParallelWorkerFinished {
                channel_id,
                label,
                exit_code,
            } => {
                println!("[parallel:{channel_id}] finished {label} (exit={exit_code})");
            }
            RunEvent::Note(note) => {
                eprintln!("{note}");
            }
            RunEvent::Finished { status, summary } => {
                println!("\n{} ({})", summary, status.label());
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
        writeln!(stdout, "{}", draft.draft)?;
        if !draft.draft.ends_with('\n') {
            writeln!(stdout)?;
        }
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
