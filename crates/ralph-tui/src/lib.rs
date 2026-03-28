use std::{
    env,
    fs::OpenOptions,
    io::{self, Read, Write},
    sync::LazyLock,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event as CEvent, KeyCode, KeyEvent,
        KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ralph_app::{RalphApp, RunDelegate, RunEvent};
use ralph_core::{ClarificationRequest, ReviewData, RunControl, SpecSummary, WorkflowState};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap,
    },
};
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, Style as SyntectStyle, Theme, ThemeSet},
    parsing::SyntaxSet,
    util::LinesWithEndings,
};
use tokio::{runtime::Handle, sync::oneshot};
use tui_textarea::{Input as TextInput, TextArea};
use unicode_width::UnicodeWidthChar;

pub fn run_tui(app: RalphApp) -> Result<()> {
    let handle = Handle::current();
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .context("failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("failed to create terminal backend")?;

    let result = TuiApp::new(app, handle).run(&mut terminal);

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
    Key(KeyEvent),
    Mouse(MouseEvent),
    Resize,
    SpecsLoaded(Result<Vec<SpecSummary>, String>),
    ReviewLoaded(Result<ReviewData, String>),
    OperationDone(Result<SpecSummary, String>),
    RunMessage(RunEvent),
    ClarificationRequested(ClarificationRequest, oneshot::Sender<Option<String>>),
}

enum Screen {
    Dashboard,
    Scoped,
    Composer(ComposerKind),
    Review,
    Running,
}

enum ComposerKind {
    Create,
    Revise(String),
    Replan(String),
}

struct ClarificationModal {
    request: ClarificationRequest,
    responder: oneshot::Sender<Option<String>>,
    input: TextArea<'static>,
}

#[derive(Debug, Clone, Copy)]
enum NoticeTone {
    Info,
    Success,
    Error,
}

struct Notice {
    tone: NoticeTone,
    message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColorMode {
    Light,
    Dark,
}

struct TuiApp {
    app: RalphApp,
    handle: Handle,
    tx: Sender<UiEvent>,
    rx: Receiver<UiEvent>,
    screen: Screen,
    specs: Vec<SpecSummary>,
    selected: usize,
    status: String,
    composer: TextArea<'static>,
    review: Option<ReviewData>,
    review_tab: usize,
    dashboard_preview_scroll: u16,
    scoped_scroll: u16,
    review_scroll: u16,
    run_logs: Vec<String>,
    run_scroll: u16,
    run_follow: bool,
    run_target: Option<String>,
    active_control: Option<RunControl>,
    pending: bool,
    color_mode: ColorMode,
    clarification: Option<ClarificationModal>,
    clarification_scroll: u16,
    notice: Option<Notice>,
    focus_spec_path: Option<String>,
    pending_editor_target: Option<String>,
    input_suspended: Arc<AtomicBool>,
    should_quit: bool,
}

impl TuiApp {
    fn new(app: RalphApp, handle: Handle) -> Self {
        let (tx, rx) = mpsc::channel();
        let color_mode = detect_color_mode();
        let composer = fresh_composer(
            "Planning Request",
            resolved_accent_color(&app.config().theme.accent_color, color_mode),
        );

        Self {
            app,
            handle,
            tx,
            rx,
            screen: Screen::Dashboard,
            specs: Vec::new(),
            selected: 0,
            status: "Loading specs…".to_owned(),
            composer,
            review: None,
            review_tab: 0,
            dashboard_preview_scroll: 0,
            scoped_scroll: 0,
            review_scroll: 0,
            run_logs: Vec::new(),
            run_scroll: 0,
            run_follow: true,
            run_target: None,
            active_control: None,
            pending: false,
            color_mode,
            clarification: None,
            clarification_scroll: 0,
            notice: None,
            focus_spec_path: None,
            pending_editor_target: None,
            input_suspended: Arc::new(AtomicBool::new(false)),
            should_quit: false,
        }
    }

    fn run(mut self, terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
        self.spawn_event_thread();
        self.load_specs();

        while !self.should_quit {
            terminal.draw(|frame| self.draw(frame))?;
            let event = self.rx.recv().context("event channel closed")?;
            self.handle_event(event);
            if let Some(target) = self.pending_editor_target.take() {
                self.perform_edit(terminal, target)?;
            }
        }

        Ok(())
    }

    fn spawn_event_thread(&self) {
        let tx = self.tx.clone();
        let input_suspended = self.input_suspended.clone();
        thread::spawn(move || {
            loop {
                if input_suspended.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(50));
                    continue;
                }
                if event::poll(Duration::from_millis(120)).ok() == Some(true) {
                    match event::read() {
                        Ok(CEvent::Key(key)) if key.kind == KeyEventKind::Press => {
                            if tx.send(UiEvent::Key(key)).is_err() {
                                break;
                            }
                        }
                        Ok(CEvent::Mouse(mouse)) => {
                            if tx.send(UiEvent::Mouse(mouse)).is_err() {
                                break;
                            }
                        }
                        Ok(CEvent::Resize(_, _)) => {
                            if tx.send(UiEvent::Resize).is_err() {
                                break;
                            }
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                } else if tx.send(UiEvent::Tick).is_err() {
                    break;
                }
            }
        });
    }

    fn load_specs(&mut self) {
        self.status = "Loading specs…".to_owned();
        let tx = self.tx.clone();
        let app = self.app.clone();
        self.handle.spawn(async move {
            let result = app.list_specs().map_err(|error| error.to_string());
            let _ = tx.send(UiEvent::SpecsLoaded(result));
        });
    }

    fn load_review(&mut self, target: String) {
        self.pending = true;
        self.status = format!("Loading review for {target}");
        let tx = self.tx.clone();
        let app = self.app.clone();
        self.handle.spawn(async move {
            let result = app
                .review_target(&target)
                .map_err(|error| error.to_string());
            let _ = tx.send(UiEvent::ReviewLoaded(result));
        });
    }

    fn run_create(&mut self, request: String) {
        let control = RunControl::new();
        self.pending = true;
        self.active_control = Some(control.clone());
        self.screen = Screen::Running;
        self.run_target = Some("new spec".to_owned());
        self.run_logs.clear();
        self.run_scroll = 0;
        self.run_follow = true;
        self.status = "Running planner…".to_owned();
        let tx = self.tx.clone();
        let app = self.app.clone();
        self.handle.spawn(async move {
            let mut delegate = ChannelDelegate { tx: tx.clone() };
            let result = app
                .create_new_with_control(&request, control, &mut delegate)
                .await
                .map_err(|error| error.to_string());
            let _ = tx.send(UiEvent::OperationDone(result));
        });
    }

    fn run_revise(&mut self, target: String, request: String) {
        let control = RunControl::new();
        self.pending = true;
        self.active_control = Some(control.clone());
        self.screen = Screen::Running;
        self.run_target = Some(target.clone());
        self.run_logs.clear();
        self.run_scroll = 0;
        self.run_follow = true;
        self.status = format!("Revising {target}");
        let tx = self.tx.clone();
        let app = self.app.clone();
        self.handle.spawn(async move {
            let mut delegate = ChannelDelegate { tx: tx.clone() };
            let result = app
                .revise_target_with_control(&target, &request, control, &mut delegate)
                .await
                .map_err(|error| error.to_string());
            let _ = tx.send(UiEvent::OperationDone(result));
        });
    }

    fn run_replan(&mut self, target: String, request: String) {
        let control = RunControl::new();
        self.pending = true;
        self.active_control = Some(control.clone());
        self.screen = Screen::Running;
        self.run_target = Some(target.clone());
        self.run_logs.clear();
        self.run_scroll = 0;
        self.run_follow = true;
        self.status = format!("Replanning {target}");
        let tx = self.tx.clone();
        let app = self.app.clone();
        self.handle.spawn(async move {
            let mut delegate = ChannelDelegate { tx: tx.clone() };
            let result = app
                .replan_target_with_control(&target, &request, control, &mut delegate)
                .await
                .map_err(|error| error.to_string());
            let _ = tx.send(UiEvent::OperationDone(result));
        });
    }

    fn run_builder(&mut self, target: String) {
        let control = RunControl::new();
        self.pending = true;
        self.active_control = Some(control.clone());
        self.screen = Screen::Running;
        self.run_target = Some(target.clone());
        self.run_logs.clear();
        self.run_scroll = 0;
        self.run_follow = true;
        self.status = format!("Running {target}");
        let tx = self.tx.clone();
        let app = self.app.clone();
        self.handle.spawn(async move {
            let mut delegate = ChannelDelegate { tx: tx.clone() };
            let result = app
                .run_target_with_control(&target, control, &mut delegate)
                .await
                .map_err(|error| error.to_string());
            let _ = tx.send(UiEvent::OperationDone(result));
        });
    }

    fn run_edit(&mut self, target: String) {
        self.status = format!("Opening editor for {target}");
        self.pending_editor_target = Some(target);
    }

    fn handle_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::Tick => {}
            UiEvent::Resize => {}
            UiEvent::Key(key) => self.handle_key(key),
            UiEvent::Mouse(mouse) => self.handle_mouse(mouse),
            UiEvent::SpecsLoaded(result) => match result {
                Ok(specs) => {
                    self.specs = specs;
                    if let Some(path) = self.focus_spec_path.take() {
                        if let Some(index) = self
                            .specs
                            .iter()
                            .position(|summary| summary.spec_path.as_str() == path)
                        {
                            self.selected = index;
                            self.screen = Screen::Scoped;
                            self.scoped_scroll = 0;
                        }
                    }
                    if self.selected >= self.specs.len() {
                        self.selected = self.specs.len().saturating_sub(1);
                    }
                    self.status = if self.specs.is_empty() {
                        "No specs yet. Press n to create one.".to_owned()
                    } else {
                        format!("Loaded {} specs", self.specs.len())
                    };
                }
                Err(error) => self.status = error,
            },
            UiEvent::ReviewLoaded(result) => {
                self.pending = false;
                match result {
                    Ok(review) => {
                        self.review = Some(review);
                        self.review_tab = 0;
                        self.review_scroll = 0;
                        self.screen = Screen::Review;
                        self.status = "Review loaded".to_owned();
                    }
                    Err(error) => self.status = error,
                }
            }
            UiEvent::OperationDone(result) => {
                self.pending = false;
                self.active_control = None;
                self.clarification = None;
                match result {
                    Ok(summary) => {
                        self.run_target = Some(summary.spec_path.to_string());
                        self.status = format!("Updated {}", summary.spec_path);
                        self.focus_spec_path = Some(summary.spec_path.to_string());
                        self.notice = Some(Notice {
                            tone: NoticeTone::Success,
                            message: format!(
                                "{} complete: {}",
                                if summary.state == WorkflowState::Completed {
                                    "Workflow"
                                } else {
                                    "Planning"
                                },
                                summary.spec_path
                            ),
                        });
                        self.load_specs();
                    }
                    Err(error) => {
                        self.status = error.clone();
                        self.run_logs.push(format!("error: {error}"));
                        self.notice = Some(Notice {
                            tone: NoticeTone::Error,
                            message: error,
                        });
                    }
                }
            }
            UiEvent::RunMessage(message) => self.apply_run_event(message),
            UiEvent::ClarificationRequested(request, responder) => {
                self.status = "Clarification required".to_owned();
                self.clarification_scroll = 0;
                let mut input = TextArea::default();
                input.set_block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Clarification")
                        .border_type(BorderType::Rounded),
                );
                input.set_cursor_line_style(Style::default().fg(self.accent_color()));
                self.clarification = Some(ClarificationModal {
                    request,
                    responder,
                    input,
                });
            }
        }
    }

    fn apply_run_event(&mut self, event: RunEvent) {
        match event {
            RunEvent::IterationStarted {
                mode,
                iteration,
                max_iterations,
            } => {
                self.run_logs.push(format!(
                    "[{} {}/{}]",
                    mode.as_str(),
                    iteration,
                    max_iterations
                ));
            }
            RunEvent::Stdout(chunk) => self.run_logs.push(normalize_stream_chunk(&chunk)),
            RunEvent::Stderr(chunk) => self.run_logs.push(normalize_stream_chunk(&chunk)),
            RunEvent::Note(note) => self.run_logs.push(format!("note: {note}")),
            RunEvent::Finished {
                mode,
                completed,
                summary,
            } => {
                self.run_logs.push(summary);
                self.status = format!(
                    "{} {}",
                    mode.as_str(),
                    if completed { "completed" } else { "finished" }
                );
                self.notice = Some(Notice {
                    tone: NoticeTone::Success,
                    message: self.status.clone(),
                });
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if self.pending
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('c')
        {
            self.cancel_active_operation();
            return;
        }

        if self.clarification.is_some() {
            self.handle_clarification_key(key);
            return;
        }

        match self.screen {
            Screen::Dashboard => self.handle_dashboard_key(key),
            Screen::Scoped => self.handle_scoped_key(key),
            Screen::Composer(_) => self.handle_composer_key(key),
            Screen::Review => self.handle_review_key(key),
            Screen::Running => self.handle_running_key(key),
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        let delta = match mouse.kind {
            MouseEventKind::ScrollUp => Some(ScrollDelta::Lines(-3)),
            MouseEventKind::ScrollDown => Some(ScrollDelta::Lines(3)),
            _ => None,
        };

        let Some(delta) = delta else {
            return;
        };

        if self.clarification.is_some() {
            self.clarification_scroll = apply_scroll_delta(self.clarification_scroll, delta, false);
            return;
        }

        match self.screen {
            Screen::Dashboard => {
                self.dashboard_preview_scroll =
                    apply_scroll_delta(self.dashboard_preview_scroll, delta, false);
            }
            Screen::Scoped => {
                self.scoped_scroll = apply_scroll_delta(self.scoped_scroll, delta, false);
            }
            Screen::Composer(_) => {}
            Screen::Review => {
                self.review_scroll = apply_scroll_delta(self.review_scroll, delta, false);
            }
            Screen::Running => {
                self.run_follow = false;
                self.run_scroll = apply_scroll_delta(self.run_scroll, delta, true);
            }
        }
    }

    fn handle_dashboard_key(&mut self, key: KeyEvent) {
        if let Some(delta) = scroll_delta(key) {
            self.dashboard_preview_scroll =
                apply_scroll_delta(self.dashboard_preview_scroll, delta, false);
            return;
        }

        match key.code {
            KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < self.specs.len() {
                    self.selected += 1;
                    self.dashboard_preview_scroll = 0;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                self.dashboard_preview_scroll = 0;
            }
            KeyCode::Char('o')
                if key.modifiers.contains(KeyModifiers::CONTROL) && !self.specs.is_empty() =>
            {
                self.screen = Screen::Scoped
            }
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.composer = fresh_composer("Create New Spec", self.accent_color());
                self.screen = Screen::Composer(ComposerKind::Create);
            }
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.load_specs()
            }
            _ => {}
        }
    }

    fn handle_scoped_key(&mut self, key: KeyEvent) {
        let Some(target) = self.selected_target() else {
            self.screen = Screen::Dashboard;
            return;
        };

        if let Some(delta) = scroll_delta(key) {
            self.scoped_scroll = apply_scroll_delta(self.scoped_scroll, delta, false);
            return;
        }

        match key.code {
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.screen = Screen::Dashboard
            }
            KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true
            }
            KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.review_scroll = 0;
                self.load_review(target)
            }
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.run_builder(target)
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.run_edit(target)
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.composer = fresh_composer("Revise Spec", self.accent_color());
                self.screen = Screen::Composer(ComposerKind::Revise(target));
            }
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.composer = fresh_composer("Replan From Scratch", self.accent_color());
                self.screen = Screen::Composer(ComposerKind::Replan(target));
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scoped_scroll =
                    apply_scroll_delta(self.scoped_scroll, ScrollDelta::Lines(1), false);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.scoped_scroll =
                    apply_scroll_delta(self.scoped_scroll, ScrollDelta::Lines(-1), false);
            }
            _ => {}
        }
    }

    fn handle_composer_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('w') {
            self.screen = match self.screen {
                Screen::Composer(ComposerKind::Create) => Screen::Dashboard,
                Screen::Composer(_) => Screen::Scoped,
                _ => Screen::Dashboard,
            };
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            let request = self.composer.lines().join("\n").trim().to_owned();
            if request.is_empty() {
                self.status = "Planning request cannot be empty".to_owned();
                return;
            }
            match std::mem::replace(&mut self.screen, Screen::Running) {
                Screen::Composer(ComposerKind::Create) => self.run_create(request),
                Screen::Composer(ComposerKind::Revise(target)) => self.run_revise(target, request),
                Screen::Composer(ComposerKind::Replan(target)) => self.run_replan(target, request),
                other => self.screen = other,
            }
            return;
        }

        self.composer.input(TextInput::from(key));
    }

    fn handle_review_key(&mut self, key: KeyEvent) {
        if let Some(delta) = scroll_delta(key) {
            self.review_scroll = apply_scroll_delta(self.review_scroll, delta, false);
            return;
        }

        match key.code {
            KeyCode::Down => {
                self.review_scroll =
                    apply_scroll_delta(self.review_scroll, ScrollDelta::Lines(5), false);
            }
            KeyCode::Up => {
                self.review_scroll =
                    apply_scroll_delta(self.review_scroll, ScrollDelta::Lines(-5), false);
            }
            KeyCode::Left => self.review_tab = 0,
            KeyCode::Right => self.review_tab = 1,
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.screen = Screen::Scoped
            }
            KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.review_tab = 0
            }
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.review_tab = 1
            }
            KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true
            }
            _ => {}
        }
    }

    fn handle_running_key(&mut self, key: KeyEvent) {
        if let Some(delta) = scroll_delta(key) {
            self.run_follow = matches!(delta, ScrollDelta::End);
            self.run_scroll = apply_scroll_delta(self.run_scroll, delta, true);
            return;
        }

        match key.code {
            KeyCode::Char('w')
                if key.modifiers.contains(KeyModifiers::CONTROL) && !self.pending =>
            {
                self.screen = Screen::Scoped
            }
            KeyCode::Char('q')
                if key.modifiers.contains(KeyModifiers::CONTROL) && !self.pending =>
            {
                self.should_quit = true
            }
            _ => {}
        }
    }

    fn cancel_active_operation(&mut self) {
        if let Some(control) = &self.active_control {
            control.cancel();
            self.status = "Canceling agent subprocess…".to_owned();
            self.run_logs.push("cancel requested".to_owned());
        }
    }

    fn handle_clarification_key(&mut self, key: KeyEvent) {
        if self.clarification.is_none() {
            return;
        }

        if let Some(delta) = scroll_delta(key) {
            self.clarification_scroll = apply_scroll_delta(self.clarification_scroll, delta, false);
            return;
        }

        match key.code {
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(modal) = self.clarification.take() {
                    let _ = modal.responder.send(None);
                }
            }
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let raw_answer = self
                    .clarification
                    .as_ref()
                    .map(|modal| modal.input.lines().join("\n").trim().to_owned())
                    .unwrap_or_default();
                let answer = if let Ok(index) = raw_answer.parse::<usize>() {
                    self.clarification
                        .as_ref()
                        .and_then(|modal| modal.request.options.get(index.saturating_sub(1)))
                        .map(|option| option.label.clone())
                        .unwrap_or(raw_answer)
                } else {
                    raw_answer
                };
                if let Some(modal) = self.clarification.take() {
                    let _ = modal.responder.send((!answer.is_empty()).then_some(answer));
                }
            }
            _ => {
                if let Some(modal) = self.clarification.as_mut() {
                    modal.input.input(TextInput::from(key));
                }
            }
        }
    }

    fn selected_target(&self) -> Option<String> {
        self.specs
            .get(self.selected)
            .map(|summary| summary.spec_path.to_string())
    }

    fn perform_edit(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        target: String,
    ) -> Result<()> {
        self.input_suspended.store(true, Ordering::SeqCst);
        if let Err(error) = suspend_terminal(terminal) {
            self.input_suspended.store(false, Ordering::SeqCst);
            return Err(error);
        }
        let result = self
            .app
            .edit_target(&target)
            .map_err(|error| error.to_string());
        if let Err(error) = resume_terminal(terminal) {
            self.input_suspended.store(false, Ordering::SeqCst);
            return Err(error);
        }
        self.input_suspended.store(false, Ordering::SeqCst);

        match result {
            Ok(()) => {
                self.status = "Editor exited".to_owned();
                self.notice = Some(Notice {
                    tone: NoticeTone::Info,
                    message: "Editor closed".to_owned(),
                });
                self.load_specs();
            }
            Err(error) => {
                self.status = error.clone();
                self.notice = Some(Notice {
                    tone: NoticeTone::Error,
                    message: error,
                });
            }
        }

        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        frame.render_widget(Clear, area);

        let has_notice = self.notice.is_some();
        let mut constraints = vec![Constraint::Length(3)];
        if has_notice {
            constraints.push(Constraint::Length(3));
        }
        constraints.push(Constraint::Min(1));
        constraints.push(Constraint::Length(2));

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);

        self.draw_header(frame, chunks[0]);

        let content_index = if has_notice {
            self.draw_notice(frame, chunks[1]);
            2
        } else {
            1
        };
        let footer_index = if has_notice { 3 } else { 2 };

        match self.screen {
            Screen::Dashboard => self.draw_dashboard(frame, chunks[content_index]),
            Screen::Scoped => self.draw_scoped(frame, chunks[content_index]),
            Screen::Composer(_) => self.draw_composer(frame, chunks[content_index]),
            Screen::Review => self.draw_review(frame, chunks[content_index]),
            Screen::Running => self.draw_running(frame, chunks[content_index]),
        }

        let footer = Paragraph::new(self.footer_text())
            .style(Style::default().fg(self.muted_color()))
            .wrap(Wrap { trim: true });
        frame.render_widget(footer, chunks[footer_index]);

        if self.clarification.is_some() {
            self.draw_clarification(frame, area);
        }
    }

    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let active_count = self
            .specs
            .iter()
            .filter(|summary| summary.state != WorkflowState::Completed)
            .count();
        let completed_count = self
            .specs
            .iter()
            .filter(|summary| summary.state == WorkflowState::Completed)
            .count();

        let header = Paragraph::new(Line::from(vec![
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
                " durable repo workflow ",
                Style::default()
                    .fg(self.text_color())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "active {}  ◆  completed {}  ◆  {}",
                    active_count,
                    completed_count,
                    self.app.project_dir()
                ),
                Style::default().fg(self.muted_color()),
            ),
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded),
        );
        frame.render_widget(header, area);
    }

    fn draw_notice(&self, frame: &mut Frame, area: Rect) {
        let Some(notice) = self.notice.as_ref() else {
            return;
        };

        let (label, fg, bg) = self.notice_palette(notice.tone);

        let banner = Paragraph::new(Line::from(vec![
            Span::styled(
                label,
                Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                &notice.message,
                Style::default()
                    .fg(self.text_color())
                    .add_modifier(Modifier::BOLD),
            ),
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded),
        );
        frame.render_widget(Clear, area);
        frame.render_widget(banner, area);
    }

    fn draw_dashboard(&mut self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(area);
        frame.render_widget(Clear, chunks[0]);
        frame.render_widget(Clear, chunks[1]);

        let items = if self.specs.is_empty() {
            vec![ListItem::new(Line::from(vec![
                Span::styled("◌", Style::default().fg(self.muted_color())),
                Span::raw(" No specs yet"),
            ]))]
        } else {
            self.specs
                .iter()
                .map(|summary| {
                    let title = Line::from(vec![
                        Span::styled(
                            format!("{} ", state_badge(summary.state)),
                            state_style(
                                summary.state,
                                self.accent_color(),
                                self.success_color(),
                                self.muted_color(),
                            ),
                        ),
                        Span::styled(
                            summary
                                .spec_path
                                .file_name()
                                .unwrap_or(summary.spec_path.as_str())
                                .to_owned(),
                            Style::default()
                                .fg(self.text_color())
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]);
                    let meta = Line::from(vec![
                        Span::styled(
                            state_label(summary.state).to_uppercase(),
                            state_style(
                                summary.state,
                                self.accent_color(),
                                self.success_color(),
                                self.muted_color(),
                            )
                            .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("  ◆  {}", summary.progress_path),
                            Style::default().fg(self.muted_color()),
                        ),
                    ]);
                    ListItem::new(vec![title, meta])
                })
                .collect()
        };

        let mut list_state = ListState::default();
        if !self.specs.is_empty() {
            list_state.select(Some(self.selected));
        }

        let list = List::new(items)
            .block(
                Block::default()
                    .title(self.title_line("Specs", "Select a workflow"))
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded),
            )
            .highlight_symbol("▶ ")
            .highlight_style(
                Style::default()
                    .fg(self.text_color())
                    .bg(self.panel_highlight())
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_stateful_widget(list, chunks[0], &mut list_state);

        let (preview_text, preview_source) = if let Some(summary) = self.specs.get(self.selected) {
            let source = format!(
                "Shortcuts\nCtrl-N create  •  Ctrl-O open  •  Ctrl-L reload  •  Ctrl-Q quit\nScroll\nPgUp/PgDn/Home/End\n\n◆ State      {}\n◆ Spec       {}\n◆ Progress   {}\n\n╭─ Spec Preview\n{}\n\n╭─ Progress Preview\n{}\n",
                state_label(summary.state),
                summary.spec_path,
                summary.progress_path,
                summary.spec_preview,
                summary.progress_preview,
            );
            let mut text = plain_text_from_string(format!(
                "Shortcuts\nCtrl-N create  •  Ctrl-O open  •  Ctrl-L reload  •  Ctrl-Q quit\nScroll\nPgUp/PgDn/Home/End\n\n◆ State      {}\n◆ Spec       {}\n◆ Progress   {}\n\n╭─ Spec Preview\n",
                state_label(summary.state),
                summary.spec_path,
                summary.progress_path,
            ));
            append_text(
                &mut text,
                highlight_markdown(summary.spec_preview.as_str(), self.color_mode),
            );
            text.lines.push(Line::raw(""));
            text.lines.push(Line::raw("╭─ Progress Preview"));
            append_text(
                &mut text,
                highlight_markdown(summary.progress_preview.as_str(), self.color_mode),
            );
            (text, source)
        } else {
            (
                plain_text_from_string("Create a spec with n to begin.".to_owned()),
                "Create a spec with n to begin.".to_owned(),
            )
        };

        let max_scroll = max_scroll_for_text(
            &preview_source,
            chunks[1].width.saturating_sub(2),
            chunks[1].height.saturating_sub(2),
        );
        if self.dashboard_preview_scroll > max_scroll {
            self.dashboard_preview_scroll = max_scroll;
        }
        let paragraph = Paragraph::new(preview_text)
            .block(
                Block::default()
                    .title(self.title_line("Preview", "Current durable state"))
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded),
            )
            .wrap(Wrap { trim: false })
            .scroll((self.dashboard_preview_scroll, 0));
        frame.render_widget(paragraph, chunks[1]);
    }

    fn draw_scoped(&mut self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(36), Constraint::Percentage(64)])
            .split(area);
        frame.render_widget(Clear, chunks[0]);
        frame.render_widget(Clear, chunks[1]);
        let summary = self.specs.get(self.selected);
        let actions = vec![
            Line::from(vec![
                Span::styled("Ctrl-B", key_style(self.accent_color())),
                Span::raw("  Run spec"),
            ]),
            Line::from(vec![
                Span::styled("Ctrl-V", key_style(self.accent_color())),
                Span::raw("  Review spec and progress"),
            ]),
            Line::from(vec![
                Span::styled("Ctrl-E", key_style(self.accent_color())),
                Span::raw("  Open spec in $EDITOR"),
            ]),
            Line::from(vec![
                Span::styled("Ctrl-U", key_style(self.warning_color())),
                Span::raw("  Revise the plan in place"),
            ]),
            Line::from(vec![
                Span::styled("Ctrl-P", key_style(self.warning_color())),
                Span::raw("  Replan from scratch"),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("Ctrl-W", key_style(self.muted_color())),
                Span::raw("  Back to dashboard"),
            ]),
            Line::from(vec![
                Span::styled("PgUp/PgDn", key_style(self.muted_color())),
                Span::raw("  Scroll the detail pane"),
            ]),
            Line::from(vec![
                Span::styled("↑ ↓ / j k", key_style(self.muted_color())),
                Span::raw("  Line-scroll the detail pane"),
            ]),
        ];
        let action_panel = Paragraph::new(actions)
            .block(
                Block::default()
                    .title(self.title_line("Actions", "One spec, many moves"))
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded),
            )
            .wrap(Wrap { trim: false });
        frame.render_widget(action_panel, chunks[0]);

        let (contents_text, contents_source) = if let Some(summary) = summary {
            let source = format!(
                "Shortcuts\nCtrl-B run  •  Ctrl-V review  •  Ctrl-E edit  •  Ctrl-U revise  •  Ctrl-P replan  •  Ctrl-W back\nScroll\n↑/↓ or j/k line scroll  •  PgUp/PgDn/Home/End page scroll\n\n{}\n{}\n\n◆ State\n{}\n\n◆ Spec Preview\n{}\n\n◆ Progress Preview\n{}\n",
                summary.spec_path,
                summary.progress_path,
                state_label(summary.state),
                summary.spec_preview,
                summary.progress_preview,
            );
            let mut text = plain_text_from_string(format!(
                "Shortcuts\nCtrl-B run  •  Ctrl-V review  •  Ctrl-E edit  •  Ctrl-U revise  •  Ctrl-P replan  •  Ctrl-W back\nScroll\n↑/↓ or j/k line scroll  •  PgUp/PgDn/Home/End page scroll\n\n{}\n{}\n\n◆ State\n{}\n\n◆ Spec Preview\n",
                summary.spec_path,
                summary.progress_path,
                state_label(summary.state),
            ));
            append_text(
                &mut text,
                highlight_markdown(summary.spec_preview.as_str(), self.color_mode),
            );
            text.lines.push(Line::raw(""));
            text.lines.push(Line::raw("◆ Progress Preview"));
            append_text(
                &mut text,
                highlight_markdown(summary.progress_preview.as_str(), self.color_mode),
            );
            (text, source)
        } else {
            (
                plain_text_from_string("No selected spec.".to_owned()),
                "No selected spec.".to_owned(),
            )
        };

        let max_scroll = max_scroll_for_text(
            &contents_source,
            chunks[1].width.saturating_sub(2),
            chunks[1].height.saturating_sub(2),
        );
        if self.scoped_scroll > max_scroll {
            self.scoped_scroll = max_scroll;
        }
        let panel = Paragraph::new(contents_text)
            .block(
                Block::default()
                    .title(self.title_line("Selected Spec", "Durable inputs at a glance"))
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded),
            )
            .wrap(Wrap { trim: false })
            .scroll((self.scoped_scroll, 0));
        frame.render_widget(panel, chunks[1]);
    }

    fn draw_composer(&mut self, frame: &mut Frame, area: Rect) {
        frame.render_widget(Clear, area);
        self.composer.set_block(
            Block::default()
                .borders(Borders::ALL)
                .title(self.title_line("Planning Request", "Ctrl-S submit  •  Ctrl-W cancel"))
                .border_type(BorderType::Rounded),
        );
        frame.render_widget(&self.composer, area);
    }

    fn draw_review(&mut self, frame: &mut Frame, area: Rect) {
        let Some(review) = self.review.as_ref() else {
            frame.render_widget(Paragraph::new("No review loaded"), area);
            return;
        };

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(1)])
            .split(area);
        frame.render_widget(Clear, chunks[0]);
        frame.render_widget(Clear, chunks[1]);

        let tabs = Tabs::new(vec!["Spec", "Progress"])
            .select(self.review_tab)
            .block(
                Block::default()
                    .title(self.title_line("Review", &review.spec_path.to_string()))
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded),
            )
            .highlight_style(
                Style::default()
                    .fg(self.accent_color())
                    .add_modifier(Modifier::BOLD),
            )
            .style(Style::default().fg(self.muted_color()));
        frame.render_widget(tabs, chunks[0]);

        let body = if self.review_tab == 0 {
            &review.spec_contents
        } else {
            &review.progress_contents
        };
        let paragraph = Paragraph::new(highlight_markdown(body, self.color_mode))
            .block(
                Block::default()
                    .title(self.title_line(
                        "Body",
                        "←/→ switch tab  •  ↑/↓ scroll 5 lines  •  Ctrl-H spec  •  Ctrl-L progress  •  Ctrl-W back  •  PgUp/PgDn or mouse scroll",
                    ))
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded),
            )
            .wrap(Wrap { trim: false });
        let max_scroll = max_scroll_for_text(
            body,
            chunks[1].width.saturating_sub(2),
            chunks[1].height.saturating_sub(2),
        );
        if self.review_scroll > max_scroll {
            self.review_scroll = max_scroll;
        }
        let paragraph = paragraph.scroll((self.review_scroll, 0));
        frame.render_widget(paragraph, chunks[1]);
    }

    fn draw_running(&mut self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(5), Constraint::Min(1)])
            .split(area);
        frame.render_widget(Clear, chunks[0]);
        frame.render_widget(Clear, chunks[1]);
        let header = Paragraph::new(format!(
            "Shortcuts  Ctrl-C cancel agent  •  Ctrl-W back when idle  •  PgUp/PgDn/Home/End scroll\n◆ Target   {}\n◆ Status   {}",
            self.run_target.as_deref().unwrap_or("unknown"),
            self.status
        ))
        .block(
                Block::default()
                    .title(self.title_line("Live Run", "Streaming agent output"))
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded),
            );
        frame.render_widget(header, chunks[0]);

        let joined = self.run_logs.join("\n");
        let body_width = chunks[1].width.saturating_sub(2);
        let body_height = chunks[1].height.saturating_sub(2);
        let wrapped_lines = wrap_visual_lines(&joined, body_width);
        let max_scroll = wrapped_lines
            .len()
            .saturating_sub(usize::from(body_height.max(1))) as u16;
        if self.run_follow {
            self.run_scroll = max_scroll;
        } else if self.run_scroll > max_scroll {
            self.run_scroll = max_scroll;
        }
        let start = usize::from(self.run_scroll.min(max_scroll));
        let end = (start + usize::from(body_height.max(1))).min(wrapped_lines.len());
        let visible = if wrapped_lines.is_empty() {
            String::new()
        } else {
            wrapped_lines[start..end].join("\n")
        };
        let logs = Paragraph::new(visible).block(
            Block::default()
                .title(self.title_line("Agent Stream", "stdout + stderr"))
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded),
        );
        frame.render_widget(logs, chunks[1]);
    }

    fn draw_clarification(&mut self, frame: &mut Frame, area: Rect) {
        let warning_color = self.warning_color();
        let title = self.title_line("Clarification", "Planner needs a decision");
        let Some(modal) = self.clarification.as_mut() else {
            return;
        };
        let popup = centered_rect(70, 55, area);
        frame.render_widget(Clear, popup);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
            .split(popup);
        frame.render_widget(Clear, chunks[0]);
        frame.render_widget(Clear, chunks[1]);

        let mut text = vec![
            Line::from(Span::styled(
                "Clarification Required",
                Style::default()
                    .fg(warning_color)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(modal.request.question.clone()),
            Line::from(""),
        ];
        for (index, option) in modal.request.options.iter().enumerate() {
            text.push(Line::from(format!(
                "{}. {} - {}",
                index + 1,
                option.label,
                option.description
            )));
        }
        text.push(Line::from(""));
        text.push(Line::from("Shortcuts"));
        text.push(Line::from("1-9 type into the input box normally"));
        text.push(Line::from("Ctrl-S submit typed number or free-form answer"));
        text.push(Line::from("Ctrl-W cancel  •  PgUp/PgDn/Home/End scroll"));

        let top = Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_type(BorderType::Rounded),
            )
            .wrap(Wrap { trim: true });
        let modal_text = modal
            .request
            .options
            .iter()
            .enumerate()
            .map(|(index, option)| {
                format!("{}. {} - {}", index + 1, option.label, option.description)
            })
            .collect::<Vec<_>>()
            .join("\n");
        let max_scroll = max_scroll_for_text(
            &format!("{}\n{}", modal.request.question, modal_text),
            chunks[0].width.saturating_sub(2),
            chunks[0].height.saturating_sub(2),
        );
        if self.clarification_scroll > max_scroll {
            self.clarification_scroll = max_scroll;
        }
        let top = top.scroll((self.clarification_scroll, 0));
        frame.render_widget(top, chunks[0]);
        frame.render_widget(&modal.input, chunks[1]);
    }

    fn footer_text(&self) -> String {
        match self.screen {
            Screen::Dashboard => format!(
                "{}    ◆  Ctrl-N create  •  Ctrl-O open  •  Ctrl-L reload  •  Ctrl-Q quit",
                self.status
            ),
            Screen::Scoped => format!(
                "{}    ◆  Ctrl-B run  •  Ctrl-V review  •  Ctrl-E edit  •  Ctrl-U revise  •  Ctrl-P replan  •  Ctrl-W back  •  ↑/↓ scroll",
                self.status
            ),
            Screen::Composer(_) => format!("{}    ◆  Ctrl-S submit  •  Ctrl-W cancel", self.status),
            Screen::Review => format!(
                "{}    ◆  ←/→ switch  •  ↑/↓ scroll  •  Ctrl-H spec  •  Ctrl-L progress  •  Ctrl-W back",
                self.status
            ),
            Screen::Running => format!(
                "{}    ◆  Ctrl-C cancel agent  •  Ctrl-W back when idle",
                self.status
            ),
        }
    }

    fn accent_color(&self) -> Color {
        resolved_accent_color(&self.app.config().theme.accent_color, self.color_mode)
    }

    fn success_color(&self) -> Color {
        resolved_success_color(&self.app.config().theme.success_color, self.color_mode)
    }

    fn warning_color(&self) -> Color {
        resolved_warning_color(&self.app.config().theme.warning_color, self.color_mode)
    }

    fn text_color(&self) -> Color {
        match self.color_mode {
            ColorMode::Light => Color::Black,
            ColorMode::Dark => Color::White,
        }
    }

    fn muted_color(&self) -> Color {
        match self.color_mode {
            ColorMode::Light => Color::Rgb(96, 103, 112),
            ColorMode::Dark => Color::Gray,
        }
    }

    fn subtle_color(&self) -> Color {
        match self.color_mode {
            ColorMode::Light => Color::Rgb(150, 157, 166),
            ColorMode::Dark => Color::DarkGray,
        }
    }

    fn panel_highlight(&self) -> Color {
        match self.color_mode {
            ColorMode::Light => Color::Rgb(220, 234, 242),
            ColorMode::Dark => Color::Rgb(24, 47, 56),
        }
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

    fn notice_palette(&self, tone: NoticeTone) -> (&'static str, Color, Color) {
        match (self.color_mode, tone) {
            (ColorMode::Light, NoticeTone::Info) => {
                (" INFO ", Color::Black, Color::Rgb(191, 219, 254))
            }
            (ColorMode::Dark, NoticeTone::Info) => {
                (" INFO ", Color::Black, Color::Rgb(103, 232, 249))
            }
            (ColorMode::Light, NoticeTone::Success) => {
                (" DONE ", Color::Black, Color::Rgb(187, 247, 208))
            }
            (ColorMode::Dark, NoticeTone::Success) => {
                (" DONE ", Color::Black, Color::Rgb(74, 222, 128))
            }
            (ColorMode::Light, NoticeTone::Error) => {
                (" ERROR ", Color::Black, Color::Rgb(254, 202, 202))
            }
            (ColorMode::Dark, NoticeTone::Error) => {
                (" ERROR ", Color::White, Color::Rgb(239, 68, 68))
            }
        }
    }
}

struct ChannelDelegate {
    tx: Sender<UiEvent>,
}

#[async_trait]
impl RunDelegate for ChannelDelegate {
    async fn on_event(&mut self, event: RunEvent) -> Result<()> {
        self.tx
            .send(UiEvent::RunMessage(event))
            .map_err(|_| anyhow!("failed to send run event"))
    }

    async fn ask_clarification(&mut self, request: ClarificationRequest) -> Result<Option<String>> {
        let (responder, receiver) = oneshot::channel();
        self.tx
            .send(UiEvent::ClarificationRequested(request, responder))
            .map_err(|_| anyhow!("failed to open clarification modal"))?;
        receiver
            .await
            .map_err(|_| anyhow!("clarification response channel closed"))
    }
}

fn fresh_composer(title: &str, cursor_color: Color) -> TextArea<'static> {
    let mut composer = TextArea::default();
    composer.set_block(
        Block::default()
            .borders(Borders::ALL)
            .title(title.to_owned())
            .border_type(BorderType::Rounded),
    );
    composer.set_cursor_line_style(Style::default().fg(cursor_color));
    composer
}

