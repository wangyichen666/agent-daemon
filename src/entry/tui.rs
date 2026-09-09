use std::collections::{HashMap, VecDeque};
use std::io::{self, Stdout};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::text::Line;
use serde_json::json;

use crate::client::{DaemonClient, RpcStream};
use crate::daemon::approval::PendingApprovalInfo;
use crate::daemon::protocol::{EventKind, RequestId, ServerFrame};
use crate::entry::recovery;
use crate::provider::{Message, Role};
use crate::session::SessionInfo;
use crate::slash::SlashResponse;

type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;

mod input_editor;
mod view;
use input_editor::InputEditor;
use view::draw_ui;

#[derive(Clone)]
struct UiMessage {
    id: u64,
    role: Role,
    kind: UiMessageKind,
    created_at: Instant,
    token_usage: Option<TokenUsage>,
    content_version: u64,
    turn_id: Option<RequestId>,
}

#[derive(Clone)]
enum UiMessageKind {
    Text(String),
    Tool(UiToolCall),
}

#[derive(Clone)]
struct TokenUsage {
    input: u32,
    output: u32,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ToolStatus {
    Running,
    Ok,
    Failed,
}

#[derive(Clone)]
struct UiToolCall {
    turn_id: RequestId,
    call_id: Option<String>,
    name: String,
    heading: String,
    status: ToolStatus,
    output: Vec<String>,
    expanded: Option<bool>,
    started_at: Instant,
    finished_at: Option<Instant>,
}

struct ActiveTurn {
    request_id: RequestId,
    stream: RpcStream,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum TuiThemeMode {
    Terminal,
    Dark,
    Light,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct RenderCacheKey {
    message_id: u64,
    content_version: u64,
    width: u16,
    show_tools: bool,
    theme_mode: TuiThemeMode,
}

impl TuiThemeMode {
    fn from_env() -> Self {
        match std::env::var("MY_AGENT_TUI_THEME") {
            Ok(value)
                if matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "dark" | "truecolor"
                ) =>
            {
                Self::Dark
            }
            Ok(value) if value.trim().eq_ignore_ascii_case("light") => Self::Light,
            _ => Self::Terminal,
        }
    }
}

struct TuiState {
    messages: Vec<UiMessage>,
    next_message_id: u64,
    input: InputEditor,
    input_history: VecDeque<String>,
    history_cursor: Option<usize>,
    active_turns: Vec<ActiveTurn>,
    recovery_active_requests: Vec<RequestId>,
    queued_turns: VecDeque<String>,
    pending_approvals: VecDeque<PendingApprovalInfo>,
    approval_scroll: usize,
    status: String,
    should_quit: bool,
    scroll: usize,
    follow_bottom: bool,
    show_tools: bool,
    workspace: String,
    theme_mode: TuiThemeMode,
    resume_choices: Vec<SessionInfo>,
    render_cache: HashMap<RenderCacheKey, Vec<Line<'static>>>,
}

impl TuiState {
    fn from_snapshot(snapshot: recovery::RecoverySnapshot) -> Self {
        let has_active = !snapshot.active_requests.is_empty();
        let has_pending = !snapshot.pending_approvals.is_empty();
        let next_message_id = snapshot.messages.len() as u64 + 1;
        Self {
            messages: snapshot
                .messages
                .into_iter()
                .enumerate()
                .filter_map(|(index, message)| message_to_ui(message, index as u64 + 1))
                .collect(),
            next_message_id,
            input: InputEditor::default(),
            input_history: VecDeque::new(),
            history_cursor: None,
            active_turns: Vec::new(),
            recovery_active_requests: snapshot.active_requests,
            queued_turns: VecDeque::new(),
            pending_approvals: snapshot.pending_approvals.into(),
            approval_scroll: 0,
            status: if has_active {
                "正在恢复活动请求".to_owned()
            } else if has_pending {
                "等待审批".to_owned()
            } else {
                "就绪".to_owned()
            },
            should_quit: false,
            scroll: 0,
            follow_bottom: true,
            show_tools: false,
            workspace: std::env::current_dir()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            theme_mode: TuiThemeMode::from_env(),
            resume_choices: Vec::new(),
            render_cache: HashMap::new(),
        }
    }

