use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{
    ActivityPhase, RenderCacheKey, ToolStatus, TuiState, TuiThemeMode, UiMessage, UiMessageKind,
};
use crate::provider::Role;

#[derive(Clone, Copy)]
struct Theme {
    background: Color,
    panel: Color,
    foreground: Color,
    muted: Color,
    border: Color,
    info: Color,
    success: Color,
    error: Color,
    warm: Color,
    code: Color,
    diff_add: Color,
    diff_remove: Color,
    muted_modifier: Modifier,
}

impl Theme {
    fn new(mode: TuiThemeMode) -> Self {
        match mode {
            TuiThemeMode::Terminal => Self {
                background: Color::Reset,
                panel: Color::Reset,
                foreground: Color::Reset,
                muted: Color::Reset,
                border: Color::Reset,
                info: Color::Blue,
                success: Color::Green,
                error: Color::Red,
                warm: Color::Yellow,
                code: Color::Reset,
                diff_add: Color::Green,
                diff_remove: Color::Red,
                muted_modifier: Modifier::DIM,
            },
            TuiThemeMode::Dark => Self {
                background: Color::Rgb(19, 22, 29),
                panel: Color::Rgb(27, 32, 42),
                foreground: Color::Rgb(220, 225, 234),
                muted: Color::Rgb(143, 155, 174),
                border: Color::Rgb(62, 73, 91),
                info: Color::Rgb(148, 181, 255),
                success: Color::Rgb(120, 205, 162),
                error: Color::Rgb(244, 143, 143),
                warm: Color::Rgb(230, 185, 119),
                code: Color::Rgb(166, 206, 189),
                diff_add: Color::Rgb(120, 205, 162),
                diff_remove: Color::Rgb(244, 143, 143),
                muted_modifier: Modifier::empty(),
            },
            TuiThemeMode::Light => Self {
                background: Color::Rgb(246, 247, 250),
                panel: Color::Rgb(255, 255, 255),
                foreground: Color::Rgb(35, 40, 50),
                muted: Color::Rgb(96, 106, 122),
                border: Color::Rgb(177, 186, 202),
                info: Color::Rgb(54, 91, 170),
                success: Color::Rgb(35, 125, 79),
                error: Color::Rgb(181, 55, 61),
                warm: Color::Rgb(139, 91, 24),
                code: Color::Rgb(40, 116, 91),
                diff_add: Color::Rgb(35, 125, 79),
                diff_remove: Color::Rgb(181, 55, 61),
                muted_modifier: Modifier::empty(),
            },
        }
    }

    fn style(self, color: Color) -> Style {
        Style::default().fg(color).bg(self.background)
    }

    fn muted_style(self) -> Style {
        self.style(self.muted).add_modifier(self.muted_modifier)
    }

    fn text(self, value: impl Into<String>, color: Color) -> Span<'static> {
        Span::styled(value.into(), self.style(color))
    }

    fn muted_text(self, value: impl Into<String>) -> Span<'static> {
        Span::styled(value.into(), self.muted_style())
    }
}