fn state_badge(state: WorkflowState) -> &'static str {
    match state {
        WorkflowState::Empty => "○",
        WorkflowState::Planned => "◉",
        WorkflowState::Completed => "✓",
    }
}

fn state_label(state: WorkflowState) -> &'static str {
    match state {
        WorkflowState::Empty => "empty",
        WorkflowState::Planned => "planned",
        WorkflowState::Completed => "completed",
    }
}

fn state_style(state: WorkflowState, accent: Color, success: Color, muted: Color) -> Style {
    match state {
        WorkflowState::Empty => Style::default().fg(muted),
        WorkflowState::Planned => Style::default().fg(accent),
        WorkflowState::Completed => Style::default().fg(success),
    }
}

fn styled_title(
    title: &str,
    subtitle: &str,
    text_color: Color,
    subtle_color: Color,
    muted_color: Color,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!(" {} ", title),
            Style::default().fg(text_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled("◆", Style::default().fg(subtle_color)),
        Span::styled(format!(" {}", subtitle), Style::default().fg(muted_color)),
    ])
}

fn key_style(color: Color) -> Style {
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

fn detect_color_mode() -> ColorMode {
    if let Ok(value) = env::var("RALPH_COLOR_MODE") {
        match value.trim().to_ascii_lowercase().as_str() {
            "light" => return ColorMode::Light,
            "dark" => return ColorMode::Dark,
            _ => {}
        }
    }

    if let Some(color_mode) = detect_color_mode_via_osc11() {
        return color_mode;
    }

    if let Ok(value) = env::var("COLORFGBG") {
        if let Some(background) = value
            .split(';')
            .next_back()
            .and_then(|token| token.parse::<u8>().ok())
        {
            return if background >= 7 {
                ColorMode::Light
            } else {
                ColorMode::Dark
            };
        }
    }

    ColorMode::Dark
}

fn detect_color_mode_via_osc11() -> Option<ColorMode> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;

        let mut tty = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .ok()?;

        tty.write_all(b"\x1b]11;?\x07").ok()?;
        tty.flush().ok()?;

        let fd = tty.as_raw_fd();
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 256];

        for _ in 0..4 {
            let mut pollfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };

            let ready = unsafe { libc::poll(&mut pollfd, 1, 120) };
            if ready <= 0 {
                break;
            }

            let bytes_read = tty.read(&mut chunk).ok()?;
            if bytes_read == 0 {
                break;
            }

            buffer.extend_from_slice(&chunk[..bytes_read]);
            if buffer.ends_with(b"\x07") || buffer.windows(2).any(|window| window == b"\x1b\\") {
                break;
            }
        }

        let response = String::from_utf8(buffer).ok()?;
        return parse_osc11_response(&response);
    }

    #[cfg(not(unix))]
    {
        None
    }
}