    fn replace_snapshot(&mut self, snapshot: recovery::RecoverySnapshot) {
        self.messages = snapshot
            .messages
            .into_iter()
            .enumerate()
            .filter_map(|(index, message)| message_to_ui(message, index as u64 + 1))
            .collect();
        self.next_message_id = self.messages.len() as u64 + 1;
        self.active_turns.clear();
        self.recovery_active_requests = snapshot.active_requests;
        self.pending_approvals = snapshot.pending_approvals.into();
        self.approval_scroll = 0;
        self.scroll = 0;
        self.follow_bottom = true;
        self.resume_choices.clear();
        self.render_cache.clear();
    }

    fn push_user(&mut self, content: String) {
        self.push_text(Role::User, content);
        self.scroll = 0;
    }

    fn record_history(&mut self, input: &str) {
        if input.trim().is_empty() || self.input_history.back().is_some_and(|last| last == input) {
            return;
        }
        self.input_history.push_back(input.to_owned());
        if self.input_history.len() > 100 {
            self.input_history.pop_front();
        }
        self.history_cursor = None;
    }

    fn history_previous(&mut self) {
        if self.input_history.is_empty() {
            return;
        }
        let index = self
            .history_cursor
            .unwrap_or(self.input_history.len())
            .saturating_sub(1);
        if let Some(item) = self.input_history.get(index) {
            self.input.replace(item);
            self.history_cursor = Some(index);
        }
    }

    fn history_next(&mut self) {
        let Some(index) = self.history_cursor else {
            return;
        };
        let next = index + 1;
        if let Some(item) = self.input_history.get(next) {
            self.input.replace(item);
            self.history_cursor = Some(next);
        } else {
            self.input.clear();
            self.history_cursor = None;
        }
    }

    fn append_assistant(&mut self, turn_id: &RequestId, delta: &str) {
        if let Some(UiMessage {
            role: Role::Assistant,
            kind: UiMessageKind::Text(content),
            content_version,
            ..
        }) = self.messages.iter_mut().rev().find(|message| {
            message.role == Role::Assistant && message.turn_id.as_ref() == Some(turn_id)
        }) {
            content.push_str(delta);
            *content_version = content_version.saturating_add(1);
        } else {
            self.push_text_for_turn(Role::Assistant, delta.to_owned(), Some(turn_id.clone()));
        }
    }

    fn push_text(&mut self, role: Role, content: String) {
        self.push_text_for_turn(role, content, None);
    }

    fn push_text_for_turn(&mut self, role: Role, content: String, turn_id: Option<RequestId>) {
        self.messages.push(UiMessage {
            id: self.next_message_id,
            role,
            kind: UiMessageKind::Text(content),
            created_at: Instant::now(),
            token_usage: None,
            content_version: 0,
            turn_id,
        });
        self.next_message_id = self.next_message_id.saturating_add(1);
    }

    fn start_tool(&mut self, turn_id: RequestId, call_id: Option<String>, name: String) {
        self.messages.push(UiMessage {
            id: self.next_message_id,
            role: Role::Tool,
            kind: UiMessageKind::Tool(UiToolCall {
                turn_id,
                heading: tool_heading(&name),
                name,
                call_id,
                status: ToolStatus::Running,
                output: Vec::new(),
                expanded: None,
                started_at: Instant::now(),
                finished_at: None,
            }),
            created_at: Instant::now(),
            token_usage: None,
            content_version: 0,
            turn_id: None,
        });
        self.next_message_id = self.next_message_id.saturating_add(1);
    }

