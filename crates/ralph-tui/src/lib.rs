mod editor;
mod ui;

use std::collections::BTreeMap;
use std::io;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event as CEvent, KeyCode, KeyEvent,
        KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
pub use editor::edit_file;
use ralph_app::{
    RalphApp, RunDelegate, RunEvent, WorkflowRequestInput, WorkflowRunInput,
    format_iteration_banner,
};
use ralph_core::{
    LastRunStatus, RunControl, RunnerConfig, WorkflowDefinition, WorkflowRunSummary,
    WorkflowRuntimeRequest, atomic_write,
};
use ralph_runner::{InteractiveSessionInvocation, InteractiveSessionOutcome};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use tokio::{runtime::Handle, sync::oneshot};
use ui::{
    normalize_terminal_text, resolved_accent_color, resolved_success_color, resolved_warning_color,
    resume_terminal, styled_title, suspend_terminal,
};

const RUNNING_SCROLLBACK_LIMIT: usize = 100_000;

#[derive(Debug, Clone, Default)]
pub struct TuiLaunchOptions {
    pub preset_workflow: Option<String>,
    pub preloaded_request: Option<TuiPreloadedRequest>,
    pub workflow_options: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct TuiPreloadedRequest {
    pub source: TuiRequestSource,
    pub text: String,
    pub file_path: Option<Utf8PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiRequestSource {
    Argv,
    File,
}

pub fn run_tui(app: RalphApp) -> Result<()> {
    run_tui_with_options(app, TuiLaunchOptions::default())
}

pub fn run_tui_with_options(app: RalphApp, launch: TuiLaunchOptions) -> Result<()> {
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

    let result = TuiApp::new(app, handle, launch)?.run(&mut terminal);

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
    RunEvent(RunEvent),
    RunDone(Result<WorkflowRunSummary, String>),
    InteractiveSession {
        config: RunnerConfig,
        invocation: InteractiveSessionInvocation,
        reply: oneshot::Sender<Result<InteractiveSessionOutcome, String>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    RequestText,
    RequestFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestMode {
    Text,
    File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestOrigin {
    None,
    Typed,
    Argv,
    File,
    Draft,
}

impl RequestOrigin {
    fn label(self) -> &'static str {
        match self {
            Self::None => "missing",
            Self::Typed => "typed",
            Self::Argv => "argv",
            Self::File => "file",
            Self::Draft => "draft file",
        }
    }
}

struct RunningState {
    prompt_name: String,
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
    last_summary: String,
}

impl RunningState {
    fn new(control: RunControl) -> Self {
        let terminal_rows = 24;
        let terminal_cols = 80;
        Self {
            prompt_name: String::new(),
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
            last_summary: String::new(),
        }
    }

    fn finish(&mut self, status: LastRunStatus, summary: impl Into<String>) {
        self.status = Some(status);
        self.last_summary = summary.into();
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
    tx: std::sync::mpsc::Sender<UiEvent>,
    rx: std::sync::mpsc::Receiver<UiEvent>,
    workflow_id: String,
    workflow: WorkflowDefinition,
    focus: Focus,
    request_mode: RequestMode,
    request_text: String,
    request_file: String,
    request_origin: RequestOrigin,
    workflow_options: BTreeMap<String, String>,
    message: String,
    running: Option<RunningState>,
    auto_start_run: bool,
}

impl TuiApp {
    fn new(app: RalphApp, handle: Handle, launch: TuiLaunchOptions) -> Result<Self> {
        let workflow_id = launch
            .preset_workflow
            .ok_or_else(|| anyhow!("opening the runner TUI requires a workflow id"))?;
        let workflow = app.load_workflow(&workflow_id)?;
        let (tx, rx) = std::sync::mpsc::channel();
        let mut this = Self {
            app,
            handle,
            tx,
            rx,
            workflow_id,
            workflow,
            focus: Focus::RequestText,
            request_mode: RequestMode::Text,
            request_text: String::new(),
            request_file: String::new(),
            request_origin: RequestOrigin::None,
            workflow_options: launch.workflow_options,
            message: String::new(),
            running: None,
            auto_start_run: false,
        };
        let has_preloaded_request = launch.preloaded_request.is_some();
        if let Some(preload) = launch.preloaded_request {
            this.apply_preloaded_request(preload);
        }
        this.sync_request_mode();
        this.focus = this.request_focus().unwrap_or(Focus::RequestText);
        this.auto_start_run = has_preloaded_request;
        Ok(this)
    }

    fn run(mut self, terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
        if self.auto_start_run {
            self.auto_start_run = false;
            self.start_run()?;
        }

        loop {
            terminal.draw(|frame| self.draw(frame))?;
            if event::poll(std::time::Duration::from_millis(50))
                .context("failed while polling input")?
            {
                match event::read().context("failed while reading input")? {
                    CEvent::Key(key) if key.kind == KeyEventKind::Press => {
                        if !self.handle_key(key, terminal)? {
                            return Ok(());
                        }
                    }
                    CEvent::Mouse(mouse) => {
                        let size = terminal.size().context("failed to read terminal size")?;
                        let area = Rect::new(0, 0, size.width, size.height);
                        self.handle_mouse(mouse, area);
                    }
                    _ => {}
                }
            }
            while let Ok(event) = self.rx.try_recv() {
                self.handle_ui_event(event, terminal)?;
            }
        }
    }

    fn handle_key(
        &mut self,
        key: KeyEvent,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> Result<bool> {
        if self.running.is_some() {
            return self.handle_running_key(key, terminal);
        }

        match key.code {
            KeyCode::Char('q') => return Ok(false),
            KeyCode::Char('a') => self.cycle_agent(None)?,
            KeyCode::Char('r') => self.start_run()?,
            KeyCode::Char('e') => self.edit_request(terminal)?,
            KeyCode::Left => self.request_mode_left(),
            KeyCode::Right => self.request_mode_right(),
            KeyCode::Enter => self.start_run()?,
            KeyCode::Backspace => self.handle_backspace(),
            KeyCode::Char(ch)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                self.handle_text_input(ch)
            }
            _ => {}
        }

        Ok(true)
    }

    fn handle_running_key(
        &mut self,
        key: KeyEvent,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> Result<bool> {
        let (is_finished, control) = match self.running.as_ref() {
            Some(running) => (running.is_finished(), running.control.clone()),
            None => return Ok(true),
        };

        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) && !is_finished => {
                control.cancel();
                self.message = "canceling run".to_owned();
            }
            KeyCode::Char('q') => {
                if !is_finished {
                    control.cancel();
                }
                return Ok(false);
            }
            KeyCode::Char('r') if is_finished => {
                self.start_run()?;
            }
            KeyCode::Char('a') => {
                self.cycle_agent(Some(control))?;
                if !is_finished {
                    self.message.push_str(" (applies next iteration)");
                }
            }
            KeyCode::Char('e') => {
                self.edit_request(terminal)?;
                if !is_finished {
                    self.message = format!("{}; rerun to apply request edits", self.message);
                }
            }
            KeyCode::Up => self.scroll_running(1),
            KeyCode::Down => self.scroll_running(-1),
            KeyCode::PageUp => self.scroll_running(10),
            KeyCode::PageDown => self.scroll_running(-10),
            KeyCode::Home => {
                let max_scroll = self.max_running_scroll();
                if let Some(running) = self.running.as_mut() {
                    running.follow = false;
                    running.scroll = max_scroll;
                }
            }
            KeyCode::End | KeyCode::Char('f') => {
                if let Some(running) = self.running.as_mut() {
                    running.follow = true;
                    running.scroll = 0;
                }
            }
            _ => {}
        }

        Ok(true)
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, screen: Rect) {
        if self.running.is_none() {
            return;
        }

        let output_area = self.running_output_area(screen);
        let within_output = mouse.column >= output_area.x
            && mouse.column < output_area.x.saturating_add(output_area.width)
            && mouse.row >= output_area.y
            && mouse.row < output_area.y.saturating_add(output_area.height);
        if !within_output {
            return;
        }

        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_running(1),
            MouseEventKind::ScrollDown => self.scroll_running(-1),
            _ => {}
        }
    }

    fn handle_ui_event(
        &mut self,
        event: UiEvent,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> Result<()> {
        match event {
            UiEvent::RunEvent(event) => {
                if let Some(running) = self.running.as_mut() {
                    match event {
                        RunEvent::IterationStarted {
                            prompt_name,
                            iteration,
                            max_iterations,
                        } => {
                            running.prompt_name = prompt_name.clone();
                            running.iteration = iteration;
                            running.max_iterations = max_iterations;
                            running.push_terminal_text(&format_iteration_banner(
                                &prompt_name,
                                iteration,
                                max_iterations,
                            ));
                            running.push_terminal_text("\n");
                        }
                        RunEvent::Output(chunk) => running.push_terminal_text(&chunk),
                        RunEvent::Note(note) => {
                            self.message = note.clone();
                            running.push_terminal_text(&format!("\n[{note}]\n"));
                        }
                        RunEvent::Finished { status, summary } => {
                            self.message = summary.clone();
                            running.finish(status, summary);
                        }
                    }
                }
            }
            UiEvent::RunDone(result) => match result {
                Ok(summary) => {
                    if let Some(running) = self.running.as_mut()
                        && running.status().is_none()
                    {
                        running.finish(summary.status, summary.status.label());
                    }
                    self.message = format!("{} [{}]", summary.workflow_id, summary.status.label());
                }
                Err(error) => {
                    if let Some(running) = self.running.as_mut() {
                        let status = if running.control.is_cancelled() {
                            LastRunStatus::Canceled
                        } else {
                            LastRunStatus::Failed
                        };
                        running.finish(status, &error);
                        running.push_terminal_text(&format!("\n[error] {error}\n"));
                    }
                    self.message = error;
                }
            },
            UiEvent::InteractiveSession {
                config,
                invocation,
                reply,
            } => {
                if let Some(running) = self.running.as_mut() {
                    running.push_terminal_text(&format!(
                        "\n[interactive session: {}]\n",
                        invocation.session_name
                    ));
                }

                let outcome =
                    self.run_interactive_session_in_terminal(terminal, &config, &invocation);

                if let Some(running) = self.running.as_mut() {
                    match &outcome {
                        Ok(result) => {
                            running.push_terminal_text(&format!(
                                "\n[interactive session exited with code {}]\n",
                                result.exit_code.unwrap_or(-1)
                            ));
                        }
                        Err(error) => {
                            running.push_terminal_text(&format!(
                                "\n[interactive session failed: {error}]\n"
                            ));
                        }
                    }
                }

                let _ = reply.send(outcome);
            }
        }

        Ok(())
    }

    fn run_interactive_session_in_terminal(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        config: &RunnerConfig,
        invocation: &InteractiveSessionInvocation,
    ) -> Result<InteractiveSessionOutcome, String> {
        suspend_terminal(terminal).map_err(|error| error.to_string())?;
        let outcome = self
            .app
            .run_interactive_session_with_config(config, invocation)
            .map_err(|error| error.to_string());
        let resume_result = resume_terminal(terminal).map_err(|error| error.to_string());

        match (outcome, resume_result) {
            (Ok(outcome), Ok(())) => Ok(outcome),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Err(error), Err(resume_error)) => Err(format!("{error}; {resume_error}")),
        }
    }

    fn apply_preloaded_request(&mut self, preload: TuiPreloadedRequest) {
        self.request_text = preload.text;
        if let Some(path) = preload.file_path {
            self.request_file = path.to_string();
        }
        self.request_origin = match preload.source {
            TuiRequestSource::Argv => RequestOrigin::Argv,
            TuiRequestSource::File => RequestOrigin::File,
        };
        if matches!(preload.source, TuiRequestSource::File) {
            self.request_mode = RequestMode::File;
        }
    }

    fn request_focus(&self) -> Option<Focus> {
        if self.can_edit_request_text() {
            Some(Focus::RequestText)
        } else if self.can_edit_request_file() {
            Some(Focus::RequestFile)
        } else {
            None
        }
    }

    fn sync_request_mode(&mut self) {
        let Some(runtime) = self.runtime_request() else {
            return;
        };

        match (runtime.argv, runtime.file_flag) {
            (true, false) => self.request_mode = RequestMode::Text,
            (false, true) => self.request_mode = RequestMode::File,
            (true, true) => {
                if matches!(
                    self.request_origin,
                    RequestOrigin::File | RequestOrigin::Draft
                ) && !self.request_file.trim().is_empty()
                {
                    self.request_mode = RequestMode::File;
                } else if self.request_file.trim().is_empty() && !self.request_text.is_empty() {
                    self.request_mode = RequestMode::Text;
                }
            }
            _ => {}
        }

        if self.request_focus().is_none() {
            self.focus = Focus::RequestText;
        }
    }

    fn request_mode_left(&mut self) {
        if self.focus == Focus::RequestFile {
            self.request_mode = RequestMode::Text;
            if let Some(focus) = self.request_focus() {
                self.focus = focus;
            }
        }
    }

    fn request_mode_right(&mut self) {
        if self.focus == Focus::RequestText {
            self.request_mode = RequestMode::File;
            if let Some(focus) = self.request_focus() {
                self.focus = focus;
            }
        }
    }

    fn handle_backspace(&mut self) {
        match self.focus {
            Focus::RequestText if self.can_edit_request_text() => {
                self.request_text.pop();
                if !self.request_text.is_empty() {
                    self.request_origin = RequestOrigin::Typed;
                }
            }
            Focus::RequestFile if self.can_edit_request_file() => {
                self.request_file.pop();
            }
            _ => {}
        }
    }

    fn handle_text_input(&mut self, ch: char) {
        match self.focus {
            Focus::RequestText if self.can_edit_request_text() => {
                self.request_text.push(ch);
                self.request_origin = RequestOrigin::Typed;
            }
            Focus::RequestFile if self.can_edit_request_file() => {
                self.request_file.push(ch);
                if !self.request_file.trim().is_empty() {
                    self.request_origin = RequestOrigin::File;
                }
            }
            _ => {}
        }
    }

    fn start_run(&mut self) -> Result<()> {
        let request_input = self.request_input_for(&self.workflow)?;
        let workflow_id = self.workflow.workflow_id.clone();
        let control = RunControl::new();
        let tx = self.tx.clone();
        let app = self.app.clone();
        let control_for_task = control.clone();
        let workflow_options = self.workflow_options.clone();

        self.message = format!("running workflow '{}'", workflow_id);
        self.running = Some(RunningState::new(control));

        self.handle.spawn(async move {
            let mut delegate = TuiRunDelegate { tx: tx.clone() };
            let result = app
                .run_workflow_with_control(
                    &workflow_id,
                    WorkflowRunInput {
                        request: request_input,
                        options: workflow_options,
                    },
                    control_for_task,
                    &mut delegate,
                )
                .await
                .map_err(|error| error.to_string());
            let _ = tx.send(UiEvent::RunDone(result));
        });

        Ok(())
    }

    fn request_input_for(&self, workflow: &WorkflowDefinition) -> Result<WorkflowRequestInput> {
        let Some(request) = workflow.request.as_ref() else {
            return Ok(WorkflowRequestInput::default());
        };

        if request.runtime.is_none() {
            return Ok(WorkflowRequestInput::default());
        }

        let runtime = request.runtime.as_ref().expect("checked is_some");
        match self.request_mode {
            RequestMode::Text if runtime.argv => Ok(WorkflowRequestInput {
                argv: Some(self.request_text.clone()),
                stdin: None,
                request_file: None,
            }),
            RequestMode::File if runtime.file_flag => {
                if self.request_file.trim().is_empty() {
                    return Err(anyhow!("enter a request file path"));
                }
                Ok(WorkflowRequestInput {
                    argv: None,
                    stdin: None,
                    request_file: Some(Utf8PathBuf::from(self.request_file.trim())),
                })
            }
            RequestMode::Text if !runtime.argv && runtime.file_flag => Err(anyhow!(
                "the selected workflow only accepts request files; switch the request source with the arrow keys"
            )),
            RequestMode::File if !runtime.file_flag && runtime.argv => Err(anyhow!(
                "the selected workflow only accepts text requests; switch the request source with the arrow keys"
            )),
            _ => Err(anyhow!(
                "the selected workflow requires a runtime request form that the TUI does not support"
            )),
        }
    }

    fn runtime_request(&self) -> Option<&WorkflowRuntimeRequest> {
        self.workflow
            .request
            .as_ref()
            .and_then(|request| request.runtime.as_ref())
    }

    fn can_edit_request_text(&self) -> bool {
        self.runtime_request()
            .is_some_and(|runtime| runtime.argv && self.request_mode == RequestMode::Text)
    }

    fn can_edit_request_file(&self) -> bool {
        self.runtime_request()
            .is_some_and(|runtime| runtime.file_flag && self.request_mode == RequestMode::File)
    }

    fn edit_request(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> Result<()> {
        let path = self.resolve_request_edit_path()?;
        self.prepare_request_edit_path(&path)?;
        suspend_terminal(terminal)?;
        let edit_result = edit_file(&path, self.app.config().editor_override.as_deref());
        resume_terminal(terminal)?;
        edit_result?;
        self.refresh_request_from_path(&path)?;
        self.message = format!(
            "updated request from {}",
            path.file_name().unwrap_or(path.as_str())
        );
        Ok(())
    }

    fn resolve_request_edit_path(&self) -> Result<Utf8PathBuf> {
        if let Some(request) = &self.workflow.request
            && let Some(file) = &request.file
        {
            return Ok(self.resolve_project_relative_path(&file.path));
        }

        if !self.request_file.trim().is_empty() {
            return Ok(self.resolve_project_relative_path(Utf8Path::new(self.request_file.trim())));
        }

        if !self.request_text.trim().is_empty() || !self.request_file.trim().is_empty() {
            return Ok(self
                .app
                .project_dir()
                .join(".ralph")
                .join("request-drafts")
                .join(format!("{}.md", self.workflow_id)));
        }

        Err(anyhow!("no editable request is available"))
    }

    fn prepare_request_edit_path(&mut self, path: &Utf8Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent.as_std_path())
                .with_context(|| format!("failed to create {}", parent))?;
        }
        if path.exists() {
            return Ok(());
        }

        let mut contents = self.request_text.clone();
        if !contents.ends_with('\n') {
            contents.push('\n');
        }
        atomic_write(path, contents)
            .with_context(|| format!("failed to create request draft {}", path))?;
        Ok(())
    }

    fn refresh_request_from_path(&mut self, path: &Utf8Path) -> Result<()> {
        self.request_text = self.app.read_utf8_file(path)?;
        self.request_file = path.to_string();
        self.request_origin = RequestOrigin::Draft;
        if self
            .workflow
            .request
            .as_ref()
            .and_then(|request| request.runtime.as_ref())
            .is_some_and(|runtime| runtime.file_flag)
            && !self.request_file.trim().is_empty()
        {
            self.request_mode = RequestMode::File;
        }
        Ok(())
    }

    fn resolve_project_relative_path(&self, path: &Utf8Path) -> Utf8PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.app.project_dir().join(path)
        }
    }

