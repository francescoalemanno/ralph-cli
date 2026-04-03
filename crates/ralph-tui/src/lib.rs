mod editor;
mod ui;
mod view;

use std::{
    io,
    sync::mpsc::{self, Receiver, Sender},
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event as CEvent, KeyCode, KeyEvent,
        KeyEventKind, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
pub use editor::edit_file;
use ralph_app::{RalphApp, RunDelegate, RunEvent};
use ralph_core::{LastRunStatus, RunControl, ScaffoldId, TargetReview, TargetSummary};
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::runtime::Handle;
use ui::{normalize_terminal_text, resume_terminal, suspend_terminal};

const RUNNING_SCROLLBACK_LIMIT: usize = 100_000;

pub fn run_tui(app: RalphApp) -> Result<()> {
    run_tui_with_target(app, None)
}

pub fn run_tui_scoped(app: RalphApp, target: &str) -> Result<()> {
    run_tui_with_target(app, Some(target.to_owned()))
}

fn run_tui_with_target(app: RalphApp, target: Option<String>) -> Result<()> {
    let handle = Handle::current();
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).context("failed to create terminal backend")?;
    enable_raw_mode().context("failed to enable raw mode")?;
    if let Err(error) = execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        EnableMouseCapture
    )
    .context("failed to enter alternate screen")
    {
        execute!(
            terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )
        .ok();
        disable_raw_mode().ok();
        return Err(error);
    }

    let result = TuiApp::new(app, handle, target).run(&mut terminal);

    disable_raw_mode().ok();
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )
    .ok();
    terminal.show_cursor().ok();
    result
}

enum UiEvent {
    Tick,
    RunEvent(RunEvent),
    RunDone(Result<TargetSummary, String>),
}

enum Screen {
    Dashboard,
    NewTarget,
    Running,
}

struct RunningState {
    target_id: String,
    prompt_name: String,
    requested_prompt: Option<String>,
    iteration: usize,
    max_iterations: usize,
    control: RunControl,
    terminal: vt100::Parser,
    terminal_bytes: Vec<u8>,
    terminal_rows: u16,
    terminal_cols: u16,
    status: Option<LastRunStatus>,
    scroll: usize,
    follow: bool,
}

impl RunningState {
    fn new(
        target_id: String,
        prompt_name: String,
        requested_prompt: Option<String>,
        control: RunControl,
    ) -> Self {
        let terminal_rows = 24;
        let terminal_cols = 80;
        Self {
            target_id,
            prompt_name,
            requested_prompt,
            iteration: 0,
            max_iterations: 0,
            control,
            terminal: vt100::Parser::new(terminal_rows, terminal_cols, RUNNING_SCROLLBACK_LIMIT),
            terminal_bytes: Vec::new(),
            terminal_rows,
            terminal_cols,
            status: None,
            scroll: 0,
            follow: true,
        }
    }

    fn finish(&mut self, status: LastRunStatus) {
        self.status = Some(status);
    }

    fn is_finished(&self) -> bool {
        self.status.is_some()
    }

    fn status(&self) -> Option<LastRunStatus> {
        self.status
    }

    fn push_terminal_text(&mut self, text: &str) {
        let normalized = normalize_terminal_text(text);
        self.terminal_bytes.extend_from_slice(&normalized);
        self.terminal.process(&normalized);
    }

    fn ensure_terminal_size(&mut self, rows: u16, cols: u16) {
        if self.terminal_rows == rows && self.terminal_cols == cols {
            return;
        }

        self.terminal_rows = rows;
        self.terminal_cols = cols;

        let mut terminal = vt100::Parser::new(rows, cols, RUNNING_SCROLLBACK_LIMIT);
        terminal.process(&self.terminal_bytes);
        self.terminal = terminal;
    }
}

struct TuiApp {
    app: RalphApp,
    handle: Handle,
    tx: Sender<UiEvent>,
    rx: Receiver<UiEvent>,
    targets: Vec<TargetSummary>,
    selected_target_review: Option<TargetReview>,
    selected_target: usize,
    selected_prompt: usize,
    screen: Screen,
    new_target_name: String,
    new_scaffold: ScaffoldId,
    message: String,
    running: Option<RunningState>,
    tick_count: u64,
}

