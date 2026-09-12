mod client;
mod config;
mod context;
mod cron;
mod daemon;
mod entry;
mod loop_engine;
mod mcp;
mod memory;
mod plan;
mod provider;
mod safety;
mod session;
mod skills;
mod slash;
mod sub_agent;
mod tool_calls;
mod tools;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use daemon::lifecycle::{DaemonStatus, RuntimePaths};
use daemon::runtime::build_daemon_state;
use daemon::server::run_unix_server;
use entry::cli::{print_session_list, print_sessions, request_result, run_chat, run_repl};
use entry::editor::run_acp_server;
use entry::serve::run_http_server;
use entry::tui::run_tui;
use serde_json::json;
use session::SessionStore;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "my-agent",
    version,
    about = "个人使用的轻量 Rust AI 编码 Agent"
)]
struct Cli {
    #[arg(long, global = true, default_value = ".", help = "Agent 工作区")]
    workspace: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    #[command(about = "进入交互对话；也可直接附带一次性问题")]
    Chat {
        #[arg(trailing_var_arg = true)]
        prompt: Vec<String>,
    },
    #[command(about = "启动终端 TUI 对话界面")]
    Tui,
    #[command(about = "启动本地 OpenAI 兼容 HTTP API")]
    Serve {
        #[arg(long, default_value = "127.0.0.1:8787", help = "HTTP 监听地址")]
        bind: SocketAddr,
    },
    #[command(about = "启动编辑器 stdio JSON-RPC 适配器")]
    Editor,
    #[command(about = "在前台运行内部 daemon", hide = true)]
    Daemon,
    #[command(about = "查看当前工作区 daemon 状态")]
    Status,
    #[command(about = "优雅停止当前工作区 daemon")]
    Stop,
    #[command(about = "列出当前工作区会话")]
    Sessions,
    #[command(about = "查看当前工作区 daemon 日志")]
    Logs {
        #[arg(long, default_value_t = 100, help = "显示最近多少行")]
        lines: usize,
        #[arg(long, help = "只显示指定 session_id 的日志")]
        session: Option<String>,
        #[arg(long, help = "只显示指定 request id 的日志，例如 2 或 task-1")]
        request: Option<String>,
    },
    #[command(subcommand, about = "配置诊断")]
    Config(ConfigCommand),
}

#[derive(Subcommand)]
enum ConfigCommand {
    #[command(about = "检查必需与可选环境变量")]
    Check,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    let workspace = canonical_workspace(&cli.workspace)?;
    let command = cli.command.unwrap_or(Command::Tui);
    match command {
        Command::Chat { prompt } => run_chat_command(&workspace, prompt).await,
        Command::Tui => run_tui_command(&workspace).await,
        Command::Serve { bind } => run_serve_command(&workspace, bind).await,
        Command::Editor => run_editor_command(&workspace).await,
        Command::Daemon => run_daemon_command(&workspace).await,
        Command::Status => run_status_command(&workspace).await,
        Command::Stop => run_stop_command(&workspace).await,
        Command::Sessions => run_sessions_command(&workspace).await,
        Command::Logs {
            lines,
            session,
            request,
        } => run_logs_command(&workspace, lines, session.as_deref(), request.as_deref()).await,
        Command::Config(ConfigCommand::Check) => run_config_check(),
    }
}

async fn run_editor_command(workspace: &Path) -> Result<()> {
    config::validate_environment()?;
    let paths = RuntimePaths::for_workspace(workspace)?;
    paths.ensure_daemon(workspace).await?;
    let client = client::DaemonClient::connect_unix(&paths.socket).await?;
    run_acp_server(client, workspace.to_path_buf()).await
}

async fn run_serve_command(workspace: &Path, bind: SocketAddr) -> Result<()> {
    config::validate_environment()?;
    let bearer_token = std::env::var("MY_AGENT_API_TOKEN")
        .ok()
        .filter(|value| !value.trim().is_empty());
    if !bind.ip().is_loopback() && bearer_token.is_none() {
        anyhow::bail!("非回环地址 {bind} 必须设置 MY_AGENT_API_TOKEN；建议默认使用 127.0.0.1:8787");
    }
    let paths = RuntimePaths::for_workspace(workspace)?;
    paths.ensure_daemon(workspace).await?;
    let client = client::DaemonClient::connect_unix(&paths.socket).await?;
    let model = std::env::var("MODEL_NAME").context("缺少环境变量 MODEL_NAME")?;
    run_http_server(client, bind, model, bearer_token).await
}

async fn run_chat_command(workspace: &Path, prompt: Vec<String>) -> Result<()> {
    config::validate_environment()?;
    let paths = RuntimePaths::for_workspace(workspace)?;
    paths.ensure_daemon(workspace).await?;
    let client = client::DaemonClient::connect_unix(&paths.socket).await?;
    let snapshot = entry::recovery::start_new_session(&client).await?;
    let mut session_id = snapshot.session_id;
    if prompt.is_empty() {
        run_repl(&client, &mut session_id).await
    } else {
        run_chat(&client, &prompt.join(" "), &session_id).await
    }
}

async fn run_tui_command(workspace: &Path) -> Result<()> {
    config::validate_environment()?;
    let paths = RuntimePaths::for_workspace(workspace)?;
    paths.ensure_daemon(workspace).await?;
    let client = client::DaemonClient::connect_unix(&paths.socket).await?;
    run_tui(client, workspace).await
}

