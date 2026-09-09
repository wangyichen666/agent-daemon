use std::io::{self, Stdout};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use serde_json::json;

use crate::client::{DaemonClient, RpcStream};
use crate::daemon::approval::PendingApprovalInfo;
use crate::daemon::protocol::{EventKind, RequestId, ServerFrame};
use crate::entry::recovery;
use crate::provider::{Message, Role};
use crate::session::SessionInfo;
use crate::slash::SlashResponse;

type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;

mod view;
use view::draw_ui;

#[derive(Clone)]
struct UiMessage {
    role: Role,
    content: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TuiThemeMode {
    Terminal,
    Dark,
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
            _ => Self::Terminal,
        }
    }
}

struct TuiState {
    messages: Vec<UiMessage>,
    input: String,
    active: Option<RpcStream>,
    active_request_id: Option<RequestId>,
    pending_approval: Option<PendingApprovalInfo>,
    approval_scroll: usize,
    status: String,
    scroll: usize,
    show_tools: bool,
    workspace: String,
    theme_mode: TuiThemeMode,
    resume_choices: Vec<SessionInfo>,
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
            approval_scroll: 0,
            status: if has_active {
                "正在恢复活动请求".to_owned()
            } else if has_pending {
                "等待审批".to_owned()
            } else {
                "就绪".to_owned()
            },
            scroll: 0,
            show_tools: false,
            workspace: std::env::current_dir()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            theme_mode: TuiThemeMode::from_env(),
            resume_choices: Vec::new(),
        }
    }

    fn replace_snapshot(&mut self, snapshot: recovery::RecoverySnapshot) {
        self.messages = snapshot
            .messages
            .into_iter()
            .filter_map(message_to_ui)
            .collect();
        self.active_request_id = snapshot.active_requests.first().cloned();
        self.pending_approval = snapshot.pending_approvals.first().cloned();
        self.approval_scroll = 0;
        self.scroll = 0;
        self.resume_choices.clear();
    }

    fn push_user(&mut self, content: String) {
        self.messages.push(UiMessage {
            role: Role::User,
            content,
        });
        self.scroll = 0;
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
    }
}

pub async fn run_tui(client: DaemonClient, workspace: &std::path::Path) -> Result<()> {
    let snapshot = recovery::start_new_session(&client).await?;
    let session_id = snapshot.session_id.clone();
    let mut state = TuiState::from_snapshot(snapshot);
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
    if let Err(error) = execute!(stdout, EnterAlternateScreen, EnableBracketedPaste) {
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
    loop {
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
                Event::Paste(text) if state.pending_approval.is_none() => {
                    state.input.push_str(&text.replace('\r', ""))
                }
                _ => {}
            }
            if state.status == "退出" {
                break;
            }
        }
        for _ in 0..128 {
            let Some(active) = state.active.as_mut() else {
                break;
            };
            match tokio::time::timeout(Duration::from_millis(1), active.next()).await {
                Ok(Some(frame)) => {
                    handle_frame(state, frame).await?;
                    dirty = true;
                }
                Ok(None) => {
                    state.active = None;
                    state.status = "连接中断 · 退出后重新运行 myagent 可恢复".to_owned();
                    dirty = true;
                    break;
                }
                Err(_) => break,
            }
        }
    }
    Ok(())
}

async fn handle_key(client: &DaemonClient, state: &mut TuiState, key: KeyEvent) -> Result<()> {
    if key.code == KeyCode::Esc {
        state.status = "退出".to_owned();
        return Ok(());
    }
    match key.code {
        KeyCode::PageUp if state.pending_approval.is_some() => {
            state.approval_scroll = state.approval_scroll.saturating_sub(4);
            return Ok(());
        }
        KeyCode::PageDown if state.pending_approval.is_some() => {
            state.approval_scroll = state.approval_scroll.saturating_add(4);
            return Ok(());
        }
        KeyCode::PageUp => {
            state.scroll = state.scroll.saturating_add(8);
            return Ok(());
        }
        KeyCode::PageDown => {
            state.scroll = state.scroll.saturating_sub(8);
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
        return Ok(());
    }
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
        KeyCode::Backspace => {
            state.input.pop();
        }
        KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.input.push(character)
        }
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => state.input.push('\n'),
        KeyCode::Enter if !state.input.trim().is_empty() && state.active.is_none() => {
            let message = std::mem::take(&mut state.input);
            submit_input(client, state, message).await?;
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
                state.messages.push(UiMessage {
                    role: Role::Tool,
                    content: format!(
                        "{}\n{}",
                        event.data["name"].as_str().unwrap_or("工具"),
                        event.data["output"].as_str().unwrap_or_default()
                    ),
                });
            }
            EventKind::ApprovalRequired => {
                state.approval_scroll = 0;
                state.pending_approval = serde_json::from_value(event.data["approval"].clone())
                    .context("审批事件格式无效")?;
                state.status = "等待审批：Y 允许 / N 拒绝".to_owned();
            }
            EventKind::TurnStarted | EventKind::TurnCompleted => {}
        },
        ServerFrame::Response(response) => {
            state.active = None;
            state.active_request_id = None;
            state.pending_approval = None;
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
    let stream = client
        .request("chat.send", json!({"message": message}))
        .await?;
    state.active_request_id = Some(stream.request_id().clone());
    state.active = Some(stream);
    state.status = "Agent 正在工作".to_owned();
    Ok(())
}

async fn execute_slash(client: &DaemonClient, state: &mut TuiState, line: &str) -> Result<()> {
    let value =
        crate::entry::cli::request_result(client, "slash.execute", json!({"line": line})).await?;
    let response: SlashResponse =
        serde_json::from_value(value).context("daemon slash.execute 格式无效")?;
    match response {
        SlashResponse::Text { content } => {
            state.messages.push(UiMessage {
                role: Role::System,
                content,
            });
            state.status = "命令已完成".to_owned();
        }
        SlashResponse::Exit => state.status = "退出".to_owned(),
        SlashResponse::Sessions { sessions, select } => {
            state.resume_choices = if select { sessions.clone() } else { Vec::new() };
            state.messages.push(UiMessage {
                role: Role::System,
                content: render_session_choices(&sessions, select),
            });
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

fn message_to_ui(message: Message) -> Option<UiMessage> {
    let content = message.content?.to_owned();
    Some(UiMessage {
        role: message.role,
        content,
    })
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
        let converted = message_to_ui(message).expect("文本消息应可显示");
        assert_eq!(converted.role, Role::User);
        assert_eq!(converted.content, "检查项目");
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
        assert!(state.messages[0].content.contains("需要恢复的旧问题"));

        submit_input(&client, &mut state, "1".to_owned())
            .await
            .unwrap();
        assert_eq!(state.messages.len(), 2);
        assert_eq!(state.messages[0].content, "需要恢复的旧问题");
        assert_eq!(state.messages[1].content, "旧回答");

        submit_input(&client, &mut state, "/ping".to_owned())
            .await
            .unwrap();
        assert_eq!(
            state
                .messages
                .last()
                .map(|message| message.content.as_str()),
            Some("pong")
        );

        let _ = std::fs::remove_file(SessionStore::pointer_path(&session_path));
        let _ = std::fs::remove_file(session_path);
    }
}
