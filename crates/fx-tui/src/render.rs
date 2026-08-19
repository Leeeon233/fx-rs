use agent_client_protocol::schema::v1::{PermissionOptionKind, ToolCallStatus};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Margin, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, Entry, EntryKind, Focus, Overlay, Phase, PickerKind};
use crate::theme::Theme;

pub fn draw(frame: &mut Frame<'_>, app: &mut App, theme: Theme) {
    let area = frame.area();
    frame.render_widget(Block::new().style(Style::new().bg(theme.background)), area);
    if area.width < 42 || area.height < 12 {
        draw_too_small(frame, area, theme);
        return;
    }

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(app.composer_height()),
            Constraint::Length(1),
        ])
        .split(area);

    draw_header(frame, layout[0], app, theme);
    draw_transcript(frame, layout[1], app, theme);
    draw_composer(frame, layout[2], app, theme);
    draw_status(frame, layout[3], app, theme);

    if app
        .permission
        .as_ref()
        .is_some_and(|permission| !permission.parked)
    {
        draw_permission(frame, area, app, theme);
    }
    if let Some(overlay) = &app.overlay {
        draw_overlay(frame, area, overlay, theme);
    }
}

fn draw_header(frame: &mut Frame<'_>, area: Rect, app: &App, theme: Theme) {
    let cwd_label = app
        .cwd
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("workspace");
    let title = app.session_title.as_deref().unwrap_or(cwd_label);
    let session = if app.session_id.is_empty() {
        "connecting".to_owned()
    } else {
        compact_id(&app.session_id)
    };
    let left = Line::from(vec![
        Span::styled(
            " fxrs ",
            Style::new().fg(theme.background).bg(theme.accent).bold(),
        ),
        Span::raw("  "),
        Span::styled(title, Style::new().fg(theme.text).bold()),
        Span::styled(format!("  {session}"), Style::new().fg(theme.muted)),
    ]);
    let mut right = Vec::new();
    if let Some(model) = app.current_model_label() {
        right.push(Span::styled(model, Style::new().fg(theme.secondary)));
    }
    if let Some(mode) = app.current_mode_label() {
        if !right.is_empty() {
            right.push(Span::styled("  ·  ", Style::new().fg(theme.muted)));
        }
        right.push(Span::styled(mode, Style::new().fg(theme.accent)));
    }

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(20), Constraint::Length(36)])
        .split(area);
    frame.render_widget(
        Paragraph::new(left).block(
            Block::new()
                .borders(Borders::BOTTOM)
                .border_style(Style::new().fg(theme.surface_high)),
        ),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(right))
            .alignment(Alignment::Right)
            .block(
                Block::new()
                    .borders(Borders::BOTTOM)
                    .border_style(Style::new().fg(theme.surface_high)),
            ),
        chunks[1],
    );
}

fn draw_transcript(frame: &mut Frame<'_>, area: Rect, app: &mut App, theme: Theme) {
    let inner = area.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });
    if app.entries.is_empty() {
        let welcome = Text::from(vec![
            Line::from(Span::styled("ƒx", Style::new().fg(theme.accent).bold())),
            Line::from(""),
            Line::from(Span::styled(
                "An ACP-native coding workspace",
                Style::new().fg(theme.text).bold(),
            )),
            Line::from(Span::styled(
                "Describe a task below · /help shows commands",
                Style::new().fg(theme.muted),
            )),
        ]);
        frame.render_widget(
            Paragraph::new(welcome)
                .alignment(Alignment::Center)
                .block(Block::new().padding(ratatui::widgets::Padding::top(
                    inner.height.saturating_sub(4) / 2,
                ))),
            inner,
        );
        return;
    }

    let width = inner.width.max(1);
    let heights = app
        .entries
        .iter_mut()
        .map(|entry| entry_height(entry, width))
        .collect::<Vec<_>>();
    let total = heights.iter().copied().sum::<u32>();
    let viewport = u32::from(inner.height);
    let max_scroll = total.saturating_sub(viewport);
    app.scroll_from_bottom = app.scroll_from_bottom.min(max_scroll);
    let view_end = total.saturating_sub(app.scroll_from_bottom);
    let view_start = view_end.saturating_sub(viewport);
    let mut cursor = 0_u32;

    for (index, (entry, height)) in app.entries.iter().zip(heights).enumerate() {
        let entry_start = cursor;
        let entry_end = cursor.saturating_add(height);
        cursor = entry_end;
        if entry_end <= view_start || entry_start >= view_end {
            continue;
        }
        let visible_start = entry_start.max(view_start);
        let visible_end = entry_end.min(view_end);
        let y = inner
            .y
            .saturating_add(u16::try_from(visible_start - view_start).unwrap_or(u16::MAX));
        let height = u16::try_from(visible_end - visible_start).unwrap_or(u16::MAX);
        let clip = u16::try_from(visible_start - entry_start).unwrap_or(u16::MAX);
        let selected = app.focus == Focus::Transcript && app.selected_entry == Some(index);
        let paragraph = Paragraph::new(entry_text(entry, theme, selected))
            .wrap(Wrap { trim: false })
            .scroll((clip, 0));
        frame.render_widget(paragraph, Rect::new(inner.x, y, inner.width, height));
    }

    if app.scroll_from_bottom > 0 {
        frame.render_widget(
            Paragraph::new(format!(" ↑ {} ", app.scroll_from_bottom))
                .style(Style::new().fg(theme.background).bg(theme.secondary).bold())
                .alignment(Alignment::Right),
            Rect::new(area.right().saturating_sub(9), area.y, 8, 1),
        );
    }
}

