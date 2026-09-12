#[cfg(test)]
use std::collections::HashSet;
use std::io::Write;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::client::{DaemonClient, RpcStream};
use crate::daemon::protocol::{EventKind, RequestId, ServerFrame};
use crate::entry::recovery;
use crate::provider::Role;
use crate::slash::SlashResponse;

#[cfg(test)]
async fn recover_connection_with<F>(client: &DaemonClient, mut decide: F) -> Result<()>
where
    F: FnMut(&str) -> Result<bool>,
{
    let snapshot = recovery::load_snapshot(client).await?;
    if !snapshot.messages.is_empty() {
        println!("已恢复 {} 条历史消息：", snapshot.messages.len());
        for message in &snapshot.messages {
            let Some(content) = message.content.as_deref() else {
                continue;
            };
            match message.role {
                Role::User => println!("[你] {content}"),
                Role::Assistant => println!("[Agent] {content}"),
                Role::System | Role::Tool => {}
            }
        }
    }

    let mut subscriptions = Vec::new();
    for request_id in &snapshot.active_requests {
        match recovery::subscribe(client, request_id).await {
            Ok(stream) => subscriptions.push((request_id.clone(), stream)),
            Err(error) => tracing::warn!(%error, ?request_id, "恢复活动请求订阅失败"),
        }
    }
    if !subscriptions.is_empty() {
        println!(
            "检测到 {} 个仍在执行的请求，正在恢复输出。",
            subscriptions.len()
        );
    }

    let mut handled_approvals = HashSet::new();
    for approval in &snapshot.pending_approvals {
        let approved = decide(&approval.prompt)?;
        recovery::respond_to_approval(client, &approval.id, approved).await?;
        handled_approvals.insert(approval.id.clone());
    }
    for (request_id, stream) in subscriptions {
        consume_recovered_stream(client, &request_id, stream, &mut handled_approvals).await?;
    }
    Ok(())
}

pub async fn run_repl(client: &DaemonClient, session_id: &mut String) -> Result<()> {
    println!("my-agent 已连接 daemon，并创建了新会话。输入 /resume 恢复历史，/help 查看命令。");
    loop {
        print!("> ");
        std::io::stdout().flush().context("刷新终端输出失败")?;
        let mut line = String::new();
        if std::io::stdin()
            .read_line(&mut line)
            .context("读取终端输入失败")?
            == 0
        {
            break;
        }
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        if input.starts_with('/') {
            if run_slash(client, input, session_id).await? {
                break;
            }
        } else {
            run_chat(client, input, session_id).await?;
        }
    }
    Ok(())
}

pub async fn run_chat(client: &DaemonClient, input: &str, session_id: &str) -> Result<()> {
    let mut stream = client
        .request(
            "chat.send",
            json!({"message": input, "session_id": session_id}),
        )
        .await?;
    let request_id = stream.request_id().clone();
    let mut printed_text = false;
    let mut cancellation_sent = false;
    loop {
        let frame = tokio::select! {
            frame = stream.next() => frame,
            interrupt = tokio::signal::ctrl_c(), if !cancellation_sent => {
                interrupt.context("监听 Ctrl-C 失败")?;
                cancel_request(client, &request_id, session_id).await?;
                cancellation_sent = true;
                eprintln!("\n正在取消本轮……");
                continue;
            }
        };
        let Some(frame) = frame else {
            bail!("daemon 在返回终态响应前断开");
        };
        match frame {
            ServerFrame::Event(event) => match event.event {
                EventKind::TextDelta => {
                    if let Some(delta) = event.data.get("delta").and_then(Value::as_str) {
                        print!("{delta}");
                        std::io::stdout().flush().context("刷新流式输出失败")?;
                        printed_text = true;
                    }
                }
                EventKind::ToolStarted => {
                    let name = event.data["name"].as_str().unwrap_or("unknown");
                    let round = event.data["round"].as_u64().unwrap_or_default();
                    eprintln!("\n[第 {round} 轮·工具开始] {name}");
                }
                EventKind::ToolFinished => {
                    let name = event.data["name"].as_str().unwrap_or("unknown");
                    let duration_ms = event.data["duration_ms"].as_u64().unwrap_or_default();
                    let success = event.data["success"].as_bool().unwrap_or(true);
                    if success {
                        eprintln!("[工具完成] {name} · {duration_ms}ms");
                    } else {
                        let error = event.data["error"]
                            .as_str()
                            .or_else(|| event.data["output"].as_str())
                            .unwrap_or("未知错误");
                        eprintln!("[工具失败] {name} · {duration_ms}ms · {error}");
                    }
                }
                EventKind::ApprovalRequired => {
                    respond_to_approval(client, &event.data).await?;
                }
                EventKind::TurnStarted | EventKind::TurnCompleted => {}
            },
            ServerFrame::Response(response) => {
                if printed_text {
                    println!();
                }
                if let Some(error) = response.error {
                    bail!("daemon RPC {}: {}", error.code, error.message);
                }
                if !printed_text
                    && let Some(content) = response
                        .result
                        .as_ref()
                        .and_then(|value| value.get("content"))
                        .and_then(Value::as_str)
                {
                    println!("{content}");
                }
                eprintln!("[任务完成] request_id={request_id:?}");
                return Ok(());
            }
        }
    }
}