    fn finish_tool(
        &mut self,
        turn_id: &RequestId,
        call_id: Option<&str>,
        name: &str,
        output: &str,
    ) {
        let Some(message) = self.messages.iter_mut().rev().find(|message| {
            matches!(
                &message.kind,
                UiMessageKind::Tool(tool)
                    if &tool.turn_id == turn_id
                        && tool.status == ToolStatus::Running
                        && (call_id.is_some_and(|id| tool.call_id.as_deref() == Some(id))
                            || (call_id.is_none() && tool.name == name))
            )
        }) else {
            self.start_tool(turn_id.clone(), call_id.map(str::to_owned), name.to_owned());
            self.finish_tool(turn_id, call_id, name, output);
            return;
        };
        if let UiMessageKind::Tool(tool) = &mut message.kind {
            tool.output = output.lines().map(str::to_owned).collect();
            tool.status = if output.starts_with("工具执行错误:") {
                ToolStatus::Failed
            } else {
                ToolStatus::Ok
            };
            tool.finished_at = Some(Instant::now());
            message.content_version = message.content_version.saturating_add(1);
        }
    }

    fn request_quit(&mut self) {
        self.should_quit = true;
        self.status = "再见".to_owned();
    }

    fn scroll_by(&mut self, lines: isize) {
        if lines.is_positive() {
            self.scroll = self.scroll.saturating_add(lines.unsigned_abs());
            self.follow_bottom = false;
        } else {
            self.scroll = self.scroll.saturating_sub(lines.unsigned_abs());
            self.follow_bottom = self.scroll == 0;
        }
    }

    fn scroll_to_top(&mut self) {
        self.scroll = usize::MAX;
        self.follow_bottom = false;
    }

    fn scroll_to_bottom(&mut self) {
        self.scroll = 0;
        self.follow_bottom = true;
    }
}

pub async fn run_tui(client: DaemonClient, workspace: &std::path::Path) -> Result<()> {
    let snapshot = recovery::start_new_session(&client).await?;
    let session_id = snapshot.session_id.clone();
    let mut state = TuiState::from_snapshot(snapshot);
    for request_id in std::mem::take(&mut state.recovery_active_requests) {
        match recovery::subscribe(&client, &request_id).await {
            Ok(stream) => state.active_turns.push(ActiveTurn { request_id, stream }),
            Err(error) => state.status = format!("恢复活动请求失败：{error:#}"),
        }
    }
    state.workspace = workspace.display().to_string();
    state.status = format!("新会话 {session_id} · /resume 恢复历史");

    let _guard = TerminalGuard;
    let mut terminal = setup_terminal()?;
    let result = run_event_loop(&client, &mut terminal, &mut state).await;
    restore_terminal(&mut terminal)?;
    result
}

fn setup_terminal() -> Result<TuiTerminal> {
    terminal::enable_raw_mode().context("启用终端 raw mode 失败")?;
    let mut stdout = io::stdout();
    let mouse_enabled = std::env::var("MY_AGENT_TUI_MOUSE").is_ok_and(|value| value == "1");
    let terminal_result = if mouse_enabled {
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableMouseCapture
        )
    } else {
        execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)
    };
    if let Err(error) = terminal_result {
        let _ = terminal::disable_raw_mode();
        return Err(error).context("进入终端 alternate screen 失败");
    }
    Terminal::new(CrosstermBackend::new(stdout)).context("创建 TUI 终端失败")
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
    }
}

fn restore_terminal(terminal: &mut TuiTerminal) -> Result<()> {
    terminal::disable_raw_mode().context("恢复终端 raw mode 失败")?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    )
    .context("退出终端 alternate screen 失败")?;
    terminal.show_cursor().context("恢复终端光标失败")
}

