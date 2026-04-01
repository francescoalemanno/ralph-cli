use std::{
    env, io,
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
    terminal::{
        Clear as TerminalClear, ClearType, EnterAlternateScreen, LeaveAlternateScreen,
        disable_raw_mode, enable_raw_mode,
    },
};
use ralph_app::{RalphApp, RunDelegate, RunEvent};
use ralph_core::{LastRunStatus, RunControl, ScaffoldId, TargetSummary};
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
use tokio::runtime::Handle;

pub fn run_tui(app: RalphApp) -> Result<()> {
    let handle = Handle::current();
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .context("failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("failed to create terminal backend")?;

    let result = TuiApp::new(app, handle, None).run(&mut terminal);

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

pub fn run_tui_scoped(app: RalphApp, target: &str) -> Result<()> {
    let handle = Handle::current();
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .context("failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("failed to create terminal backend")?;

    let result = TuiApp::new(app, handle, Some(target.to_owned())).run(&mut terminal);

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

#[derive(Clone, Copy)]
enum ColorMode {
    Light,
    Dark,
}

struct RunningState {
    target_id: String,
    prompt_name: String,
    iteration: usize,
    max_iterations: usize,
    control: RunControl,
    terminal: vt100::Parser,
    finished: bool,
    scroll: usize,
    follow: bool,
}

struct TuiApp {
    app: RalphApp,
    handle: Handle,
    tx: Sender<UiEvent>,
    rx: Receiver<UiEvent>,
    targets: Vec<TargetSummary>,
    selected_target: usize,
    selected_prompt: usize,
    screen: Screen,
    new_target_name: String,
    new_scaffold: ScaffoldId,
    message: String,
    running: Option<RunningState>,
    tick_count: u64,
    color_mode: ColorMode,
}

impl TuiApp {
    fn new(app: RalphApp, handle: Handle, target: Option<String>) -> Self {
        let (tx, rx) = mpsc::channel();
        let mut this = Self {
            app,
            handle,
            tx,
            rx,
            targets: Vec::new(),
            selected_target: 0,
            selected_prompt: 0,
            screen: Screen::Dashboard,
            new_target_name: String::new(),
            new_scaffold: ScaffoldId::Default,
            message: String::new(),
            running: None,
            tick_count: 0,
            color_mode: detect_color_mode(),
        };
        this.reload_targets();
        if let Some(target) = target {
            if let Some(index) = this.targets.iter().position(|summary| summary.id == target) {
                this.selected_target = index;
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
            Screen::NewTarget => self.handle_new_target_key(key),
            Screen::Running => self.handle_running_key(key),
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
            }
            KeyCode::Down => {
                if self.selected_target + 1 < self.targets.len() {
                    self.selected_target += 1;
                    self.selected_prompt = 0;
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
                self.new_scaffold = ScaffoldId::Default;
            }
            KeyCode::Char('r') => self.start_run()?,
            KeyCode::Char('e') => {
                let Some((target_id, prompt_name)) = self.selected_target_and_prompt() else {
                    self.message = "select a target and prompt first".to_owned();
                    return Ok(());
                };
                suspend_terminal(terminal)?;
                let result = self.app.edit_prompt(&target_id, Some(&prompt_name));
                resume_terminal(terminal)?;
                result?;
                self.message = format!("opened {prompt_name}");
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

    fn handle_new_target_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc => {
                self.screen = Screen::Dashboard;
            }
            KeyCode::Tab => {
                self.new_scaffold = match self.new_scaffold {
                    ScaffoldId::Blank => ScaffoldId::Default,
                    ScaffoldId::Default => ScaffoldId::Blank,
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
                let summary = self
                    .app
                    .create_target(self.new_target_name.trim(), Some(self.new_scaffold))?;
                self.reload_targets();
                if let Some(index) = self.targets.iter().position(|item| item.id == summary.id) {
                    self.selected_target = index;
                    self.selected_prompt = 0;
                }
                self.message = format!("created {}", summary.id);
                self.screen = Screen::Dashboard;
            }
            KeyCode::Char(ch) => {
                self.new_target_name.push(ch);
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_running_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc => {
                if self
                    .running
                    .as_ref()
                    .is_some_and(|running| running.finished)
                {
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
                    && running.finished
                {
                    let target_id = running.target_id.clone();
                    let prompt_name = running.prompt_name.clone();
                    self.start_run_for(&target_id, &prompt_name)?;
                }
            }
            KeyCode::Char('a') => {
                let running_control = self.running.as_ref().map(|running| running.control.clone());
                self.cycle_agent(running_control)?;
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
                            running.iteration = iteration;
                            running.max_iterations = max_iterations;
                            process_terminal_text(
                                &mut running.terminal,
                                &format!(
                                    "{}\n",
                                    ralph_app::format_iteration_banner(
                                        &prompt_name,
                                        iteration,
                                        max_iterations
                                    )
                                ),
                            );
                        }
                        RunEvent::Output(chunk) => {
                            process_terminal_text(&mut running.terminal, &chunk);
                        }
                        RunEvent::Note(note) => {
                            process_terminal_text(&mut running.terminal, &format!("\n{note}\n"));
                        }
                        RunEvent::Finished { status, summary } => {
                            running.finished = true;
                            process_terminal_text(
                                &mut running.terminal,
                                &format!("\n{summary} ({})\nPress Esc to return.", status.label()),
                            );
                        }
                    }
                    if running.follow {
                        running.scroll = 0;
                    }
                }
            }
            UiEvent::RunDone(result) => match result {
                Ok(summary) => {
                    self.reload_targets();
                    if let Some(index) = self.targets.iter().position(|item| item.id == summary.id)
                    {
                        self.selected_target = index;
                    }
                }
                Err(error) => {
                    if let Some(running) = &mut self.running {
                        running.finished = true;
                        process_terminal_text(
                            &mut running.terminal,
                            &format!("\nerror: {error}\nPress Esc to return."),
                        );
                    }
                }
            },
        }
    }

    fn draw(&mut self, frame: &mut Frame<'_>) {
        let has_notice = !self.message.trim().is_empty();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(if has_notice { 3 } else { 0 }),
                Constraint::Min(1),
                Constraint::Length(2),
            ])
            .split(frame.area());

        self.draw_header(frame, chunks[0]);

        if has_notice {
            self.draw_notice(frame, chunks[1]);
        }
        let content_area = chunks[2];
        let footer_area = chunks[3];

        match self.screen {
            Screen::Dashboard => self.draw_dashboard(frame, content_area),
            Screen::NewTarget => {
                self.draw_dashboard(frame, content_area);
                self.draw_new_target_modal(frame);
            }
            Screen::Running => self.draw_running(frame, content_area),
        }

        self.draw_footer(frame, footer_area);
    }

    fn draw_header(&self, frame: &mut Frame<'_>, area: Rect) {
        let active_count = self
            .targets
            .iter()
            .filter(|target| target.last_run_status != LastRunStatus::Completed)
            .count();
        let completed_count = self
            .targets
            .iter()
            .filter(|target| target.last_run_status == LastRunStatus::Completed)
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
                " durable prompt loop ",
                Style::default()
                    .fg(self.text_color())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "active {}  ◆  completed {}  ◆  agent {}  ◆  {}",
                    active_count,
                    completed_count,
                    self.app.coding_agent().label(),
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
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded),
        );
        frame.render_widget(banner, area);
    }

    fn draw_footer(&self, frame: &mut Frame<'_>, area: Rect) {
        let footer = Paragraph::new(Line::from(vec![
            Span::styled("N", key_style(self.accent_color())),
            Span::styled(" new  ", Style::default().fg(self.muted_color())),
            Span::styled("R", key_style(self.success_color())),
            Span::styled(" run  ", Style::default().fg(self.muted_color())),
            Span::styled("E", key_style(self.accent_color())),
            Span::styled(" edit  ", Style::default().fg(self.muted_color())),
            Span::styled("D", key_style(self.warning_color())),
            Span::styled(" delete  ", Style::default().fg(self.muted_color())),
            Span::styled("A", key_style(self.accent_color())),
            Span::styled(" agent  ", Style::default().fg(self.muted_color())),
            Span::styled("Arrows", key_style(self.text_color())),
            Span::styled(" navigate  ", Style::default().fg(self.muted_color())),
            Span::styled("Q", key_style(self.warning_color())),
            Span::styled(" quit/cancel", Style::default().fg(self.muted_color())),
        ]))
        .style(Style::default().fg(self.muted_color()))
        .wrap(Wrap { trim: true });
        frame.render_widget(footer, area);
    }

    fn draw_dashboard(&self, frame: &mut Frame<'_>, area: Rect) {
        let main = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
            .split(area);

        self.draw_target_list(frame, main[0]);
        self.draw_target_detail(frame, main[1]);
    }

    fn draw_target_list(&self, frame: &mut Frame<'_>, area: Rect) {
        let items = if self.targets.is_empty() {
            vec![ListItem::new(Line::from(vec![Span::styled(
                "No targets yet. Press N to create one.",
                Style::default().fg(self.muted_color()),
            )]))]
        } else {
            self.targets
                .iter()
                .map(|target| {
                    let prompt_count = target.prompt_files.len();
                    let scaffold = target
                        .scaffold
                        .map(|value| value.as_str())
                        .unwrap_or("none");
                    let status = target.last_run_status;
                    ListItem::new(Text::from(vec![
                        Line::from(vec![
                            Span::styled(
                                format!(" {} ", status_badge(status)),
                                status_style(
                                    status,
                                    self.accent_color(),
                                    self.success_color(),
                                    self.warning_color(),
                                    self.muted_color(),
                                )
                                .add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(
                                format!(" {}", target.id),
                                Style::default()
                                    .fg(self.text_color())
                                    .add_modifier(Modifier::BOLD),
                            ),
                        ]),
                        Line::from(vec![
                            Span::styled(
                                format!(" status {} ", status_label(status)),
                                Style::default().fg(self.muted_color()),
                            ),
                            Span::styled("◆", Style::default().fg(self.subtle_color())),
                            Span::styled(
                                format!(" prompts {} ", prompt_count),
                                Style::default().fg(self.muted_color()),
                            ),
                            Span::styled("◆", Style::default().fg(self.subtle_color())),
                            Span::styled(
                                format!(" scaffold {}", scaffold),
                                Style::default().fg(self.muted_color()),
                            ),
                        ]),
                    ]))
                })
                .collect::<Vec<_>>()
        };

        let mut state = ListState::default();
        if !self.targets.is_empty() {
            state.select(Some(self.selected_target.min(self.targets.len() - 1)));
        }

        let list = List::new(items)
            .block(
                Block::default()
                    .title(self.title_line("Targets", "Select a workspace"))
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded),
            )
            .highlight_style(
                Style::default()
                    .bg(self.panel_highlight())
                    .fg(self.text_color())
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▌ ");
        frame.render_stateful_widget(list, area, &mut state);
    }

    fn draw_target_detail(&self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::default()
            .title(self.title_line("Selected Target", "Prompt selection and durable files"))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded);

        if let Some(target) = self.selected_target() {
            let inner = block.inner(area);
            frame.render_widget(block, area);

            let sections = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(4),
                    Constraint::Length(3),
                    Constraint::Min(8),
                    Constraint::Length(7),
                ])
                .split(inner);

            let header = Paragraph::new(Text::from(vec![
                Line::from(vec![
                    Span::styled(
                        format!(" {} ", status_badge(target.last_run_status)),
                        status_style(
                            target.last_run_status,
                            self.accent_color(),
                            self.success_color(),
                            self.warning_color(),
                            self.muted_color(),
                        )
                        .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(" {}", target.id),
                        Style::default()
                            .fg(self.text_color())
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(vec![Span::styled(
                    format!(
                        "scaffold {}  ◆  last_prompt {}",
                        target
                            .scaffold
                            .map(|value| value.as_str())
                            .unwrap_or("none"),
                        target.last_prompt.as_deref().unwrap_or("<none>")
                    ),
                    Style::default().fg(self.muted_color()),
                )]),
                Line::from(vec![Span::styled(
                    format!(
                        "{} prompt files  ◆  {} total files",
                        target.prompt_files.len(),
                        target.files.len()
                    ),
                    Style::default().fg(self.muted_color()),
                )]),
            ]))
            .block(
                Block::default()
                    .title(self.title_line("Overview", "Current target metadata"))
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded),
            );
            frame.render_widget(header, sections[0]);

            let titles = if target.prompt_files.is_empty() {
                vec![Line::from("no prompts")]
            } else {
                target
                    .prompt_files
                    .iter()
                    .map(|prompt| Line::from(prompt.name.clone()))
                    .collect::<Vec<_>>()
            };
            let tabs = Tabs::new(titles)
                .select(
                    self.selected_prompt
                        .min(target.prompt_files.len().saturating_sub(1)),
                )
                .block(
                    Block::default()
                        .title(self.title_line("Prompts", "Choose which loop prompt to run"))
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded),
                )
                .highlight_style(
                    Style::default()
                        .fg(self.accent_color())
                        .add_modifier(Modifier::BOLD),
                )
                .style(Style::default().fg(self.muted_color()))
                .divider(" ◆ ");
            frame.render_widget(tabs, sections[1]);

            let prompt_preview = self
                .selected_prompt()
                .and_then(|prompt| {
                    self.app.review_target(&target.id).ok().and_then(|review| {
                        review
                            .files
                            .into_iter()
                            .find(|file| file.name == prompt.name)
                            .map(|file| file.contents)
                    })
                })
                .unwrap_or_else(|| "<missing prompt>".to_owned());

            let preview = Paragraph::new(prompt_preview)
                .block(
                    Block::default()
                        .title(self.title_line("Prompt Preview", "Selected runnable prompt"))
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded),
                )
                .style(Style::default().fg(self.text_color()))
                .wrap(Wrap { trim: false });
            frame.render_widget(preview, sections[2]);

            let files = Paragraph::new(
                target
                    .files
                    .iter()
                    .map(|file| {
                        if file.is_prompt {
                            format!("* {}", file.name)
                        } else {
                            file.name.clone()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
            .block(
                Block::default()
                    .title(self.title_line("Files", "Runnable prompts are marked with *"))
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded),
            )
            .style(Style::default().fg(self.muted_color()))
            .wrap(Wrap { trim: false });
            frame.render_widget(files, sections[3]);
        } else {
            frame.render_widget(
                Paragraph::new(Line::from(vec![Span::styled(
                    "No target selected",
                    Style::default().fg(self.muted_color()),
                )]))
                .block(block),
                area,
            );
        }
    }

    fn draw_new_target_modal(&self, frame: &mut Frame<'_>) {
        let area = centered_rect(60, 34, frame.area());
        frame.render_widget(Clear, area);
        let cursor_on = (self.tick_count / 4).is_multiple_of(2);
        let name_display = if cursor_on {
            format!("{}█", self.new_target_name)
        } else {
            format!("{} ", self.new_target_name)
        };
        let text = Text::from(vec![
            Line::from(vec![Span::styled(
                "Create a target with an initialization scaffold.",
                Style::default().fg(self.muted_color()),
            )]),
            Line::from(""),
            Line::from(vec![
                Span::styled("name ", key_style(self.accent_color())),
                Span::styled(name_display, Style::default().fg(self.text_color())),
            ]),
            Line::from(vec![
                Span::styled("scaffold ", key_style(self.success_color())),
                Span::styled(
                    self.new_scaffold.as_str(),
                    Style::default()
                        .fg(self.text_color())
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("Tab", key_style(self.accent_color())),
                Span::styled(
                    " switch scaffold  ",
                    Style::default().fg(self.muted_color()),
                ),
                Span::styled("Enter", key_style(self.success_color())),
                Span::styled(" create  ", Style::default().fg(self.muted_color())),
                Span::styled("Esc", key_style(self.warning_color())),
                Span::styled(" cancel", Style::default().fg(self.muted_color())),
            ]),
        ]);
        let widget = Paragraph::new(text)
            .block(
                Block::default()
                    .title(self.title_line("New Target", "Default or blank scaffold"))
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded),
            )
            .style(Style::default().fg(self.text_color()))
            .wrap(Wrap { trim: false });
        frame.render_widget(widget, area);
    }

    fn draw_running(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(5), Constraint::Min(1)])
            .split(area);

        let telemetry = if let Some(running) = self.running.as_ref() {
            let pulse = if (self.tick_count / 2).is_multiple_of(2) {
                "●"
            } else {
                "◉"
            };
            Paragraph::new(Text::from(vec![
                Line::from(vec![
                    Span::styled(
                        format!(" {} ", if running.finished { "DONE" } else { "LIVE" }),
                        if running.finished {
                            Style::default()
                                .fg(Color::Black)
                                .bg(self.success_color())
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                                .fg(Color::Black)
                                .bg(self.warning_color())
                                .add_modifier(Modifier::BOLD)
                        },
                    ),
                    Span::raw(" "),
                    Span::styled(
                        format!(
                            "{} / {}  ◆  iter {}/{}",
                            running.target_id,
                            running.prompt_name,
                            running.iteration,
                            running.max_iterations
                        ),
                        Style::default()
                            .fg(self.text_color())
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(vec![
                    Span::styled(pulse, Style::default().fg(self.success_color())),
                    Span::styled(
                        if running.finished {
                            " run finished"
                        } else {
                            " streaming agent output"
                        },
                        Style::default().fg(self.muted_color()),
                    ),
                    Span::styled("  ◆  ", Style::default().fg(self.subtle_color())),
                    Span::styled(
                        if running.finished {
                            "R reruns  ◆  Esc returns to dashboard"
                        } else {
                            "Q sends cancel"
                        },
                        Style::default().fg(self.muted_color()),
                    ),
                ]),
            ]))
        } else {
            Paragraph::new("No run selected")
        }
        .block(
            Block::default()
                .title(self.title_line("Live Run", "Telemetry and controls"))
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded),
        );
        frame.render_widget(telemetry, sections[0]);

        let block = Block::default()
            .title(self.title_line("Agent Stream", "stdout and stderr"))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded);
        let inner = block.inner(sections[1]);
        let output = if let Some(running) = &mut self.running {
            running
                .terminal
                .set_size(inner.height.max(1), inner.width.max(1));
            let scroll = if running.follow { 0 } else { running.scroll };
            running.terminal.set_scrollback(scroll);
            running.terminal.screen().contents()
        } else {
            String::new()
        };
        let paragraph = Paragraph::new(output)
            .block(block)
            .style(Style::default().fg(self.text_color()));
        frame.render_widget(paragraph, sections[1]);
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
            }
            Err(error) => {
                self.targets = Vec::new();
                self.message = error.to_string();
            }
        }
    }

    fn selected_target(&self) -> Option<&TargetSummary> {
        self.targets.get(self.selected_target)
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

    fn start_run(&mut self) -> Result<()> {
        let Some((target_id, prompt_name)) = self.selected_target_and_prompt() else {
            self.message = "select a target and prompt first".to_owned();
            return Ok(());
        };

        self.start_run_for(&target_id, &prompt_name)
    }

    fn start_run_for(&mut self, target_id: &str, prompt_name: &str) -> Result<()> {
        let tx = self.tx.clone();
        let app = self.app.clone();
        let control = RunControl::new();
        let run_control = control.clone();
        let target_id = target_id.to_owned();
        let prompt_name = prompt_name.to_owned();
        self.running = Some(RunningState {
            target_id: target_id.to_owned(),
            prompt_name: prompt_name.to_owned(),
            iteration: 0,
            max_iterations: 0,
            control,
            terminal: vt100::Parser::new(24, 80, 100_000),
            finished: false,
            scroll: 0,
            follow: true,
        });
        self.screen = Screen::Running;

        self.handle.spawn(async move {
            let mut delegate = ChannelDelegate { tx: tx.clone() };
            let result = app
                .run_target_with_control(&target_id, Some(&prompt_name), run_control, &mut delegate)
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

    fn notice_palette(&self) -> (&'static str, Color, Color) {
        match self.color_mode {
            ColorMode::Light => (" INFO ", Color::Black, Color::Rgb(191, 219, 254)),
            ColorMode::Dark => (" INFO ", Color::Black, Color::Rgb(103, 232, 249)),
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
            .send(UiEvent::RunEvent(event))
            .map_err(|_| anyhow!("failed to send run event"))
    }
}

fn suspend_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen)
        .context("failed to leave alternate screen")?;
    Ok(())
}

fn resume_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    execute!(terminal.backend_mut(), EnterAlternateScreen)
        .context("failed to re-enter alternate screen")?;
    enable_raw_mode().context("failed to re-enable raw mode")?;
    execute!(terminal.backend_mut(), TerminalClear(ClearType::All))
        .context("failed to clear terminal after editor exit")?;
    terminal
        .clear()
        .context("failed to reset terminal buffer after editor exit")?;
    terminal.hide_cursor().ok();
    Ok(())
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup = Layout::default()
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
        .split(popup[1])[1]
}

fn detect_color_mode() -> ColorMode {
    if let Ok(value) = env::var("RALPH_COLOR_MODE") {
        match value.trim().to_ascii_lowercase().as_str() {
            "light" => return ColorMode::Light,
            "dark" => return ColorMode::Dark,
            _ => {}
        }
    }

    if let Ok(value) = env::var("COLORFGBG")
        && let Some(background) = value
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

    ColorMode::Dark
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

fn process_terminal_text(terminal: &mut vt100::Parser, text: &str) {
    let mut normalized = Vec::with_capacity(text.len() + 16);
    let mut previous = None;
    for byte in text.bytes() {
        if byte == b'\n' && previous != Some(b'\r') {
            normalized.push(b'\r');
        }
        normalized.push(byte);
        previous = Some(byte);
    }
    terminal.process(&normalized);
}

fn status_badge(status: LastRunStatus) -> &'static str {
    match status {
        LastRunStatus::NeverRun => "○",
        LastRunStatus::Completed => "✓",
        LastRunStatus::MaxIterations => "◉",
        LastRunStatus::Failed => "!",
        LastRunStatus::Canceled => "×",
    }
}

fn status_label(status: LastRunStatus) -> &'static str {
    match status {
        LastRunStatus::NeverRun => "never run",
        LastRunStatus::Completed => "completed",
        LastRunStatus::MaxIterations => "max iterations",
        LastRunStatus::Failed => "failed",
        LastRunStatus::Canceled => "canceled",
    }
}

fn status_style(
    status: LastRunStatus,
    accent: Color,
    success: Color,
    warning: Color,
    muted: Color,
) -> Style {
    match status {
        LastRunStatus::NeverRun => Style::default().fg(muted),
        LastRunStatus::Completed => Style::default().fg(Color::Black).bg(success),
        LastRunStatus::MaxIterations => Style::default().fg(Color::Black).bg(warning),
        LastRunStatus::Failed => Style::default().fg(Color::White).bg(Color::Red),
        LastRunStatus::Canceled => Style::default().fg(accent),
    }
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

fn color_from_name(name: &str) -> Option<Color> {
    let normalized = name.trim().to_ascii_lowercase();
    Some(match normalized.as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "gray" | "grey" => Color::Gray,
        "darkgray" | "dark_gray" | "darkgrey" | "dark_grey" => Color::DarkGray,
        "lightred" | "light_red" => Color::LightRed,
        "lightgreen" | "light_green" => Color::LightGreen,
        "lightyellow" | "light_yellow" => Color::LightYellow,
        "lightblue" | "light_blue" => Color::LightBlue,
        "lightmagenta" | "light_magenta" => Color::LightMagenta,
        "lightcyan" | "light_cyan" => Color::LightCyan,
        "white" => Color::White,
        _ => return None,
    })
}