pub async fn print_sessions(client: &DaemonClient) -> Result<()> {
    let response = request_result(client, "slash.execute", json!({"line": "/sessions"})).await?;
    let SlashResponse::Sessions { sessions, .. } =
        serde_json::from_value(response).context("daemon slash.execute 格式无效")?
    else {
        bail!("/sessions 未返回会话清单");
    };
    print_session_list(&sessions);
    Ok(())
}

pub fn print_session_list(sessions: &[crate::session::SessionInfo]) {
    if sessions.is_empty() {
        println!("暂无会话记录。");
        return;
    }
    for (index, session) in sessions.iter().enumerate() {
        let marker = if session.active { "*" } else { " " };
        println!(
            "{marker} {}. {} · {} · {} 个活动请求 · {} 条消息 · {}",
            index + 1,
            session.id,
            session.status,
            session.active_requests,
            session.message_count,
            session.preview.as_deref().unwrap_or("无摘要")
        );
    }
}

async fn run_slash(client: &DaemonClient, line: &str, session_id: &mut String) -> Result<bool> {
    let value = request_result(
        client,
        "slash.execute",
        json!({"line": line, "session_id": session_id}),
    )
    .await?;
    let mut response: SlashResponse =
        serde_json::from_value(value).context("daemon slash.execute 格式无效")?;
    loop {
        match response {
            SlashResponse::Text { content } => {
                println!("{content}");
                return Ok(false);
            }
            SlashResponse::Exit => return Ok(true),
            SlashResponse::Sessions { sessions, select } => {
                print_session_list(&sessions);
                if !select || sessions.is_empty() {
                    return Ok(false);
                }
                print!("请选择会话编号或输入 session ID：");
                std::io::stdout().flush().context("刷新会话选择提示失败")?;
                let mut answer = String::new();
                std::io::stdin()
                    .read_line(&mut answer)
                    .context("读取会话选择失败")?;
                let answer = answer.trim();
                if answer.is_empty() {
                    println!("已取消恢复。");
                    return Ok(false);
                }
                let value = request_result(
                    client,
                    "slash.execute",
                    json!({
                        "line": format!("/resume {answer}"),
                        "session_id": session_id,
                    }),
                )
                .await?;
                response = serde_json::from_value(value)
                    .context("daemon slash.execute 恢复响应格式无效")?;
            }
            SlashResponse::SessionChanged { message, snapshot } => {
                println!("{message}");
                let snapshot = recovery::parse_snapshot(snapshot, "slash.execute")?;
                *session_id = snapshot.session_id.clone();
                for message in snapshot.messages {
                    let Some(content) = message.content else {
                        continue;
                    };
                    match message.role {
                        Role::User => println!("[你] {content}"),
                        Role::Assistant => println!("[Agent] {content}"),
                        Role::System | Role::Tool => {}
                    }
                }
                return Ok(false);
            }
        }
    }
}