pub(super) fn draw_ui(frame: &mut Frame<'_>, state: &mut TuiState) {
    let theme = Theme::new(state.theme_mode);
    let area = frame.area();
    frame.render_widget(Block::default().style(theme.style(theme.foreground)), area);
    if area.width < 24 || area.height < 12 {
        frame.render_widget(
            Paragraph::new("请放大终端\n至少 24 列 × 12 行\nEsc 退出").style(theme.muted_style()),
            area,
        );
        return;
    }
    let width = area.width.saturating_sub(4).min(108);
    let content = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + 1,
        width,
        area.height.saturating_sub(2),
    );
    let input_lines = wrap_lines(
        vec![Line::from(theme.text(state.input.text(), theme.foreground))],
        width.saturating_sub(4),
    );
    let max_input_lines = content
        .height
        .saturating_div(2)
        .saturating_sub(2)
        .clamp(1, 8);
    let input_height = (input_lines.len() as u16).clamp(1, max_input_lines) + 2;
    let approval_lines = state
        .pending_approvals
        .front()
        .map(|approval| {
            wrap_lines(
                vec![Line::from(theme.text(&approval.prompt, theme.warm))],
                width.saturating_sub(4),
            )
        })
        .unwrap_or_default();
    let approval_height = if approval_lines.is_empty() {
        0
    } else {
        (approval_lines.len() as u16 + 4)
            .min(content.height.saturating_sub(8))
            .max(4)
    };
    let regions = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(approval_height),
        Constraint::Length(input_height),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(content);

    let workspace = state
        .workspace
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("workspace");
    let session_label = short_id(&state.session_id, 18);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    "✦ my-agent",
                    theme.style(theme.info).add_modifier(Modifier::BOLD),
                ),
                theme.text("  ·  ", theme.border),
                theme.text(workspace, theme.foreground),
                theme.muted_text(format!("  ·  session {session_label}")),
            ]),
            Line::from(theme.muted_text("对话 · 规划 · 工具执行   ·   Ctrl+/ 查看快捷键")),
        ]),
        regions[0],
    );

    let message_width = width.saturating_sub(2);
    let anchor_message_id = state.preserve_message_id.take();
    let (lines, message_anchor) = if state.messages.is_empty() {
        (
            vec![
                Line::default(),
                Line::from(theme.text("从一个想法开始。", theme.foreground)),
                Line::default(),
                Line::from(theme.muted_text("  读取 README，帮我了解这个项目")),
                Line::from(theme.muted_text("  先制定计划，再为项目补充测试")),
                Line::from(theme.muted_text("  调研配置读取位置，只返回结论")),
                Line::default(),
                Line::from(theme.muted_text("  /resume  恢复历史会话")),
            ],
            None,
        )
    } else {
        let messages = state.messages.clone();
        transcript_lines_with_anchor(state, &messages, message_width, theme, anchor_message_id)
    };
    let lines = if state.messages.is_empty() {
        wrap_lines(lines, message_width)
    } else {
        lines
    };
    let viewport_height = usize::from(regions[1].height);
    let max_scroll = lines.len().saturating_sub(viewport_height);
    let preserved_start = state.preserve_transcript_start.take();
    let start = if let Some(anchor) = message_anchor.or(preserved_start) {
        let start = anchor.min(max_scroll);
        state.scroll = max_scroll.saturating_sub(start);
        state.follow_bottom = state.scroll == 0;
        if state.follow_bottom {
            state.unread_messages = 0;
        }
        start
    } else if state.follow_bottom {
        state.scroll = 0;
        max_scroll
    } else {
        state.scroll = state.scroll.min(max_scroll);
        max_scroll.saturating_sub(state.scroll)
    };
    state.transcript_start = start;
    let visible: Vec<_> = lines
        .into_iter()
        .skip(start)
        .take(viewport_height)
        .collect();
    frame.render_widget(
        Paragraph::new(visible).style(theme.style(theme.foreground)),
        regions[1],
    );
    if !state.follow_bottom {
        let label = if state.unread_messages == 0 {
            "↥ 回到底部".to_owned()
        } else {
            format!("↥ {} 条新消息 · 回到底部", state.unread_messages)
        };
        let pill_width = (label.width() as u16 + 4).min(regions[1].width);
        let pill = Rect::new(
            regions[1].x + regions[1].width.saturating_sub(pill_width) / 2,
            regions[1].y + regions[1].height.saturating_sub(1),
            pill_width,
            1,
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("  {label}  "),
                theme.style(theme.info).add_modifier(Modifier::BOLD),
            )))
            .style(theme.style(theme.info)),
            pill,
        );
    }

    if !approval_lines.is_empty() {
        let mut lines = approval_lines;
        let visible = usize::from(regions[2].height.saturating_sub(4));
        state.approval_scroll = state
            .approval_scroll
            .min(lines.len().saturating_sub(visible));
        lines = lines
            .into_iter()
            .skip(state.approval_scroll)
            .take(visible)
            .collect();
        lines.push(Line::default());
        lines.push(Line::from(vec![
            theme.text("Y 允许一次", theme.warm),
            theme.muted_text("    N / Enter 拒绝"),
        ]));
        frame.render_widget(
            Paragraph::new(lines)
                .style(theme.style(theme.foreground))
                .block(
                    Block::default()
                        .title(format!(
                            " 确认 {}/{} · PgUp/Dn 翻页 ",
                            1,
                            state.pending_approvals.len()
                        ))
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(theme.style(theme.warm)),
                ),
            regions[2],
        );
    }

    let input_area = regions[3];
    let border_color = if !state.pending_approvals.is_empty() {
        theme.border
    } else {
        theme.info
    };
    let title = if !state.active_turns.is_empty() {
        format!(
            " 正在工作 {} 轮 · 队列 {} · 可以先起草下一条 ",
            state.active_turns.len(),
            state.queued_turns.len()
        )
    } else {
        " 发送消息 ".to_owned()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color).bg(theme.panel))
        .style(Style::default().fg(theme.foreground).bg(theme.panel))
        .title(title);
    frame.render_widget(block, input_area);
    let inner = Rect::new(
        input_area.x + 2,
        input_area.y + 1,
        input_area.width.saturating_sub(4),
        input_area.height.saturating_sub(2),
    );
    let skip = input_lines.len().saturating_sub(usize::from(inner.height));
    if state.input.is_empty() {
        frame.render_widget(
            Paragraph::new("描述你想做什么…").style(theme.muted_style().bg(theme.panel)),
            inner,
        );
    } else {
        let visible: Vec<_> = input_lines
            .iter()
            .skip(skip)
            .cloned()
            .map(|mut line| {
                for span in &mut line.spans {
                    span.style = span.style.bg(theme.panel);
                }
                line
            })
            .collect();
        frame.render_widget(
            Paragraph::new(visible).style(Style::default().fg(theme.foreground).bg(theme.panel)),
            inner,
        );
    }
    if state.pending_approvals.is_empty() && inner.height > 0 {
        let (cursor_row, cursor_column) = state.input.visual_cursor(inner.width);
        let cursor_row = cursor_row.saturating_sub(skip);
        frame.set_cursor_position((
            inner.x + (cursor_column as u16).min(inner.width.saturating_sub(1)),
            inner.y + (cursor_row as u16).min(inner.height - 1),
        ));
    }
    let status = status_line(state, theme, width);
    let footer = if width >= 90 {
        "Enter 发送 · Alt↵ 换行 · PgUp/Dn 翻页 · Ctrl+↑↓ 滚动 · Ctrl+End 回底 · Ctrl+/ 帮助 · Esc 退出"
            .to_owned()
    } else if width >= 55 {
        "Enter 发送 · Ctrl+C 取消 · Ctrl+End 回底 · Ctrl+/ 帮助 · Esc 退出".to_owned()
    } else {
        "Enter 发送 · Ctrl+/ 帮助 · Esc 退出".to_owned()
    };
    frame.render_widget(
        Paragraph::new(status).style(theme.muted_style()),
        regions[4],
    );
    frame.render_widget(
        Paragraph::new(footer).style(theme.muted_style()),
        regions[5],
    );

    if state.show_help {
        render_help(frame, state, theme, content);
    }
}

