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
    ConfirmationDialog, Screen, TuiApp,
    ui::{
        centered_rect, key_style, resolved_accent_color, resolved_success_color,
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

struct OwnedShortcutHint {
    key: String,
    label: String,
    tone: ShortcutTone,
}

impl TuiApp {
    pub(super) fn draw(&mut self, frame: &mut Frame<'_>) {
        frame.render_widget(
            Block::default().style(Style::default().bg(self.background_color())),
            frame.area(),
        );

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
            Screen::Dashboard => {
                self.draw_dashboard(frame, content_area);
                if let Some(dialog) = self.confirmation.as_ref() {
                    self.draw_confirmation_modal(frame, dialog);
                }
            }
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
                    self.app.agent_name(),
                    self.app.project_dir()
                ),
                Style::default().fg(self.muted_color()),
            ),
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

    fn draw_footer(&self, frame: &mut Frame<'_>, area: Rect) {
        let footer = Paragraph::new(Line::from(self.footer_spans()))
            .style(Style::default().fg(self.muted_color()))
            .wrap(Wrap { trim: true });
        frame.render_widget(footer, area);
    }

    fn footer_spans(&self) -> Vec<Span<'static>> {
        if let Some(_dialog) = self.confirmation.as_ref() {
            return shortcut_spans(
                &[
                    ShortcutHint {
                        key: "Tab/←→",
                        label: "switch",
                        tone: ShortcutTone::Accent,
                    },
                    ShortcutHint {
                        key: "Enter",
                        label: "choose",
                        tone: ShortcutTone::Success,
                    },
                    ShortcutHint {
                        key: "Esc",
                        label: "cancel",
                        tone: ShortcutTone::Warning,
                    },
                ],
                self,
            );
        }

        match self.screen {
            Screen::Dashboard => {
                let mut hints = vec![
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
                ];
                hints.extend([
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
                ]);
                let mut spans = shortcut_spans(&hints, self);
                if self.selected_target_has_flow_entrypoints() {
                    let dynamic_hints = self
                        .selected_flow_actions()
                        .into_iter()
                        .filter_map(|action| {
                            let shortcut = action.shortcut?;
                            Some(OwnedShortcutHint {
                                key: shortcut,
                                label: action.label,
                                tone: ShortcutTone::Accent,
                            })
                        })
                        .collect::<Vec<_>>();
                    if !dynamic_hints.is_empty() {
                        spans.push(Span::styled("  ", Style::default().fg(self.muted_color())));
                        spans.extend(shortcut_spans_owned(&dynamic_hints, self));
                    }
                }
                spans
            }
            Screen::NewTarget => shortcut_spans(
                &[
                    ShortcutHint {
                        key: "Tab",
                        label: "switch template",
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
                    let template = target
                        .template
                        .clone()
                        .or_else(|| target.scaffold.map(|value| value.as_str().to_owned()))
                        .unwrap_or_else(|| "none".to_owned());
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
                                format!(" template {}", template),
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
                self.panel_block()
                    .title(self.title_line("Targets", "Select a workspace")),
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
        let block = self
            .panel_block()
            .title(self.title_line("Selected Target", "Prompt selection and durable files"))
            .style(Style::default().bg(self.background_color()));

        if let Some(target) = self.selected_target() {
            let inner = block.inner(area);
            frame.render_widget(block, area);

            let sections = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(5),
                    Constraint::Length(3),
                    Constraint::Min(8),
                    Constraint::Length(7),
                ])
                .split(inner);

            let template = target
                .template
                .clone()
                .or_else(|| target.scaffold.map(|value| value.as_str().to_owned()))
                .unwrap_or_else(|| "none".to_owned());
            let flow_status = self.selected_flow_status();
            let mut header_lines = vec![
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
                        "template {}  ◆  last_prompt {}",
                        template,
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
            ];
            if let Some(flow_status) = flow_status {
                let flow_state = if flow_status.pause.is_some() {
                    "paused"
                } else {
                    "active"
                };
                header_lines.push(Line::from(vec![Span::styled(
                    format!(
                        "entrypoint {}  ◆  node {}  ◆  state {}",
                        flow_status.entrypoint_id,
                        flow_status.current_node.as_deref().unwrap_or("<start>"),
                        flow_state,
                    ),
                    Style::default().fg(self.muted_color()),
                )]));
            }

            let header = Paragraph::new(Text::from(header_lines)).block(
                self.panel_block()
                    .title(self.title_line("Overview", "Current target metadata")),
            );
            frame.render_widget(header, sections[0]);

            let has_flow_entrypoints = target.has_flow_entrypoints();
            let titles = if has_flow_entrypoints {
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
                .block(self.panel_block().title(self.title_line(
                    "Prompts",
                    if has_flow_entrypoints {
                        "Flow entrypoints run internally"
                    } else {
                        "Choose which loop prompt to run"
                    },
                )))
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
                    if has_flow_entrypoints {
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
                    if has_flow_entrypoints {
                        "<missing GOAL.md>".to_owned()
                    } else {
                        "<missing prompt>".to_owned()
                    }
                });

            let preview = Paragraph::new(prompt_preview)
                .block(self.panel_block().title(self.title_line(
                    if has_flow_entrypoints {
                        "Goal Preview"
                    } else {
                        "Prompt Preview"
                    },
                    if has_flow_entrypoints {
                        "User-facing workflow input"
                    } else {
                        "Selected runnable prompt"
                    },
                )))
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
            .block(self.panel_block().title(self.title_line(
                "Files",
                if has_flow_entrypoints {
                    "Flow targets expose GOAL.md and state files"
                } else {
                    "Runnable prompts are marked with *"
                },
            )))
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
        let selected_template = self.selected_workflow_template();
        let template_name = selected_template
            .map(|template| template.name.as_str())
            .unwrap_or("<none>");
        let template_source = selected_template
            .map(|template| template.source.label())
            .unwrap_or("none");
        let template_description = selected_template
            .and_then(|template| template.description.as_deref())
            .unwrap_or("No workflow templates are available.");
        let text = Text::from(vec![
            Line::from(vec![Span::styled(
                "Create a target from a discovered workflow template.",
                Style::default().fg(self.muted_color()),
            )]),
            Line::from(""),
            Line::from(vec![
                Span::styled("name ", key_style(self.accent_color())),
                Span::styled(name_display, Style::default().fg(self.text_color())),
            ]),
            Line::from(vec![
                Span::styled("template ", key_style(self.success_color())),
                Span::styled(
                    template_name,
                    Style::default()
                        .fg(self.text_color())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  ({template_source})"),
                    Style::default().fg(self.muted_color()),
                ),
            ]),
            Line::from(vec![Span::styled(
                template_description,
                Style::default().fg(self.muted_color()),
            )]),
            Line::from(""),
            Line::from(vec![
                Span::styled("Tab", key_style(self.accent_color())),
                Span::styled(
                    " switch template  ",
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
                self.panel_block()
                    .title(self.title_line("New Target", "Builtin and user workflow templates")),
            )
            .style(
                Style::default()
                    .fg(self.text_color())
                    .bg(self.background_color()),
            )
            .wrap(Wrap { trim: false });
        frame.render_widget(widget, area);
    }

    fn draw_confirmation_modal(&self, frame: &mut Frame<'_>, dialog: &ConfirmationDialog) {
        let area = centered_rect(58, 32, frame.area());
        frame.render_widget(Clear, area);

        let no_style = if dialog.confirm_selected {
            Style::default().fg(self.muted_color())
        } else {
            Style::default()
                .fg(Color::Black)
                .bg(self.warning_color())
                .add_modifier(Modifier::BOLD)
        };
        let yes_style = if dialog.confirm_selected {
            Style::default()
                .fg(Color::Black)
                .bg(self.success_color())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(self.muted_color())
        };

        let text = Text::from(vec![
            Line::from(vec![Span::styled(
                "This action is destructive.",
                Style::default()
                    .fg(self.warning_color())
                    .add_modifier(Modifier::BOLD),
            )]),
            Line::from(""),
            Line::from(vec![Span::styled(
                dialog.body.clone(),
                Style::default().fg(self.text_color()),
            )]),
            Line::from(""),
            Line::from(vec![
                Span::styled("Tab", key_style(self.accent_color())),
                Span::styled(" switch  ", Style::default().fg(self.muted_color())),
                Span::styled("Enter", key_style(self.success_color())),
                Span::styled(" choose  ", Style::default().fg(self.muted_color())),
                Span::styled("Esc", key_style(self.warning_color())),
                Span::styled(" cancel", Style::default().fg(self.muted_color())),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled(" No ", no_style),
                Span::styled("   ", Style::default().fg(self.background_color())),
                Span::styled(" Yes ", yes_style),
            ]),
        ]);

        let widget = Paragraph::new(text)
            .block(
                self.panel_block()
                    .title(self.title_line(&dialog.title, "No is selected by default")),
            )
            .style(
                Style::default()
                    .fg(self.text_color())
                    .bg(self.background_color()),
            )
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
            let (badge_text, badge_style, indicator, indicator_style, status_text) =
                match running.status() {
                    None => (
                        "LIVE",
                        Style::default()
                            .fg(Color::Black)
                            .bg(self.warning_color())
                            .add_modifier(Modifier::BOLD),
                        pulse,
                        Style::default().fg(self.success_color()),
                        " streaming agent output",
                    ),
                    Some(LastRunStatus::Completed) => (
                        "DONE",
                        Style::default()
                            .fg(Color::Black)
                            .bg(self.success_color())
                            .add_modifier(Modifier::BOLD),
                        status_badge(LastRunStatus::Completed),
                        Style::default()
                            .fg(self.success_color())
                            .add_modifier(Modifier::BOLD),
                        " run completed",
                    ),
                    Some(LastRunStatus::MaxIterations) => (
                        "LIMIT",
                        Style::default()
                            .fg(Color::Black)
                            .bg(self.warning_color())
                            .add_modifier(Modifier::BOLD),
                        status_badge(LastRunStatus::MaxIterations),
                        Style::default()
                            .fg(self.warning_color())
                            .add_modifier(Modifier::BOLD),
                        " max iterations reached",
                    ),
                    Some(LastRunStatus::Failed) => (
                        "FAIL",
                        Style::default()
                            .fg(Color::White)
                            .bg(Color::Red)
                            .add_modifier(Modifier::BOLD),
                        status_badge(LastRunStatus::Failed),
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                        " run failed",
                    ),
                    Some(LastRunStatus::Canceled) => (
                        "CANCELED",
                        Style::default()
                            .fg(Color::Black)
                            .bg(self.accent_color())
                            .add_modifier(Modifier::BOLD),
                        status_badge(LastRunStatus::Canceled),
                        Style::default()
                            .fg(self.accent_color())
                            .add_modifier(Modifier::BOLD),
                        " run canceled",
                    ),
                    Some(LastRunStatus::NeverRun) => (
                        "DONE",
                        Style::default()
                            .fg(Color::Black)
                            .bg(self.success_color())
                            .add_modifier(Modifier::BOLD),
                        status_badge(LastRunStatus::NeverRun),
                        Style::default().fg(self.muted_color()),
                        " run finished",
                    ),
                };
            Paragraph::new(Text::from(vec![
                Line::from(vec![
                    Span::styled(format!(" {badge_text} "), badge_style),
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
                    Span::styled(indicator, indicator_style),
                    Span::styled(status_text, Style::default().fg(self.muted_color())),
                    Span::styled("  ◆  ", Style::default().fg(self.subtle_color())),
                    Span::styled(
                        if running.is_finished() {
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
            self.panel_block()
                .title(self.title_line("Live Run", "Telemetry and controls")),
        );
        frame.render_widget(telemetry, sections[0]);

        let block = self
            .panel_block()
            .title(self.title_line("Agent Stream", "stdout and stderr"))
            .style(Style::default().bg(self.background_color()));
        let inner = block.inner(sections[1]);
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
        frame.render_widget(paragraph, sections[1]);
    }

    fn running_shortcuts(&self) -> Vec<ShortcutHint> {
        let finished = self
            .running
            .as_ref()
            .is_some_and(|running| running.is_finished());
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
        resolved_accent_color(&self.app.config().theme.accent_color)
    }

    fn success_color(&self) -> Color {
        resolved_success_color(&self.app.config().theme.success_color)
    }

    fn warning_color(&self) -> Color {
        resolved_warning_color(&self.app.config().theme.warning_color)
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

    fn background_color(&self) -> Color {
        Color::Rgb(8, 12, 18)
    }

    fn panel_highlight(&self) -> Color {
        Color::Rgb(24, 47, 56)
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

    fn panel_block(&self) -> Block<'static> {
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(self.background_color()))
    }

    fn notice_palette(&self) -> (&'static str, Color, Color) {
        (" INFO ", Color::Black, Color::Rgb(103, 232, 249))
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

fn shortcut_spans_owned(hints: &[OwnedShortcutHint], app: &TuiApp) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (index, hint) in hints.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("  ", Style::default().fg(app.muted_color())));
        }
        spans.push(Span::styled(
            hint.key.clone(),
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

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use camino::Utf8PathBuf;
    use ralph_app::RalphApp;
    use ralph_core::{LastRunStatus, ScaffoldId};
    use ratatui::{Terminal, backend::TestBackend};
    use std::sync::OnceLock;
    use tempfile::TempDir;
    use tokio::runtime::Runtime;

    use crate::{RunningState, Screen, TuiApp};

    fn configure_test_user_config_home() {
        static TEST_CONFIG_HOME: OnceLock<Utf8PathBuf> = OnceLock::new();
        let path = TEST_CONFIG_HOME.get_or_init(|| {
            let path = Utf8PathBuf::from_path_buf(
                std::env::temp_dir().join(format!("ralph-test-config-{}", std::process::id())),
            )
            .unwrap();
            std::fs::create_dir_all(&path).unwrap();
            path
        });
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", path.as_str());
        }
    }

    fn temp_project_dir() -> (TempDir, Utf8PathBuf) {
        configure_test_user_config_home();
        let temp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        (temp, path)
    }

    fn buffer_rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
        let buffer = terminal.backend().buffer();
        let width = buffer.area.width as usize;

        buffer
            .content
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect())
            .collect()
    }

    #[test]
    fn live_run_stream_soft_wraps_long_logical_lines() -> Result<()> {
        let (_temp, project_dir) = temp_project_dir();
        let app = RalphApp::load(project_dir)?;
        app.create_target("demo", Some(ScaffoldId::SinglePrompt))?;

        let runtime = Runtime::new()?;
        let mut tui = TuiApp::new(app, runtime.handle().clone(), Some("demo".to_owned()));
        let mut running = RunningState::new(
            "demo".to_owned(),
            "prompt_main.md".to_owned(),
            Some("prompt_main.md".to_owned()),
            ralph_core::RunControl::new(),
        );
        running.push_terminal_text(
            "softwrapalpha softwrapbeta softwrapgamma softwrapdelta softwrapepsilon",
        );
        tui.running = Some(running);
        tui.screen = Screen::Running;

        let backend = TestBackend::new(48, 18);
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| tui.draw(frame))?;

        let rows = buffer_rows(&terminal);
        assert!(rows.iter().any(|row| row.contains("softwrapalpha")));
        assert!(rows.iter().any(|row| row.contains("softwrapepsilon")));

        Ok(())
    }

    #[test]
    fn finished_runs_render_final_status_instead_of_live_spinner() -> Result<()> {
        let statuses = [
            (LastRunStatus::Completed, "DONE", "run completed"),
            (
                LastRunStatus::MaxIterations,
                "LIMIT",
                "max iterations reached",
            ),
            (LastRunStatus::Failed, "FAIL", "run failed"),
            (LastRunStatus::Canceled, "CANCELED", "run canceled"),
        ];

        for (status, badge, text) in statuses {
            let (_temp, project_dir) = temp_project_dir();
            let app = RalphApp::load(project_dir)?;
            app.create_target("demo", Some(ScaffoldId::SinglePrompt))?;

            let runtime = Runtime::new()?;
            let mut tui = TuiApp::new(app, runtime.handle().clone(), Some("demo".to_owned()));
            let mut running = RunningState::new(
                "demo".to_owned(),
                "prompt_main.md".to_owned(),
                Some("prompt_main.md".to_owned()),
                ralph_core::RunControl::new(),
            );
            running.finish(status);
            tui.running = Some(running);
            tui.screen = Screen::Running;

            let backend = TestBackend::new(80, 18);
            let mut terminal = Terminal::new(backend)?;
            terminal.draw(|frame| tui.draw(frame))?;

            let rows = buffer_rows(&terminal).join("\n");
            assert!(rows.contains(badge), "missing badge {badge} for {status:?}");
            assert!(rows.contains(text), "missing text {text} for {status:?}");
            assert!(
                !rows.contains("LIVE"),
                "live badge still shown for {status:?}"
            );
            assert!(
                !rows.contains("streaming agent output"),
                "live spinner text still shown for {status:?}"
            );
        }

        Ok(())
    }

    #[test]
    fn dashboard_footer_shows_dynamic_pause_actions_from_flow_state() -> Result<()> {
        let (_temp, project_dir) = temp_project_dir();
        let app = RalphApp::load(&project_dir)?;
        app.create_target("prompt", Some(ScaffoldId::SinglePrompt))?;
        app.create_target("tasks", Some(ScaffoldId::TaskDriven))?;
        app.create_target("workflow", Some(ScaffoldId::PlanDriven))?;
        for (target, node_id) in [
            ("tasks", "task_driven_paused"),
            ("workflow", "plan_driven_paused"),
        ] {
            let target_config = project_dir.join(format!(".ralph/targets/{target}/target.toml"));
            let mut raw = std::fs::read_to_string(&target_config)?;
            raw.push_str(&format!(
                "\n[runtime]\nactive_entrypoint = \"main\"\ncurrent_node = \"{node_id}\"\n"
            ));
            std::fs::write(target_config, raw)?;
        }

        let runtime = Runtime::new()?;
        let prompt_tui = TuiApp::new(
            app.clone(),
            runtime.handle().clone(),
            Some("prompt".to_owned()),
        );
        let prompt_footer = prompt_tui
            .footer_spans()
            .into_iter()
            .map(|span| span.content.to_string())
            .collect::<String>();
        assert!(!prompt_footer.contains("Interview goal"));
        assert!(!prompt_footer.contains("rebase"));
        assert!(!prompt_footer.contains("rebuild"));
        assert!(!prompt_footer.contains("build"));

        let task_tui = TuiApp::new(
            app.clone(),
            runtime.handle().clone(),
            Some("tasks".to_owned()),
        );
        let task_footer = task_tui
            .footer_spans()
            .into_iter()
            .map(|span| span.content.to_string())
            .collect::<String>();
        assert!(task_footer.contains("Build current backlog"));
        assert!(task_footer.contains("Interview goal"));
        assert!(task_footer.contains("Rebase backlog"));
        assert!(task_footer.contains("Rebuild from scratch"));

        let workflow_tui = TuiApp::new(app, runtime.handle().clone(), Some("workflow".to_owned()));
        let workflow_footer = workflow_tui
            .footer_spans()
            .into_iter()
            .map(|span| span.content.to_string())
            .collect::<String>();
        assert!(workflow_footer.contains("Build current plan"));
        assert!(workflow_footer.contains("Interview goal"));
        assert!(workflow_footer.contains("Rebase plan"));
        assert!(workflow_footer.contains("Rebuild from scratch"));

        Ok(())
    }

    #[test]
    fn dashboard_footer_shows_flow_actions_for_fresh_flow_targets() -> Result<()> {
        let (_temp, project_dir) = temp_project_dir();
        let app = RalphApp::load(&project_dir)?;
        app.create_target("tasks", Some(ScaffoldId::TaskDriven))?;

        let runtime = Runtime::new()?;
        let tui = TuiApp::new(app, runtime.handle().clone(), Some("tasks".to_owned()));
        let footer = tui
            .footer_spans()
            .into_iter()
            .map(|span| span.content.to_string())
            .collect::<String>();

        assert!(footer.contains("Build current backlog"));
        assert!(footer.contains("Interview goal"));
        assert!(footer.contains("Rebase backlog"));
        assert!(footer.contains("Rebuild from scratch"));

        Ok(())
    }
}