async fn cancel_request(
    client: &DaemonClient,
    request_id: &RequestId,
    session_id: &str,
) -> Result<()> {
    request_result(
        client,
        "agent.cancel",
        json!({"request_id": request_id, "session_id": session_id}),
    )
    .await
    .map(|_| ())
}

async fn respond_to_approval(client: &DaemonClient, data: &Value) -> Result<()> {
    let approval = data.get("approval").context("审批事件缺少 approval")?;
    let approval_id = approval["id"].as_str().context("审批事件缺少 id")?;
    let prompt = approval["prompt"].as_str().context("审批事件缺少 prompt")?;
    let approved = ask_approval(prompt)?;
    request_result(
        client,
        "approval.respond",
        json!({"approval_id": approval_id, "approved": approved}),
    )
    .await
    .map(|_| ())
}

pub async fn request_result(client: &DaemonClient, method: &str, params: Value) -> Result<Value> {
    let stream = client.request(method, params).await?;
    require_result(stream).await
}

async fn require_result(mut stream: RpcStream) -> Result<Value> {
    while let Some(frame) = stream.next().await {
        if let ServerFrame::Response(response) = frame {
            if let Some(error) = response.error {
                bail!("daemon RPC {}: {}", error.code, error.message);
            }
            return response.result.context("daemon 响应缺少 result");
        }
    }
    bail!("daemon 在返回响应前断开")
}