fn parse_osc11_response(response: &str) -> Option<ColorMode> {
    let payload = response
        .strip_prefix("\u{1b}]11;")?
        .trim_end_matches('\u{7}')
        .trim_end_matches("\u{1b}\\");
    let rgb = payload.strip_prefix("rgb:")?;
    let mut channels = rgb.split('/');
    let red = parse_osc_hex_channel(channels.next()?)?;
    let green = parse_osc_hex_channel(channels.next()?)?;
    let blue = parse_osc_hex_channel(channels.next()?)?;
    if channels.next().is_some() {
        return None;
    }

    let luminance = 0.2126 * red + 0.7152 * green + 0.0722 * blue;
    Some(if luminance >= 0.5 {
        ColorMode::Light
    } else {
        ColorMode::Dark
    })
}

fn parse_osc_hex_channel(channel: &str) -> Option<f32> {
    let digits = channel.trim();
    if digits.is_empty() || digits.len() > 4 {
        return None;
    }
    let value = u16::from_str_radix(digits, 16).ok()? as f32;
    let max = ((1u32 << (digits.len() * 4)) - 1) as f32;
    Some(value / max)
}

fn resolved_accent_color(name: &str, color_mode: ColorMode) -> Color {
    if name.trim().eq_ignore_ascii_case("cyan") {
        match color_mode {
            ColorMode::Light => Color::Rgb(0, 102, 204),
            ColorMode::Dark => Color::Cyan,
        }
    } else {
        color_from_name(name).unwrap_or(match color_mode {
            ColorMode::Light => Color::Rgb(0, 102, 204),
            ColorMode::Dark => Color::Cyan,
        })
    }
}

