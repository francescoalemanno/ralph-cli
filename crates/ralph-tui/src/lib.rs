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
    PlanningDraftDecision, PlanningDraftDecisionKind, PlanningDraftReview, PlanningQuestion,
    PlanningQuestionAnswer, RalphApp, RunDelegate, RunEvent, WorkflowRequestInput,
    WorkflowRunInput, format_iteration_banner,
};
use ralph_core::{
    LastRunStatus, RunControl, WorkflowDefinition, WorkflowRunSummary, WorkflowRuntimeRequest,
    atomic_write,
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use textwrap::Options as TextWrapOptions;
use tokio::{runtime::Handle, sync::oneshot};
use tui_textarea::{Input as TextAreaInput, Key as TextAreaKey, TextArea};
use ui::{normalize_terminal_text, ratatui_color, resume_terminal, styled_title, suspend_terminal};

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
    PlanningQuestion {
        question: PlanningQuestion,
        reply: oneshot::Sender<Result<PlanningQuestionAnswer, String>>,
    },
    PlanningDraftReview {
        draft: PlanningDraftReview,
        reply: oneshot::Sender<Result<PlanningDraftDecision, String>>,
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

#[derive(Debug, Clone)]
struct RequestEditTarget {
    path: Utf8PathBuf,
    kind: RequestEditKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestEditKind {
    TextDraft,
    File,
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

struct PlanningQuestionDialog {
    question: PlanningQuestion,
    selected: usize,
    custom_answer: String,
    reply: oneshot::Sender<Result<PlanningQuestionAnswer, String>>,
}

struct PlanningDraftDialog {
    draft: PlanningDraftReview,
    selected: usize,
    scroll: usize,
    mode: PlanningDraftMode,
    feedback: String,
    reply: oneshot::Sender<Result<PlanningDraftDecision, String>>,
}

enum PlanningDraftMode {
    Review,
    Revising { textarea: TextArea<'static> },
}

impl PlanningDraftDialog {
    fn is_revising(&self) -> bool {
        matches!(self.mode, PlanningDraftMode::Revising { .. })
    }
}

enum ActiveDialog {
    PlanningQuestion(PlanningQuestionDialog),
    PlanningDraft(PlanningDraftDialog),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DraftLineKind {
    Normal,
    Heading1,
    Heading2,
    Heading3,
    Quote,
    Code,
    Rule,
    Muted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DraftWrappedLine {
    text: String,
    kind: DraftLineKind,
}

struct PlanningDraftLayout {
    area: Rect,
    header: Rect,
    content: Rect,
    feedback: Option<Rect>,
    actions: Rect,
    help: Rect,
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
    active_dialog: Option<ActiveDialog>,
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
            active_dialog: None,
            auto_start_run: false,
        };
        let has_preloaded_request = launch.preloaded_request.is_some();
        if let Some(preload) = launch.preloaded_request {
            this.apply_preloaded_request(preload);
        }
        this.sync_request_mode();
        this.focus = this.request_focus().unwrap_or(Focus::RequestText);
        this.auto_start_run = has_preloaded_request || !this.uses_user_request();
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
        if self.active_dialog.is_some() {
            let size = terminal.size().context("failed to read terminal size")?;
            let screen = Rect::new(0, 0, size.width, size.height);
            return self.handle_dialog_key(key, screen);
        }

        if self.running.is_some() {
            return self.handle_running_key(key, terminal);
        }

        match key.code {
            KeyCode::Char('q') => return Ok(false),
            KeyCode::Char('a') => self.cycle_agent(None)?,
            KeyCode::Char('r') => self.start_run()?,
            KeyCode::Char('e') if self.can_edit_request() => self.edit_request(terminal)?,
            KeyCode::Left if self.can_switch_request_source() => self.request_mode_left(),
            KeyCode::Right if self.can_switch_request_source() => self.request_mode_right(),
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
            KeyCode::Char('e') if self.can_edit_request() => {
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

    fn handle_dialog_key(&mut self, key: KeyEvent, screen: Rect) -> Result<bool> {
        let revise_input_active = matches!(
            self.active_dialog.as_ref(),
            Some(ActiveDialog::PlanningDraft(dialog)) if dialog.is_revising()
        );
        if key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c')
                && key.modifiers.contains(KeyModifiers::CONTROL)
                && !revise_input_active)
        {
            if let Some(running) = &self.running {
                running.control.cancel();
            }
            self.cancel_active_dialog("planning interaction canceled");
            self.message = "planning interaction canceled".to_owned();
            return Ok(true);
        }

        let Some(dialog_state) = self.active_dialog.take() else {
            return Ok(true);
        };

        match dialog_state {
            ActiveDialog::PlanningQuestion(mut dialog) => {
                let choice_count = dialog.question.options.len() + 1;
                match key.code {
                    KeyCode::Up => {
                        dialog.selected = dialog.selected.saturating_sub(1);
                    }
                    KeyCode::Down => {
                        if dialog.selected + 1 < choice_count {
                            dialog.selected += 1;
                        }
                    }
                    KeyCode::Backspace if dialog.selected == dialog.question.options.len() => {
                        dialog.custom_answer.pop();
                    }
                    KeyCode::Char(ch)
                        if dialog.selected == dialog.question.options.len()
                            && (key.modifiers.is_empty()
                                || key.modifiers == KeyModifiers::SHIFT) =>
                    {
                        dialog.custom_answer.push(ch);
                    }
                    KeyCode::Enter => {
                        if dialog.selected < dialog.question.options.len() {
                            let answer = dialog.question.options[dialog.selected].clone();
                            let reply = dialog.reply;
                            let _ = reply.send(Ok(PlanningQuestionAnswer {
                                answer,
                                source: ralph_app::PlanningAnswerSource::Option,
                            }));
                            self.message = "planner answer captured".to_owned();
                            return Ok(true);
                        } else if !dialog.custom_answer.trim().is_empty() {
                            let reply = dialog.reply;
                            let _ = reply.send(Ok(PlanningQuestionAnswer {
                                answer: dialog.custom_answer.trim().to_owned(),
                                source: ralph_app::PlanningAnswerSource::Custom,
                            }));
                            self.message = "planner answer captured".to_owned();
                            return Ok(true);
                        } else {
                            self.message = "enter a custom answer".to_owned();
                        }
                    }
                    _ => {}
                }
                self.active_dialog = Some(ActiveDialog::PlanningQuestion(dialog));
            }
            ActiveDialog::PlanningDraft(mut dialog) => {
                match &mut dialog.mode {
                    PlanningDraftMode::Review => match key.code {
                        KeyCode::Left | KeyCode::BackTab | KeyCode::Char('h') => {
                            dialog.selected = dialog.selected.saturating_sub(1);
                        }
                        KeyCode::Right | KeyCode::Tab | KeyCode::Char('l') => {
                            if dialog.selected < 2 {
                                dialog.selected += 1;
                            }
                        }
                        KeyCode::Char('1') => dialog.selected = 0,
                        KeyCode::Char('2') => dialog.selected = 1,
                        KeyCode::Char('3') => dialog.selected = 2,
                        KeyCode::Up | KeyCode::Char('k') => {
                            dialog.scroll = dialog.scroll.saturating_sub(1);
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            let max_scroll =
                                self.max_planning_draft_scroll_for_screen(&dialog, screen);
                            dialog.scroll = (dialog.scroll + 1).min(max_scroll);
                        }
                        KeyCode::PageUp => {
                            let step = self.planning_draft_page_step(screen, dialog.is_revising());
                            dialog.scroll = dialog.scroll.saturating_sub(step);
                        }
                        KeyCode::PageDown => {
                            let step = self.planning_draft_page_step(screen, dialog.is_revising());
                            let max_scroll =
                                self.max_planning_draft_scroll_for_screen(&dialog, screen);
                            dialog.scroll = (dialog.scroll + step).min(max_scroll);
                        }
                        KeyCode::Home => {
                            dialog.scroll = 0;
                        }
                        KeyCode::End => {
                            dialog.scroll =
                                self.max_planning_draft_scroll_for_screen(&dialog, screen);
                        }
                        KeyCode::Enter => match dialog.selected {
                            0 => {
                                let reply = dialog.reply;
                                let _ = reply.send(Ok(PlanningDraftDecision {
                                    kind: PlanningDraftDecisionKind::Accept,
                                    feedback: None,
                                }));
                                self.message = "accepted plan draft".to_owned();
                                return Ok(true);
                            }
                            1 => {
                                dialog.mode = PlanningDraftMode::Revising {
                                    textarea: self.new_planning_revision_textarea(&dialog.feedback),
                                };
                                self.message = "editing revision feedback".to_owned();
                            }
                            2 => {
                                let reply = dialog.reply;
                                let _ = reply.send(Ok(PlanningDraftDecision {
                                    kind: PlanningDraftDecisionKind::Reject,
                                    feedback: None,
                                }));
                                self.message = "rejected plan draft".to_owned();
                                return Ok(true);
                            }
                            _ => {}
                        },
                        _ => {}
                    },
                    PlanningDraftMode::Revising { textarea } => {
                        let input: TextAreaInput = key.into();
                        match input {
                            TextAreaInput {
                                key: TextAreaKey::Char('s'),
                                ctrl: true,
                                ..
                            } => {
                                let feedback = textarea.lines().join("\n");
                                if feedback.trim().is_empty() {
                                    self.message = "enter revision feedback".to_owned();
                                } else {
                                    let reply = dialog.reply;
                                    let _ = reply.send(Ok(PlanningDraftDecision {
                                        kind: PlanningDraftDecisionKind::Revise,
                                        feedback: Some(feedback.trim().to_owned()),
                                    }));
                                    self.message = "revision feedback captured".to_owned();
                                    return Ok(true);
                                }
                            }
                            TextAreaInput {
                                key: TextAreaKey::Char('c'),
                                ctrl: true,
                                ..
                            } => {
                                dialog.feedback = textarea.lines().join("\n");
                                dialog.mode = PlanningDraftMode::Review;
                                self.message = "revision feedback editor canceled".to_owned();
                            }
                            input => {
                                textarea.input(input);
                                dialog.feedback = textarea.lines().join("\n");
                            }
                        }
                    }
                }
                self.active_dialog = Some(ActiveDialog::PlanningDraft(dialog));
            }
        }

        Ok(true)
    }

    fn cancel_active_dialog(&mut self, reason: &str) {
        match self.active_dialog.take() {
            Some(ActiveDialog::PlanningQuestion(dialog)) => {
                let _ = dialog.reply.send(Err(reason.to_owned()));
            }
            Some(ActiveDialog::PlanningDraft(dialog)) => {
                let _ = dialog.reply.send(Err(reason.to_owned()));
            }
            None => {}
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, screen: Rect) {
        if self.handle_dialog_mouse(mouse, screen) {
            return;
        }

        if self.running.is_none() || self.active_dialog.is_some() {
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

    fn handle_dialog_mouse(&mut self, mouse: MouseEvent, screen: Rect) -> bool {
        let Some(ActiveDialog::PlanningDraft(dialog)) = self.active_dialog.as_mut() else {
            return false;
        };

        let layout = planning_draft_layout(screen, dialog.is_revising());
        let within_dialog = mouse.column >= layout.area.x
            && mouse.column < layout.area.x.saturating_add(layout.area.width)
            && mouse.row >= layout.area.y
            && mouse.row < layout.area.y.saturating_add(layout.area.height);
        if !within_dialog {
            return false;
        }

        if dialog.is_revising() {
            return true;
        }

        let max_scroll = max_planning_draft_scroll(dialog, screen);
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                dialog.scroll = dialog.scroll.saturating_sub(1);
            }
            MouseEventKind::ScrollDown => {
                dialog.scroll = (dialog.scroll + 1).min(max_scroll);
            }
            _ => {}
        }
        true
    }

    fn handle_ui_event(
        &mut self,
        event: UiEvent,
        _terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
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
                        RunEvent::ParallelWorkerLaunched { channel_id, label } => {
                            running.push_terminal_text(&format!(
                                "\n[parallel:{channel_id}] launched {label}\n"
                            ));
                        }
                        RunEvent::ParallelWorkerStarted { channel_id, label } => {
                            running.push_terminal_text(&format!(
                                "\n[parallel:{channel_id}] started {label}\n"
                            ));
                        }
                        RunEvent::ParallelWorkerFinished {
                            channel_id,
                            label,
                            exit_code,
                        } => {
                            running.push_terminal_text(&format!(
                                "\n[parallel:{channel_id}] finished {label} (exit={exit_code})\n"
                            ));
                        }
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
            UiEvent::PlanningQuestion { question, reply } => {
                if let Some(running) = self.running.as_mut() {
                    running.push_terminal_text(&format!(
                        "\n[planner question] {}\n",
                        question.question
                    ));
                }
                self.message = "planner is waiting for your answer".to_owned();
                self.active_dialog = Some(ActiveDialog::PlanningQuestion(PlanningQuestionDialog {
                    question,
                    selected: 0,
                    custom_answer: String::new(),
                    reply,
                }));
            }
            UiEvent::PlanningDraftReview { draft, reply } => {
                if let Some(running) = self.running.as_mut() {
                    running.push_terminal_text(&format!(
                        "\n[plan draft ready] {}\n",
                        draft.target_path
                    ));
                }
                self.message = "review the plan draft".to_owned();
                self.active_dialog = Some(ActiveDialog::PlanningDraft(PlanningDraftDialog {
                    draft,
                    selected: 0,
                    scroll: 0,
                    mode: PlanningDraftMode::Review,
                    feedback: String::new(),
                    reply,
                }));
            }
        }

        Ok(())
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
        if !workflow.uses_request_token() {
            return Ok(WorkflowRequestInput::default());
        }

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
        if !self.uses_user_request() {
            return None;
        }

        self.workflow
            .request
            .as_ref()
            .and_then(|request| request.runtime.as_ref())
    }

    fn uses_user_request(&self) -> bool {
        self.workflow.uses_request_token()
    }

    fn can_edit_request(&self) -> bool {
        self.uses_user_request()
    }

    fn can_switch_request_source(&self) -> bool {
        self.runtime_request()
            .is_some_and(|runtime| runtime.argv && runtime.file_flag)
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
        if !self.can_edit_request() {
            return Err(anyhow!("this workflow does not accept request edits"));
        }
        let target = self.resolve_request_edit_target()?;
        self.prepare_request_edit_target(&target)?;
        suspend_terminal(terminal)?;
        let edit_result = edit_file(
            &target.path,
            self.app.config().editor_override.as_deref(),
            &self.app.config().theme,
        );
        resume_terminal(terminal)?;
        edit_result?;
        self.refresh_request_from_target(&target)?;
        self.message = format!(
            "updated request from {}",
            target.path.file_name().unwrap_or(target.path.as_str())
        );
        Ok(())
    }

    fn resolve_request_edit_target(&self) -> Result<RequestEditTarget> {
        if let Some(runtime) = self.runtime_request() {
            match self.request_mode {
                RequestMode::Text if runtime.argv => {
                    return Ok(RequestEditTarget {
                        path: self
                            .app
                            .project_dir()
                            .join(".ralph")
                            .join("request-drafts")
                            .join(format!("{}.md", self.workflow_id)),
                        kind: RequestEditKind::TextDraft,
                    });
                }
                RequestMode::File if runtime.file_flag => {
                    if !self.request_file.trim().is_empty() {
                        return Ok(RequestEditTarget {
                            path: self.resolve_project_relative_path(Utf8Path::new(
                                self.request_file.trim(),
                            )),
                            kind: RequestEditKind::File,
                        });
                    }
                }
                _ => {}
            }
        }

        if let Some(request) = &self.workflow.request
            && let Some(file) = &request.file
        {
            return Ok(RequestEditTarget {
                path: self.resolve_project_relative_path(&file.path),
                kind: RequestEditKind::File,
            });
        }

        if !self.request_file.trim().is_empty() {
            return Ok(RequestEditTarget {
                path: self.resolve_project_relative_path(Utf8Path::new(self.request_file.trim())),
                kind: RequestEditKind::File,
            });
        }

        if !self.request_text.trim().is_empty() {
            return Ok(RequestEditTarget {
                path: self
                    .app
                    .project_dir()
                    .join(".ralph")
                    .join("request-drafts")
                    .join(format!("{}.md", self.workflow_id)),
                kind: RequestEditKind::TextDraft,
            });
        }

        Err(anyhow!("no editable request is available"))
    }

    fn prepare_request_edit_target(&mut self, target: &RequestEditTarget) -> Result<()> {
        if let Some(parent) = target.path.parent() {
            std::fs::create_dir_all(parent.as_std_path())
                .with_context(|| format!("failed to create {}", parent))?;
        }

        if target.kind == RequestEditKind::TextDraft {
            let mut contents = self.request_text.clone();
            if !contents.ends_with('\n') {
                contents.push('\n');
            }
            atomic_write(&target.path, contents)
                .with_context(|| format!("failed to create request draft {}", target.path))?;
            return Ok(());
        }

        if target.path.exists() {
            return Ok(());
        }

        let mut contents = self.request_text.clone();
        if !contents.ends_with('\n') {
            contents.push('\n');
        }
        atomic_write(&target.path, contents)
            .with_context(|| format!("failed to create request draft {}", target.path))?;
        Ok(())
    }

    fn refresh_request_from_target(&mut self, target: &RequestEditTarget) -> Result<()> {
        self.request_text = self.app.read_utf8_file(&target.path)?;
        match target.kind {
            RequestEditKind::TextDraft => {
                self.request_file.clear();
                self.request_origin = RequestOrigin::Draft;
                self.request_mode = RequestMode::Text;
            }
            RequestEditKind::File => {
                self.request_file = target.path.to_string();
                self.request_origin = RequestOrigin::File;
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
            }
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
        if self.active_dialog.is_some() {
            self.draw_active_dialog(frame);
        }
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
        let footer = self.footer_text();

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
        if !self.uses_user_request() {
            return Text::from(vec![
                Line::from("This workflow does not use a user request."),
                Line::from(""),
                Line::from("The run starts immediately."),
            ]);
        }

        let Some(request) = self.workflow.request.as_ref() else {
            return Text::from(vec![
                Line::from("This workflow does not use a user request."),
                Line::from(""),
                Line::from("The run starts immediately."),
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
        let text = self.idle_output_panel_text();
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
            let theme = self.theme();
            Span::styled(
                label,
                Style::default()
                    .fg(ratatui_color(theme.accent.contrast()))
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
        let note = if !self.can_edit_request() {
            "This workflow does not use a request."
        } else if self.running.as_ref().is_some_and(RunningState::is_finished) {
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

    fn footer_text(&self) -> &'static str {
        if let Some(dialog) = self.active_dialog.as_ref() {
            return match dialog {
                ActiveDialog::PlanningQuestion(_) => {
                    "Enter submit  •  ↑/↓ move  •  Esc cancel interaction"
                }
                ActiveDialog::PlanningDraft(dialog) if dialog.is_revising() => {
                    "Ctrl-S submit feedback  •  Ctrl-C cancel revise editor  •  textarea keys move and edit"
                }
                ActiveDialog::PlanningDraft(_) => {
                    "1/2/3 or ←/→ choose action  •  wheel/↑/↓/PgUp/PgDn/Home/End scroll draft  •  Enter submit  •  Esc cancel"
                }
            };
        }

        if let Some(running) = self.running.as_ref() {
            return match (running.is_finished(), self.can_edit_request()) {
                (true, true) => {
                    "R rerun  •  E edit request  •  A cycle next agent  •  F/End follow  •  wheel/↑/↓/PgUp/PgDn scroll  •  Q quit"
                }
                (true, false) => {
                    "R rerun  •  A cycle next agent  •  F/End follow  •  wheel/↑/↓/PgUp/PgDn scroll  •  Q quit"
                }
                (false, true) => {
                    "Ctrl-C cancel run  •  A cycle next agent  •  E edit request  •  F/End follow  •  wheel/↑/↓/PgUp/PgDn scroll  •  Q quit"
                }
                (false, false) => {
                    "Ctrl-C cancel run  •  A cycle next agent  •  F/End follow  •  wheel/↑/↓/PgUp/PgDn scroll  •  Q quit"
                }
            };
        }

        match (self.can_switch_request_source(), self.can_edit_request()) {
            (true, true) => {
                "Enter/R run  •  ←/→ switch request source  •  E edit request  •  A cycle agent  •  Q quit"
            }
            (false, true) => "Enter/R run  •  E edit request  •  A cycle agent  •  Q quit",
            (_, false) => "A cycle agent  •  Q quit",
        }
    }

    fn idle_output_panel_text(&self) -> Text<'static> {
        let mut lines = vec![
            Line::from(vec![
                Span::styled("workflow ", Style::default().fg(self.subtle_color())),
                Span::styled(
                    self.workflow_id.clone(),
                    Style::default().fg(self.text_color()),
                ),
            ]),
            Line::from(""),
        ];

        if self.can_edit_request() {
            lines.push(Line::from("Press Enter or R to start the run."));
            lines.push(Line::from(
                "Use E to edit the request in a file-backed editor.",
            ));
        } else {
            lines.push(Line::from(
                "This workflow starts immediately because it does not use a user request.",
            ));
        }

        Text::from(lines)
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

    fn draw_active_dialog(&self, frame: &mut Frame<'_>) {
        match &self.active_dialog {
            Some(ActiveDialog::PlanningQuestion(dialog)) => {
                let area = centered_rect(80, 18, frame.area());
                frame.render_widget(Clear, area);
                let mut lines = vec![
                    Line::from(vec![
                        Span::styled(
                            "Question",
                            Style::default()
                                .fg(self.accent_color())
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(" "),
                        Span::styled(
                            "planner needs one answer",
                            Style::default().fg(self.muted_color()),
                        ),
                    ]),
                    Line::from(""),
                    Line::from(dialog.question.question.clone()),
                ];
                if let Some(context) = &dialog.question.context
                    && !context.trim().is_empty()
                {
                    lines.push(Line::from(""));
                    lines.push(Line::from(vec![
                        Span::styled("Context ", Style::default().fg(self.subtle_color())),
                        Span::styled(context.clone(), Style::default().fg(self.text_color())),
                    ]));
                }
                lines.push(Line::from(""));
                for (index, option) in dialog.question.options.iter().enumerate() {
                    lines.push(self.dialog_option_line(
                        dialog.selected == index,
                        &format!("{}.", index + 1),
                        option,
                    ));
                }
                let custom_index = dialog.question.options.len();
                lines.push(self.dialog_option_line(
                    dialog.selected == custom_index,
                    &format!("{}.", custom_index + 1),
                    "Other (type your own answer)",
                ));
                if dialog.selected == custom_index {
                    lines.push(Line::from(""));
                    let mut custom = dialog.custom_answer.clone();
                    custom.push('█');
                    if dialog.custom_answer.is_empty() {
                        custom = "Type your answer here█".to_owned();
                    }
                    lines.push(Line::from(vec![
                        Span::styled("Custom ", Style::default().fg(self.subtle_color())),
                        Span::styled(custom, Style::default().fg(self.text_color())),
                    ]));
                }
                lines.push(Line::from(""));
                lines.push(Line::from(vec![
                    Span::styled("Enter ", Style::default().fg(self.subtle_color())),
                    Span::styled("submit", Style::default().fg(self.text_color())),
                    Span::styled("  •  ", Style::default().fg(self.subtle_color())),
                    Span::styled("↑/↓", Style::default().fg(self.text_color())),
                    Span::styled(" move  •  ", Style::default().fg(self.subtle_color())),
                    Span::styled("Esc", Style::default().fg(self.text_color())),
                    Span::styled(" cancel", Style::default().fg(self.subtle_color())),
                ]));
                frame.render_widget(
                    Paragraph::new(Text::from(lines))
                        .block(
                            self.panel_block()
                                .title(self.title_line("Planning", "Question")),
                        )
                        .wrap(Wrap { trim: false }),
                    area,
                );
            }
            Some(ActiveDialog::PlanningDraft(dialog)) => {
                let layout = planning_draft_layout(frame.area(), dialog.is_revising());
                let modal_block = self
                    .panel_block()
                    .title(self.title_line("Planning", "Draft review"));
                let content_block = self.panel_block().title(self.title_line(
                    "Draft",
                    &self.planning_draft_scroll_label(dialog, frame.area()),
                ));
                let content_inner = content_block.inner(layout.content);
                let wrapped_lines =
                    wrap_planning_draft(&dialog.draft.draft, content_inner.width.max(1));
                let max_scroll = wrapped_lines
                    .len()
                    .saturating_sub(content_inner.height as usize);
                let scroll = dialog.scroll.min(max_scroll);
                let visible_lines = wrapped_lines
                    .iter()
                    .skip(scroll)
                    .take(content_inner.height as usize)
                    .map(|line| self.styled_planning_draft_line(line))
                    .collect::<Vec<_>>();

                frame.render_widget(Clear, layout.area);
                frame.render_widget(modal_block, layout.area);
                frame.render_widget(
                    Paragraph::new(Text::from(vec![
                        Line::from(vec![
                            Span::styled("Target ", Style::default().fg(self.subtle_color())),
                            Span::styled(
                                dialog.draft.target_path.to_string(),
                                Style::default().fg(self.text_color()),
                            ),
                        ]),
                        Line::from(vec![
                            Span::styled("Decision ", Style::default().fg(self.subtle_color())),
                            self.dialog_choice_span(dialog.selected == 0, "1. Accept"),
                            Span::raw(" "),
                            self.dialog_choice_span(dialog.selected == 1, "2. Revise"),
                            Span::raw(" "),
                            self.dialog_choice_span(dialog.selected == 2, "3. Reject"),
                        ]),
                    ]))
                    .wrap(Wrap { trim: false }),
                    layout.header,
                );
                frame.render_widget(
                    Paragraph::new(Text::from(visible_lines)).block(content_block),
                    layout.content,
                );

                if let Some(feedback_area) = layout.feedback
                    && let PlanningDraftMode::Revising { textarea } = &dialog.mode
                {
                    frame.render_widget(textarea, feedback_area);
                }

                let actions = if dialog.is_revising() {
                    Line::from(vec![
                        Span::styled("Revise ", Style::default().fg(self.subtle_color())),
                        Span::styled(
                            "Ctrl-S submits feedback",
                            Style::default().fg(self.text_color()),
                        ),
                        Span::styled("  •  ", Style::default().fg(self.subtle_color())),
                        Span::styled("Ctrl-C", Style::default().fg(self.text_color())),
                        Span::styled(
                            " returns to draft review",
                            Style::default().fg(self.subtle_color()),
                        ),
                    ])
                } else {
                    Line::from(vec![
                        Span::styled("Browse ", Style::default().fg(self.subtle_color())),
                        Span::styled(
                            "wheel/↑/↓/PgUp/PgDn/Home/End",
                            Style::default().fg(self.text_color()),
                        ),
                        Span::styled("  •  Action ", Style::default().fg(self.subtle_color())),
                        Span::styled("1/2/3 or ←/→", Style::default().fg(self.text_color())),
                        Span::styled("  •  ", Style::default().fg(self.subtle_color())),
                        Span::styled("Enter", Style::default().fg(self.text_color())),
                        Span::styled(" confirm", Style::default().fg(self.subtle_color())),
                    ])
                };
                frame.render_widget(
                    Paragraph::new(actions).wrap(Wrap { trim: true }),
                    layout.actions,
                );

                let help = if dialog.is_revising() {
                    Line::from(vec![
                        Span::styled("Draft ", Style::default().fg(self.subtle_color())),
                        Span::styled(
                            "stays visible while the revise textarea owns the keyboard",
                            Style::default().fg(self.text_color()),
                        ),
                    ])
                } else {
                    Line::from(vec![
                        Span::styled("Esc", Style::default().fg(self.text_color())),
                        Span::styled(
                            " cancels the planning interaction",
                            Style::default().fg(self.subtle_color()),
                        ),
                    ])
                };
                frame.render_widget(Paragraph::new(help).wrap(Wrap { trim: true }), layout.help);
            }
            None => {}
        }
    }

    fn dialog_option_line(&self, selected: bool, prefix: &str, text: &str) -> Line<'static> {
        let style = if selected {
            let theme = self.theme();
            Style::default()
                .fg(ratatui_color(theme.accent.contrast()))
                .bg(self.accent_color())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(self.text_color())
        };
        Line::from(vec![
            Span::styled(format!(" {prefix} "), style),
            Span::styled(text.to_owned(), style),
        ])
    }

    fn dialog_choice_span(&self, selected: bool, label: &str) -> Span<'static> {
        if selected {
            let theme = self.theme();
            Span::styled(
                format!(" {label} "),
                Style::default()
                    .fg(ratatui_color(theme.accent.contrast()))
                    .bg(self.accent_color())
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(format!(" {label} "), Style::default().fg(self.text_color()))
        }
    }

    fn planning_draft_page_step(&self, screen: Rect, show_feedback: bool) -> usize {
        planning_draft_page_step(screen, show_feedback)
    }

    fn max_planning_draft_scroll_for_screen(
        &self,
        dialog: &PlanningDraftDialog,
        screen: Rect,
    ) -> usize {
        max_planning_draft_scroll(dialog, screen)
    }

    fn planning_draft_scroll_label(&self, dialog: &PlanningDraftDialog, screen: Rect) -> String {
        planning_draft_scroll_label(dialog, screen)
    }

    fn styled_planning_draft_line(&self, line: &DraftWrappedLine) -> Line<'static> {
        let style = match line.kind {
            DraftLineKind::Normal => Style::default().fg(self.text_color()),
            DraftLineKind::Heading1 => Style::default()
                .fg(self.accent_color())
                .add_modifier(Modifier::BOLD),
            DraftLineKind::Heading2 => Style::default()
                .fg(self.text_color())
                .add_modifier(Modifier::BOLD),
            DraftLineKind::Heading3 => Style::default()
                .fg(self.muted_color())
                .add_modifier(Modifier::BOLD),
            DraftLineKind::Quote => Style::default()
                .fg(self.muted_color())
                .add_modifier(Modifier::ITALIC),
            DraftLineKind::Code => Style::default().fg(self.text_color()),
            DraftLineKind::Rule => Style::default().fg(self.subtle_color()),
            DraftLineKind::Muted => Style::default().fg(self.muted_color()),
        };
        Line::from(Span::styled(line.text.clone(), style))
    }

    fn new_planning_revision_textarea(&self, initial_text: &str) -> TextArea<'static> {
        let mut textarea = if initial_text.is_empty() {
            TextArea::default()
        } else {
            TextArea::new(initial_text.split('\n').map(str::to_owned).collect())
        };
        textarea.set_block(
            self.panel_block()
                .title(self.title_line("Revise", "Ctrl-S submit  ◆  Ctrl-C cancel")),
        );
        textarea.set_placeholder_text("Describe what should change in the plan.");
        textarea.set_placeholder_style(Style::default().fg(self.muted_color()));
        textarea.set_cursor_line_style(Style::default());
        textarea.set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
        textarea
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
        let theme = self.theme();
        styled_title(
            title,
            subtitle,
            ratatui_color(theme.text),
            ratatui_color(theme.subtle),
            ratatui_color(theme.muted),
        )
    }

    fn theme(&self) -> ralph_core::ResolvedTheme {
        self.app.config().theme.resolve()
    }

    fn accent_color(&self) -> Color {
        ratatui_color(self.theme().accent)
    }

    fn warning_color(&self) -> Color {
        ratatui_color(self.theme().warning)
    }

    fn background_color(&self) -> Color {
        ratatui_color(self.theme().background)
    }

    fn text_color(&self) -> Color {
        ratatui_color(self.theme().text)
    }

    fn muted_color(&self) -> Color {
        ratatui_color(self.theme().muted)
    }

    fn subtle_color(&self) -> Color {
        ratatui_color(self.theme().subtle)
    }

    fn notice_palette(&self) -> (&'static str, Color, Color) {
        let theme = self.theme();
        if let Some(running) = self.running.as_ref() {
            match running.status() {
                Some(LastRunStatus::Completed) => (
                    " DONE ",
                    ratatui_color(theme.success.contrast()),
                    ratatui_color(theme.success),
                ),
                Some(LastRunStatus::Failed) => (
                    " FAIL ",
                    ratatui_color(theme.error.contrast()),
                    ratatui_color(theme.error),
                ),
                Some(LastRunStatus::Canceled) => (
                    " CANCELED ",
                    ratatui_color(theme.accent.contrast()),
                    ratatui_color(theme.accent),
                ),
                Some(LastRunStatus::MaxIterations) => (
                    " LIMIT ",
                    ratatui_color(theme.warning.contrast()),
                    ratatui_color(theme.warning),
                ),
                Some(LastRunStatus::NeverRun) | None => (
                    " INFO ",
                    ratatui_color(theme.accent.contrast()),
                    ratatui_color(theme.accent),
                ),
            }
        } else {
            (
                " INFO ",
                ratatui_color(theme.accent.contrast()),
                ratatui_color(theme.accent),
            )
        }
    }
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let popup_width = width.min(area.width.saturating_sub(2)).max(20);
    let popup_height = height.min(area.height.saturating_sub(2)).max(8);
    Rect {
        x: area.x + (area.width.saturating_sub(popup_width)) / 2,
        y: area.y + (area.height.saturating_sub(popup_height)) / 2,
        width: popup_width,
        height: popup_height,
    }
}

fn planning_draft_layout(screen: Rect, show_feedback: bool) -> PlanningDraftLayout {
    let area = centered_rect(
        screen.width.saturating_sub(4),
        screen.height.saturating_sub(2),
        screen,
    );
    let inner = Block::default().borders(Borders::ALL).inner(area);
    let constraints = if show_feedback {
        vec![
            Constraint::Length(2),
            Constraint::Min(6),
            Constraint::Length(8),
            Constraint::Length(2),
            Constraint::Length(1),
        ]
    } else {
        vec![
            Constraint::Length(2),
            Constraint::Min(6),
            Constraint::Length(2),
            Constraint::Length(1),
        ]
    };
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    if show_feedback {
        PlanningDraftLayout {
            area,
            header: sections[0],
            content: sections[1],
            feedback: Some(sections[2]),
            actions: sections[3],
            help: sections[4],
        }
    } else {
        PlanningDraftLayout {
            area,
            header: sections[0],
            content: sections[1],
            feedback: None,
            actions: sections[2],
            help: sections[3],
        }
    }
}

fn planning_draft_content_inner(area: Rect) -> Rect {
    Block::default().borders(Borders::ALL).inner(area)
}

fn planning_draft_page_step(screen: Rect, show_feedback: bool) -> usize {
    let layout = planning_draft_layout(screen, show_feedback);
    let content_inner = planning_draft_content_inner(layout.content);
    content_inner.height.saturating_sub(1).max(1) as usize
}

fn max_planning_draft_scroll(dialog: &PlanningDraftDialog, screen: Rect) -> usize {
    let layout = planning_draft_layout(screen, dialog.is_revising());
    let content_inner = planning_draft_content_inner(layout.content);
    let wrapped = wrap_planning_draft(&dialog.draft.draft, content_inner.width.max(1));
    wrapped.len().saturating_sub(content_inner.height as usize)
}

fn planning_draft_scroll_label(dialog: &PlanningDraftDialog, screen: Rect) -> String {
    let layout = planning_draft_layout(screen, dialog.is_revising());
    let content_inner = planning_draft_content_inner(layout.content);
    let wrapped = wrap_planning_draft(&dialog.draft.draft, content_inner.width.max(1));
    if wrapped.is_empty() || content_inner.height == 0 {
        return "empty".to_owned();
    }

    let max_scroll = wrapped.len().saturating_sub(content_inner.height as usize);
    let scroll = dialog.scroll.min(max_scroll);
    let start = scroll + 1;
    let end = (scroll + content_inner.height as usize).min(wrapped.len());
    format!("{start}-{end} / {}", wrapped.len())
}

fn wrap_planning_draft(markdown: &str, width: u16) -> Vec<DraftWrappedLine> {
    let width = width.max(1) as usize;
    let mut lines = Vec::new();
    let mut in_code_block = false;

    for raw_line in markdown.lines() {
        let trimmed = raw_line.trim();

        if is_code_fence(trimmed) {
            in_code_block = !in_code_block;
            continue;
        }

        if trimmed.is_empty() {
            lines.push(DraftWrappedLine {
                text: String::new(),
                kind: DraftLineKind::Normal,
            });
            continue;
        }

        if in_code_block {
            push_wrapped_draft_lines(
                &mut lines,
                raw_line.trim_end(),
                width,
                "",
                "",
                DraftLineKind::Code,
            );
            continue;
        }

        if is_horizontal_rule(trimmed) {
            lines.push(DraftWrappedLine {
                text: "─".repeat(width.min(64).max(3)),
                kind: DraftLineKind::Rule,
            });
            continue;
        }

        if let Some((level, heading)) = parse_markdown_heading(trimmed) {
            let kind = match level {
                1 => DraftLineKind::Heading1,
                2 => DraftLineKind::Heading2,
                _ => DraftLineKind::Heading3,
            };
            push_wrapped_draft_lines(&mut lines, heading, width, "", "", kind);
            continue;
        }

        if let Some(quote) = trimmed
            .strip_prefix("> ")
            .or_else(|| trimmed.strip_prefix('>'))
        {
            push_wrapped_draft_lines(
                &mut lines,
                quote.trim(),
                width,
                "│ ",
                "│ ",
                DraftLineKind::Quote,
            );
            continue;
        }

        if let Some((first_indent, rest_indent, item, kind)) = parse_markdown_list_item(raw_line) {
            push_wrapped_draft_lines(&mut lines, item, width, &first_indent, &rest_indent, kind);
            continue;
        }

        push_wrapped_draft_lines(
            &mut lines,
            raw_line.trim_end(),
            width,
            "",
            "",
            DraftLineKind::Normal,
        );
    }

    if lines.is_empty() {
        lines.push(DraftWrappedLine {
            text: "<empty draft>".to_owned(),
            kind: DraftLineKind::Muted,
        });
    }

    lines
}

fn push_wrapped_draft_lines(
    lines: &mut Vec<DraftWrappedLine>,
    text: &str,
    width: usize,
    initial_indent: &str,
    subsequent_indent: &str,
    kind: DraftLineKind,
) {
    let wrapped = textwrap::wrap(
        text,
        TextWrapOptions::new(width)
            .initial_indent(initial_indent)
            .subsequent_indent(subsequent_indent),
    );
    if wrapped.is_empty() {
        lines.push(DraftWrappedLine {
            text: initial_indent.to_owned(),
            kind,
        });
        return;
    }

    for line in wrapped {
        lines.push(DraftWrappedLine {
            text: line.into_owned(),
            kind,
        });
    }
}

fn is_code_fence(line: &str) -> bool {
    line.starts_with("```") || line.starts_with("~~~")
}

fn is_horizontal_rule(line: &str) -> bool {
    matches!(line, "---" | "***" | "___")
}

fn parse_markdown_heading(line: &str) -> Option<(usize, &str)> {
    let level = line.chars().take_while(|ch| *ch == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let title = line.get(level..)?.strip_prefix(' ')?;
    Some((level, title.trim()))
}

fn parse_markdown_list_item(line: &str) -> Option<(String, String, &str, DraftLineKind)> {
    let indent_width = line.len().saturating_sub(line.trim_start().len());
    let indent = " ".repeat(indent_width.min(8));
    let trimmed = line.trim_start();

    for (marker, label) in [
        ("- [ ] ", "[ ] "),
        ("* [ ] ", "[ ] "),
        ("- [x] ", "[x] "),
        ("* [x] ", "[x] "),
        ("- [X] ", "[x] "),
        ("* [X] ", "[x] "),
        ("- ", "• "),
        ("* ", "• "),
        ("+ ", "• "),
    ] {
        if let Some(item) = trimmed.strip_prefix(marker) {
            let first_indent = format!("{indent}{label}");
            let rest_indent = format!("{indent}{}", " ".repeat(label.len()));
            return Some((
                first_indent,
                rest_indent,
                item.trim(),
                DraftLineKind::Normal,
            ));
        }
    }

    let digits = trimmed.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }

    let prefix = trimmed.get(..digits)?;
    let remainder = trimmed.get(digits..)?;
    let item = remainder.strip_prefix(". ")?;
    let label = format!("{prefix}. ");
    let first_indent = format!("{indent}{label}");
    let rest_indent = format!("{indent}{}", " ".repeat(label.len()));
    Some((
        first_indent,
        rest_indent,
        item.trim(),
        DraftLineKind::Normal,
    ))
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

    async fn answer_planning_question(
        &mut self,
        question: &PlanningQuestion,
    ) -> Result<PlanningQuestionAnswer> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(UiEvent::PlanningQuestion {
                question: question.clone(),
                reply: reply_tx,
            })
            .map_err(|_| anyhow!("TUI event channel closed"))?;
        reply_rx
            .await
            .map_err(|_| anyhow!("planning question reply channel closed"))?
            .map_err(anyhow::Error::msg)
    }

    async fn review_planning_draft(
        &mut self,
        draft: &PlanningDraftReview,
    ) -> Result<PlanningDraftDecision> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(UiEvent::PlanningDraftReview {
                draft: draft.clone(),
                reply: reply_tx,
            })
            .map_err(|_| anyhow!("TUI event channel closed"))?;
        reply_rx
            .await
            .map_err(|_| anyhow!("planning draft reply channel closed"))?
            .map_err(anyhow::Error::msg)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{
        ActiveDialog, DraftLineKind, PlanningDraftDialog, PlanningDraftMode, RequestEditKind,
        RequestMode, RequestOrigin, RunningState, TuiApp, TuiLaunchOptions, TuiPreloadedRequest,
        TuiRequestSource, planning_draft_layout, wrap_planning_draft,
    };
    use camino::Utf8PathBuf;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
    use ralph_app::{PlanningDraftDecisionKind, PlanningDraftReview, RalphApp};
    use ralph_core::{
        RunControl, ScopedGlobalConfigDirOverride, scoped_global_config_dir_override,
    };
    use ratatui::layout::Rect;
    use tokio::sync::oneshot;

    struct TestProjectDir {
        _config_home: ScopedGlobalConfigDirOverride,
        temp: tempfile::TempDir,
    }

    impl TestProjectDir {
        fn path(&self) -> &std::path::Path {
            self.temp.path()
        }
    }

    fn configure_test_config_home() -> ScopedGlobalConfigDirOverride {
        let path = Utf8PathBuf::from_path_buf(
            std::env::temp_dir().join(format!("ralph-tui-test-config-{}", std::process::id())),
        )
        .unwrap();
        fs::create_dir_all(&path).unwrap();
        scoped_global_config_dir_override(path)
    }

    fn temp_project_dir() -> TestProjectDir {
        TestProjectDir {
            _config_home: configure_test_config_home(),
            temp: tempfile::tempdir().unwrap(),
        }
    }

    fn new_test_tui() -> (TuiApp, TestProjectDir) {
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
        (tui, temp)
    }

    fn output_for_scroll(running: &mut RunningState) -> String {
        let scroll = if running.follow { 0 } else { running.scroll };
        running.terminal.set_scrollback(scroll);
        running.terminal.screen().contents()
    }

    fn new_planning_draft_dialog(
        draft: &str,
    ) -> (
        PlanningDraftDialog,
        oneshot::Receiver<Result<ralph_app::PlanningDraftDecision, String>>,
    ) {
        let (reply, reply_rx) = oneshot::channel();
        (
            PlanningDraftDialog {
                draft: PlanningDraftReview {
                    target_path: Utf8PathBuf::from("docs/plans/plan.md"),
                    draft: draft.to_owned(),
                },
                selected: 0,
                scroll: 0,
                mode: PlanningDraftMode::Review,
                feedback: String::new(),
                reply,
            },
            reply_rx,
        )
    }

    fn planning_draft_dialog(tui: &TuiApp) -> &PlanningDraftDialog {
        match tui.active_dialog.as_ref() {
            Some(ActiveDialog::PlanningDraft(dialog)) => dialog,
            _ => panic!("expected planning draft dialog"),
        }
    }

    fn planning_draft_textarea_text(tui: &TuiApp) -> Option<String> {
        match tui.active_dialog.as_ref() {
            Some(ActiveDialog::PlanningDraft(PlanningDraftDialog {
                mode: PlanningDraftMode::Revising { textarea },
                ..
            })) => Some(textarea.lines().join("\n")),
            _ => None,
        }
    }

    #[test]
    fn editing_argv_request_uses_current_text_instead_of_stale_draft() {
        let temp = temp_project_dir();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let draft_dir = project_dir.join(".ralph").join("request-drafts");
        fs::create_dir_all(draft_dir.as_std_path()).unwrap();
        let draft_path = draft_dir.join("bare.md");
        fs::write(draft_path.as_std_path(), "stale draft\n").unwrap();

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let app = RalphApp::load(project_dir).unwrap();
        let mut tui = TuiApp::new(
            app,
            runtime.handle().clone(),
            TuiLaunchOptions {
                preset_workflow: Some("bare".to_owned()),
                preloaded_request: Some(TuiPreloadedRequest {
                    source: TuiRequestSource::Argv,
                    text: "implement x".to_owned(),
                    file_path: None,
                }),
                workflow_options: Default::default(),
            },
        )
        .unwrap();

        let target = tui.resolve_request_edit_target().unwrap();
        assert_eq!(target.kind, RequestEditKind::TextDraft);
        tui.prepare_request_edit_target(&target).unwrap();

        assert_eq!(
            fs::read_to_string(target.path.as_std_path()).unwrap(),
            "implement x\n"
        );
    }

    #[test]
    fn edited_argv_request_stays_in_text_mode_for_reruns() {
        let temp = temp_project_dir();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let app = RalphApp::load(project_dir).unwrap();
        let mut tui = TuiApp::new(
            app,
            runtime.handle().clone(),
            TuiLaunchOptions {
                preset_workflow: Some("bare".to_owned()),
                preloaded_request: Some(TuiPreloadedRequest {
                    source: TuiRequestSource::Argv,
                    text: "implement x".to_owned(),
                    file_path: None,
                }),
                workflow_options: Default::default(),
            },
        )
        .unwrap();

        let target = tui.resolve_request_edit_target().unwrap();
        tui.prepare_request_edit_target(&target).unwrap();
        fs::write(target.path.as_std_path(), "edited request\n").unwrap();
        tui.refresh_request_from_target(&target).unwrap();

        assert_eq!(tui.request_mode, RequestMode::Text);
        assert_eq!(tui.request_origin, RequestOrigin::Draft);
        assert!(tui.request_file.is_empty());
        assert_eq!(tui.request_text, "edited request\n");

        let request_input = tui.request_input_for(&tui.workflow).unwrap();
        assert_eq!(request_input.argv.as_deref(), Some("edited request\n"));
        assert!(request_input.request_file.is_none());
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
                preset_workflow: Some("default".to_owned()),
                preloaded_request: Some(TuiPreloadedRequest {
                    source: TuiRequestSource::Argv,
                    text: "ship it".to_owned(),
                    file_path: None,
                }),
                workflow_options: Default::default(),
            },
        )
        .unwrap();

        assert_eq!(tui.workflow_id, "default");
        assert_eq!(tui.request_text, "ship it");
        assert_eq!(tui.request_origin, RequestOrigin::Argv);
        assert!(tui.auto_start_run);
    }

    #[test]
    fn workflows_without_request_token_ignore_request_input() {
        let temp = temp_project_dir();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let app = RalphApp::load(project_dir).unwrap();
        let tui = TuiApp::new(
            app,
            runtime.handle().clone(),
            TuiLaunchOptions {
                preset_workflow: Some("test-workflow".to_owned()),
                preloaded_request: None,
                workflow_options: Default::default(),
            },
        )
        .unwrap();

        let request_input = tui.request_input_for(&tui.workflow).unwrap();
        assert!(request_input.argv.is_none());
        assert!(request_input.stdin.is_none());
        assert!(request_input.request_file.is_none());
        assert!(tui.auto_start_run);
        assert!(!tui.can_edit_request());
        assert_eq!(tui.footer_text(), "A cycle agent  •  Q quit");
        assert!(
            !tui.idle_output_panel_text()
                .to_string()
                .contains("Use E to edit the request")
        );
        assert!(
            !tui.request_panel_text()
                .to_string()
                .contains("Press Enter or R to run.")
        );
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

[agents.runner]
mode = "shell"
command = "echo ok"
prompt_input = "argv"
prompt_env_var = "PROMPT"

[[agents]]
id = "two"
name = "Two"
builtin = false

[agents.runner]
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
        let (mut tui, _temp) = new_test_tui();
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
        let (mut tui, _temp) = new_test_tui();
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

    #[test]
    fn planning_draft_dialog_scrolls_with_keyboard() {
        let (mut tui, _temp) = new_test_tui();
        let draft = (1..=48)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (dialog, _reply_rx) = new_planning_draft_dialog(&draft);
        tui.active_dialog = Some(ActiveDialog::PlanningDraft(dialog));
        let screen = Rect::new(0, 0, 100, 30);

        tui.handle_dialog_key(KeyEvent::from(KeyCode::Down), screen)
            .unwrap();
        assert_eq!(planning_draft_dialog(&tui).scroll, 1);
        assert_eq!(planning_draft_dialog(&tui).selected, 0);

        tui.handle_dialog_key(KeyEvent::from(KeyCode::PageDown), screen)
            .unwrap();
        assert!(planning_draft_dialog(&tui).scroll > 1);

        tui.handle_dialog_key(KeyEvent::from(KeyCode::Right), screen)
            .unwrap();
        assert_eq!(planning_draft_dialog(&tui).selected, 1);

        tui.handle_dialog_key(KeyEvent::from(KeyCode::End), screen)
            .unwrap();
        let max_scroll =
            tui.max_planning_draft_scroll_for_screen(planning_draft_dialog(&tui), screen);
        assert_eq!(planning_draft_dialog(&tui).scroll, max_scroll);

        tui.handle_dialog_key(KeyEvent::from(KeyCode::Home), screen)
            .unwrap();
        assert_eq!(planning_draft_dialog(&tui).scroll, 0);
    }

    #[test]
    fn mouse_wheel_scrolls_planning_draft_dialog() {
        let (mut tui, _temp) = new_test_tui();
        let draft = (1..=48)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (dialog, _reply_rx) = new_planning_draft_dialog(&draft);
        tui.active_dialog = Some(ActiveDialog::PlanningDraft(dialog));
        let screen = Rect::new(0, 0, 100, 30);
        let layout = planning_draft_layout(screen, false);

        tui.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: layout.area.x + 1,
                row: layout.area.y + 1,
                modifiers: KeyModifiers::empty(),
            },
            screen,
        );
        assert_eq!(planning_draft_dialog(&tui).scroll, 1);

        tui.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: layout.area.x + 1,
                row: layout.area.y + 1,
                modifiers: KeyModifiers::empty(),
            },
            screen,
        );
        assert_eq!(planning_draft_dialog(&tui).scroll, 0);
    }

    #[test]
    fn revise_feedback_is_edited_in_textarea_after_confirming_revise() {
        let (mut tui, _temp) = new_test_tui();
        let (dialog, reply_rx) = new_planning_draft_dialog("# Title");
        tui.active_dialog = Some(ActiveDialog::PlanningDraft(dialog));
        let screen = Rect::new(0, 0, 100, 30);

        tui.handle_dialog_key(KeyEvent::from(KeyCode::Char('2')), screen)
            .unwrap();
        tui.handle_dialog_key(KeyEvent::from(KeyCode::Enter), screen)
            .unwrap();

        assert!(planning_draft_dialog(&tui).is_revising());
        assert_eq!(planning_draft_dialog(&tui).selected, 1);

        tui.handle_dialog_key(KeyEvent::from(KeyCode::Char('q')), screen)
            .unwrap();
        assert_eq!(planning_draft_textarea_text(&tui).as_deref(), Some("q"));

        tui.handle_dialog_key(KeyEvent::from(KeyCode::Left), screen)
            .unwrap();
        assert_eq!(planning_draft_dialog(&tui).selected, 1);
        assert!(planning_draft_dialog(&tui).is_revising());

        tui.handle_dialog_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            screen,
        )
        .unwrap();
        assert!(!planning_draft_dialog(&tui).is_revising());
        assert_eq!(planning_draft_dialog(&tui).feedback, "q");

        tui.handle_dialog_key(KeyEvent::from(KeyCode::Enter), screen)
            .unwrap();
        assert_eq!(planning_draft_textarea_text(&tui).as_deref(), Some("q"));

        tui.handle_dialog_key(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            screen,
        )
        .unwrap();
        assert!(tui.active_dialog.is_none());

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let decision = runtime
            .block_on(reply_rx)
            .unwrap()
            .expect("revision should be accepted");
        assert_eq!(decision.kind, PlanningDraftDecisionKind::Revise);
        assert_eq!(decision.feedback.as_deref(), Some("q"));
    }

    #[test]
    fn planning_draft_markdown_wrapping_formats_common_blocks() {
        let lines = wrap_planning_draft(
            "# Title\n\n- item one\n1. item two\n> quoted\n```\ncode sample\n```",
            40,
        );

        assert_eq!(lines[0].text, "Title");
        assert_eq!(lines[0].kind, DraftLineKind::Heading1);
        assert_eq!(lines[2].text, "• item one");
        assert_eq!(lines[3].text, "1. item two");
        assert_eq!(lines[4].text, "│ quoted");
        assert_eq!(lines[5].text, "code sample");
        assert_eq!(lines[5].kind, DraftLineKind::Code);
    }
}