fn draw_composer(frame: &mut Frame<'_>, area: Rect, app: &mut App, theme: Theme) {
    let focused = app.focus == Focus::Composer && app.overlay.is_none();
    let border = if focused {
        theme.accent
    } else {
        theme.surface_high
    };
    let title = if matches!(app.phase, Phase::Running | Phase::Cancelling) {
        format!(" Message · {} queued ", app.queued.len())
    } else {
        " Message ".into()
    };
    app.composer.set_block(
        Block::new()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::new().fg(border))
            .style(Style::new().bg(theme.surface)),
    );
    app.composer
        .set_style(Style::new().fg(theme.text).bg(theme.surface));
    app.composer
        .set_cursor_style(Style::new().fg(theme.background).bg(if focused {
            theme.accent
        } else {
            theme.muted
        }));
    app.composer.set_cursor_line_style(Style::new());
    app.composer
        .set_placeholder_text("Ask fxrs to inspect, explain, or change your code…");
    app.composer
        .set_placeholder_style(Style::new().fg(theme.muted));
    frame.render_widget(&app.composer, area);
}

fn draw_status(frame: &mut Frame<'_>, area: Rect, app: &App, theme: Theme) {
    let spinner = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let active = matches!(
        app.phase,
        Phase::Running | Phase::Cancelling | Phase::Working
    );
    let status = if active {
        format!(
            "{} {}",
            spinner[app.spinner_frame % spinner.len()],
            app.status
        )
    } else {
        format!("● {}", app.status)
    };
    let hint = if app
        .permission
        .as_ref()
        .is_some_and(|permission| permission.parked)
    {
        "Tab permission"
    } else if app.focus == Focus::Transcript {
        "j/k select  ←/→ fold  PgUp/PgDn scroll  Tab compose"
    } else {
        "Enter send  ⇧Enter newline  Esc cancel  ^M model  ⇧Tab mode  ^P help"
    };
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(area);
    frame.render_widget(
        Paragraph::new(status).style(Style::new().fg(if active {
            theme.warning
        } else {
            theme.success
        })),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(hint)
            .style(Style::new().fg(theme.muted))
            .alignment(Alignment::Right),
        chunks[1],
    );
}