fn resolved_success_color(name: &str, color_mode: ColorMode) -> Color {
    if name.trim().eq_ignore_ascii_case("green") {
        match color_mode {
            ColorMode::Light => Color::Rgb(36, 138, 61),
            ColorMode::Dark => Color::LightGreen,
        }
    } else {
        color_from_name(name).unwrap_or(match color_mode {
            ColorMode::Light => Color::Rgb(36, 138, 61),
            ColorMode::Dark => Color::LightGreen,
        })
    }
}

fn resolved_warning_color(name: &str, color_mode: ColorMode) -> Color {
    if name.trim().eq_ignore_ascii_case("yellow") {
        match color_mode {
            ColorMode::Light => Color::Rgb(160, 100, 0),
            ColorMode::Dark => Color::LightYellow,
        }
    } else {
        color_from_name(name).unwrap_or(match color_mode {
            ColorMode::Light => Color::Rgb(160, 100, 0),
            ColorMode::Dark => Color::LightYellow,
        })
    }
}

static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static DARK_THEME: LazyLock<Theme> = LazyLock::new(|| {
    let themes = ThemeSet::load_defaults();
    themes
        .themes
        .get("base16-ocean.dark")
        .cloned()
        .or_else(|| themes.themes.values().next().cloned())
        .expect("syntect default themes must exist")
});
static LIGHT_THEME: LazyLock<Theme> = LazyLock::new(|| {
    let themes = ThemeSet::load_defaults();
    themes
        .themes
        .get("InspiredGitHub")
        .cloned()
        .or_else(|| themes.themes.values().next().cloned())
        .expect("syntect default themes must exist")
});