impl TuiApp {
    fn selected_target_uses_hidden_workflow(&self) -> bool {
        self.selected_target()
            .is_some_and(ralph_core::TargetSummary::uses_hidden_workflow)
    }

    fn new(app: RalphApp, handle: Handle, target: Option<String>) -> Self {
        let (tx, rx) = mpsc::channel();
        let mut this = Self {
            app,
            handle,
            tx,
            rx,
            targets: Vec::new(),
            selected_target_review: None,
            selected_target: 0,
            selected_prompt: 0,
            screen: Screen::Dashboard,
            new_target_name: String::new(),
            new_scaffold: ScaffoldId::TaskBased,
            message: String::new(),
            running: None,
            tick_count: 0,
        };
        this.reload_targets();
        if let Some(target) = target {
            if let Some(index) = this.targets.iter().position(|summary| summary.id == target) {
                this.selected_target = index;
                this.refresh_selected_target_review();
            } else {
                this.message = format!("target '{target}' was not found");
            }
        }
        this
    }

    fn run(mut self, terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            loop {
                if tx.send(UiEvent::Tick).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(150));
            }
        });

        loop {
            terminal.draw(|frame| self.draw(frame))?;
            if event::poll(Duration::from_millis(50)).context("failed while polling input")? {
                match event::read().context("failed while reading input")? {
                    CEvent::Key(key) if key.kind == KeyEventKind::Press => {
                        self.handle_key(key, terminal)?;
                        if matches!(key.code, KeyCode::Char('q'))
                            && matches!(self.screen, Screen::Dashboard)
                        {
                            break;
                        }
                    }
                    CEvent::Mouse(mouse) => self.handle_mouse(mouse)?,
                    _ => {}
                }
            }

            while let Ok(event) = self.rx.try_recv() {
                self.handle_ui_event(event);
            }
        }

        Ok(())
    }

    fn handle_key(
        &mut self,
        key: KeyEvent,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> Result<()> {
        match self.screen {
            Screen::Dashboard => self.handle_dashboard_key(key, terminal),
            Screen::NewTarget => self.handle_new_target_key(key, terminal),
            Screen::Running => self.handle_running_key(key, terminal),
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> Result<()> {
        if !matches!(self.screen, Screen::Running) {
            return Ok(());
        }

        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_running(3),
            MouseEventKind::ScrollDown => self.scroll_running(-3),
            _ => {}
        }

        Ok(())
    }

    fn handle_dashboard_key(
        &mut self,
        key: KeyEvent,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> Result<()> {
        match key.code {
            KeyCode::Up => {
                self.selected_target = self.selected_target.saturating_sub(1);
                self.selected_prompt = 0;
                self.refresh_selected_target_review();
            }
            KeyCode::Down => {
                if self.selected_target + 1 < self.targets.len() {
                    self.selected_target += 1;
                    self.selected_prompt = 0;
                    self.refresh_selected_target_review();
                }
            }
            KeyCode::Left => {
                self.selected_prompt = self.selected_prompt.saturating_sub(1);
            }
            KeyCode::Right => {
                if let Some(target) = self.selected_target()
                    && self.selected_prompt + 1 < target.prompt_files.len()
                {
                    self.selected_prompt += 1;
                }
            }
            KeyCode::Char('n') => {
                self.screen = Screen::NewTarget;
                self.new_target_name.clear();
                self.new_scaffold = ScaffoldId::TaskBased;
            }
            KeyCode::Char('r') => self.start_run()?,
            KeyCode::Char('e') => {
                let prompt_path = match self.resolve_selected_edit_path() {
                    Ok(path) => path,
                    Err(error) => {
                        self.message = error.to_string();
                        return Ok(());
                    }
                };
                let editor = self.app.config().editor_override.clone();
                suspend_terminal(terminal)?;
                let result = edit_file(&prompt_path, editor.as_deref());
                resume_terminal(terminal)?;
                result?;
                self.refresh_selected_target_review();
                self.message = format!("opened {}", prompt_path.file_name().unwrap_or("file"));
            }
            KeyCode::Char('d') => {
                let Some(target) = self.selected_target() else {
                    return Ok(());
                };
                let target_id = target.id.clone();
                self.app.delete_target(&target_id)?;
                self.reload_targets();
                self.message = format!("deleted {target_id}");
            }
            KeyCode::Char('a') => {
                self.cycle_agent(None)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_new_target_key(
        &mut self,
        key: KeyEvent,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> Result<()> {
        match key.code {
            KeyCode::Esc => {
                self.screen = Screen::Dashboard;
            }
            KeyCode::Tab => {
                self.new_scaffold = match self.new_scaffold {
                    ScaffoldId::TaskBased => ScaffoldId::GoalDriven,
                    ScaffoldId::GoalDriven => ScaffoldId::SinglePrompt,
                    ScaffoldId::SinglePrompt => ScaffoldId::PlanBuild,
                    ScaffoldId::PlanBuild => ScaffoldId::TaskBased,
                };
            }
            KeyCode::Backspace => {
                self.new_target_name.pop();
            }
            KeyCode::Enter => {
                if self.new_target_name.trim().is_empty() {
                    self.message = "target name cannot be empty".to_owned();
                    return Ok(());
                }
                let scaffold = self.new_scaffold;
                let summary = self
                    .app
                    .create_target(self.new_target_name.trim(), Some(scaffold))?;
                self.reload_targets();
                if let Some(index) = self.targets.iter().position(|item| item.id == summary.id) {
                    self.selected_target = index;
                    self.selected_prompt = 0;
                    self.refresh_selected_target_review();
                }
                self.screen = Screen::Dashboard;
                let opened_path =
                    if matches!(scaffold, ScaffoldId::TaskBased | ScaffoldId::GoalDriven) {
                        Some(self.app.resolve_target_edit_path(&summary.id, None)?)
                    } else {
                        summary
                            .prompt_files
                            .first()
                            .map(|prompt| prompt.path.clone())
                    };
                if let Some(opened_path) = opened_path {
                    let editor = self.app.config().editor_override.clone();
                    suspend_terminal(terminal)?;
                    let result = edit_file(&opened_path, editor.as_deref());
                    resume_terminal(terminal)?;
                    result?;
                    self.refresh_selected_target_review();
                    self.message = format!(
                        "created {} and opened {}",
                        summary.id,
                        opened_path.file_name().unwrap_or("file")
                    );
                } else {
                    self.message = format!("created {}", summary.id);
                }
            }
            KeyCode::Char(ch) => {
                self.new_target_name.push(ch);
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_running_key(
        &mut self,
        key: KeyEvent,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> Result<()> {
        match key.code {
            KeyCode::Esc => {
                if self.running.as_ref().is_some_and(RunningState::is_finished) {
                    self.running = None;
                    self.screen = Screen::Dashboard;
                }
            }
            KeyCode::Char('q') => {
                if let Some(running) = &self.running {
                    running.control.cancel();
                }
            }
            KeyCode::Char('r') => {
                if let Some(running) = &self.running
                    && running.is_finished()
                {
                    let target_id = running.target_id.clone();
                    let requested_prompt = running.requested_prompt.clone();
                    self.start_run_for(&target_id, requested_prompt.as_deref())?;
                }
            }
            KeyCode::Char('a') => {
                let running_control = self.running.as_ref().map(|running| running.control.clone());
                self.cycle_agent(running_control)?;
            }
            KeyCode::Char('e') => {
                let prompt_path = self.resolve_running_edit_path()?;
                let editor = self.app.config().editor_override.clone();
                suspend_terminal(terminal)?;
                let result = edit_file(&prompt_path, editor.as_deref());
                resume_terminal(terminal)?;
                result?;
                self.refresh_selected_target_review();
                self.message = format!(
                    "opened {} for steering",
                    prompt_path.file_name().unwrap_or("file")
                );
            }
            KeyCode::Up => self.scroll_running(1),
            KeyCode::Down => self.scroll_running(-1),
            KeyCode::PageUp => self.scroll_running(10),
            KeyCode::PageDown => self.scroll_running(-10),
            KeyCode::Home => {
                let max_scroll = self.max_running_scroll();
                if let Some(running) = &mut self.running {
                    running.follow = false;
                    running.scroll = max_scroll;
                }
            }
            KeyCode::End => {
                if let Some(running) = &mut self.running {
                    running.follow = true;
                    running.scroll = 0;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_ui_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::Tick => {
                self.tick_count = self.tick_count.wrapping_add(1);
            }
            UiEvent::RunEvent(event) => {
                if let Some(running) = &mut self.running {
                    match event {
                        RunEvent::IterationStarted {
                            prompt_name,
                            iteration,
                            max_iterations,
                        } => {
                            running.prompt_name = prompt_name.clone();
                            running.iteration = iteration;
                            running.max_iterations = max_iterations;
                            running.push_terminal_text(&format!(
                                "{}\n",
                                ralph_app::format_iteration_banner(
                                    &prompt_name,
                                    iteration,
                                    max_iterations
                                )
                            ));
                        }
                        RunEvent::Output(chunk) => {
                            running.push_terminal_text(&chunk);
                        }
                        RunEvent::Note(note) => {
                            running.push_terminal_text(&format!("\n{note}\n"));
                        }
                        RunEvent::Finished { status, summary } => {
                            running.finish(status);
                            running.push_terminal_text(&format!(
                                "\n{summary} ({})\nPress Esc to return.",
                                status.label()
                            ));
                        }
                    }
                    if running.follow {
                        running.scroll = 0;
                    }
                }
            }
            UiEvent::RunDone(result) => match result {
                Ok(summary) => {
                    if let Some(running) = &mut self.running
                        && !running.is_finished()
                    {
                        running.finish(summary.last_run_status);
                        running.push_terminal_text(&format!(
                            "\nRun ended with status: {}.\nPress Esc to return.",
                            summary.last_run_status.label()
                        ));
                    }
                    self.reload_targets();
                    if let Some(index) = self.targets.iter().position(|item| item.id == summary.id)
                    {
                        self.selected_target = index;
                        self.refresh_selected_target_review();
                    }
                }
                Err(error) => {
                    if let Some(running) = &mut self.running {
                        let status = if running.control.is_cancelled() {
                            LastRunStatus::Canceled
                        } else {
                            LastRunStatus::Failed
                        };
                        running.finish(status);
                        running.push_terminal_text(&format!(
                            "\nerror: {error} ({})\nPress Esc to return.",
                            status.label()
                        ));
                    }
                }
            },
        }
    }

    fn reload_targets(&mut self) {
        match self.app.list_targets() {
            Ok(targets) => {
                self.targets = targets;
                if self.targets.is_empty() {
                    self.selected_target = 0;
                    self.selected_prompt = 0;
                } else {
                    self.selected_target = self.selected_target.min(self.targets.len() - 1);
                    let prompt_count = self.targets[self.selected_target].prompt_files.len();
                    self.selected_prompt = if prompt_count == 0 {
                        0
                    } else {
                        self.selected_prompt.min(prompt_count - 1)
                    };
                }
                self.refresh_selected_target_review();
            }
            Err(error) => {
                self.targets = Vec::new();
                self.selected_target_review = None;
                self.message = error.to_string();
            }
        }
    }

    fn selected_target(&self) -> Option<&TargetSummary> {
        self.targets.get(self.selected_target)
    }

    fn selected_target_review(&self) -> Option<&TargetReview> {
        self.selected_target_review.as_ref()
    }

    fn selected_prompt(&self) -> Option<&ralph_core::PromptFile> {
        self.selected_target()
            .and_then(|target| target.prompt_files.get(self.selected_prompt))
    }

    fn selected_target_and_prompt(&self) -> Option<(String, String)> {
        let target = self.selected_target()?;
        let prompt = self.selected_prompt()?;
        Some((target.id.clone(), prompt.name.clone()))
    }

    fn resolve_selected_edit_path(&self) -> Result<camino::Utf8PathBuf> {
        let Some(target) = self.selected_target() else {
            return Err(anyhow!("select a target first"));
        };
        let target_id = target.id.clone();
        let prompt_name = if target.uses_hidden_workflow() {
            None
        } else {
            self.selected_prompt().map(|prompt| prompt.name.clone())
        };
        self.app
            .resolve_target_edit_path(&target_id, prompt_name.as_deref())
    }

    fn resolve_running_edit_path(&self) -> Result<camino::Utf8PathBuf> {
        let Some(running) = self.running.as_ref() else {
            return Err(anyhow!("no run in progress"));
        };
        self.app
            .resolve_target_edit_path(&running.target_id, running.requested_prompt.as_deref())
    }

    fn start_run(&mut self) -> Result<()> {
        if self.selected_target_uses_hidden_workflow() {
            let Some(target) = self.selected_target() else {
                self.message = "select a target first".to_owned();
                return Ok(());
            };
            let target_id = target.id.clone();
            return self.start_run_for(&target_id, None);
        }

        let Some((target_id, prompt_name)) = self.selected_target_and_prompt() else {
            self.message = "select a target and prompt first".to_owned();
            return Ok(());
        };

        self.start_run_for(&target_id, Some(&prompt_name))
    }

    fn start_run_for(&mut self, target_id: &str, prompt_name: Option<&str>) -> Result<()> {
        let tx = self.tx.clone();
        let app = self.app.clone();
        let control = RunControl::new();
        let run_control = control.clone();
        let target_id = target_id.to_owned();
        let requested_prompt = prompt_name.map(ToOwned::to_owned);
        let display_prompt = requested_prompt
            .clone()
            .unwrap_or_else(|| "workflow_auto".to_owned());
        self.running = Some(RunningState::new(
            target_id.to_owned(),
            display_prompt,
            requested_prompt.clone(),
            control,
        ));
        self.screen = Screen::Running;

        self.handle.spawn(async move {
            let mut delegate = ChannelDelegate { tx: tx.clone() };
            let result = app
                .run_target_with_control(
                    &target_id,
                    requested_prompt.as_deref(),
                    run_control,
                    &mut delegate,
                )
                .await
                .map_err(|error| error.to_string());
            let _ = tx.send(UiEvent::RunDone(result));
        });

        Ok(())
    }

    fn cycle_agent(&mut self, run_control: Option<RunControl>) -> Result<()> {
        let detected = ralph_core::CodingAgent::detected();
        if detected.is_empty() {
            self.message = "no supported agents detected on PATH".to_owned();
            return Ok(());
        }

        let current = self.app.coding_agent();
        let index = detected
            .iter()
            .position(|agent| *agent == current)
            .unwrap_or(0);
        let next = detected[(index + 1) % detected.len()];
        self.app.persist_coding_agent(next)?;
        if let Some(control) = run_control {
            control.set_coding_agent(next);
        }
        self.message = format!("agent={}", next.label());
        Ok(())
    }

    fn scroll_running(&mut self, delta: i32) {
        let max_scroll = self.max_running_scroll();
        let Some(running) = &mut self.running else {
            return;
        };

        let next = ((running.scroll as i32) + delta).clamp(0, max_scroll as i32) as usize;
        running.scroll = next;
        running.follow = next == 0;
    }

    fn max_running_scroll(&mut self) -> usize {
        let Some(running) = &mut self.running else {
            return 0;
        };
        let current = if running.follow { 0 } else { running.scroll };
        running.terminal.set_scrollback(usize::MAX);
        let max = running.terminal.screen().scrollback();
        running.terminal.set_scrollback(current.min(max));
        max
    }

    fn refresh_selected_target_review(&mut self) {
        self.selected_target_review = self
            .selected_target()
            .and_then(|target| self.app.review_target(&target.id).ok());
    }
}

struct ChannelDelegate {
    tx: Sender<UiEvent>,
}

#[async_trait]
impl RunDelegate for ChannelDelegate {
    async fn on_event(&mut self, event: RunEvent) -> Result<()> {
        self.tx
            .send(UiEvent::RunEvent(event))
            .map_err(|_| anyhow!("failed to send run event"))
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use camino::Utf8PathBuf;
    use ralph_app::RalphApp;
    use ralph_core::{LastRunStatus, ScaffoldId};
    use tempfile::TempDir;
    use tokio::runtime::Runtime;

    use super::{RunningState, TuiApp, UiEvent};

    fn temp_project_dir() -> (TempDir, Utf8PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        (temp, path)
    }

    #[test]
    fn selected_target_review_tracks_selection() -> Result<()> {
        let (_temp, project_dir) = temp_project_dir();
        let app = RalphApp::load(project_dir.clone())?;
        app.create_target("alpha", Some(ScaffoldId::SinglePrompt))?;
        app.create_target("beta", Some(ScaffoldId::SinglePrompt))?;

        let runtime = Runtime::new()?;
        let mut tui = TuiApp::new(app, runtime.handle().clone(), None);

        assert_eq!(
            tui.selected_target_review()
                .map(|review| review.summary.id.as_str()),
            tui.selected_target().map(|target| target.id.as_str())
        );

        tui.selected_target = 1;
        tui.refresh_selected_target_review();

        assert_eq!(
            tui.selected_target_review()
                .map(|review| review.summary.id.as_str()),
            tui.selected_target().map(|target| target.id.as_str())
        );
        Ok(())
    }

    #[test]
    fn selected_target_review_refreshes_after_file_changes() -> Result<()> {
        let (_temp, project_dir) = temp_project_dir();
        let app = RalphApp::load(project_dir.clone())?;
        let summary = app.create_target("demo", Some(ScaffoldId::SinglePrompt))?;
        let prompt_path = summary.prompt_files[0].path.clone();

        let runtime = Runtime::new()?;
        let mut tui = TuiApp::new(app, runtime.handle().clone(), None);
        let original_contents = tui
            .selected_target_review()
            .and_then(|review| {
                review
                    .files
                    .iter()
                    .find(|file| file.name == "prompt_main.md")
            })
            .map(|file| file.contents.clone());

        std::fs::write(&prompt_path, "# Prompt\n\nUpdated\n")?;
        tui.refresh_selected_target_review();

        let refreshed_contents = tui
            .selected_target_review()
            .and_then(|review| {
                review
                    .files
                    .iter()
                    .find(|file| file.name == "prompt_main.md")
            })
            .map(|file| file.contents.clone());

        assert_ne!(original_contents, refreshed_contents);
        assert_eq!(refreshed_contents.as_deref(), Some("# Prompt\n\nUpdated\n"));
        Ok(())
    }

    #[test]
    fn workflow_targets_are_detected_from_mode_even_without_scaffold() -> Result<()> {
        let (_temp, project_dir) = temp_project_dir();
        let app = RalphApp::load(project_dir.clone())?;
        app.create_target("demo", Some(ScaffoldId::GoalDriven))?;
        std::fs::write(
            project_dir.join(".ralph/targets/demo/target.toml"),
            "id = \"demo\"\nmode = \"goal_driven\"\nlast_run_status = \"never_run\"\n\n[workflow]\nphase = \"plan\"\n",
        )?;

        let runtime = Runtime::new()?;
        let tui = TuiApp::new(app, runtime.handle().clone(), Some("demo".to_owned()));

        assert!(tui.selected_target_uses_hidden_workflow());
        Ok(())
    }

    #[test]
    fn selected_prompt_edit_path_tracks_selected_tab() -> Result<()> {
        let (_temp, project_dir) = temp_project_dir();
        let app = RalphApp::load(project_dir.clone())?;
        let summary = app.create_target("demo", Some(ScaffoldId::PlanBuild))?;

        let runtime = Runtime::new()?;
        let mut tui = TuiApp::new(app, runtime.handle().clone(), Some("demo".to_owned()));
        tui.selected_prompt = 1;

        let edit_path = tui.resolve_selected_edit_path()?;

        assert_eq!(edit_path, summary.prompt_files[1].path);
        Ok(())
    }

    #[test]
    fn running_prompt_edit_path_tracks_requested_prompt() -> Result<()> {
        let (_temp, project_dir) = temp_project_dir();
        let app = RalphApp::load(project_dir)?;
        let summary = app.create_target("demo", Some(ScaffoldId::PlanBuild))?;

        let runtime = Runtime::new()?;
        let mut tui = TuiApp::new(app, runtime.handle().clone(), Some("demo".to_owned()));
        tui.running = Some(super::RunningState::new(
            "demo".to_owned(),
            "1_build.md".to_owned(),
            Some("1_build.md".to_owned()),
            ralph_core::RunControl::new(),
        ));

        let edit_path = tui.resolve_running_edit_path()?;

        assert_eq!(edit_path, summary.prompt_files[1].path);
        Ok(())
    }

    #[test]
    fn running_terminal_resize_reflows_from_full_transcript() {
        let mut running = super::RunningState::new(
            "demo".to_owned(),
            "prompt_main.md".to_owned(),
            Some("prompt_main.md".to_owned()),
            ralph_core::RunControl::new(),
        );
        let long_line = "abcdefghijklmnopqrstuvwxyz0123456789";

        running.push_terminal_text(&format!("{long_line}\n"));
        running.ensure_terminal_size(24, 10);
        running.ensure_terminal_size(24, 80);

        let output = running.terminal.screen().contents();
        assert!(output.contains(long_line));
    }

    #[test]
    fn run_done_marks_run_finished_even_without_finished_event() -> Result<()> {
        let (_temp, project_dir) = temp_project_dir();
        let app = RalphApp::load(project_dir)?;
        let mut summary = app.create_target("demo", Some(ScaffoldId::SinglePrompt))?;
        summary.last_run_status = LastRunStatus::Completed;

        let runtime = Runtime::new()?;
        let mut tui = TuiApp::new(app, runtime.handle().clone(), Some("demo".to_owned()));
        tui.running = Some(RunningState::new(
            "demo".to_owned(),
            "prompt_main.md".to_owned(),
            Some("prompt_main.md".to_owned()),
            ralph_core::RunControl::new(),
        ));

        tui.handle_ui_event(UiEvent::RunDone(Ok(summary)));

        let running = tui.running.as_ref().expect("running state");
        assert!(running.is_finished());
        assert_eq!(running.status(), Some(LastRunStatus::Completed));
        assert!(
            running
                .terminal
                .screen()
                .contents()
                .contains("Run ended with status: completed.")
        );
        Ok(())
    }

    #[test]
    fn run_done_error_marks_failed_or_canceled_runs_finished() -> Result<()> {
        let (_temp, project_dir) = temp_project_dir();
        let app = RalphApp::load(project_dir.clone())?;
        app.create_target("demo", Some(ScaffoldId::SinglePrompt))?;
        app.create_target("cancelled", Some(ScaffoldId::SinglePrompt))?;
        let runtime = Runtime::new()?;

        let mut failed_tui = TuiApp::new(
            app.clone(),
            runtime.handle().clone(),
            Some("demo".to_owned()),
        );
        failed_tui.running = Some(RunningState::new(
            "demo".to_owned(),
            "prompt_main.md".to_owned(),
            Some("prompt_main.md".to_owned()),
            ralph_core::RunControl::new(),
        ));
        failed_tui.handle_ui_event(UiEvent::RunDone(Err("boom".to_owned())));
        assert_eq!(
            failed_tui.running.as_ref().and_then(RunningState::status),
            Some(LastRunStatus::Failed)
        );

        let mut cancelled_tui =
            TuiApp::new(app, runtime.handle().clone(), Some("cancelled".to_owned()));
        let running = RunningState::new(
            "cancelled".to_owned(),
            "prompt_main.md".to_owned(),
            Some("prompt_main.md".to_owned()),
            ralph_core::RunControl::new(),
        );
        running.control.cancel();
        cancelled_tui.running = Some(running);
        cancelled_tui.handle_ui_event(UiEvent::RunDone(Err("operation canceled".to_owned())));
        assert_eq!(
            cancelled_tui
                .running
                .as_ref()
                .and_then(RunningState::status),
            Some(LastRunStatus::Canceled)
        );
        Ok(())
    }
}