    fn cycle_agent(&mut self, run_control: Option<RunControl>) -> Result<()> {
        let available = self.app.available_agents();
        if available.is_empty() {
            self.message = "no configured agents are currently available".to_owned();
            return Ok(());
        }

        let current = self.app.agent_id().to_owned();
        let index = available
            .iter()
            .position(|agent| agent.id == current)
            .unwrap_or(0);
        let next = available[(index + 1) % available.len()];
        let next_id = next.id.clone();
        let next_name = next.name.clone();
        self.app.persist_agent(&next_id)?;
        if let Some(control) = run_control {
            control.set_agent_id(next_id);
        }
        self.message = format!("agent={next_name}");
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

    fn running_output_area(&self, screen: Rect) -> Rect {
        let has_notice = !self.message.trim().is_empty();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(4),
                Constraint::Length(if has_notice { 3 } else { 0 }),
                Constraint::Min(1),
                Constraint::Length(2),
            ])
            .split(screen);
        let running_body = chunks[2];
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(6), Constraint::Min(1)])
            .split(running_body);
        sections[1]
    }

    fn draw(&mut self, frame: &mut Frame<'_>) {
        frame.render_widget(
            Block::default().style(Style::default().bg(self.background_color())),
            frame.area(),
        );

        let has_notice = !self.message.trim().is_empty();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(4),
                Constraint::Length(if has_notice { 3 } else { 0 }),
                Constraint::Min(1),
                Constraint::Length(2),
            ])
            .split(frame.area());

        self.draw_header(frame, chunks[0]);
        if has_notice {
            self.draw_notice(frame, chunks[1]);
        }
        if self.running.is_some() {
            self.draw_running_body(frame, chunks[2]);
        } else {
            self.draw_idle_body(frame, chunks[2]);
        }
        self.draw_footer(frame, chunks[3]);
    }

    fn draw_header(&self, frame: &mut Frame<'_>, area: Rect) {
        let iteration = self
            .running
            .as_ref()
            .filter(|running| running.iteration > 0)
            .map(|running| running.iteration.to_string())
            .unwrap_or_else(|| "-".to_owned());
        let header = Paragraph::new(Text::from(vec![
            Line::from(vec![
                Span::styled(
                    " RALPH ",
                    Style::default()
                        .fg(self.accent_color())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "◆",
                    Style::default()
                        .fg(self.warning_color())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" workflow: {} ", self.workflow_id),
                    Style::default()
                        .fg(self.text_color())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "◆",
                    Style::default()
                        .fg(self.warning_color())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" Agent {}", self.app.agent_name()),
                    Style::default()
                        .fg(self.text_color())
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled(" Iteration ", Style::default().fg(self.muted_color())),
                Span::styled(iteration, Style::default().fg(self.text_color())),
                Span::styled(" - ", Style::default().fg(self.muted_color())),
                Span::styled(
                    self.app.project_dir().to_string(),
                    Style::default().fg(self.text_color()),
                ),
            ]),
        ]))
        .block(self.panel_block());
        frame.render_widget(header, area);
    }

    fn draw_notice(&self, frame: &mut Frame<'_>, area: Rect) {
        let (label, fg, bg) = self.notice_palette();
        let banner = Paragraph::new(Line::from(vec![
            Span::styled(
                label,
                Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                &self.message,
                Style::default()
                    .fg(self.text_color())
                    .add_modifier(Modifier::BOLD),
            ),
        ]))
        .block(self.panel_block());
        frame.render_widget(banner, area);
    }

    fn draw_idle_body(&self, frame: &mut Frame<'_>, area: Rect) {
        let main = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(8), Constraint::Min(1)])
            .split(area);

        self.draw_request_panel(frame, main[0]);
        self.draw_idle_output_panel(frame, main[1]);
    }

    fn draw_running_body(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(6), Constraint::Min(1)])
            .split(area);

        self.draw_running_context(frame, sections[0]);
        self.draw_output_panel(frame, sections[1]);
    }

    fn draw_footer(&self, frame: &mut Frame<'_>, area: Rect) {
        let footer = if let Some(running) = self.running.as_ref() {
            if running.is_finished() {
                "R rerun  •  E edit request  •  A cycle next agent  •  F/End follow  •  wheel/↑/↓/PgUp/PgDn scroll  •  Q quit"
            } else {
                "Ctrl-C cancel run  •  A cycle next agent  •  E edit request  •  F/End follow  •  wheel/↑/↓/PgUp/PgDn scroll  •  Q quit"
            }
        } else {
            "Enter/R run  •  ←/→ switch request source  •  E edit request  •  A cycle agent  •  Q quit"
        };

        let paragraph = Paragraph::new(footer)
            .style(Style::default().fg(self.muted_color()))
            .wrap(Wrap { trim: true });
        frame.render_widget(paragraph, area);
    }

    fn draw_request_panel(&self, frame: &mut Frame<'_>, area: Rect) {
        let border_style = Style::default().fg(self.accent_color());
        let text = self.request_panel_text();

        let paragraph = Paragraph::new(text)
            .block(
                self.panel_block()
                    .border_style(border_style)
                    .title(self.title_line("Request", "Input")),
            )
            .wrap(Wrap { trim: false });
        frame.render_widget(paragraph, area);
    }

    fn request_panel_text(&self) -> Text<'static> {
        let Some(request) = self.workflow.request.as_ref() else {
            return Text::from(vec![
                Line::from("This workflow does not require a request."),
                Line::from(""),
                Line::from("Press Enter or R to run."),
            ]);
        };

        if let Some(runtime) = &request.runtime {
            return self.runtime_request_text(runtime);
        }

        if let Some(file) = &request.file {
            return Text::from(vec![
                Line::from("This workflow reads its request from a project file."),
                Line::from(""),
                Line::from(vec![
                    Span::styled("path ", Style::default().fg(self.subtle_color())),
                    Span::styled(
                        file.path.to_string(),
                        Style::default().fg(self.text_color()),
                    ),
                ]),
                Line::from(""),
                Line::from("Press E to edit the file, then Enter or R to run."),
            ]);
        }

        if let Some(inline) = &request.inline {
            return Text::from(vec![
                Line::from("This workflow has an inline request."),
                Line::from(""),
                Line::from(inline.clone()),
            ]);
        }

        Text::from("Unsupported request configuration.")
    }

    fn draw_idle_output_panel(&self, frame: &mut Frame<'_>, area: Rect) {
        let text = Text::from(vec![
            Line::from(vec![
                Span::styled("workflow ", Style::default().fg(self.subtle_color())),
                Span::styled(
                    self.workflow_id.clone(),
                    Style::default().fg(self.text_color()),
                ),
            ]),
            Line::from(""),
            Line::from("Press Enter or R to start the run."),
            Line::from("Use E to edit the request in a file-backed editor."),
        ]);
        let paragraph = Paragraph::new(text)
            .block(
                self.panel_block()
                    .title(self.title_line("Runner Output", "Ready to run")),
            )
            .wrap(Wrap { trim: false });
        frame.render_widget(paragraph, area);
    }

    fn runtime_request_text(&self, runtime: &WorkflowRuntimeRequest) -> Text<'static> {
        let mut lines = Vec::new();

        if runtime.argv && runtime.file_flag {
            lines.push(Line::from(vec![
                self.request_mode_span(RequestMode::Text),
                Span::raw(" "),
                self.request_mode_span(RequestMode::File),
            ]));
            lines.push(Line::from(""));
        }

        lines.push(Line::from(vec![
            Span::styled("source ", Style::default().fg(self.subtle_color())),
            Span::styled(
                self.request_origin.label(),
                Style::default().fg(self.text_color()),
            ),
            Span::styled("  •  accepted ", Style::default().fg(self.subtle_color())),
            Span::styled(
                self.allowed_runtime_sources(runtime),
                Style::default().fg(self.text_color()),
            ),
        ]));
        lines.push(Line::from(""));

        match self.request_mode {
            RequestMode::Text => {
                let mut value = self.request_text.clone();
                if self.focus == Focus::RequestText {
                    value.push('█');
                }
                if value.is_empty() {
                    value.push_str("Type a request here.");
                }
                lines.push(Line::from(value));
            }
            RequestMode::File => {
                let mut path = self.request_file.clone();
                if self.focus == Focus::RequestFile {
                    path.push('█');
                }
                if path.is_empty() {
                    path.push_str("Enter a request file path.");
                }
                lines.push(Line::from(vec![
                    Span::styled("file ", Style::default().fg(self.subtle_color())),
                    Span::styled(path, Style::default().fg(self.text_color())),
                ]));
                if !self.request_text.trim().is_empty() {
                    lines.push(Line::from(""));
                    lines.push(Line::from(self.request_preview()));
                }
            }
        }

        Text::from(lines)
    }

    fn request_mode_span(&self, mode: RequestMode) -> Span<'static> {
        let (label, active) = match mode {
            RequestMode::Text => ("[ Text ]", self.request_mode == RequestMode::Text),
            RequestMode::File => ("[ File ]", self.request_mode == RequestMode::File),
        };
        if active {
            Span::styled(
                label,
                Style::default()
                    .fg(Color::Black)
                    .bg(self.accent_color())
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(label, Style::default().fg(self.muted_color()))
        }
    }

    fn draw_running_context(&self, frame: &mut Frame<'_>, area: Rect) {
        let prompt_name = self
            .running
            .as_ref()
            .map(|running| running.prompt_name.as_str())
            .filter(|name| !name.is_empty())
            .unwrap_or("<pending>");
        let note = if self.running.as_ref().is_some_and(RunningState::is_finished) {
            "E edits the request used for rerun."
        } else {
            "Agent switches apply on the next iteration only."
        };
        let mut lines = vec![
            Line::from(vec![
                Span::styled("workflow ", Style::default().fg(self.subtle_color())),
                Span::styled(
                    self.workflow_id.clone(),
                    Style::default().fg(self.text_color()),
                ),
                Span::styled("  •  prompt ", Style::default().fg(self.subtle_color())),
                Span::styled(prompt_name, Style::default().fg(self.text_color())),
            ]),
            Line::from(vec![
                Span::styled("request source ", Style::default().fg(self.subtle_color())),
                Span::styled(
                    self.request_origin.label(),
                    Style::default().fg(self.text_color()),
                ),
            ]),
        ];
        if !self.request_file.trim().is_empty() {
            lines.push(Line::from(vec![
                Span::styled("request target ", Style::default().fg(self.subtle_color())),
                Span::styled(
                    self.request_file.clone(),
                    Style::default().fg(self.text_color()),
                ),
            ]));
        }
        if !self.request_text.trim().is_empty() {
            lines.push(Line::from(vec![
                Span::styled("request summary ", Style::default().fg(self.subtle_color())),
                Span::styled(
                    self.request_summary(),
                    Style::default().fg(self.text_color()),
                ),
            ]));
        }
        lines.push(Line::from(vec![
            Span::styled("note ", Style::default().fg(self.subtle_color())),
            Span::styled(note, Style::default().fg(self.text_color())),
        ]));

        let paragraph = Paragraph::new(Text::from(lines)).block(
            self.panel_block()
                .title(self.title_line("Context", "Current run")),
        );
        frame.render_widget(paragraph, area);
    }

    fn draw_output_panel(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let block = self
            .panel_block()
            .title(self.title_line("Runner Output", "Live output"));
        let inner = block.inner(area);
        let output = if let Some(running) = &mut self.running {
            running.ensure_terminal_size(inner.height.max(1), inner.width.max(1));
            let scroll = if running.follow { 0 } else { running.scroll };
            running.terminal.set_scrollback(scroll);
            running.terminal.screen().contents()
        } else {
            String::new()
        };
        let paragraph = Paragraph::new(output)
            .block(block)
            .style(Style::default().fg(self.text_color()))
            .wrap(Wrap { trim: false });
        frame.render_widget(paragraph, area);
    }

    fn allowed_runtime_sources(&self, runtime: &WorkflowRuntimeRequest) -> String {
        let mut parts = Vec::new();
        if runtime.argv {
            parts.push("text");
        }
        if runtime.file_flag {
            parts.push("file");
        }
        if runtime.stdin {
            parts.push("stdin");
        }
        parts.join(", ")
    }

    fn request_preview(&self) -> String {
        let preview = self
            .request_text
            .lines()
            .take(6)
            .collect::<Vec<_>>()
            .join("\n");
        if preview.trim().is_empty() {
            "<empty request>".to_owned()
        } else {
            preview
        }
    }

    fn request_summary(&self) -> String {
        let first = self
            .request_text
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("<empty>");
        let mut summary = first.trim().to_owned();
        if summary.len() > 80 {
            summary.truncate(80);
            summary.push_str("...");
        }
        summary
    }

    fn panel_block(&self) -> Block<'static> {
        Block::default()
            .borders(Borders::ALL)
            .style(Style::default().bg(self.background_color()))
    }

    fn title_line(&self, title: &str, subtitle: &str) -> Line<'static> {
        styled_title(
            title,
            subtitle,
            self.text_color(),
            self.subtle_color(),
            self.muted_color(),
        )
    }

    fn accent_color(&self) -> Color {
        resolved_accent_color(&self.app.config().theme.accent_color)
    }

    fn success_color(&self) -> Color {
        resolved_success_color(&self.app.config().theme.success_color)
    }

    fn warning_color(&self) -> Color {
        resolved_warning_color(&self.app.config().theme.warning_color)
    }

    fn background_color(&self) -> Color {
        Color::Black
    }

    fn text_color(&self) -> Color {
        Color::White
    }

    fn muted_color(&self) -> Color {
        Color::Gray
    }

    fn subtle_color(&self) -> Color {
        Color::DarkGray
    }

    fn notice_palette(&self) -> (&'static str, Color, Color) {
        if let Some(running) = self.running.as_ref() {
            match running.status() {
                Some(LastRunStatus::Completed) => (" DONE ", Color::Black, self.success_color()),
                Some(LastRunStatus::Failed) => (" FAIL ", Color::White, Color::Red),
                Some(LastRunStatus::Canceled) => (" CANCELED ", Color::Black, self.accent_color()),
                Some(LastRunStatus::MaxIterations) => {
                    (" LIMIT ", Color::Black, self.warning_color())
                }
                Some(LastRunStatus::NeverRun) | None => {
                    (" INFO ", Color::Black, self.accent_color())
                }
            }
        } else {
            (" INFO ", Color::Black, self.accent_color())
        }
    }
}