fn plain_text_from_string(content: String) -> Text<'static> {
    Text::from(content)
}

fn append_text(target: &mut Text<'static>, mut extra: Text<'static>) {
    target.lines.append(&mut extra.lines);
}

fn highlight_markdown(input: &str, color_mode: ColorMode) -> Text<'static> {
    let syntax = SYNTAX_SET
        .find_syntax_by_extension("md")
        .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text());
    let theme = match color_mode {
        ColorMode::Light => &LIGHT_THEME,
        ColorMode::Dark => &DARK_THEME,
    };
    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut lines = Vec::new();

    for raw_line in LinesWithEndings::from(input) {
        let highlighted = highlighter
            .highlight_line(raw_line, &SYNTAX_SET)
            .unwrap_or_else(|_| vec![(SyntectStyle::default(), raw_line)]);
        let spans = highlighted
            .into_iter()
            .filter_map(|(style, slice)| {
                let content = slice.trim_end_matches(['\n', '\r']);
                if content.is_empty() {
                    None
                } else {
                    Some(Span::styled(
                        content.to_owned(),
                        convert_syntect_style(style),
                    ))
                }
            })
            .collect::<Vec<_>>();
        lines.push(Line::from(spans));
    }

    if lines.is_empty() {
        lines.push(Line::raw(""));
    }

    Text::from(lines)
}