fn status_line(state: &TuiState, theme: Theme, width: u16) -> Line<'static> {
    let state_text = if state.scroll > 0 {
        format!("↑ 历史 · 距底部 {} 行", state.scroll)
    } else {
        state.status.clone()
    };
    let activity = if state.pending_approvals.is_empty() {
        format!(
            "{} 活动 · {} 排队",
            state.active_turns.len(),
            state.queued_turns.len()
        )
    } else {
        format!("{} 个审批待处理", state.pending_approvals.len())
    };
    let (indicator, indicator_color) = match state.activity_phase {
        ActivityPhase::WaitingApproval => ("◆ ".to_owned(), theme.warm),
        phase if phase.is_animated() => (
            indeterminate_indicator(state.animation_tick, width >= 55),
            theme.info,
        ),
        _ if state.status.contains("失败") || state.status.contains("中断") => {
            ("✗ ".to_owned(), theme.error)
        }
        _ => ("● ".to_owned(), theme.success),
    };
    let reserved = indicator.width() + activity.width() + 3;
    let compact = truncate_text(&state_text, usize::from(width).saturating_sub(reserved));
    Line::from(vec![
        Span::styled(indicator, theme.style(indicator_color)),
        Span::styled(compact, theme.style(theme.foreground)),
        theme.muted_text(format!("   {activity}")),
    ])
}

fn indeterminate_indicator(tick: u64, show_bar: bool) -> String {
    const SPINNERS: [&str; 4] = ["◐", "◓", "◑", "◒"];
    let spinner = SPINNERS[(tick as usize) % SPINNERS.len()];
    if !show_bar {
        return format!("{spinner} ");
    }
    const WIDTH: usize = 8;
    let cycle = WIDTH * 2 - 2;
    let step = (tick as usize) % cycle;
    let head = if step < WIDTH { step } else { cycle - step };
    let bar = (0..WIDTH)
        .map(|index| {
            if index == head || index + 1 == head {
                '━'
            } else {
                '·'
            }
        })
        .collect::<String>();
    format!("{spinner} [{bar}] ")
}