fn draw_permission(frame: &mut Frame<'_>, area: Rect, app: &mut App, theme: Theme) {
    let width = area.width.saturating_sub(8).clamp(36, 88);
    let Some(permission) = &mut app.permission else {
        return;
    };
    let height = u16::try_from(permission.options.len())
        .unwrap_or(u16::MAX)
        .saturating_mul(2)
        .saturating_add(7)
        .min(area.height.saturating_sub(4));
    let modal = centered(area, width, height);
    frame.render_widget(Clear, modal);
    frame.render_widget(
        Block::new()
            .title(" Permission required ")
            .title_style(Style::new().fg(theme.warning).bold())
            .borders(Borders::ALL)
            .border_style(Style::new().fg(theme.warning))
            .style(Style::new().bg(theme.surface)),
        modal,
    );
    let inner = modal.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });
    let title_height = 3;
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                &permission.title,
                Style::new().fg(theme.text).bold(),
            )),
            Line::from(Span::styled(
                "Choose an authorization scope. Esc parks this request.",
                Style::new().fg(theme.muted),
            )),
        ])
        .wrap(Wrap { trim: true }),
        Rect::new(inner.x, inner.y, inner.width, title_height),
    );
    permission.hits.clear();
    for (index, option) in permission.options.iter().enumerate() {
        let y = inner
            .y
            .saturating_add(title_height)
            .saturating_add(u16::try_from(index).unwrap_or(u16::MAX).saturating_mul(2));
        let option_area = Rect::new(inner.x, y, inner.width, 1);
        permission.hits.push(option_area);
        let selected = index == permission.selected;
        let color = match option.kind {
            PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways => theme.success,
            PermissionOptionKind::RejectOnce | PermissionOptionKind::RejectAlways => theme.danger,
            _ => theme.text,
        };
        let style = if selected {
            Style::new().fg(theme.background).bg(color).bold()
        } else {
            Style::new().fg(color).bg(theme.surface_high)
        };
        frame.render_widget(
            Paragraph::new(format!(" {}  {} ", index + 1, option.name)).style(style),
            option_area,
        );
    }
}

fn draw_overlay(frame: &mut Frame<'_>, area: Rect, overlay: &Overlay, theme: Theme) {
    match overlay {
        Overlay::Help => {
            let modal = centered(
                area,
                area.width.saturating_sub(8).min(78),
                20.min(area.height.saturating_sub(2)),
            );
            frame.render_widget(Clear, modal);
            let text = Text::from(vec![
                Line::from(Span::styled(
                    "Navigation",
                    Style::new().fg(theme.accent).bold(),
                )),
                Line::from("Tab focus · j/k select · PgUp/PgDn scroll · ←/→ fold"),
                Line::from(""),
                Line::from(Span::styled("Prompt", Style::new().fg(theme.accent).bold())),
                Line::from("Enter send/queue · Shift+Enter newline · Esc cancel"),
                Line::from("Ctrl+M model · Shift+Tab next mode · Ctrl+P help"),
                Line::from(""),
                Line::from(Span::styled(
                    "Commands",
                    Style::new().fg(theme.accent).bold(),
                )),
                Line::from("/login · /model [id] · /mode [id] · /resume <id>"),
                Line::from("/new · /clear · /help · /quit"),
                Line::from(""),
                Line::from(Span::styled(
                    "Esc or Enter closes this panel",
                    Style::new().fg(theme.muted),
                )),
            ]);
            frame.render_widget(
                Paragraph::new(text)
                    .wrap(Wrap { trim: false })
                    .block(modal_block(" Keyboard & commands ", theme)),
                modal,
            );
        }
        Overlay::Picker {
            kind,
            title,
            options,
            selected,
        } => {
            let desired = u16::try_from(options.len())
                .unwrap_or(u16::MAX)
                .saturating_mul(2)
                .saturating_add(4);
            let modal = centered(
                area,
                area.width.saturating_sub(8).min(76),
                desired.min(area.height.saturating_sub(2)).max(7),
            );
            frame.render_widget(Clear, modal);
            frame.render_widget(modal_block(&format!(" {title} "), theme), modal);
            let inner = modal.inner(Margin {
                horizontal: 2,
                vertical: 1,
            });
            let visible = usize::from(inner.height / 2).max(1);
            let start = selected.saturating_sub(visible.saturating_sub(1));
            for (offset, option) in options.iter().skip(start).take(visible).enumerate() {
                let index = start + offset;
                let y = inner
                    .y
                    .saturating_add(u16::try_from(offset).unwrap_or(u16::MAX).saturating_mul(2));
                let style = if index == *selected {
                    Style::new().fg(theme.background).bg(theme.accent).bold()
                } else {
                    Style::new().fg(theme.text)
                };
                frame.render_widget(
                    Paragraph::new(format!(
                        " {}  {}",
                        if index == *selected { "›" } else { " " },
                        option.name
                    ))
                    .style(style),
                    Rect::new(inner.x, y, inner.width, 1),
                );
                if let Some(description) = &option.description {
                    frame.render_widget(
                        Paragraph::new(format!("    {description}"))
                            .style(Style::new().fg(theme.muted)),
                        Rect::new(inner.x, y.saturating_add(1), inner.width, 1),
                    );
                }
            }
            let label = match kind {
                PickerKind::Authenticate => "provider",
                PickerKind::Config(_) => "configuration",
                PickerKind::Mode => "mode",
            };
            frame.render_widget(
                Paragraph::new(format!("Select {label} · Enter apply · Esc close"))
                    .style(Style::new().fg(theme.muted))
                    .alignment(Alignment::Right),
                Rect::new(
                    modal.x + 2,
                    modal.bottom().saturating_sub(2),
                    modal.width.saturating_sub(4),
                    1,
                ),
            );
        }
    }
}