fn convert_syntect_style(style: SyntectStyle) -> Style {
    let mut converted = Style::default().fg(Color::Rgb(
        style.foreground.r,
        style.foreground.g,
        style.foreground.b,
    ));

    if style.font_style.contains(FontStyle::BOLD) {
        converted = converted.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        converted = converted.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        converted = converted.add_modifier(Modifier::UNDERLINED);
    }

    converted
}

fn color_from_name(name: &str) -> Option<Color> {
    match name.trim().to_ascii_lowercase().as_str() {
        "black" => Some(Color::Black),
        "red" => Some(Color::Red),
        "green" => Some(Color::Green),
        "yellow" => Some(Color::Yellow),
        "blue" => Some(Color::Blue),
        "magenta" => Some(Color::Magenta),
        "cyan" => Some(Color::Cyan),
        "gray" | "grey" => Some(Color::Gray),
        "darkgray" | "dark_gray" | "darkgrey" | "dark-grey" => Some(Color::DarkGray),
        "lightred" | "light_red" => Some(Color::LightRed),
        "lightgreen" | "light_green" => Some(Color::LightGreen),
        "lightyellow" | "light_yellow" => Some(Color::LightYellow),
        "lightblue" | "light_blue" => Some(Color::LightBlue),
        "lightmagenta" | "light_magenta" => Some(Color::LightMagenta),
        "lightcyan" | "light_cyan" => Some(Color::LightCyan),
        "white" => Some(Color::White),
        _ => None,
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn suspend_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    disable_raw_mode().context("failed to disable raw mode for editor launch")?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )
    .context("failed to leave alternate screen for editor launch")?;
    terminal
        .show_cursor()
        .context("failed to show cursor for editor launch")?;
    Ok(())
}