async fn run_event_loop(
    client: &DaemonClient,
    terminal: &mut TuiTerminal,
    state: &mut TuiState,
) -> Result<()> {
    let mut dirty = true;
    while !state.should_quit {
        if dirty {
            terminal
                .draw(|frame| draw_ui(frame, state))
                .context("绘制 TUI 失败")?;
            dirty = false;
        }

        if event::poll(Duration::from_millis(16)).context("读取终端事件失败")? {
            dirty = true;
            match event::read().context("读取键盘事件失败")? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    if let Err(error) = handle_key(client, state, key).await {
                        state.status = format!("操作失败：{error:#}");
                    }
                }
                Event::Paste(text) if state.pending_approvals.is_empty() => {
                    state.input.insert_text(&text)
                }
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollUp if !state.pending_approvals.is_empty() => {
                        state.approval_scroll = state.approval_scroll.saturating_sub(3);
                    }
                    MouseEventKind::ScrollDown if !state.pending_approvals.is_empty() => {
                        state.approval_scroll = state.approval_scroll.saturating_add(3);
                    }
                    MouseEventKind::ScrollUp => state.scroll_by(3),
                    MouseEventKind::ScrollDown => state.scroll_by(-3),
                    _ => {}
                },
                _ => {}
            }
        }
        let mut index = 0;
        for _ in 0..128 {
            if index >= state.active_turns.len() {
                break;
            }
            let request_id = state.active_turns[index].request_id.clone();
            let next = {
                let active = &mut state.active_turns[index];
                tokio::time::timeout(Duration::from_millis(1), active.stream.next()).await
            };
            match next {
                Ok(Some(frame)) => {
                    handle_frame(state, &request_id, frame).await?;
                    start_next_turn(client, state).await?;
                    dirty = true;
                    if state
                        .active_turns
                        .get(index)
                        .is_some_and(|active| active.request_id == request_id)
                    {
                        index += 1;
                    }
                }
                Ok(None) => {
                    state.active_turns.remove(index);
                    state.status = "连接中断 · 退出后重新运行 myagent 可恢复".to_owned();
                    dirty = true;
                }
                Err(_) => index += 1,
            }
        }
    }
    Ok(())
}

async fn handle_key(client: &DaemonClient, state: &mut TuiState, key: KeyEvent) -> Result<()> {
    if key.code == KeyCode::Esc {
        state.request_quit();
        return Ok(());
    }
    match key.code {
        KeyCode::PageUp if !state.pending_approvals.is_empty() => {
            state.approval_scroll = state.approval_scroll.saturating_sub(4);
            return Ok(());
        }
        KeyCode::PageDown if !state.pending_approvals.is_empty() => {
            state.approval_scroll = state.approval_scroll.saturating_add(4);
            return Ok(());
        }
        KeyCode::PageUp => {
            state.scroll_by(8);
            return Ok(());
        }
        KeyCode::PageDown => {
            state.scroll_by(-8);
            return Ok(());
        }
        _ => {}
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('t') {
        state.show_tools = !state.show_tools;
        return Ok(());
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('u') {
        state.input.clear();
        state.history_cursor = None;
        return Ok(());
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('k') {
        let cleared = state.queued_turns.len();
        state.queued_turns.clear();
        state.status = if cleared == 0 {
            "发送队列为空".to_owned()
        } else {
            format!("已清空 {cleared} 条排队消息")
        };
        return Ok(());
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        if let Some(request_id) = state
            .active_turns
            .last()
            .map(|turn| turn.request_id.clone())
        {
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

    if let Some(approval) = state.pending_approvals.front().cloned() {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                recovery::respond_to_approval(client, &approval.id, true).await?;
                state.pending_approvals.pop_front();
                state.status = "审批已允许，继续执行".to_owned();
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Enter => {
                recovery::respond_to_approval(client, &approval.id, false).await?;
                state.pending_approvals.pop_front();
                state.status = "审批已拒绝，继续执行".to_owned();
            }
            _ => {}
        }
        return Ok(());
    }

    match key.code {
        KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.input.move_word_left()
        }
        KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.input.move_word_right()
        }
        KeyCode::Left => state.input.move_left(),
        KeyCode::Right => state.input.move_right(),
        KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => state.scroll_to_top(),
        KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => state.scroll_to_bottom(),
        KeyCode::Up if key.modifiers.contains(KeyModifiers::CONTROL) => state.scroll_by(1),
        KeyCode::Down if key.modifiers.contains(KeyModifiers::CONTROL) => state.scroll_by(-1),
        KeyCode::Home => state.input.move_line_start(),
        KeyCode::End => state.input.move_line_end(),
        KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.input.move_line_start()
        }
        KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.input.move_line_end()
        }
        KeyCode::Up if state.input.is_single_line() => state.history_previous(),
        KeyCode::Down if state.input.is_single_line() => state.history_next(),
        KeyCode::Backspace if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.input.delete_word_left()
        }
        KeyCode::Backspace => state.input.backspace(),
        KeyCode::Delete => state.input.delete(),
        KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.input.delete_word_left()
        }
        KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.input.insert(character)
        }
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => state.input.insert('\n'),
        KeyCode::Enter if !state.input.is_blank() => {
            let message = state.input.take();
            state.record_history(&message);
            submit_input(client, state, message).await?;
        }
        _ => {}
    }
    Ok(())
}