fn render_help(frame: &mut Frame<'_>, state: &TuiState, theme: Theme, content: Rect) {
    let width = content.width.min(72);
    let height = content.height.min(18);
    let area = Rect::new(
        content.x + content.width.saturating_sub(width) / 2,
        content.y + content.height.saturating_sub(height) / 2,
        width,
        height,
    );
    let lines = vec![
        Line::from(Span::styled(
            "快捷键",
            theme.style(theme.info).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("Enter        发送消息"),
        Line::from("Alt+Enter    插入换行"),
        Line::from("↑ / ↓        浏览输入历史"),
        Line::from("Ctrl+↑ / ↓   滚动对话"),
        Line::from("Ctrl+Home    跳到对话顶部"),
        Line::from("Ctrl+End     回到底部"),
        Line::from("Ctrl+T       展开/收起工具调用与输出（默认折叠）"),
        Line::from("Ctrl+K       清空排队消息"),
        Line::from("Ctrl+C       取消当前请求"),
        Line::from("Ctrl+/       打开/关闭帮助"),
        Line::from("Esc          关闭帮助 / 退出"),
        Line::from(""),
        Line::from(Span::styled(
            format!("当前 session: {}", short_id(&state.session_id, 32)),
            theme.muted_style(),
        )),
    ];
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .style(theme.style(theme.foreground))
            .block(
                Block::default()
                    .title(" 帮助 ")
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(theme.style(theme.info))
                    .style(theme.style(theme.foreground)),
            ),
        area,
    );
}

fn short_id(value: &str, max_width: usize) -> String {
    truncate_text(value, max_width)
}

fn truncate_text(value: &str, max_width: usize) -> String {
    if value.width() <= max_width {
        return value.to_owned();
    }
    let limit = max_width.saturating_sub(1);
    let mut used = 0;
    let mut result = String::new();
    for character in value.chars() {
        let width = character.width().unwrap_or(0);
        if used + width > limit {
            break;
        }
        result.push(character);
        used += width;
    }
    result.push('…');
    result
}

fn cached_message_lines(
    state: &mut TuiState,
    message: &UiMessage,
    width: u16,
    theme: Theme,
) -> Vec<Line<'static>> {
    let key = RenderCacheKey {
        message_id: message.id,
        content_version: message.content_version,
        width,
        show_tools: state.show_tools,
        theme_mode: state.theme_mode,
    };
    if let Some(lines) = state.render_cache.get(&key) {
        return lines.clone();
    }
    let lines = wrap_lines(message_lines(message, state.show_tools, theme), width);
    if state.render_cache.len() >= 512 {
        state.render_cache.clear();
    }
    state.render_cache.insert(key, lines.clone());
    lines
}

#[cfg(test)]
fn transcript_lines(
    state: &mut TuiState,
    messages: &[UiMessage],
    width: u16,
    theme: Theme,
) -> Vec<Line<'static>> {
    transcript_lines_with_anchor(state, messages, width, theme, None).0
}

fn transcript_lines_with_anchor(
    state: &mut TuiState,
    messages: &[UiMessage],
    width: u16,
    theme: Theme,
    anchor_message_id: Option<u64>,
) -> (Vec<Line<'static>>, Option<usize>) {
    let mut lines = Vec::new();
    let mut anchor_start = None;
    let mut index = 0;
    while index < messages.len() {
        if !state.show_tools && matches!(messages[index].kind, UiMessageKind::Tool(_)) {
            let start = index;
            while index < messages.len() && matches!(messages[index].kind, UiMessageKind::Tool(_)) {
                index += 1;
            }
            lines.extend(collapsed_tool_group(&messages[start..index], theme));
        } else {
            if anchor_message_id == Some(messages[index].id) {
                anchor_start = Some(lines.len());
            }
            lines.extend(cached_message_lines(state, &messages[index], width, theme));
            index += 1;
        }
    }
    (lines, anchor_start)
}

