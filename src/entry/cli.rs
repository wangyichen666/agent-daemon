use std::io::Write;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::client::{DaemonClient, RpcStream};
use crate::daemon::protocol::{EventKind, RequestId, ServerFrame};

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

fn print_help() {
    println!(
        "/help      查看命令\n/status    查看当前会话状态\n/sessions  列出会话文件\n/new       备份当前会话并新建会话\n/cancel    无前台请求时显示提示；运行中按 Ctrl-C 取消\n/exit      断开并退出"
    );
}
