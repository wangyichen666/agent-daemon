use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
};
use unicode_width::UnicodeWidthChar;

use super::{TuiState, TuiThemeMode, UiMessage};
use crate::provider::Role;

#[derive(Clone, Copy)]
struct Theme {
    background: Color,
    panel: Color,
    foreground: Color,
    muted: Color,
    border: Color,
    accent: Color,
    warm: Color,
    code: Color,
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
                accent: Color::Reset,
                warm: Color::Reset,
                code: Color::Reset,
                muted_modifier: Modifier::DIM,
            },
            TuiThemeMode::Dark => Self {
                background: Color::Rgb(19, 22, 29),
                panel: Color::Rgb(27, 32, 42),
                foreground: Color::Rgb(220, 225, 234),
                muted: Color::Rgb(143, 155, 174),
                border: Color::Rgb(62, 73, 91),
                accent: Color::Rgb(148, 181, 255),
                warm: Color::Rgb(230, 185, 119),
                code: Color::Rgb(166, 206, 189),
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
        vec![Line::from(
            theme.text(format!("{} ", state.input), theme.foreground),
        )],
        width.saturating_sub(4),
    );
    let input_height = (input_lines.len() as u16).clamp(1, 4) + 2;
    let approval_lines = state
        .pending_approval
        .as_ref()
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
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(approval_height),
        Constraint::Length(input_height),
        Constraint::Length(1),
    ])
    .split(content);

    let workspace = state
        .workspace
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("workspace");
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    "✦ my-agent",
                    theme.style(theme.accent).add_modifier(Modifier::BOLD),
                ),
                theme.text("   /   ", theme.border),
                theme.text(workspace, theme.foreground),
            ]),
            Line::from(theme.muted_text("你的终端编码助手 · 对话、规划、执行")),
        ]),
        regions[0],
    );

    let lines = if state.messages.is_empty() {
        vec![
            Line::default(),
            Line::from(theme.text("从一个想法开始。", theme.foreground)),
            Line::default(),
            Line::from(theme.muted_text("  读取 README，帮我了解这个项目")),
            Line::from(theme.muted_text("  先制定计划，再为项目补充测试")),
            Line::from(theme.muted_text("  调研配置读取位置，只返回结论")),
            Line::default(),
            Line::from(theme.muted_text("  /resume  恢复历史会话")),
        ]
    } else {
        state
            .messages
            .iter()
            .flat_map(|message| message_lines(message, state.show_tools, theme))
            .collect()
    };
    let lines = wrap_lines(lines, width.saturating_sub(2));
    let max_scroll = lines.len().saturating_sub(usize::from(regions[1].height));
    state.scroll = state.scroll.min(max_scroll);
    let start = max_scroll.saturating_sub(state.scroll);
    let visible: Vec<_> = lines
        .into_iter()
        .skip(start)
        .take(usize::from(regions[1].height))
        .collect();
    frame.render_widget(
        Paragraph::new(visible).style(theme.style(theme.foreground)),
        regions[1],
    );

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
                        .title(" 确认 · PgUp/Dn 翻页 ")
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(theme.style(theme.warm)),
                ),
            regions[2],
        );
    }

    let input_area = regions[3];
    let border_color = if state.pending_approval.is_some() {
        theme.border
    } else {
        theme.accent
    };
    let title = if state.active.is_some() {
        " 正在工作 · 可以先起草下一条 "
    } else {
        " 发送消息 "
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
    if state.pending_approval.is_none() && inner.height > 0 {
        let column = input_lines
            .last()
            .map_or(0, |line| line.width().saturating_sub(1));
        frame.set_cursor_position((
            inner.x + (column as u16).min(inner.width.saturating_sub(1)),
            inner.y + (input_lines.len().saturating_sub(skip + 1) as u16).min(inner.height - 1),
        ));
    }
    let status = if state.scroll > 0 {
        format!("↑ 历史 · 距底部 {} 行", state.scroll)
    } else {
        format!("● {}", state.status)
    };
    let footer = if width >= 90 {
        format!("{status}    Enter 发送 · Alt↵ 换行 · PgUp/Dn 滚动 · Ctrl+T 工具 · Esc 退出")
    } else if width >= 55 {
        format!("{status}   Enter 发送 · Ctrl+C 取消 · Esc 退出")
    } else {
        "Enter 发送 · Esc 退出".to_owned()
    };
    frame.render_widget(
        Paragraph::new(footer).style(theme.muted_style()),
        regions[4],
    );
}

