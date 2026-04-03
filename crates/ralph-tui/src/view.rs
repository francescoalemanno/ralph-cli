use ralph_core::LastRunStatus;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap,
    },
};

use crate::{
    Screen, TuiApp,
    ui::{
        ColorMode, centered_rect, key_style, resolved_accent_color, resolved_success_color,
        resolved_warning_color, status_badge, status_label, status_style, styled_title,
    },
};

#[derive(Clone, Copy)]
enum ShortcutTone {
    Accent,
    Success,
    Warning,
    Neutral,
}

#[derive(Clone, Copy)]
struct ShortcutHint {
    key: &'static str,
    label: &'static str,
    tone: ShortcutTone,
}

impl TuiApp {
    pub(super) fn draw(&mut self, frame: &mut Frame<'_>) {
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
        let footer = Paragraph::new(Line::from(self.footer_spans()))
            .style(Style::default().fg(self.muted_color()))
            .wrap(Wrap { trim: true });
        frame.render_widget(footer, area);
    }

    fn footer_spans(&self) -> Vec<Span<'static>> {
        match self.screen {
            Screen::Dashboard => shortcut_spans(
                &[
                    ShortcutHint {
                        key: "N",
                        label: "new",
                        tone: ShortcutTone::Accent,
                    },
                    ShortcutHint {
                        key: "R",
                        label: "run",
                        tone: ShortcutTone::Success,
                    },
                    ShortcutHint {
                        key: "E",
                        label: "edit",
                        tone: ShortcutTone::Accent,
                    },
                    ShortcutHint {
                        key: "D",
                        label: "delete",
                        tone: ShortcutTone::Warning,
                    },
                    ShortcutHint {
                        key: "A",
                        label: "agent",
                        tone: ShortcutTone::Accent,
                    },
                    ShortcutHint {
                        key: "Arrows",
                        label: "navigate",
                        tone: ShortcutTone::Neutral,
                    },
                    ShortcutHint {
                        key: "Q",
                        label: "quit/cancel",
                        tone: ShortcutTone::Warning,
                    },
                ],
                self,
            ),
            Screen::NewTarget => shortcut_spans(
                &[
                    ShortcutHint {
                        key: "Tab",
                        label: "switch scaffold",
                        tone: ShortcutTone::Accent,
                    },
                    ShortcutHint {
                        key: "Enter",
                        label: "create",
                        tone: ShortcutTone::Success,
                    },
                    ShortcutHint {
                        key: "Backspace",
                        label: "erase",
                        tone: ShortcutTone::Warning,
                    },
                    ShortcutHint {
                        key: "Esc",
                        label: "cancel",
                        tone: ShortcutTone::Warning,
                    },
                ],
                self,
            ),
            Screen::Running => shortcut_spans(&self.running_shortcuts(), self),
        }
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

            let uses_hidden_workflow = target.uses_hidden_workflow();
            let titles = if uses_hidden_workflow {
                vec![Line::from("workflow input")]
            } else if target.prompt_files.is_empty() {
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
                        .title(self.title_line(
                            "Prompts",
                            if uses_hidden_workflow {
                                "Workflow targets run internally"
                            } else {
                                "Choose which loop prompt to run"
                            },
                        ))
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
                .selected_target_review()
                .and_then(|review| {
                    if uses_hidden_workflow {
                        review
                            .files
                            .iter()
                            .find(|file| file.name == "GOAL.md")
                            .map(|file| file.contents.clone())
                    } else {
                        self.selected_prompt().and_then(|prompt| {
                            review
                                .files
                                .iter()
                                .find(|file| file.name == prompt.name)
                                .map(|file| file.contents.clone())
                        })
                    }
                })
                .unwrap_or_else(|| {
                    if uses_hidden_workflow {
                        "<missing GOAL.md>".to_owned()
                    } else {
                        "<missing prompt>".to_owned()
                    }
                });

            let preview = Paragraph::new(prompt_preview)
                .block(
                    Block::default()
                        .title(self.title_line(
                            if uses_hidden_workflow {
                                "Goal Preview"
                            } else {
                                "Prompt Preview"
                            },
                            if uses_hidden_workflow {
                                "User-facing workflow input"
                            } else {
                                "Selected runnable prompt"
                            },
                        ))
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
                    .title(self.title_line(
                        "Files",
                        if uses_hidden_workflow {
                            "Workflow targets expose GOAL.md and state files"
                        } else {
                            "Runnable prompts are marked with *"
                        },
                    ))
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
                    .title(self.title_line(
                        "New Target",
                        "Task-based, goal-driven, single-prompt, or plan-build scaffold",
                    ))
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
                            "E edits input  ◆  R reruns  ◆  Esc returns"
                        } else {
                            "E edits input  ◆  A switches agent  ◆  Q cancels"
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

    fn running_shortcuts(&self) -> Vec<ShortcutHint> {
        let finished = self
            .running
            .as_ref()
            .is_some_and(|running| running.finished);
        let mut hints = vec![
            ShortcutHint {
                key: "E",
                label: "edit input",
                tone: ShortcutTone::Accent,
            },
            ShortcutHint {
                key: "A",
                label: "agent",
                tone: ShortcutTone::Accent,
            },
        ];

        if finished {
            hints.push(ShortcutHint {
                key: "R",
                label: "rerun",
                tone: ShortcutTone::Success,
            });
            hints.push(ShortcutHint {
                key: "Esc",
                label: "dashboard",
                tone: ShortcutTone::Warning,
            });
        } else {
            hints.push(ShortcutHint {
                key: "Q",
                label: "cancel",
                tone: ShortcutTone::Warning,
            });
        }

        hints.extend([
            ShortcutHint {
                key: "↑↓",
                label: "scroll",
                tone: ShortcutTone::Neutral,
            },
            ShortcutHint {
                key: "PgUp/PgDn",
                label: "page",
                tone: ShortcutTone::Neutral,
            },
            ShortcutHint {
                key: "Home/End",
                label: "jump",
                tone: ShortcutTone::Neutral,
            },
        ]);
        hints
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

fn shortcut_spans(hints: &[ShortcutHint], app: &TuiApp) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (index, hint) in hints.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("  ", Style::default().fg(app.muted_color())));
        }
        spans.push(Span::styled(
            hint.key,
            key_style(match hint.tone {
                ShortcutTone::Accent => app.accent_color(),
                ShortcutTone::Success => app.success_color(),
                ShortcutTone::Warning => app.warning_color(),
                ShortcutTone::Neutral => app.text_color(),
            }),
        ));
        spans.push(Span::styled(
            format!(" {}", hint.label),
            Style::default().fg(app.muted_color()),
        ));
    }
    spans
}