fn collapsed_tool_group(messages: &[UiMessage], theme: Theme) -> Vec<Line<'static>> {
    let tools = messages
        .iter()
        .filter_map(|message| match &message.kind {
            UiMessageKind::Tool(tool) => Some(tool),
            UiMessageKind::Text(_) => None,
        })
        .collect::<Vec<_>>();
    if tools.is_empty() {
        return Vec::new();
    }
    let failed = tools
        .iter()
        .filter(|tool| tool.status == ToolStatus::Failed)
        .count();
    let running = tools
        .iter()
        .filter(|tool| tool.status == ToolStatus::Running)
        .count();
    let total_ms = tools
        .iter()
        .filter_map(|tool| tool.duration_ms)
        .sum::<u64>();
    let names = tools
        .iter()
        .take(4)
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>()
        .join("、");
    let names = if tools.len() > 4 {
        format!("{names}…")
    } else {
        names
    };
    let status = if running > 0 {
        ("○", "执行中", theme.info)
    } else if failed > 0 {
        ("✗", "有失败", theme.error)
    } else {
        ("●", "已完成", theme.success)
    };
    let rounds = tools
        .iter()
        .map(|tool| tool.round)
        .min()
        .zip(tools.iter().map(|tool| tool.round).max());
    let round_label = rounds.map_or_else(String::new, |(first, last)| {
        if first == last {
            format!(" · 第 {first} 轮")
        } else {
            format!(" · 第 {first}-{last} 轮")
        }
    });
    let duration_label = if total_ms == 0 {
        String::new()
    } else {
        format!(" · {:.1}s", total_ms as f32 / 1000.0)
    };
    vec![Line::from(theme.text(
        format!(
            "  {} 工具调用已折叠 · {} 次 · {}{}{} · {} · Ctrl+T 展开详情",
            status.0,
            tools.len(),
            status.1,
            round_label,
            duration_label,
            names
        ),
        status.2,
    ))]
}

fn message_lines(message: &UiMessage, show_tools: bool, theme: Theme) -> Vec<Line<'static>> {
    let UiMessageKind::Text(content) = &message.kind else {
        let UiMessageKind::Tool(tool) = &message.kind else {
            return Vec::new();
        };
        let icon = match tool.status {
            ToolStatus::Running => "○",
            ToolStatus::Ok => "●",
            ToolStatus::Failed => "✗",
        };
        let elapsed = tool.duration_ms.map_or_else(
            || {
                tool.finished_at
                    .map(|finished| {
                        format!(
                            " · {:.1}s",
                            finished.duration_since(tool.started_at).as_secs_f32()
                        )
                    })
                    .unwrap_or_default()
            },
            |duration_ms| format!(" · {:.1}s", duration_ms as f32 / 1000.0),
        );
        let expanded = tool.expanded.unwrap_or(show_tools);
        let status_color = match tool.status {
            ToolStatus::Running => theme.info,
            ToolStatus::Ok => theme.success,
            ToolStatus::Failed => theme.error,
        };
        let mut lines = vec![Line::from(theme.text(
            format!(
                "  {icon} 工具  第 {} 轮 · {}{elapsed}",
                tool.round, tool.heading
            ),
            status_color,
        ))];
        if expanded {
            lines.extend(
                tool.output
                    .iter()
                    .map(|line| Line::from(theme.muted_text(format!("    │ {line}")))),
            );
        }
        return lines;
    };
    let (label, color) = match message.role {
        Role::User => ("›  你", theme.warm),
        Role::Assistant => ("✦  Agent", theme.info),
        _ => ("·  提示", theme.muted),
    };
    let age = message.created_at.elapsed().as_secs();
    let usage = message
        .token_usage
        .as_ref()
        .map_or_else(String::new, |usage| {
            format!(" · {}↑/{}↓", usage.input, usage.output)
        });
    let label = format!("{label} · #{} · {age}s{usage}", message.id);
    let mut lines = vec![Line::from(Span::styled(
        label,
        theme.style(color).add_modifier(Modifier::BOLD),
    ))];
    let mut code = false;
    for source in content.lines() {
        let trimmed = source.trim_start();
        if trimmed.starts_with("```") {
            code = !code;
            if code {
                lines.push(Line::from(
                    theme.muted_text(format!("  ┌─ {}", trimmed.trim_start_matches('`'))),
                ));
            } else {
                lines.push(Line::from(theme.muted_text("  └─")));
            }
        } else if code {
            lines.push(Line::from(vec![
                theme.muted_text("  │ "),
                theme.text(
                    source,
                    if source.trim_start().starts_with('+') {
                        theme.diff_add
                    } else if source.trim_start().starts_with('-') {
                        theme.diff_remove
                    } else {
                        theme.code
                    },
                ),
            ]));
        } else if message.role == Role::User {
            lines.push(Line::from(
                theme.text(format!("  {source}"), theme.foreground),
            ));
        } else {
            let heading =
                trimmed.starts_with('#') && trimmed.trim_start_matches('#').starts_with(' ');
            let body = if heading {
                trimmed.trim_start_matches('#').trim_start()
            } else {
                source
            };
            let body = body
                .strip_prefix("- ")
                .map(|rest| format!("• {rest}"))
                .unwrap_or_else(|| body.to_owned());
            let base = if heading {
                theme.style(theme.foreground).add_modifier(Modifier::BOLD)
            } else {
                theme.style(theme.foreground)
            };
            let prefix = if message.role == Role::Assistant {
                "  │ "
            } else {
                "  "
            };
            let mut spans = vec![theme.text(prefix, theme.foreground)];
            spans.extend(inline(&body, base, theme));
            lines.push(Line::from(spans));
        }
    }
    lines.push(Line::from(theme.muted_text("  ·")));
    lines
}