async fn handle_frame(state: &mut TuiState, turn_id: &RequestId, frame: ServerFrame) -> Result<()> {
    match frame {
        ServerFrame::Event(event) => match event.event {
            EventKind::TextDelta => {
                if let Some(delta) = event.data["delta"].as_str() {
                    state.append_assistant(turn_id, delta);
                }
            }
            EventKind::ToolStarted => {
                state.start_tool(
                    turn_id.clone(),
                    event.data["tool_call_id"].as_str().map(str::to_owned),
                    event.data["name"].as_str().unwrap_or("unknown").to_owned(),
                );
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
                state.finish_tool(
                    turn_id,
                    event.data["tool_call_id"].as_str(),
                    event.data["name"].as_str().unwrap_or("工具"),
                    event.data["output"].as_str().unwrap_or_default(),
                );
            }
            EventKind::ApprovalRequired => {
                state.approval_scroll = 0;
                state.pending_approvals.push_back(
                    serde_json::from_value(event.data["approval"].clone())
                        .context("审批事件格式无效")?,
                );
                state.status = format!("等待审批：还有 {} 项", state.pending_approvals.len());
            }
            EventKind::TurnStarted | EventKind::TurnCompleted => {}
        },
        ServerFrame::Response(response) => {
            state
                .active_turns
                .retain(|turn| &turn.request_id != turn_id);
            state
                .pending_approvals
                .retain(|approval| &approval.request_id != turn_id);
            if let Some(error) = response.error {
                state.status = format!("请求失败（{}）：{}", error.code, error.message);
            } else {
                state.status = "就绪".to_owned();
            }
        }
    }
    Ok(())
}

async fn submit_input(client: &DaemonClient, state: &mut TuiState, message: String) -> Result<()> {
    let input = message.trim();
    if input.starts_with('/') {
        execute_slash(client, state, input).await?;
        return Ok(());
    }
    if !state.resume_choices.is_empty() && input.chars().all(|character| character.is_ascii_digit())
    {
        execute_slash(client, state, &format!("/resume {input}")).await?;
        return Ok(());
    }

    state.resume_choices.clear();
    state.push_user(message.clone());
    if !state.active_turns.is_empty()
        || !state.recovery_active_requests.is_empty()
        || !state.pending_approvals.is_empty()
    {
        state.queued_turns.push_back(message);
        state.status = format!("已排队，前方还有 {} 条消息", state.queued_turns.len());
        return Ok(());
    }
    begin_turn(client, state, message).await
}

async fn start_next_turn(client: &DaemonClient, state: &mut TuiState) -> Result<()> {
    if !state.active_turns.is_empty()
        || !state.recovery_active_requests.is_empty()
        || !state.pending_approvals.is_empty()
    {
        return Ok(());
    }
    let Some(message) = state.queued_turns.pop_front() else {
        return Ok(());
    };
    begin_turn(client, state, message).await
}

async fn begin_turn(client: &DaemonClient, state: &mut TuiState, message: String) -> Result<()> {
    let stream = client
        .request("chat.send", json!({"message": message}))
        .await?;
    let request_id = stream.request_id().clone();
    state.active_turns.push(ActiveTurn { request_id, stream });
    state.status = if state.queued_turns.is_empty() {
        "Agent 正在工作".to_owned()
    } else {
        format!("Agent 正在工作 · 队列 {}", state.queued_turns.len())
    };
    Ok(())
}