fn entry_height(entry: &mut Entry, width: u16) -> u32 {
    if entry.cached_width == width && entry.cached_revision == entry.revision() {
        return entry.cached_height;
    }
    let content_width = usize::from(width.saturating_sub(4).max(1));
    let body_height = if entry.collapsed {
        u32::from(!entry.body.is_empty())
    } else {
        entry
            .body
            .lines()
            .map(|line| {
                let cells = UnicodeWidthStr::width(line);
                u32::try_from(cells.max(1).div_ceil(content_width)).unwrap_or(u32::MAX)
            })
            .sum()
    };
    let height = 1_u32.saturating_add(body_height).saturating_add(1);
    entry.cached_width = width;
    entry.cached_revision = entry.revision();
    entry.cached_height = height;
    height
}

fn entry_text(entry: &Entry, theme: Theme, selected: bool) -> Text<'static> {
    let (glyph, color) = match entry.kind {
        EntryKind::User => ("YOU", theme.accent),
        EntryKind::Assistant => ("FX", theme.secondary),
        EntryKind::Thought => ("THINK", theme.muted),
        EntryKind::Tool => (
            tool_glyph(entry.tool_status),
            tool_color(entry.tool_status, theme),
        ),
        EntryKind::Plan => ("PLAN", theme.secondary),
        EntryKind::Notice => ("NOTE", theme.warning),
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(if selected { "▌ " } else { "│ " }, Style::new().fg(color)),
        Span::styled(glyph, Style::new().fg(color).bold()),
        Span::styled(
            format!("  {}", entry.title),
            Style::new().fg(theme.text).bold(),
        ),
        Span::styled(
            if entry.collapsed { "  [collapsed]" } else { "" },
            Style::new().fg(theme.muted),
        ),
    ])];
    if entry.collapsed {
        if let Some(preview) = entry.body.lines().find(|line| !line.trim().is_empty()) {
            lines.push(Line::from(vec![
                Span::styled("│   ", Style::new().fg(color)),
                Span::styled(ellipsize(preview, 96), Style::new().fg(theme.muted)),
            ]));
        }
    } else {
        let body = if entry.kind == EntryKind::Assistant {
            tui_markdown::from_str(&entry.body)
        } else {
            Text::from(entry.body.clone())
        };
        for line in body.lines {
            let mut spans = vec![Span::styled("│   ", Style::new().fg(color))];
            spans.extend(
                line.spans
                    .into_iter()
                    .map(|span| Span::styled(span.content.into_owned(), span.style)),
            );
            lines.push(Line::from(spans));
        }
    }
    Text::from(lines).style(Style::new().fg(if entry.kind == EntryKind::Thought {
        theme.muted
    } else {
        theme.text
    }))
}

fn tool_glyph(status: Option<ToolCallStatus>) -> &'static str {
    match status {
        Some(ToolCallStatus::Pending) => "TOOL ○",
        Some(ToolCallStatus::InProgress) => "TOOL ◆",
        Some(ToolCallStatus::Completed) => "TOOL ✓",
        Some(ToolCallStatus::Failed) => "TOOL ×",
        _ => "TOOL",
    }
}

fn tool_color(status: Option<ToolCallStatus>, theme: Theme) -> ratatui::style::Color {
    match status {
        Some(ToolCallStatus::Completed) => theme.success,
        Some(ToolCallStatus::Failed) => theme.danger,
        Some(ToolCallStatus::Pending | ToolCallStatus::InProgress) => theme.warning,
        _ => theme.muted,
    }
}