fn message_lines(message: &UiMessage, show_tools: bool, theme: Theme) -> Vec<Line<'static>> {
    if message.role == Role::Tool {
        let heading = message.content.lines().next().unwrap_or("执行结果");
        let mut lines = vec![Line::from(theme.muted_text(format!(
            "  ✓ 工具  {}",
            heading.chars().take(70).collect::<String>()
        )))];
        if show_tools {
            lines.extend(
                message
                    .content
                    .lines()
                    .skip(1)
                    .map(|line| Line::from(theme.muted_text(format!("    {line}")))),
            );
        }
        return lines;
    }
    let (label, color) = match message.role {
        Role::User => ("›  你", theme.warm),
        Role::Assistant => ("✦  Agent", theme.accent),
        _ => ("·  提示", theme.muted),
    };
    let mut lines = vec![
        Line::default(),
        Line::from(Span::styled(
            label,
            theme.style(color).add_modifier(Modifier::BOLD),
        )),
        Line::default(),
    ];
    let mut code = false;
    for source in message.content.lines() {
        let trimmed = source.trim_start();
        if trimmed.starts_with("```") {
            code = !code;
            if code {
                lines.push(Line::from(
                    theme.muted_text(format!("  ┌ {}", trimmed.trim_start_matches('`'))),
                ));
            } else {
                lines.push(Line::from(theme.muted_text("  └")));
            }
        } else if code {
            lines.push(Line::from(vec![
                theme.muted_text("  │ "),
                theme.text(source, theme.code),
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
            let mut spans = vec![theme.text("  ", theme.foreground)];
            spans.extend(inline(&body, base, theme));
            lines.push(Line::from(spans));
        }
    }
    lines.push(Line::default());
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
            base.fg(theme.accent)
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
        state.messages = vec![
            UiMessage { role: Role::User, content: "帮我了解这个项目，并给出下一步建议。".into() },
            UiMessage { role: Role::Tool, content: "read_file · README.md\n原始工具输出默认收起".into() },
            UiMessage { role: Role::Assistant, content: "## 一个专注个人开发的编码助手\n\n项目使用 **Rust**，由工作区 daemon 管理会话和工具执行。\n\n### 现在可以做什么\n- 读取与修改代码，运行测试\n- 用 `plan` 拆解任务，用子 Agent 调研\n- 在 CLI、ACP 和 WebSocket 之间恢复会话\n\n### 从这里开始\n```bash\nmyagent chat \"读取 README 并总结\"\n```\n\n建议先为配置模块补充测试，再逐步改进交互体验。".into() },
        ];
        state
    }

    #[test]
    fn wraps_chinese_by_display_width_and_scrolls_to_actual_last_line() {
        let lines = wrap_lines(vec![Line::from("中文内容abcdef")], 6);
        assert!(lines.iter().all(|line| line.width() <= 6));
        let mut state = fixture();
        state.messages[2].content = "长文本".repeat(200) + "\n最后一行";
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        terminal.draw(|frame| draw_ui(frame, &mut state)).unwrap();
        let rendered = buffer_text(terminal.backend().buffer());
        assert!(rendered.replace(' ', "").contains("最后一行"));
        state.scroll = 8;
        terminal.draw(|frame| draw_ui(frame, &mut state)).unwrap();
        assert!(
            !buffer_text(terminal.backend().buffer())
                .replace(' ', "")
                .contains("最后一行")
        );
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
            state.pending_approval = Some(crate::daemon::approval::PendingApprovalInfo {
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

    fn rgb(color: Color) -> String {
        match color {
            Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
            _ => "#13161d".into(),
        }
    }
}