async fn execute_slash(client: &DaemonClient, state: &mut TuiState, line: &str) -> Result<()> {
    let value =
        crate::entry::cli::request_result(client, "slash.execute", json!({"line": line})).await?;
    let response: SlashResponse =
        serde_json::from_value(value).context("daemon slash.execute 格式无效")?;
    match response {
        SlashResponse::Text { content } => {
            state.push_text(Role::System, content);
            state.status = "命令已完成".to_owned();
        }
        SlashResponse::Exit => state.request_quit(),
        SlashResponse::Sessions { sessions, select } => {
            state.resume_choices = if select { sessions.clone() } else { Vec::new() };
            state.push_text(Role::System, render_session_choices(&sessions, select));
            state.scroll = 0;
            state.status = if sessions.is_empty() {
                "暂无会话记录".to_owned()
            } else if select {
                "请选择要恢复的 session".to_owned()
            } else {
                "已列出 session".to_owned()
            };
        }
        SlashResponse::SessionChanged { message, snapshot } => {
            let snapshot = recovery::parse_snapshot(snapshot, "slash.execute")?;
            state.replace_snapshot(snapshot);
            state.status = message;
        }
    }
    Ok(())
}

fn render_session_choices(sessions: &[SessionInfo], select: bool) -> String {
    if sessions.is_empty() {
        return if select {
            "暂无可恢复的历史会话。".to_owned()
        } else {
            "暂无会话记录。".to_owned()
        };
    }
    let mut lines = vec!["可恢复的历史会话：".to_owned()];
    for (index, session) in sessions.iter().enumerate() {
        let marker = if session.active { " · 当前" } else { "" };
        let preview = session.preview.as_deref().unwrap_or("无摘要");
        lines.push(format!(
            "{}. {} · {} 条消息{marker}\n   {preview}",
            index + 1,
            session.id,
            session.message_count
        ));
    }
    if select {
        lines.push("输入编号，或使用 /resume <编号或 session ID>。".to_owned());
    }
    lines.join("\n")
}

fn message_to_ui(message: Message, id: u64) -> Option<UiMessage> {
    let content = message.content?.to_owned();
    Some(UiMessage {
        id,
        role: message.role,
        kind: UiMessageKind::Text(content),
        created_at: Instant::now(),
        token_usage: None,
        content_version: 0,
        turn_id: None,
    })
}

fn tool_heading(name: &str) -> String {
    match name {
        "read_file" => "读取文件".to_owned(),
        "write_file" => "写入文件".to_owned(),
        "edit_file" => "编辑文件".to_owned(),
        "exec" => "运行命令".to_owned(),
        _ => format!("执行 {name}"),
    }
}