fn resume_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    enable_raw_mode().context("failed to re-enable raw mode after editor exit")?;
    execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        EnableMouseCapture
    )
    .context("failed to re-enter alternate screen after editor exit")?;
    terminal
        .hide_cursor()
        .context("failed to hide cursor after editor exit")?;
    terminal
        .clear()
        .context("failed to clear terminal after editor exit")?;
    Ok(())
}

enum ScrollDelta {
    Lines(i16),
    Pages(i16),
    Home,
    End,
}

fn scroll_delta(key: KeyEvent) -> Option<ScrollDelta> {
    match key.code {
        KeyCode::PageUp => Some(ScrollDelta::Pages(-1)),
        KeyCode::PageDown => Some(ScrollDelta::Pages(1)),
        KeyCode::Home => Some(ScrollDelta::Home),
        KeyCode::End => Some(ScrollDelta::End),
        _ => None,
    }
}

fn apply_scroll_delta(current: u16, delta: ScrollDelta, sticky_bottom: bool) -> u16 {
    match delta {
        ScrollDelta::Lines(lines) => current.saturating_add_signed(lines),
        ScrollDelta::Pages(pages) => current.saturating_add_signed(pages * 12),
        ScrollDelta::Home => 0,
        ScrollDelta::End => {
            if sticky_bottom {
                u16::MAX
            } else {
                u16::MAX
            }
        }
    }
}

