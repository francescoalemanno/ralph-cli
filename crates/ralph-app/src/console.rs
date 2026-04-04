use anyhow::Result;
use async_trait::async_trait;

use crate::{RunDelegate, RunEvent, format_iteration_banner};

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
            RunEvent::Note(note) => {
                eprintln!("{note}");
            }
            RunEvent::InteractiveSessionStart { prompt_name, ready } => {
                eprintln!("starting interactive session '{prompt_name}'");
                let _ = ready.send(());
            }
            RunEvent::InteractiveSessionEnd { prompt_name, ready } => {
                eprintln!("interactive session '{prompt_name}' finished");
                let _ = ready.send(());
            }
            RunEvent::Finished { status, summary } => {
                println!("\n{} ({})", summary, status.label());
            }
        }
        Ok(())
    }
}