// 小范围 Markdown 展示：不修改原始会话，未闭合的流式标记按原文显示。
fn inline(value: &str, base: Style, theme: Theme) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = value;
    while !rest.is_empty() {
        let marker = ["**", "`"]
            .iter()
            .filter_map(|m| rest.find(m).map(|at| (at, *m)))
            .min_by_key(|(at, _)| *at);
        let Some((at, marker)) = marker else {
            spans.push(Span::styled(rest.to_owned(), base));
            break;
        };
        let after = &rest[at + marker.len()..];
        let Some(end) = after.find(marker) else {
            spans.push(Span::styled(rest.to_owned(), base));
            break;
        };
        spans.push(Span::styled(rest[..at].to_owned(), base));
        let emphasis = if marker == "**" {
            base.add_modifier(Modifier::BOLD)
        } else {
            base.fg(theme.info)
                .bg(theme.panel)
                .add_modifier(Modifier::BOLD)
        };
        spans.push(Span::styled(after[..end].to_owned(), emphasis));
        rest = &after[end + marker.len()..];
    }
    spans
}

// 按终端显示列宽换行，滚动计数和屏幕绘制使用同一份行列表。
fn wrap_lines(lines: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let mut output = Vec::new();
    for line in lines {
        let mut row = Vec::new();
        let mut used = 0;
        for span in line.spans {
            for ch in span.content.chars() {
                if ch == '\n' {
                    output.push(Line::from(std::mem::take(&mut row)));
                    used = 0;
                    continue;
                }
                if ch.is_control() {
                    continue;
                }
                let cells = ch.width().unwrap_or(0);
                if used + cells > width && used > 0 {
                    output.push(Line::from(std::mem::take(&mut row)));
                    used = 0;
                }
                row.push(Span::styled(ch.to_string(), span.style));
                used += cells;
            }
        }
        output.push(Line::from(row));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::recovery::RecoverySnapshot;
    use ratatui::{Terminal, backend::TestBackend};

    fn fixture() -> TuiState {
        let mut state = TuiState::from_snapshot(RecoverySnapshot {
            session_id: "preview".into(),
            messages: vec![],
            pending_approvals: vec![],
            active_requests: vec![],
        });
        state.workspace = "/Users/pilot/Documents/myproject/agent-rust".into();
        state.push_text(Role::User, "帮我了解这个项目，并给出下一步建议。".into());
        let turn_id = crate::daemon::protocol::RequestId::String("preview-turn".into());
        state.start_tool(
            turn_id.clone(),
            Some("preview-read".into()),
            "read_file".into(),
            1,
        );
        state.finish_tool(
            &turn_id,
            Some("preview-read"),
            "read_file",
            "原始工具输出默认收起",
            true,
            12,
        );
        state.push_text(Role::Assistant, "## 一个专注个人开发的编码助手\n\n项目使用 **Rust**，由工作区 daemon 管理会话和工具执行。\n\n### 现在可以做什么\n- 读取与修改代码，运行测试\n- 用 `plan` 拆解任务，用子 Agent 调研\n- 在 CLI、ACP 和 WebSocket 之间恢复会话\n\n### 从这里开始\n```bash\nmyagent chat \"读取 README 并总结\"\n```\n\n建议先为配置模块补充测试，再逐步改进交互体验。".into());
        state
    }

    #[test]
    fn wraps_chinese_by_display_width_and_scrolls_to_actual_last_line() {
        let lines = wrap_lines(vec![Line::from("中文内容abcdef")], 6);
        assert!(lines.iter().all(|line| line.width() <= 6));
        let mut state = fixture();
        let UiMessageKind::Text(content) = &mut state.messages[2].kind else {
            panic!("第三条预览消息应为文本");
        };
        *content = "长文本".repeat(200) + "\n最后一行";
        state.messages[2].content_version += 1;
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        terminal.draw(|frame| draw_ui(frame, &mut state)).unwrap();
        let rendered = buffer_text(terminal.backend().buffer());
        assert!(rendered.replace(' ', "").contains("最后一行"));
        state.scroll_by(8);
        terminal.draw(|frame| draw_ui(frame, &mut state)).unwrap();
        assert!(
            !buffer_text(terminal.backend().buffer())
                .replace(' ', "")
                .contains("最后一行")
        );
    }

    #[test]
    fn caches_wrapped_messages_by_content_version_and_width() {
        let mut state = fixture();
        state.show_tools = true;
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|frame| draw_ui(frame, &mut state)).unwrap();
        let cached = state.render_cache.len();
        assert!(cached >= 3);
        terminal.draw(|frame| draw_ui(frame, &mut state)).unwrap();
        assert_eq!(state.render_cache.len(), cached);

        let UiMessageKind::Text(content) = &mut state.messages[2].kind else {
            panic!("预览消息应为文本");
        };
        content.push_str("\n新内容");
        state.messages[2].content_version += 1;
        terminal.draw(|frame| draw_ui(frame, &mut state)).unwrap();
        assert!(state.render_cache.len() > cached);
    }

    #[test]
    fn collapses_tool_runs_by_default_and_expands_details_on_toggle() {
        let mut state = fixture();
        let messages = state.messages.clone();
        let collapsed = transcript_lines(
            &mut state,
            &messages,
            100,
            Theme::new(TuiThemeMode::Terminal),
        );
        let collapsed_text = collapsed
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(collapsed_text.contains("工具调用已折叠"));
        assert!(!collapsed_text.contains("原始工具输出默认收起"));

        state.show_tools = true;
        let messages = state.messages.clone();
        let expanded = transcript_lines(
            &mut state,
            &messages,
            100,
            Theme::new(TuiThemeMode::Terminal),
        );
        let expanded_text = expanded
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(expanded_text.contains("原始工具输出默认收起"));
    }

    #[test]
    fn expanding_tool_details_keeps_the_original_query_in_view() {
        let mut state = TuiState::from_snapshot(RecoverySnapshot {
            session_id: "anchor-preview".into(),
            messages: vec![],
            pending_approvals: vec![],
            active_requests: vec![],
        });
        state.workspace = "workspace".into();
        state.push_text(Role::User, "上一轮 Query".into());
        state.push_text(
            Role::Assistant,
            "上一轮回答第一行\n上一轮回答第二行\n上一轮回答第三行".into(),
        );
        state.push_text(Role::User, "这是必须保留可见的原始 Query".into());
        let turn_id = crate::daemon::protocol::RequestId::String("anchor-turn".into());
        state.start_tool(turn_id.clone(), Some("long-tool".into()), "exec".into(), 1);
        state.finish_tool(
            &turn_id,
            Some("long-tool"),
            "exec",
            &(0..80)
                .map(|index| format!("工具详情第 {index} 行"))
                .collect::<Vec<_>>()
                .join("\n"),
            true,
            100,
        );

        let mut terminal = Terminal::new(TestBackend::new(70, 20)).unwrap();
        terminal.draw(|frame| draw_ui(frame, &mut state)).unwrap();
        assert!(
            buffer_text(terminal.backend().buffer())
                .replace(' ', "")
                .contains("原始Query")
        );

        state.toggle_tool_details();
        terminal.draw(|frame| draw_ui(frame, &mut state)).unwrap();
        let expanded = buffer_text(terminal.backend().buffer()).replace(' ', "");
        assert!(expanded.contains("原始Query"));
        assert!(expanded.contains("工具详情第0行"));
        assert!(!state.follow_bottom);
        assert!(state.transcript_start > 0);
    }

    #[test]
    fn short_transcript_starts_near_the_top_of_the_screen() {
        let mut state = TuiState::from_snapshot(RecoverySnapshot {
            session_id: "top-preview".into(),
            messages: vec![],
            pending_approvals: vec![],
            active_requests: vec![],
        });
        state.workspace = "workspace".into();
        state.push_text(Role::User, "短 Query".into());
        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        terminal.draw(|frame| draw_ui(frame, &mut state)).unwrap();
        let rows = buffer_text(terminal.backend().buffer())
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let header_row = rows
            .iter()
            .position(|row| row.contains("my-agent"))
            .expect("标题应可见");
        let query_row = rows
            .iter()
            .position(|row| row.contains('›'))
            .expect("Query 应可见");
        assert!(header_row <= 2);
        assert!(query_row <= 6);
    }

    #[test]
    fn active_status_uses_an_animated_indeterminate_progress_bar() {
        let mut state = fixture();
        state.activity_phase = ActivityPhase::WaitingModel;
        state.status = "等待模型响应".into();
        state.animation_tick = 0;
        let first = status_line(&state, Theme::new(TuiThemeMode::Terminal), 80)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        state.advance_animation();
        let second = status_line(&state, Theme::new(TuiThemeMode::Terminal), 80)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(first.contains('[') && first.contains(']'));
        assert_ne!(first, second);

        state.activity_phase = ActivityPhase::WaitingApproval;
        let approval = status_line(&state, Theme::new(TuiThemeMode::Terminal), 80)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(approval.starts_with("◆ "));
        assert!(!approval.contains('['));
    }

    fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
        buffer
            .content
            .chunks(usize::from(buffer.area.width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn render_wide_narrow_and_approval_preview() {
        for (width, height) in [(110, 42), (44, 30), (24, 12), (16, 8)] {
            let mut state = fixture();
            state.theme_mode = TuiThemeMode::Dark;
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| draw_ui(frame, &mut state)).unwrap();
            for cell in &terminal.backend().buffer().content {
                if !cell.symbol().trim().is_empty() {
                    assert_ne!(cell.fg, Color::Reset);
                    assert_ne!(cell.bg, Color::Reset);
                }
            }
            if width == 110 {
                let content = buffer_text(terminal.backend().buffer());
                assert!(!content.contains("**Rust**"));
                assert!(!content.contains("原始工具输出"));
                if let Ok(path) = std::env::var("TUI_PREVIEW_PATH") {
                    let buffer = terminal.backend().buffer();
                    let cells: Vec<_> = buffer.content.iter().map(|cell| serde_json::json!({"text": cell.symbol(), "fg": rgb(cell.fg), "bg": rgb(cell.bg), "bold": cell.modifier.contains(Modifier::BOLD)})).collect();
                    std::fs::write(
                        path,
                        serde_json::to_vec(
                            &serde_json::json!({"width":width,"height":height,"cells":cells}),
                        )
                        .unwrap(),
                    )
                    .unwrap();
                }
            }
            state
                .pending_approvals
                .push_back(crate::daemon::approval::PendingApprovalInfo {
                    id: "a".into(),
                    request_id: crate::daemon::protocol::RequestId::Number(1),
                    prompt: "允许写入工作区外的文件 /tmp/demo.txt 吗？".into(),
                });
            terminal.draw(|frame| draw_ui(frame, &mut state)).unwrap();
        }
    }

    #[test]
    fn terminal_theme_never_paints_a_fixed_background() {
        let mut state = fixture();
        state.theme_mode = TuiThemeMode::Terminal;
        let mut terminal = Terminal::new(TestBackend::new(110, 42)).unwrap();
        terminal.draw(|frame| draw_ui(frame, &mut state)).unwrap();
        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .all(|cell| cell.bg == Color::Reset)
        );
    }

    #[test]
    fn light_theme_has_distinct_semantic_status_colors() {
        let theme = Theme::new(TuiThemeMode::Light);
        assert_ne!(theme.background, Color::Reset);
        assert_ne!(theme.info, theme.success);
        assert_ne!(theme.success, theme.error);
        assert_ne!(theme.diff_add, theme.diff_remove);
    }

    #[test]
    fn dark_and_light_themes_paint_non_terminal_semantic_surfaces() {
        for mode in [TuiThemeMode::Dark, TuiThemeMode::Light] {
            let mut state = fixture();
            state.theme_mode = mode;
            let mut terminal = Terminal::new(TestBackend::new(110, 42)).unwrap();
            terminal.draw(|frame| draw_ui(frame, &mut state)).unwrap();
            let theme = Theme::new(mode);
            let cells = &terminal.backend().buffer().content;
            assert!(cells.iter().any(|cell| cell.bg == theme.background));
            assert!(cells.iter().any(|cell| cell.fg == theme.info));
            assert!(cells.iter().any(|cell| cell.fg == theme.success));
        }
    }

    #[test]
    fn renders_help_overlay_with_shortcuts() {
        let mut state = fixture();
        state.show_help = true;
        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        terminal.draw(|frame| draw_ui(frame, &mut state)).unwrap();
        let rendered = buffer_text(terminal.backend().buffer());
        let compact = rendered.replace(' ', "");
        assert!(compact.contains("快捷键"));
        assert!(compact.contains("Ctrl+C"));
        assert!(compact.contains("当前session"));
    }

    fn rgb(color: Color) -> String {
        match color {
            Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
            _ => "#13161d".into(),
        }
    }
}