fn max_scroll_for_text(text: &str, width: u16, height: u16) -> u16 {
    let available_width = usize::from(width.max(1));
    let visible_lines = usize::from(height.max(1));

    let mut rendered_line_count = 0usize;
    for raw_line in text.lines() {
        if raw_line.is_empty() {
            rendered_line_count += 1;
            continue;
        }
        let wraps = textwrap::wrap(raw_line, available_width);
        rendered_line_count += wraps.len().max(1);
    }

    rendered_line_count.saturating_sub(visible_lines) as u16
}

fn normalize_stream_chunk(chunk: &str) -> String {
    let mut normalized = String::new();
    let mut chars = chunk.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\u{1b}' => {
                if matches!(chars.peek(), Some('[')) {
                    chars.next();
                    while let Some(next) = chars.next() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
            }
            '\r' => normalized.push('\n'),
            '\t' => normalized.push_str("    "),
            '\u{8}' => {}
            _ => normalized.push(ch),
        }
    }

    normalized
}

fn wrap_visual_lines(text: &str, width: u16) -> Vec<String> {
    let width = usize::from(width.max(1));
    let mut wrapped = Vec::new();

    for raw_line in text.split('\n') {
        if raw_line.is_empty() {
            wrapped.push(String::new());
            continue;
        }

        let mut current = String::new();
        let mut current_width = 0usize;
        for ch in raw_line.chars() {
            let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
            if current_width + ch_width > width && !current.is_empty() {
                wrapped.push(std::mem::take(&mut current));
                current_width = 0;
            }
            current.push(ch);
            current_width += ch_width;
        }

        wrapped.push(current);
    }

    if wrapped.is_empty() {
        wrapped.push(String::new());
    }

    wrapped
}

#[cfg(test)]
mod tests {
    use super::{
        ColorMode, normalize_stream_chunk, parse_osc_hex_channel, parse_osc11_response,
        wrap_visual_lines,
    };

    #[test]
    fn normalizes_carriage_returns_and_ansi_sequences() {
        let normalized = normalize_stream_chunk("abc\rdef\u{1b}[31mred\u{1b}[0m");
        assert_eq!(normalized, "abc\ndefred");
    }

    #[test]
    fn wraps_visual_lines_consistently() {
        let wrapped = wrap_visual_lines("abcdefghij", 4);
        assert_eq!(wrapped, vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn parses_dark_osc11_response() {
        let mode = parse_osc11_response("\u{1b}]11;rgb:0000/0000/0000\u{7}");
        assert_eq!(mode, Some(ColorMode::Dark));
    }

    #[test]
    fn parses_light_osc11_response() {
        let mode = parse_osc11_response("\u{1b}]11;rgb:ffff/ffff/ffff\u{1b}\\");
        assert_eq!(mode, Some(ColorMode::Light));
    }

    #[test]
    fn normalizes_variable_length_osc_channels() {
        let value = parse_osc_hex_channel("ff").unwrap();
        assert!((value - 1.0).abs() < 0.0001);
    }
}