struct TuiRunDelegate {
    tx: std::sync::mpsc::Sender<UiEvent>,
}

#[async_trait]
impl RunDelegate for TuiRunDelegate {
    async fn on_event(&mut self, event: RunEvent) -> Result<()> {
        self.tx
            .send(UiEvent::RunEvent(event))
            .map_err(|_| anyhow!("TUI event channel closed"))?;
        Ok(())
    }

    async fn run_interactive_session(
        &mut self,
        config: &RunnerConfig,
        invocation: &InteractiveSessionInvocation,
    ) -> Result<Option<InteractiveSessionOutcome>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(UiEvent::InteractiveSession {
                config: config.clone(),
                invocation: invocation.clone(),
                reply: reply_tx,
            })
            .map_err(|_| anyhow!("TUI event channel closed"))?;
        let outcome = reply_rx
            .await
            .map_err(|_| anyhow!("interactive session reply channel closed"))?
            .map_err(anyhow::Error::msg)?;
        Ok(Some(outcome))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{
        RequestOrigin, RunningState, TuiApp, TuiLaunchOptions, TuiPreloadedRequest,
        TuiRequestSource,
    };
    use camino::Utf8PathBuf;
    use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};
    use ralph_app::RalphApp;
    use ralph_core::RunControl;
    use ratatui::layout::Rect;

    fn configure_test_config_home() -> Utf8PathBuf {
        let path = Utf8PathBuf::from_path_buf(
            std::env::temp_dir().join(format!("ralph-tui-test-config-{}", std::process::id())),
        )
        .unwrap();
        fs::create_dir_all(&path).unwrap();
        unsafe {
            std::env::set_var("RALPH_CONFIG_HOME", path.as_str());
        }
        path
    }

    fn temp_project_dir() -> tempfile::TempDir {
        configure_test_config_home();
        tempfile::tempdir().unwrap()
    }

    fn new_test_tui() -> TuiApp {
        let temp = temp_project_dir();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let app = RalphApp::load(project_dir).unwrap();
        let tui = TuiApp::new(
            app,
            runtime.handle().clone(),
            TuiLaunchOptions {
                preset_workflow: Some("bare".to_owned()),
                preloaded_request: Some(TuiPreloadedRequest {
                    source: TuiRequestSource::Argv,
                    text: "ship it".to_owned(),
                    file_path: None,
                }),
                workflow_options: Default::default(),
            },
        )
        .unwrap();
        std::mem::forget(temp);
        tui
    }

    fn output_for_scroll(running: &mut RunningState) -> String {
        let scroll = if running.follow { 0 } else { running.scroll };
        running.terminal.set_scrollback(scroll);
        running.terminal.screen().contents()
    }

    #[test]
    fn launch_options_preload_request_and_select_workflow() {
        let temp = temp_project_dir();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let app = RalphApp::load(project_dir).unwrap();
        let tui = TuiApp::new(
            app,
            runtime.handle().clone(),
            TuiLaunchOptions {
                preset_workflow: Some("task-based".to_owned()),
                preloaded_request: Some(TuiPreloadedRequest {
                    source: TuiRequestSource::Argv,
                    text: "ship it".to_owned(),
                    file_path: None,
                }),
                workflow_options: Default::default(),
            },
        )
        .unwrap();

        assert_eq!(tui.workflow_id, "task-based");
        assert_eq!(tui.request_text, "ship it");
        assert_eq!(tui.request_origin, RequestOrigin::Argv);
        assert!(tui.auto_start_run);
    }

    #[test]
    fn cycle_agent_persists_next_available_agent() {
        let temp = temp_project_dir();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        fs::create_dir_all(project_dir.join(".ralph").as_std_path()).unwrap();
        fs::write(
            project_dir.join(".ralph/config.toml").as_std_path(),
            r#"
default_agent = "one"
agent = "one"

[[agents]]
id = "one"
name = "One"
builtin = false

[agents.non_interactive]
mode = "shell"
command = "echo ok"
prompt_input = "argv"
prompt_env_var = "PROMPT"

[agents.interactive]
mode = "shell"
command = "echo ok"
prompt_input = "argv"
prompt_env_var = "PROMPT"

[[agents]]
id = "two"
name = "Two"
builtin = false

[agents.non_interactive]
mode = "shell"
command = "echo ok"
prompt_input = "argv"
prompt_env_var = "PROMPT"

[agents.interactive]
mode = "shell"
command = "echo ok"
prompt_input = "argv"
prompt_env_var = "PROMPT"
"#,
        )
        .unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let app = RalphApp::load(project_dir).unwrap();

        let mut tui = TuiApp::new(
            app,
            runtime.handle().clone(),
            TuiLaunchOptions {
                preset_workflow: Some("bare".to_owned()),
                preloaded_request: Some(TuiPreloadedRequest {
                    source: TuiRequestSource::Argv,
                    text: "ship it".to_owned(),
                    file_path: None,
                }),
                workflow_options: Default::default(),
            },
        )
        .unwrap();
        tui.cycle_agent(None).unwrap();

        assert_eq!(tui.app.agent_id(), "two");
    }

    #[test]
    fn running_scroll_changes_visible_output() {
        let mut tui = new_test_tui();
        let mut running = RunningState::new(RunControl::new());
        running.ensure_terminal_size(4, 24);
        running.push_terminal_text("line 1\nline 2\nline 3\nline 4\nline 5\nline 6");
        tui.running = Some(running);

        assert_eq!(tui.max_running_scroll(), 2);
        assert_eq!(
            output_for_scroll(tui.running.as_mut().unwrap()),
            "line 3\nline 4\nline 5\nline 6"
        );

        tui.scroll_running(1);
        assert_eq!(
            output_for_scroll(tui.running.as_mut().unwrap()),
            "line 2\nline 3\nline 4\nline 5"
        );

        tui.scroll_running(1);
        assert_eq!(
            output_for_scroll(tui.running.as_mut().unwrap()),
            "line 1\nline 2\nline 3\nline 4"
        );

        tui.scroll_running(-1);
        assert_eq!(
            output_for_scroll(tui.running.as_mut().unwrap()),
            "line 2\nline 3\nline 4\nline 5"
        );
    }

    #[test]
    fn mouse_wheel_scrolls_runner_output_panel() {
        let mut tui = new_test_tui();
        let mut running = RunningState::new(RunControl::new());
        running.ensure_terminal_size(4, 24);
        running.push_terminal_text("line 1\nline 2\nline 3\nline 4\nline 5\nline 6");
        tui.running = Some(running);
        tui.message = "run failed".to_owned();

        let output_area = tui.running_output_area(Rect::new(0, 0, 100, 30));
        tui.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: output_area.x + 1,
                row: output_area.y + 1,
                modifiers: KeyModifiers::empty(),
            },
            Rect::new(0, 0, 100, 30),
        );
        assert_eq!(tui.running.as_ref().unwrap().scroll, 1);

        tui.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::empty(),
            },
            Rect::new(0, 0, 100, 30),
        );
        assert_eq!(tui.running.as_ref().unwrap().scroll, 1);
    }
}
