use std::{
    io,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use crossterm::{
    event::{self, Event as CEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
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
use textwrap::fill;
use tokio::{runtime::Handle, sync::oneshot};
use tui_textarea::{Input as TextInput, TextArea};

pub fn run_tui(app: RalphApp) -> Result<()> {
    let handle = Handle::current();
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("failed to create terminal backend")?;

    let result = TuiApp::new(app, handle).run(&mut terminal);

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();
    result
}

enum UiEvent {
    Tick,
    Key(KeyEvent),
    Resize,
    SpecsLoaded(Result<Vec<SpecSummary>, String>),
    ReviewLoaded(Result<ReviewData, String>),
    OperationDone(Result<SpecSummary, String>),
    EditDone(Result<(), String>),
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
    run_logs: Vec<String>,
    run_target: Option<String>,
    active_control: Option<RunControl>,
    pending: bool,
    clarification: Option<ClarificationModal>,
    should_quit: bool,
}

impl TuiApp {
    fn new(app: RalphApp, handle: Handle) -> Self {
        let (tx, rx) = mpsc::channel();
        let mut composer = TextArea::default();
        composer.set_block(
            Block::default()
                .borders(Borders::ALL)
                .title("Planning Request")
                .border_type(BorderType::Rounded),
        );
        composer.set_cursor_line_style(Style::default().fg(Color::Cyan));

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
            run_logs: Vec::new(),
            run_target: None,
            active_control: None,
            pending: false,
            clarification: None,
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
        }

        Ok(())
    }

    fn spawn_event_thread(&self) {
        let tx = self.tx.clone();
        thread::spawn(move || {
            loop {
                if event::poll(Duration::from_millis(120)).ok() == Some(true) {
                    match event::read() {
                        Ok(CEvent::Key(key)) if key.kind == KeyEventKind::Press => {
                            if tx.send(UiEvent::Key(key)).is_err() {
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
        self.pending = true;
        self.status = format!("Opening editor for {target}");
        let tx = self.tx.clone();
        let app = self.app.clone();
        self.handle.spawn(async move {
            let result = app.edit_target(&target).map_err(|error| error.to_string());
            let _ = tx.send(UiEvent::EditDone(result));
        });
    }

    fn handle_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::Tick => {}
            UiEvent::Resize => {}
            UiEvent::Key(key) => self.handle_key(key),
            UiEvent::SpecsLoaded(result) => match result {
                Ok(specs) => {
                    self.specs = specs;
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
                        self.load_specs();
                    }
                    Err(error) => {
                        self.status = error.clone();
                        self.run_logs.push(format!("error: {error}"));
                    }
                }
            }
            UiEvent::EditDone(result) => {
                self.pending = false;
                self.active_control = None;
                match result {
                    Ok(()) => {
                        self.status = "Editor exited".to_owned();
                        self.load_specs();
                    }
                    Err(error) => self.status = error,
                }
            }
            UiEvent::RunMessage(message) => self.apply_run_event(message),
            UiEvent::ClarificationRequested(request, responder) => {
                self.status = "Clarification required".to_owned();
                let mut input = TextArea::default();
                input.set_block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Clarification")
                        .border_type(BorderType::Rounded),
                );
                input.set_cursor_line_style(Style::default().fg(Color::Cyan));
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
            RunEvent::Stdout(chunk) => self.run_logs.push(chunk),
            RunEvent::Stderr(chunk) => self.run_logs.push(format!("stderr: {chunk}")),
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

    fn handle_dashboard_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < self.specs.len() {
                    self.selected += 1;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
            }
            KeyCode::Enter if !self.specs.is_empty() => self.screen = Screen::Scoped,
            KeyCode::Char('n') => {
                self.composer = fresh_composer("Create New Spec");
                self.screen = Screen::Composer(ComposerKind::Create);
            }
            KeyCode::Char('l') => self.load_specs(),
            _ => {}
        }
    }

    fn handle_scoped_key(&mut self, key: KeyEvent) {
        let Some(target) = self.selected_target() else {
            self.screen = Screen::Dashboard;
            return;
        };

        match key.code {
            KeyCode::Esc => self.screen = Screen::Dashboard,
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('v') => self.load_review(target),
            KeyCode::Char('r') => self.run_builder(target),
            KeyCode::Char('e') => self.run_edit(target),
            KeyCode::Char('R') => {
                self.composer = fresh_composer("Revise Spec");
                self.screen = Screen::Composer(ComposerKind::Revise(target));
            }
            KeyCode::Char('p') => {
                self.composer = fresh_composer("Replan From Scratch");
                self.screen = Screen::Composer(ComposerKind::Replan(target));
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < self.specs.len() {
                    self.selected += 1;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
            }
            _ => {}
        }
    }

    fn handle_composer_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
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
        match key.code {
            KeyCode::Esc => self.screen = Screen::Scoped,
            KeyCode::Tab => self.review_tab = (self.review_tab + 1) % 2,
            KeyCode::BackTab => self.review_tab = self.review_tab.saturating_sub(1),
            KeyCode::Char('q') => self.should_quit = true,
            _ => {}
        }
    }

    fn handle_running_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc if !self.pending => self.screen = Screen::Scoped,
            KeyCode::Char('q') if !self.pending => self.should_quit = true,
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

        match key.code {
            KeyCode::Esc => {
                if let Some(modal) = self.clarification.take() {
                    let _ = modal.responder.send(None);
                }
            }
            KeyCode::Char(ch) if matches!(ch, '1' | '2' | '3') => {
                let index = ch.to_digit(10).unwrap_or_default() as usize - 1;
                let answer = self
                    .clarification
                    .as_ref()
                    .and_then(|modal| modal.request.options.get(index))
                    .map(|option| option.label.clone());
                if let Some(answer) = answer {
                    if let Some(modal) = self.clarification.take() {
                        let _ = modal.responder.send(Some(answer));
                    }
                }
            }
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let answer = self
                    .clarification
                    .as_ref()
                    .map(|modal| modal.input.lines().join("\n").trim().to_owned())
                    .unwrap_or_default();
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

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(2)])
            .split(area);

        match self.screen {
            Screen::Dashboard => self.draw_dashboard(frame, chunks[0]),
            Screen::Scoped => self.draw_scoped(frame, chunks[0]),
            Screen::Composer(_) => self.draw_composer(frame, chunks[0]),
            Screen::Review => self.draw_review(frame, chunks[0]),
            Screen::Running => self.draw_running(frame, chunks[0]),
        }

        let footer = Paragraph::new(self.footer_text())
            .style(Style::default().fg(Color::Gray))
            .wrap(Wrap { trim: true });
        frame.render_widget(footer, chunks[1]);

        if self.clarification.is_some() {
            self.draw_clarification(frame, area);
        }
    }

    fn draw_dashboard(&mut self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
            .split(area);

        let items = if self.specs.is_empty() {
            vec![ListItem::new("No specs yet")]
        } else {
            self.specs
                .iter()
                .map(|summary| {
                    let label = format!(
                        "{} {}",
                        state_badge(summary.state),
                        summary
                            .spec_path
                            .file_name()
                            .unwrap_or(summary.spec_path.as_str())
                    );
                    ListItem::new(Line::from(vec![
                        Span::styled(label, state_style(summary.state)),
                        Span::raw(format!("  {}", summary.progress_path)),
                    ]))
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
                    .title("Specs")
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        frame.render_stateful_widget(list, chunks[0], &mut list_state);

        let preview = if let Some(summary) = self.specs.get(self.selected) {
            format!(
                "State: {}\nSpec: {}\nProgress: {}\n\nSpec Preview\n{}\n\nProgress Preview\n{}\n",
                state_label(summary.state),
                summary.spec_path,
                summary.progress_path,
                fill(
                    &summary.spec_preview,
                    usize::from(chunks[1].width.saturating_sub(4))
                ),
                fill(
                    &summary.progress_preview,
                    usize::from(chunks[1].width.saturating_sub(4))
                ),
            )
        } else {
            "Create a spec with n to begin.".to_owned()
        };

        let paragraph = Paragraph::new(preview)
            .block(
                Block::default()
                    .title("Preview")
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded),
            )
            .wrap(Wrap { trim: false });
        frame.render_widget(paragraph, chunks[1]);
    }

    fn draw_scoped(&mut self, frame: &mut Frame, area: Rect) {
        let summary = self.specs.get(self.selected);
        let contents = if let Some(summary) = summary {
            format!(
                "{}\n{}\n\nState: {}\n\nSpec Preview\n{}\n\nProgress Preview\n{}\n",
                summary.spec_path,
                summary.progress_path,
                state_label(summary.state),
                summary.spec_preview,
                summary.progress_preview,
            )
        } else {
            "No selected spec.".to_owned()
        };

        let panel = Paragraph::new(contents)
            .block(
                Block::default()
                    .title("Scoped Actions")
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded),
            )
            .wrap(Wrap { trim: false });
        frame.render_widget(panel, area);
    }

    fn draw_composer(&mut self, frame: &mut Frame, area: Rect) {
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

        let tabs = Tabs::new(vec!["Spec", "Progress"])
            .select(self.review_tab)
            .block(
                Block::default()
                    .title(format!("Review • {}", review.spec_path))
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded),
            )
            .highlight_style(Style::default().fg(Color::Cyan));
        frame.render_widget(tabs, chunks[0]);

        let body = if self.review_tab == 0 {
            &review.spec_contents
        } else {
            &review.progress_contents
        };
        let paragraph = Paragraph::new(Text::from(body.clone()))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded),
            )
            .wrap(Wrap { trim: false });
        frame.render_widget(paragraph, chunks[1]);
    }

    fn draw_running(&mut self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(1)])
            .split(area);
        let header = Paragraph::new(format!(
            "Target: {}\nStatus: {}",
            self.run_target.as_deref().unwrap_or("unknown"),
            self.status
        ))
        .block(
            Block::default()
                .title("Run")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded),
        );
        frame.render_widget(header, chunks[0]);

        let joined = self.run_logs.join("\n");
        let logs = Paragraph::new(joined)
            .block(
                Block::default()
                    .title("Live Output")
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded),
            )
            .wrap(Wrap { trim: false });
        frame.render_widget(logs, chunks[1]);
    }

    fn draw_clarification(&mut self, frame: &mut Frame, area: Rect) {
        let Some(modal) = self.clarification.as_mut() else {
            return;
        };
        let popup = centered_rect(70, 55, area);
        frame.render_widget(Clear, popup);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(8), Constraint::Min(6)])
            .split(popup);

        let mut text = vec![
            Line::from(Span::styled(
                "Clarification Required",
                Style::default()
                    .fg(Color::Yellow)
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
        text.push(Line::from(
            "Press 1-3 for an option, Ctrl-S to submit text, Esc to cancel.",
        ));

        let top = Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Question")
                    .border_type(BorderType::Rounded),
            )
            .wrap(Wrap { trim: true });
        frame.render_widget(top, chunks[0]);
        frame.render_widget(&modal.input, chunks[1]);
    }

    fn footer_text(&self) -> String {
        match self.screen {
            Screen::Dashboard => format!(
                "{}    Keys: n create • Enter open • j/k move • l reload • q quit",
                self.status
            ),
            Screen::Scoped => format!(
                "{}    Keys: r run • v review • e edit • R revise • p replan • Esc back",
                self.status
            ),
            Screen::Composer(_) => format!("{}    Keys: Ctrl-S submit • Esc cancel", self.status),
            Screen::Review => format!("{}    Keys: Tab switch pane • Esc back", self.status),
            Screen::Running => format!(
                "{}    Keys: Ctrl-C cancel agent • Esc back when idle",
                self.status
            ),
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

fn fresh_composer(title: &str) -> TextArea<'static> {
    let mut composer = TextArea::default();
    composer.set_block(
        Block::default()
            .borders(Borders::ALL)
            .title(title.to_owned())
            .border_type(BorderType::Rounded),
    );
    composer.set_cursor_line_style(Style::default().fg(Color::Cyan));
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

fn state_style(state: WorkflowState) -> Style {
    match state {
        WorkflowState::Empty => Style::default().fg(Color::Gray),
        WorkflowState::Planned => Style::default().fg(Color::Cyan),
        WorkflowState::Completed => Style::default().fg(Color::Green),
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