fn modal_block<'a>(title: &'a str, theme: Theme) -> Block<'a> {
    Block::new()
        .title(title)
        .title_style(Style::new().fg(theme.accent).bold())
        .borders(Borders::ALL)
        .border_style(Style::new().fg(theme.accent))
        .style(Style::new().fg(theme.text).bg(theme.surface))
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width.min(area.width),
        height.min(area.height),
    )
}

fn compact_id(id: &str) -> String {
    if id.chars().count() <= 12 {
        return id.to_owned();
    }
    format!("{}…", id.chars().take(11).collect::<String>())
}

fn ellipsize(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        value.to_owned()
    } else {
        format!(
            "{}…",
            value
                .chars()
                .take(max_chars.saturating_sub(1))
                .collect::<String>()
        )
    }
}

fn draw_too_small(frame: &mut Frame<'_>, area: Rect, theme: Theme) {
    frame.render_widget(
        Paragraph::new("fxrs needs a terminal at least 42×12\nResize the window to continue")
            .style(Style::new().fg(theme.warning).add_modifier(Modifier::BOLD))
            .alignment(Alignment::Center)
            .block(Block::new().padding(ratatui::widgets::Padding::top(area.height / 3))),
        area,
    );
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use agent_client_protocol::schema::v1::{
        ContentBlock, ContentChunk, PermissionOption, PermissionOptionKind,
        RequestPermissionRequest, SessionUpdate, TextContent, ToolCall, ToolCallUpdate,
        ToolCallUpdateFields,
    };
    use ratatui::{Terminal, backend::TestBackend};
    use tokio::sync::oneshot;

    use super::*;
    use crate::app::PendingPermission;

    fn rendered(app: &mut App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, app, Theme::dark()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn wide_layout_contains_welcome_and_shortcuts() {
        let mut app = App::new(PathBuf::from("/tmp/project"));
        app.phase = Phase::Idle;
        app.status = "Ready".into();
        let screen = rendered(&mut app, 100, 28);
        assert!(screen.contains("An ACP-native coding workspace"));
        assert!(screen.contains("Enter send"));
        assert!(screen.contains("Message"));
    }

    #[test]
    fn narrow_terminal_has_a_stable_fallback() {
        let mut app = App::new(PathBuf::from("/tmp/project"));
        let screen = rendered(&mut app, 30, 8);
        assert!(screen.lines().count() <= 8);
        assert!(screen.contains("fxrs needs a terminal"));
    }

    #[test]
    fn conversation_cards_and_permission_modal_are_presented() {
        let mut app = App::new(PathBuf::from("/tmp/project"));
        app.phase = Phase::Idle;
        app.mark_prompt_started("Inspect the workspace");
        app.on_session_update(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new("## Result\nThe workspace is ready.")),
        )));
        app.on_session_update(SessionUpdate::ToolCall(
            ToolCall::new("tool-1", "Read Cargo.toml").status(ToolCallStatus::Completed),
        ));
        let screen = rendered(&mut app, 100, 32);
        assert!(screen.contains("YOU"));
        assert!(screen.contains("FX"));
        assert!(screen.contains("TOOL ✓"));

        let request = RequestPermissionRequest::new(
            "session",
            ToolCallUpdate::new(
                "tool-2",
                ToolCallUpdateFields::new().title("Run cargo test"),
            ),
            vec![PermissionOption::new(
                "allow_once",
                "Allow once",
                PermissionOptionKind::AllowOnce,
            )],
        );
        let (sender, _receiver) = oneshot::channel();
        app.set_permission(PendingPermission::from_request(request, sender));
        let screen = rendered(&mut app, 100, 32);
        assert!(screen.contains("Permission required"));
        assert!(screen.contains("Run cargo test"));
        assert!(screen.contains("Allow once"));
    }

    #[test]
    fn cached_scrollback_render_stays_interactive() {
        let mut app = App::new(PathBuf::from("/tmp/project"));
        app.phase = Phase::Idle;
        for index in 0..2_000 {
            app.add_notice(
                format!("Event {index}"),
                "A bounded transcript entry with enough content to exercise wrapping.",
            );
        }
        let _ = rendered(&mut app, 120, 36);
        let started = Instant::now();
        for _ in 0..20 {
            let _ = rendered(&mut app, 120, 36);
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "cached rendering regressed: {:?}",
            started.elapsed()
        );
    }
}