fn ask_approval(prompt: &str) -> Result<bool> {
    print!("\n需要审批：{prompt}\n允许执行？[y/N] ");
    std::io::stdout().flush().context("刷新审批提示失败")?;
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("读取审批结果失败")?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

#[cfg(test)]
async fn consume_recovered_stream(
    client: &DaemonClient,
    request_id: &RequestId,
    mut stream: RpcStream,
    handled_approvals: &mut HashSet<String>,
) -> Result<()> {
    let mut printed_text = false;
    while let Some(frame) = stream.next().await {
        match frame {
            ServerFrame::Event(event) => match event.event {
                EventKind::TextDelta => {
                    if let Some(delta) = event.data["delta"].as_str() {
                        print!("{delta}");
                        std::io::stdout().flush().context("刷新恢复输出失败")?;
                        printed_text = true;
                    }
                }
                EventKind::ToolStarted => {
                    eprintln!(
                        "\n[恢复·第 {} 轮·工具开始] {}",
                        event.data["round"].as_u64().unwrap_or_default(),
                        event.data["name"].as_str().unwrap_or("unknown")
                    );
                }
                EventKind::ToolFinished => {
                    let name = event.data["name"].as_str().unwrap_or("unknown");
                    let duration_ms = event.data["duration_ms"].as_u64().unwrap_or_default();
                    let success = event.data["success"].as_bool().unwrap_or(true);
                    if success {
                        eprintln!("[恢复工具完成] {name} · {duration_ms}ms");
                    } else {
                        let error = event.data["error"]
                            .as_str()
                            .or_else(|| event.data["output"].as_str())
                            .unwrap_or("未知错误");
                        eprintln!("[恢复工具失败] {name} · {duration_ms}ms · {error}");
                    }
                }
                EventKind::ApprovalRequired => {
                    let approval = event
                        .data
                        .get("approval")
                        .context("审批事件缺少 approval")?;
                    let approval_id = approval["id"].as_str().context("审批事件缺少 id")?;
                    if handled_approvals.insert(approval_id.to_owned()) {
                        respond_to_approval(client, &event.data).await?;
                    }
                }
                EventKind::TurnStarted | EventKind::TurnCompleted => {}
            },
            ServerFrame::Response(response) => {
                if printed_text {
                    println!();
                }
                if let Some(error) = response.error {
                    bail!(
                        "恢复请求 {:?} 失败（{}）：{}",
                        request_id,
                        error.code,
                        error.message
                    );
                }
                return Ok(());
            }
        }
    }
    bail!("恢复请求 {request_id:?} 时 daemon 在终态前断开")
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use anyhow::Result;
    use async_trait::async_trait;
    use serde_json::{Value, json};

    use super::*;
    use crate::context::{ContextConfig, ContextManager};
    use crate::daemon::DaemonState;
    use crate::daemon::approval::ApprovalBroker;
    use crate::daemon::protocol::{EventKind, RequestId};
    use crate::daemon::server::InMemoryServer;
    use crate::loop_engine::LoopEngine;
    use crate::plan::PlanStore;
    use crate::provider::{Message, Provider, Response, ToolCall, ToolSpec};
    use crate::safety::Approval;
    use crate::session::SessionStore;
    use crate::tools::{Tool, ToolRegistry};

    static NEXT_TEST: AtomicUsize = AtomicUsize::new(0);

    struct MockProvider {
        responses: StdMutex<VecDeque<Response>>,
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn chat(&self, _messages: &[Message], _tools: &[ToolSpec]) -> Result<Response> {
            self.responses
                .lock()
                .map_err(|_| anyhow::anyhow!("mock provider 锁已损坏"))?
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("mock 响应不足"))
        }
    }

    struct ApprovalTool {
        approvals: ApprovalBroker,
    }

    #[async_trait]
    impl Tool for ApprovalTool {
        fn name(&self) -> &str {
            "danger"
        }

        fn description(&self) -> &str {
            "测试 CLI 断线审批恢复"
        }

        fn parameters(&self) -> Value {
            json!({"type": "object"})
        }

        async fn execute(&self, _args: Value) -> Result<String> {
            if self.approvals.request("执行 CLI 恢复测试动作").await? {
                Ok("approved".to_owned())
            } else {
                anyhow::bail!("测试动作被拒绝")
            }
        }
    }

    #[tokio::test]
    async fn cli_reconnect_helper_recovers_active_turn_and_approval() {
        let id = NEXT_TEST.fetch_add(1, Ordering::SeqCst);
        let session_path = std::env::temp_dir().join(format!(
            "my-agent-cli-reconnect-{}-{id}.jsonl",
            std::process::id()
        ));
        let provider: Arc<dyn Provider> = Arc::new(MockProvider {
            responses: StdMutex::new(VecDeque::from([
                Response::ToolCalls(vec![ToolCall {
                    id: "cli-danger".to_owned(),
                    name: "danger".to_owned(),
                    arguments: json!({}),
                }]),
                Response::Text("CLI 恢复完成".to_owned()),
            ])),
        });
        let approvals = ApprovalBroker::new();
        let mut tools = ToolRegistry::new();
        tools.register(ApprovalTool {
            approvals: approvals.clone(),
        });
        let session = Arc::new(SessionStore::new(&session_path));
        let context = ContextManager::new(
            provider.clone(),
            std::env::current_dir().expect("测试工作区应存在"),
            ContextConfig {
                token_budget: 1_000_000,
                recent_messages: 100,
                mild_compression_percent: 60,
                strong_compression_percent: 85,
                summary_chunk_tokens: 100_000,
            },
            Arc::new(PlanStore::memory_only()),
        )
        .expect("测试上下文应可创建");
        let engine = Arc::new(LoopEngine::new(provider, tools, context, session.clone()));
        let client = InMemoryServer::start(Arc::new(DaemonState::new(
            engine,
            Vec::new(),
            session,
            approvals,
        )));
        let request_id = RequestId::String("cli-lost-turn".to_owned());
        let mut original = client
            .request_with_id(request_id, "chat.send", json!({"message": "断线后恢复"}))
            .await
            .expect("应启动测试 turn");
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(2), original.next())
                .await
                .expect("原连接应收到审批")
                .expect("原连接不应提前关闭");
            if matches!(
                frame,
                ServerFrame::Event(ref event) if event.event == EventKind::ApprovalRequired
            ) {
                break;
            }
        }
        drop(original);

        recover_connection_with(&client, |_| Ok(true))
            .await
            .expect("CLI 恢复助手应完成审批与输出订阅");
        let snapshot = recovery::load_snapshot(&client)
            .await
            .expect("应读取最终恢复快照");
        assert!(snapshot.pending_approvals.is_empty());
        assert!(snapshot.active_requests.is_empty());
        let _ = std::fs::remove_file(session_path);
    }
}