#[cfg(test)]
fn message_text(message: &UiMessage) -> &str {
    match &message.kind {
        UiMessageKind::Text(content) => content,
        UiMessageKind::Tool(_) => "",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use super::*;
    use crate::context::{ContextConfig, ContextManager};
    use crate::daemon::DaemonState;
    use crate::daemon::approval::ApprovalBroker;
    use crate::daemon::protocol::EventFrame;
    use crate::daemon::server::InMemoryServer;
    use crate::loop_engine::LoopEngine;
    use crate::plan::PlanStore;
    use crate::provider::{Provider, Response, ToolSpec};
    use crate::session::SessionStore;
    use crate::tools::ToolRegistry;

    static NEXT_TEST: AtomicUsize = AtomicUsize::new(0);

    struct UnusedProvider;

    #[async_trait]
    impl Provider for UnusedProvider {
        async fn chat(&self, _messages: &[Message], _tools: &[ToolSpec]) -> Result<Response> {
            anyhow::bail!("恢复会话测试不应调用 Provider")
        }
    }

    #[test]
    fn converts_persisted_message_to_ui_message() {
        let message = Message::text(Role::User, "检查项目");
        let converted = message_to_ui(message, 7).expect("文本消息应可显示");
        assert_eq!(converted.role, Role::User);
        assert_eq!(converted.id, 7);
        assert!(matches!(converted.kind, UiMessageKind::Text(content) if content == "检查项目"));
    }

    #[test]
    fn quit_signal_is_independent_from_status_text() {
        let mut state = TuiState::from_snapshot(recovery::RecoverySnapshot {
            session_id: "test".to_owned(),
            messages: Vec::new(),
            pending_approvals: Vec::new(),
            active_requests: Vec::new(),
        });
        state.status = "任意状态文案".to_owned();
        assert!(!state.should_quit);
        state.request_quit();
        assert!(state.should_quit);
        assert_eq!(state.status, "再见");
    }

    #[tokio::test]
    async fn preserves_concurrent_turns_and_approval_queue() {
        let first = RequestId::String("turn-a".to_owned());
        let second = RequestId::String("turn-b".to_owned());
        let mut state = TuiState::from_snapshot(recovery::RecoverySnapshot {
            session_id: "test".to_owned(),
            messages: Vec::new(),
            pending_approvals: vec![
                PendingApprovalInfo {
                    id: "approval-a".to_owned(),
                    request_id: first.clone(),
                    prompt: "允许 A".to_owned(),
                },
                PendingApprovalInfo {
                    id: "approval-b".to_owned(),
                    request_id: second.clone(),
                    prompt: "允许 B".to_owned(),
                },
            ],
            active_requests: vec![first.clone(), second.clone()],
        });
        assert_eq!(state.pending_approvals.len(), 2);
        assert_eq!(
            state.recovery_active_requests,
            vec![first.clone(), second.clone()]
        );

        handle_frame(
            &mut state,
            &first,
            ServerFrame::Event(EventFrame::new(
                first.clone(),
                EventKind::TextDelta,
                json!({"delta": "来自 A"}),
            )),
        )
        .await
        .unwrap();
        handle_frame(
            &mut state,
            &second,
            ServerFrame::Event(EventFrame::new(
                second.clone(),
                EventKind::TextDelta,
                json!({"delta": "来自 B"}),
            )),
        )
        .await
        .unwrap();
        assert_eq!(state.messages.len(), 2);
        assert!(state.messages.iter().any(|message| {
            message.turn_id.as_ref() == Some(&first) && message_text(message) == "来自 A"
        }));
        assert!(state.messages.iter().any(|message| {
            message.turn_id.as_ref() == Some(&second) && message_text(message) == "来自 B"
        }));
    }

    #[tokio::test]
    async fn resume_command_lists_and_restores_selected_session() {
        let id = NEXT_TEST.fetch_add(1, Ordering::SeqCst);
        let session_path = std::env::temp_dir().join(format!(
            "my-agent-tui-resume-{}-{id}.jsonl",
            std::process::id()
        ));
        let session = Arc::new(SessionStore::new(&session_path));
        session
            .append(&Message::text(Role::User, "需要恢复的旧问题"))
            .await
            .unwrap();
        session
            .append(&Message::text(Role::Assistant, "旧回答"))
            .await
            .unwrap();
        let history = session.load().await.unwrap();
        let provider: Arc<dyn Provider> = Arc::new(UnusedProvider);
        let context = ContextManager::new(
            provider.clone(),
            std::env::current_dir().unwrap(),
            ContextConfig {
                token_budget: 1_000_000,
                recent_messages: 100,
                mild_compression_percent: 60,
                strong_compression_percent: 85,
                summary_chunk_tokens: 100_000,
            },
            Arc::new(PlanStore::memory_only()),
        )
        .unwrap();
        let engine = Arc::new(LoopEngine::new(
            provider,
            ToolRegistry::new(),
            context,
            session.clone(),
        ));
        let client = InMemoryServer::start(Arc::new(DaemonState::new(
            engine,
            history,
            session,
            ApprovalBroker::new(),
        )));

        let fresh = recovery::start_new_session(&client).await.unwrap();
        let mut state = TuiState::from_snapshot(fresh);
        assert!(state.messages.is_empty());
        submit_input(&client, &mut state, "/resume".to_owned())
            .await
            .unwrap();
        assert_eq!(state.resume_choices.len(), 1);
        assert!(message_text(&state.messages[0]).contains("需要恢复的旧问题"));

        submit_input(&client, &mut state, "1".to_owned())
            .await
            .unwrap();
        assert_eq!(state.messages.len(), 2);
        assert_eq!(message_text(&state.messages[0]), "需要恢复的旧问题");
        assert_eq!(message_text(&state.messages[1]), "旧回答");

        submit_input(&client, &mut state, "/ping".to_owned())
            .await
            .unwrap();
        assert_eq!(state.messages.last().map(message_text), Some("pong"));

        let _ = std::fs::remove_file(SessionStore::pointer_path(&session_path));
        let _ = std::fs::remove_file(session_path);
    }
}
