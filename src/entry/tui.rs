use std::io::{self, Stdout};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use serde_json::json;

use crate::client::{DaemonClient, RpcStream};
use crate::daemon::approval::PendingApprovalInfo;
use crate::daemon::protocol::{EventKind, RequestId, ServerFrame};
use crate::entry::recovery;
use crate::provider::{Message, Role};

type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;

#[derive(Clone)]
struct UiMessage {
    role: Role,
    content: String,
}

struct TuiState {
    messages: Vec<UiMessage>,
    input: String,
    active: Option<RpcStream>,
    active_request_id: Option<RequestId>,
    pending_approval: Option<PendingApprovalInfo>,
    status: String,
    scroll: usize,
}

impl TuiState {
    fn from_snapshot(snapshot: recovery::RecoverySnapshot) -> Self {
        let active_request_id = snapshot.active_requests.first().cloned();
        let pending_approval = snapshot.pending_approvals.first().cloned();
        let has_active = active_request_id.is_some();
        let has_pending = pending_approval.is_some();
        Self {
            messages: snapshot
                .messages
                .into_iter()
                .filter_map(message_to_ui)
                .collect(),
            input: String::new(),
            active: None,
            active_request_id,
            pending_approval,
            status: if has_active {
                "正在恢复活动请求".to_owned()
            } else if has_pending {
                "等待审批".to_owned()
            } else {
                "就绪".to_owned()
            },
            scroll: usize::MAX,
        }
    }

    fn push_user(&mut self, content: String) {
        self.messages.push(UiMessage {
            role: Role::User,
            content,
        });
        self.scroll = usize::MAX;
    }

    fn append_assistant(&mut self, delta: &str) {
        if let Some(last) = self.messages.last_mut()
            && last.role == Role::Assistant
        {
            last.content.push_str(delta);
        } else {
            self.messages.push(UiMessage {
                role: Role::Assistant,
                content: delta.to_owned(),
            });
        }
        self.scroll = usize::MAX;
    }
}

pub async fn run_tui(client: DaemonClient) -> Result<()> {
    let snapshot = recovery::load_snapshot(&client).await?;
    let mut state = TuiState::from_snapshot(snapshot);
    if let Some(request_id) = state.active_request_id.clone() {
        match recovery::subscribe(&client, &request_id).await {
            Ok(stream) => state.active = Some(stream),
            Err(error) => state.status = format!("恢复订阅失败：{error:#}"),
        }
    }

    let mut terminal = setup_terminal()?;
    let result = run_event_loop(&client, &mut terminal, &mut state).await;
    restore_terminal(&mut terminal)?;
    result
}

fn setup_terminal() -> Result<TuiTerminal> {
    terminal::enable_raw_mode().context("启用终端 raw mode 失败")?;
    let mut stdout = io::stdout();
    if let Err(error) = execute!(stdout, EnterAlternateScreen) {
        let _ = terminal::disable_raw_mode();
        return Err(error).context("进入终端 alternate screen 失败");
    }
    Terminal::new(CrosstermBackend::new(stdout)).context("创建 TUI 终端失败")
}

fn restore_terminal(terminal: &mut TuiTerminal) -> Result<()> {
    terminal::disable_raw_mode().context("恢复终端 raw mode 失败")?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)
        .context("退出终端 alternate screen 失败")?;
    terminal.show_cursor().context("恢复终端光标失败")
}

async fn run_event_loop(
    client: &DaemonClient,
    terminal: &mut TuiTerminal,
    state: &mut TuiState,
) -> Result<()> {
    loop {
        terminal
            .draw(|frame| draw_ui(frame, state))
            .context("绘制 TUI 失败")?;

        if event::poll(Duration::from_millis(40)).context("读取终端事件失败")?
            && let Event::Key(key) = event::read().context("读取键盘事件失败")?
        {
            handle_key(client, state, key).await?;
            if state.status == "退出" {
                break;
            }
        }
        if let Some(active) = state.active.as_mut() {
            let frame = tokio::time::timeout(Duration::from_millis(1), active.next())
                .await
                .ok()
                .flatten();
            if let Some(frame) = frame {
                handle_frame(state, frame).await?;
            }
        }
    }
    Ok(())
}

async fn handle_key(client: &DaemonClient, state: &mut TuiState, key: KeyEvent) -> Result<()> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        if let Some(request_id) = state.active_request_id.clone() {
            let _ = crate::entry::cli::request_result(
                client,
                "agent.cancel",
                json!({"request_id": request_id}),
            )
            .await?;
            state.status = "已发送取消请求".to_owned();
        }
        return Ok(());
    }

    if let Some(approval) = state.pending_approval.clone() {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                recovery::respond_to_approval(client, &approval.id, true).await?;
                state.pending_approval = None;
                state.status = "审批已允许，继续执行".to_owned();
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Enter => {
                recovery::respond_to_approval(client, &approval.id, false).await?;
                state.pending_approval = None;
                state.status = "审批已拒绝，继续执行".to_owned();
            }
            _ => {}
        }
        return Ok(());
    }

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') if state.input.is_empty() => {
            state.status = "退出".to_owned()
        }
        KeyCode::PageUp => state.scroll = state.scroll.saturating_sub(8),
        KeyCode::PageDown => state.scroll = state.scroll.saturating_add(8),
        KeyCode::Backspace => {
            state.input.pop();
        }
        KeyCode::Char(character) => state.input.push(character),
        KeyCode::Enter if !state.input.trim().is_empty() && state.active.is_none() => {
            let message = std::mem::take(&mut state.input);
            state.push_user(message.clone());
            let stream = client
                .request("chat.send", json!({"message": message}))
                .await?;
            state.active_request_id = Some(stream.request_id().clone());
            state.active = Some(stream);
            state.status = "Agent 正在工作".to_owned();
        }
        _ => {}
    }
    Ok(())
}