async fn run_daemon_command(workspace: &Path) -> Result<()> {
    config::validate_environment()?;
    let paths = RuntimePaths::for_workspace(workspace)?;
    if matches!(paths.status().await, DaemonStatus::Ready { .. }) {
        anyhow::bail!("该工作区的 daemon 已在运行");
    }
    let state = build_daemon_state(workspace).await?;
    run_unix_server(state, &paths, workspace).await
}

async fn run_status_command(workspace: &Path) -> Result<()> {
    let paths = RuntimePaths::for_workspace(workspace)?;
    match paths.status().await {
        DaemonStatus::Ready { pid } => {
            println!(
                "ready · pid={pid} · socket={} · log={} · sessions={}",
                paths.socket.display(),
                paths.log.display(),
                workspace.join(".my-agent").display()
            )
        }
        DaemonStatus::Starting { pid } => println!(
            "starting · pid={pid:?} · log={} · sessions={}",
            paths.log.display(),
            workspace.join(".my-agent").display()
        ),
        DaemonStatus::Stale { pid } => {
            println!(
                "stale · pid={pid:?} · log={} · 可再次运行 `my-agent` 自动清理并重启",
                paths.log.display()
            )
        }
        DaemonStatus::Stopped => println!("stopped · log={}", paths.log.display()),
    }
    Ok(())
}

async fn run_stop_command(workspace: &Path) -> Result<()> {
    let paths = RuntimePaths::for_workspace(workspace)?;
    match paths.status().await {
        DaemonStatus::Ready { .. } => {
            let client = client::DaemonClient::connect_unix(&paths.socket).await?;
            request_result(&client, "daemon.stop", json!({})).await?;
            println!("已请求 daemon 优雅停止；正在执行的 turn 不会被超时强杀。")
        }
        DaemonStatus::Stale { .. } => {
            paths.cleanup().await;
            println!("已清理失效的 daemon 运行标记。")
        }
        DaemonStatus::Starting { pid } => {
            println!("daemon 正在启动（pid={pid:?}），请稍后重试 stop。")
        }
        DaemonStatus::Stopped => println!("daemon 未运行。"),
    }
    Ok(())
}

async fn run_sessions_command(workspace: &Path) -> Result<()> {
    let paths = RuntimePaths::for_workspace(workspace)?;
    if matches!(paths.status().await, DaemonStatus::Ready { .. }) {
        let client = client::DaemonClient::connect_unix(&paths.socket).await?;
        return print_sessions(&client).await;
    }
    let store = SessionStore::from_env(workspace);
    let sessions = store
        .list_sessions()
        .await
        .context("读取本地会话清单失败")?;
    println!("daemon 未运行，以下为本地会话快照：");
    print_session_list(&sessions);
    Ok(())
}

async fn run_logs_command(
    workspace: &Path,
    lines: usize,
    session: Option<&str>,
    request: Option<&str>,
) -> Result<()> {
    if lines == 0 {
        anyhow::bail!("--lines 必须大于 0");
    }
    let paths = RuntimePaths::for_workspace(workspace)?;
    let content = match tokio::fs::read_to_string(&paths.log).await {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            println!("暂无 daemon 日志：{}", paths.log.display());
            return Ok(());
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("读取 daemon 日志失败: {}", paths.log.display()));
        }
    };
    let recent = content
        .lines()
        .filter(|line| {
            session.is_none_or(|session| line.contains(&format!("session_id={session}")))
                && request.is_none_or(|request| log_line_matches_request(line, request))
        })
        .rev()
        .take(lines)
        .collect::<Vec<_>>();
    for line in recent.into_iter().rev() {
        println!("{line}");
    }
    Ok(())
}

fn log_line_matches_request(line: &str, request: &str) -> bool {
    if request.starts_with("Number(") || request.starts_with("String(") {
        return line.contains(&format!("request_id={request}"));
    }
    line.contains(&format!("request_id=Number({request})"))
        || line.contains(&format!("request_id=String(\"{request}\")"))
        || line.contains(&format!("request_id={request}"))
}

fn run_config_check() -> Result<()> {
    let issues = config::check_environment();
    if issues.is_empty() {
        println!("配置检查通过。API 密钥已设置（值不会显示）。");
        return Ok(());
    }
    println!("配置检查发现 {} 个问题：", issues.len());
    for issue in &issues {
        println!("- {}：{}", issue.variable, issue.message);
    }
    anyhow::bail!("配置尚未就绪；请修正以上环境变量")
}

fn canonical_workspace(path: &Path) -> Result<PathBuf> {
    std::fs::canonicalize(path).with_context(|| format!("无法访问工作区：{}", path.display()))
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

#[cfg(test)]
mod main_tests {
    use super::log_line_matches_request;

    #[test]
    fn filters_numeric_and_string_request_ids_without_partial_matches() {
        let numeric = "agent_turn{request_id=Number(2)}: 开始 ReAct 轮次";
        let string = "agent_turn{request_id=String(\"task-2\")}: 开始 ReAct 轮次";

        assert!(log_line_matches_request(numeric, "2"));
        assert!(log_line_matches_request(numeric, "Number(2)"));
        assert!(log_line_matches_request(string, "task-2"));
        assert!(!log_line_matches_request(numeric, "1"));
    }
}
