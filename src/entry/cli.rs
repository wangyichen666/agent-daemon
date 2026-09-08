use std::collections::HashSet;
use std::io::Write;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::client::{DaemonClient, RpcStream};
use crate::daemon::protocol::{EventKind, RequestId, ServerFrame};
use crate::entry::recovery;
use crate::provider::Role;

pub async fn recover_connection(client: &DaemonClient) -> Result<()> {
    recover_connection_with(client, ask_approval).await
}

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

pub async fn run_repl(client: &DaemonClient) -> Result<()> {
    println!("my-agent 已连接 daemon。输入 /help 查看命令。");
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
        match input {
            "" => continue,
            "/exit" | "/quit" => break,
            "/help" => print_help(),
            "/status" => print_session_status(client).await?,
            "/sessions" => print_sessions(client).await?,
            "/new" => create_session(client).await?,
            "/cancel" => println!("当前没有前台请求；运行中按 Ctrl-C 可取消本轮。"),
            _ if input.starts_with('/') => println!("未知命令：{input}。输入 /help 查看命令。"),
            _ => run_chat(client, input).await?,
        }
    }
    Ok(())
}

pub async fn run_chat(client: &DaemonClient, input: &str) -> Result<()> {
    let mut stream = client
        .request("chat.send", json!({"message": input}))
        .await?;
    let request_id = stream.request_id().clone();
    let mut printed_text = false;
    let mut cancellation_sent = false;
    loop {
        let frame = tokio::select! {
            frame = stream.next() => frame,
            interrupt = tokio::signal::ctrl_c(), if !cancellation_sent => {
                interrupt.context("监听 Ctrl-C 失败")?;
                cancel_request(client, &request_id).await?;
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
                    eprintln!("\n[工具开始] {name}");
                }
                EventKind::ToolFinished => {
                    let name = event.data["name"].as_str().unwrap_or("unknown");
                    eprintln!("[工具完成] {name}");
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
                return Ok(());
            }
        }
    }
}

pub async fn print_sessions(client: &DaemonClient) -> Result<()> {
    let result = request_result(client, "session.list", json!({})).await?;
    let sessions = result["sessions"].as_array().context("会话列表格式无效")?;
    if sessions.is_empty() {
        println!("暂无会话记录。");
        return Ok(());
    }
    for session in sessions {
        let marker = if session["active"].as_bool() == Some(true) {
            "*"
        } else {
            " "
        };
        println!(
            "{marker} {} · {} 条消息 · {}",
            session["id"].as_str().unwrap_or("unknown"),
            session["message_count"].as_u64().unwrap_or(0),
            session["path"].as_str().unwrap_or("unknown")
        );
    }
    Ok(())
}

async fn print_session_status(client: &DaemonClient) -> Result<()> {
    let result = request_result(client, "session.load", json!({})).await?;
    println!(
        "历史 {} 条，活动请求 {} 个，待审批 {} 个。",
        result["messages"].as_array().map_or(0, Vec::len),
        result["active_requests"].as_array().map_or(0, Vec::len),
        result["pending_approvals"].as_array().map_or(0, Vec::len),
    );
    Ok(())
}

async fn create_session(client: &DaemonClient) -> Result<()> {
    let result = request_result(client, "session.new", json!({})).await?;
    if let Some(path) = result["backup"].as_str() {
        println!("已新建会话；旧会话备份到 {path}");
    } else {
        println!("已新建会话。");
    }
    Ok(())
}

async fn cancel_request(client: &DaemonClient, request_id: &RequestId) -> Result<()> {
    request_result(client, "agent.cancel", json!({"request_id": request_id}))
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
                        "\n[恢复工具开始] {}",
                        event.data["name"].as_str().unwrap_or("unknown")
                    );
                }
                EventKind::ToolFinished => {
                    eprintln!(
                        "[恢复工具完成] {}",
                        event.data["name"].as_str().unwrap_or("unknown")
                    );
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

fn print_help() {
    println!(
        "/help      查看命令\n/status    查看当前会话状态\n/sessions  列出会话文件\n/new       备份当前会话并新建会话\n/cancel    无前台请求时显示提示；运行中按 Ctrl-C 取消\n/exit      断开并退出"
    );
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