async fn handle_frame(state: &mut TuiState, frame: ServerFrame) -> Result<()> {
    match frame {
        ServerFrame::Event(event) => match event.event {
            EventKind::TextDelta => {
                if let Some(delta) = event.data["delta"].as_str() {
                    state.append_assistant(delta);
                }
            }
            EventKind::ToolStarted => {
                state.status = format!(
                    "工具执行中：{}",
                    event.data["name"].as_str().unwrap_or("unknown")
                );
            }
            EventKind::ToolFinished => {
                state.status = format!(
                    "工具完成：{}",
                    event.data["name"].as_str().unwrap_or("unknown")
                );
            }
            EventKind::ApprovalRequired => {
                state.pending_approval = serde_json::from_value(event.data["approval"].clone())
                    .context("审批事件格式无效")?;
                state.status = "等待审批：Y 允许 / N 拒绝".to_owned();
            }
            EventKind::TurnStarted | EventKind::TurnCompleted => {}
        },
        ServerFrame::Response(response) => {
            state.active = None;
            state.active_request_id = None;
            if let Some(error) = response.error {
                state.status = format!("请求失败（{}）：{}", error.code, error.message);
            } else {
                state.status = "就绪".to_owned();
            }
        }
    }
    Ok(())
}

fn draw_ui(frame: &mut ratatui::Frame<'_>, state: &TuiState) {
    let area = frame.area();
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(4),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);

    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            " my-agent ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("· daemon TUI"),
    ]));
    frame.render_widget(header, layout[0]);

    let lines = state
        .messages
        .iter()
        .flat_map(message_lines)
        .collect::<Vec<Line<'static>>>();
    let visible_height = usize::from(layout[1].height.saturating_sub(2));
    let max_scroll = lines.len().saturating_sub(visible_height);
    let offset = if state.scroll == usize::MAX {
        max_scroll
    } else {
        state.scroll.min(max_scroll)
    };
    let messages = Paragraph::new(Text::from(lines))
        .block(Block::default().borders(Borders::ALL).title("对话"))
        .wrap(Wrap { trim: false })
        .scroll((u16::try_from(offset).unwrap_or(u16::MAX), 0));
    frame.render_widget(messages, layout[1]);

    let input_title = if let Some(approval) = &state.pending_approval {
        format!("审批：{} [Y/N]", approval.prompt)
    } else {
        "输入（Enter 发送，Ctrl-C 取消，Esc 退出）".to_owned()
    };
    let input = Paragraph::new(state.input.as_str())
        .block(Block::default().borders(Borders::ALL).title(input_title));
    frame.render_widget(input, layout[2]);

    let footer = List::new(vec![ListItem::new(Line::from(vec![
        Span::styled("状态：", Style::default().fg(Color::Yellow)),
        Span::raw(state.status.as_str()),
    ]))]);
    frame.render_widget(footer, layout[3]);
}

fn message_lines(message: &UiMessage) -> Vec<Line<'static>> {
    let (label, color) = match message.role {
        Role::User => ("你", Color::Green),
        Role::Assistant => ("Agent", Color::Cyan),
        Role::Tool => ("工具", Color::Yellow),
        Role::System => ("系统", Color::Magenta),
    };
    let mut lines = Vec::new();
    for (index, content) in message.content.lines().enumerate() {
        let prefix = if index == 0 {
            format!("[{label}] ")
        } else {
            "       ".to_owned()
        };
        lines.push(Line::from(vec![
            Span::styled(
                prefix,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::raw(content.to_owned()),
        ]));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("[{label}]"),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )));
    }
    lines
}

fn message_to_ui(message: Message) -> Option<UiMessage> {
    let content = message.content?.to_owned();
    Some(UiMessage {
        role: message.role,
        content,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_role_prefix_and_multiline_content() {
        let message = UiMessage {
            role: Role::Assistant,
            content: "第一行\n第二行".to_owned(),
        };
        let lines = message_lines(&message);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].spans[0].content, "[Agent] ");
        assert_eq!(lines[1].spans[0].content, "       ");
    }

    #[test]
    fn converts_persisted_message_to_ui_message() {
        let message = Message::text(Role::User, "检查项目");
        let converted = message_to_ui(message).expect("文本消息应可显示");
        assert_eq!(converted.role, Role::User);
        assert_eq!(converted.content, "检查项目");
    }
}
