use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Context;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::protocol::{EventKind, JsonRpcRequest, JsonRpcResponse, RequestId, ServerFrame};
use super::{ActiveRequest, ActiveRequestUpdate, DaemonState};
use crate::cron::ScheduleSpec;
use crate::loop_engine::{AgentEvent, CancellationToken};
use crate::slash::{SlashAction, SlashParse, SlashRegistry, SlashResponse};

const INVALID_PARAMS: i64 = -32602;
const METHOD_NOT_FOUND: i64 = -32601;
const INTERNAL_ERROR: i64 = -32603;
const REQUEST_CANCELLED: i64 = -32800;
const REQUEST_CONFLICT: i64 = -32001;

#[derive(Deserialize)]
struct ChatSendParams {
    message: String,
}

#[derive(Deserialize)]
struct ApprovalRespondParams {
    approval_id: String,
    approved: bool,
}

#[derive(Deserialize)]
struct CancelParams {
    request_id: RequestId,
}

#[derive(Deserialize)]
struct SessionResumeParams {
    session_id: String,
}

#[derive(Deserialize)]
struct SlashExecuteParams {
    line: String,
}

impl DaemonState {
    pub async fn handle_request(
        self: Arc<Self>,
        request: JsonRpcRequest,
        frames: mpsc::UnboundedSender<ServerFrame>,
    ) {
        match request.method.as_str() {
            "chat.send" => self.handle_chat_send(request, frames).await,
            "session.load" => {
                let result = self.session_load().await;
                send_result(&frames, request.id, result);
            }
            "session.list" => {
                let result = self.session_list().await;
                send_result(&frames, request.id, result);
            }
            "session.new" => {
                let result = self.session_new().await;
                send_result(&frames, request.id, result);
            }
            "session.resume" => {
                let result = match parse_params::<SessionResumeParams>(&request.params) {
                    Ok(params) => self.session_resume(&params.session_id).await,
                    Err(error) => Err((INVALID_PARAMS, error)),
                };
                send_result(&frames, request.id, result);
            }
            "approval.respond" => {
                let result = match parse_params::<ApprovalRespondParams>(&request.params) {
                    Ok(params) => self
                        .approvals
                        .respond(&params.approval_id, params.approved)
                        .await
                        .map(|()| json!({"accepted": true}))
                        .map_err(|error| (INVALID_PARAMS, format!("{error:#}"))),
                    Err(error) => Err((INVALID_PARAMS, error)),
                };
                send_result(&frames, request.id, result);
            }
            "agent.cancel" => {
                let result = match parse_params::<CancelParams>(&request.params) {
                    Ok(params) => self.cancel(&params.request_id).await,
                    Err(error) => Err((INVALID_PARAMS, error)),
                };
                send_result(&frames, request.id, result);
            }
            "agent.subscribe" => self.handle_subscribe(request, frames).await,
            "slash.execute" => {
                let result = match parse_params::<SlashExecuteParams>(&request.params) {
                    Ok(params) => self.execute_slash(&params.line).await,
                    Err(error) => Err((INVALID_PARAMS, error)),
                };
                send_result(&frames, request.id, result);
            }
            "daemon.stop" => {
                send_result(
                    &frames,
                    request.id,
                    Ok(json!({"stopping": true, "active_turns_finish_gracefully": true})),
                );
                let shutdown = self.shutdown.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    shutdown.cancel();
                });
            }
            _ => {
                let _ = frames.send(ServerFrame::Response(JsonRpcResponse::failure(
                    request.id,
                    METHOD_NOT_FOUND,
                    format!("未知 RPC 方法: {}", request.method),
                )));
            }
        }
    }

    async fn handle_chat_send(
        self: Arc<Self>,
        request: JsonRpcRequest,
        frames: mpsc::UnboundedSender<ServerFrame>,
    ) {
        let params = match parse_params::<ChatSendParams>(&request.params) {
            Ok(params) if !params.message.trim().is_empty() => params,
            Ok(_) => {
                send_result(
                    &frames,
                    request.id,
                    Err((INVALID_PARAMS, "message 不能为空".to_owned())),
                );
                return;
            }
            Err(error) => {
                send_result(&frames, request.id, Err((INVALID_PARAMS, error)));
                return;
            }
        };

        let cancellation = CancellationToken::new();
        let mut active = self.active.lock().await;
        if active.contains_key(&request.id) {
            drop(active);
            send_result(
                &frames,
                request.id,
                Err((REQUEST_CONFLICT, "请求 id 正在执行".to_owned())),
            );
            return;
        }
        active.insert(request.id.clone(), ActiveRequest::new(cancellation.clone()));
        drop(active);

        let (agent_events, mut event_receiver) = mpsc::unbounded_channel();
        let (approval_events, mut approval_receiver) = mpsc::unbounded_channel();
        let run = self
            .approvals
            .with_context(request.id.clone(), approval_events, async {
                let mut history = self.history.lock().await;
                self.engine
                    .run_turn_with_events(
                        &mut history,
                        params.message,
                        Some(agent_events),
                        cancellation,
                    )
                    .await
            });
        tokio::pin!(run);

        let result = loop {
            tokio::select! {
                result = &mut run => break result,
                event = event_receiver.recv() => {
                    if let Some(event) = event {
                        self.publish_update(
                            &frames,
                            &request.id,
                            agent_event_update(event),
                        ).await;
                    }
                }
                approval = approval_receiver.recv() => {
                    if let Some(ServerFrame::Event(event)) = approval {
                        self.publish_update(
                            &frames,
                            &request.id,
                            ActiveRequestUpdate::Event {
                                kind: event.event,
                                data: event.data,
                            },
                        ).await;
                    }
                }
            }
        };
        while let Ok(event) = event_receiver.try_recv() {
            self.publish_update(&frames, &request.id, agent_event_update(event))
                .await;
        }
        while let Ok(ServerFrame::Event(event)) = approval_receiver.try_recv() {
            self.publish_update(
                &frames,
                &request.id,
                ActiveRequestUpdate::Event {
                    kind: event.event,
                    data: event.data,
                },
            )
            .await;
        }

        let response = match result {
            Ok(content) => Ok(json!({"content": content})),
            Err(error) if error.to_string().contains("请求已取消") => {
                Err((REQUEST_CANCELLED, "请求已取消".to_owned()))
            }
            Err(error) => Err((INTERNAL_ERROR, format!("{error:#}"))),
        };
        self.publish_update(
            &frames,
            &request.id,
            ActiveRequestUpdate::Terminal(response),
        )
        .await;
        self.active.lock().await.remove(&request.id);
    }

    async fn session_load(&self) -> Result<Value, (i64, String)> {
        let _switch = self.session_switch.lock().await;
        self.session_snapshot().await
    }

    async fn session_snapshot(&self) -> Result<Value, (i64, String)> {
        // 活动 turn（尤其是等待人工审批时）会长期持有内存历史锁。恢复端必须仍能
        // 立即读取快照，因此以每条消息均已 flush 的 append-only 会话文件为来源。
        let history = self
            .session
            .load()
            .await
            .map_err(|error| (INTERNAL_ERROR, format!("{error:#}")))?;
        let approvals = self.approvals.pending().await;
        let active_requests = self
            .active
            .lock()
            .await
            .keys()
            .cloned()
            .collect::<Vec<RequestId>>();
        Ok(json!({
            "session_id": self.session.current_id().await,
            "messages": history,
            "pending_approvals": approvals,
            "active_requests": active_requests,
        }))
    }

    async fn session_list(&self) -> Result<Value, (i64, String)> {
        self.session
            .list_sessions()
            .await
            .map(|sessions| json!({"sessions": sessions}))
            .map_err(|error| (INTERNAL_ERROR, format!("{error:#}")))
    }

    async fn session_new(&self) -> Result<Value, (i64, String)> {
        let _switch = self.session_switch.lock().await;
        let active = self.active.lock().await;
        if !active.is_empty() {
            return Err((REQUEST_CONFLICT, "有请求正在执行，不能新建会话".to_owned()));
        }
        let mut history = self.history.lock().await;
        let session_id = self
            .session
            .start_new()
            .await
            .map_err(|error| (INTERNAL_ERROR, format!("{error:#}")))?;
        history.clear();
        Ok(json!({
            "created": true,
            "session_id": session_id,
            "messages": [],
            "pending_approvals": [],
            "active_requests": [],
        }))
    }

    async fn session_resume(&self, session_id: &str) -> Result<Value, (i64, String)> {
        let _switch = self.session_switch.lock().await;
        if self.session.current_id().await == session_id {
            return self.session_snapshot().await;
        }
        let active = self.active.lock().await;
        if !active.is_empty() {
            return Err((REQUEST_CONFLICT, "有请求正在执行，不能切换会话".to_owned()));
        }
        let mut current_history = self.history.lock().await;
        let history = self
            .session
            .resume(session_id)
            .await
            .map_err(|error| (INVALID_PARAMS, format!("{error:#}")))?;
        current_history.clone_from(&history);
        Ok(json!({
            "resumed": true,
            "session_id": self.session.current_id().await,
            "messages": history,
            "pending_approvals": [],
            "active_requests": [],
        }))
    }

    async fn execute_slash(&self, line: &str) -> Result<Value, (i64, String)> {
        let registry = SlashRegistry::builtin();
        let response =
            match registry.parse(line) {
                SlashParse::NotCommand => SlashResponse::Text {
                    content: "输入不是 slash 命令".to_owned(),
                },
                SlashParse::Error(content) => SlashResponse::Text { content },
                SlashParse::Command(invocation) => match invocation.action {
                    SlashAction::Help => SlashResponse::Text {
                        content: registry.help(),
                    },
                    SlashAction::Status => {
                        let snapshot = self.session_snapshot().await?;
                        SlashResponse::Text {
                            content: format!(
                                "会话 {} · 历史 {} 条 · 活动请求 {} 个 · 待审批 {} 个",
                                snapshot["session_id"].as_str().unwrap_or("unknown"),
                                snapshot["messages"].as_array().map_or(0, Vec::len),
                                snapshot["active_requests"].as_array().map_or(0, Vec::len),
                                snapshot["pending_approvals"].as_array().map_or(0, Vec::len),
                            ),
                        }
                    }
                    SlashAction::Sessions => SlashResponse::Sessions {
                        sessions: self.session.list_sessions().await.map_err(|error| {
                            (INTERNAL_ERROR, format!("列出会话失败：{error:#}"))
                        })?,
                        select: false,
                    },
                    SlashAction::Resume if invocation.args.is_empty() => SlashResponse::Sessions {
                        sessions: self
                            .session
                            .list_sessions()
                            .await
                            .map_err(|error| (INTERNAL_ERROR, format!("列出会话失败：{error:#}")))?
                            .into_iter()
                            .filter(|session| session.message_count > 0)
                            .collect(),
                        select: true,
                    },
                    SlashAction::Resume => {
                        let sessions = self
                            .session
                            .list_sessions()
                            .await
                            .map_err(|error| (INTERNAL_ERROR, format!("列出会话失败：{error:#}")))?
                            .into_iter()
                            .filter(|session| session.message_count > 0)
                            .collect::<Vec<_>>();
                        let selection = &invocation.args[0];
                        let session_id = match selection.parse::<usize>() {
                            Ok(index) if index > 0 => sessions
                                .get(index - 1)
                                .map(|session| session.id.clone())
                                .ok_or_else(|| {
                                    (INVALID_PARAMS, format!("会话编号超出范围：{index}"))
                                })?,
                            Ok(_) => return Err((INVALID_PARAMS, "会话编号从 1 开始".to_owned())),
                            Err(_) => selection.clone(),
                        };
                        let snapshot = self.session_resume(&session_id).await?;
                        let count = snapshot["messages"].as_array().map_or(0, Vec::len);
                        SlashResponse::SessionChanged {
                            message: format!("已恢复会话 {session_id}，共 {count} 条消息。"),
                            snapshot,
                        }
                    }
                    SlashAction::New => {
                        let snapshot = self.session_new().await?;
                        let session_id = snapshot["session_id"].as_str().unwrap_or("unknown");
                        SlashResponse::SessionChanged {
                            message: format!("已新建会话：{session_id}"),
                            snapshot,
                        }
                    }
                    SlashAction::Cancel => SlashResponse::Text {
                        content: "当前没有前台请求；运行中按 Ctrl-C 可取消本轮。".to_owned(),
                    },
                    SlashAction::Skill => self.execute_skill_command(&invocation.args).await,
                    SlashAction::Cron => self.execute_cron_command(&invocation.args).await,
                    SlashAction::Mcp => self.execute_mcp_command(&invocation.args).await,
                    SlashAction::Ping => SlashResponse::Text {
                        content: "pong".to_owned(),
                    },
                    SlashAction::Exit => SlashResponse::Exit,
                },
            };
        serde_json::to_value(response)
            .map_err(|error| (INTERNAL_ERROR, format!("序列化 slash 响应失败：{error}")))
    }

    async fn execute_skill_command(&self, args: &[String]) -> SlashResponse {
        let Some(skills) = self.skills.as_ref() else {
            return SlashResponse::Text {
                content: "当前 daemon 未启用 skill 管理器。".to_owned(),
            };
        };
        let result = match args.first().map(String::as_str) {
            Some("list") if args.len() == 1 => skills.list().await.map(|items| {
                if items.is_empty() {
                    "暂无已安装 skill。".to_owned()
                } else {
                    items
                        .iter()
                        .map(|item| format!("{}@{}：{}", item.name, item.version, item.description))
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            }),
            Some("install") if args.len() >= 2 => {
                let force = args.last().is_some_and(|arg| arg == "--force");
                let end = args.len() - usize::from(force);
                let path = args[1..end].join(" ");
                if path.is_empty() {
                    Err(anyhow::anyhow!("用法：/skill install <路径> [--force]"))
                } else {
                    skills
                        .install(std::path::Path::new(&path), force)
                        .await
                        .map(|outcomes| {
                            outcomes
                                .iter()
                                .map(ToString::to_string)
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                }
            }
            Some("update") if args.len() >= 3 => {
                let path = args[2..].join(" ");
                skills
                    .update(&args[1], std::path::Path::new(&path))
                    .await
                    .map(|outcome| outcome.to_string())
            }
            Some("remove") if args.len() >= 2 => {
                let confirmed = args.get(2).is_some_and(|arg| arg == "--confirm");
                if args.len() > 3 {
                    Err(anyhow::anyhow!("用法：/skill remove <name> --confirm"))
                } else {
                    skills.remove(&args[1], confirmed).await
                }
            }
            _ => Err(anyhow::anyhow!(
                "用法：/skill list | install <路径> [--force] | update <name> <路径> | remove <name> --confirm"
            )),
        };
        SlashResponse::Text {
            content: match result {
                Ok(content) => content,
                Err(error) => format!("Skill 命令失败：{error:#}"),
            },
        }
    }

    async fn execute_cron_command(&self, args: &[String]) -> SlashResponse {
        let Some(cron) = self.cron.as_ref() else {
            return SlashResponse::Text {
                content: "当前 daemon 未启用 cron 管理器。".to_owned(),
            };
        };
        let result = match args.first().map(String::as_str) {
            Some("list") if args.len() == 1 => {
                let jobs = cron.store().list().await;
                Ok(if jobs.is_empty() {
                    "暂无 cron 任务。".to_owned()
                } else {
                    jobs.iter()
                        .map(|job| {
                            let last = job.history.last().map_or_else(
                                || "尚未运行".to_owned(),
                                |run| {
                                    format!(
                                        "上次={}，尝试={}，结果={}",
                                        run.finished_at,
                                        run.attempts,
                                        if run.success { "成功" } else { "失败" }
                                    )
                                },
                            );
                            format!(
                                "{} [{}] {} · {} · next={} · {}",
                                job.name,
                                job.id,
                                if job.enabled { "启用" } else { "停用" },
                                job.schedule.display(),
                                job.next_run_at,
                                last
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
            }
            Some("add") if args.len() >= 4 => match parse_cron_add(args) {
                Ok((name, schedule, prompt, retries, backoff)) => cron
                    .store()
                    .add(name, schedule, prompt, retries, backoff)
                    .await
                    .map(|job| {
                        format!(
                            "已添加 {} [{}]，下次运行 {}",
                            job.name, job.id, job.next_run_at
                        )
                    }),
                Err(error) => Err(error),
            },
            Some("enable") if args.len() == 2 => cron
                .store()
                .set_enabled(&args[1], true)
                .await
                .map(|job| format!("已启用 {}", job.name)),
            Some("disable") if args.len() == 2 => cron
                .store()
                .set_enabled(&args[1], false)
                .await
                .map(|job| format!("已停用 {}", job.name)),
            Some("run-now") if args.len() == 2 => cron.run_now(&args[1]).await.map(|run| {
                format!(
                    "立即运行{}（尝试 {} 次）：{}",
                    if run.success { "成功" } else { "失败" },
                    run.attempts,
                    run.result
                )
            }),
            Some("remove") if args.len() == 3 && args[2] == "--confirm" => cron
                .store()
                .remove(&args[1])
                .await
                .map(|job| format!("已删除 {}", job.name)),
            Some("remove") if args.len() == 2 => Err(anyhow::anyhow!(
                "删除需要显式确认：/cron remove {} --confirm",
                args[1]
            )),
            _ => Err(anyhow::anyhow!(
                "用法：/cron list | add <name> interval=<秒>|cron=<分,时,日,月,周> [--retries=N] [--backoff=N] <prompt> | enable|disable|run-now <ID或名称> | remove <ID或名称> --confirm"
            )),
        };
        SlashResponse::Text {
            content: match result {
                Ok(content) => content,
                Err(error) => format!("Cron 命令失败：{error:#}"),
            },
        }
    }

    async fn execute_mcp_command(&self, args: &[String]) -> SlashResponse {
        let Some(mcp) = self.mcp.as_ref() else {
            return SlashResponse::Text {
                content: "当前 daemon 未启用 MCP 管理器。".to_owned(),
            };
        };
        let content = match args {
            [command] if command == "reload" => {
                mcp.reload().await;
                format_mcp_status(&mcp.status(), true)
            }
            [command] if command == "status" => format_mcp_status(&mcp.status(), true),
            [command] if command == "list" => format_mcp_status(&mcp.status(), false),
            _ => "MCP 命令失败：用法：/mcp list | status | reload".to_owned(),
        };
        SlashResponse::Text { content }
    }

    async fn handle_subscribe(
        self: Arc<Self>,
        request: JsonRpcRequest,
        frames: mpsc::UnboundedSender<ServerFrame>,
    ) {
        let params = match parse_params::<CancelParams>(&request.params) {
            Ok(params) => params,
            Err(error) => {
                send_result(&frames, request.id, Err((INVALID_PARAMS, error)));
                return;
            }
        };
        let Some((replay, mut receiver)) = self
            .active
            .lock()
            .await
            .get(&params.request_id)
            .map(ActiveRequest::subscribe)
        else {
            send_result(
                &frames,
                request.id,
                Ok(json!({
                    "subscribed": false,
                    "request_id": params.request_id,
                    "reason": "请求未在执行",
                })),
            );
            return;
        };
        let pending_ids = self
            .approvals
            .pending()
            .await
            .into_iter()
            .map(|approval| approval.id)
            .collect::<HashSet<String>>();
        for update in replay {
            if is_resolved_approval(&update, &pending_ids) {
                continue;
            }
            let terminal = matches!(update, ActiveRequestUpdate::Terminal(_));
            let _ = frames.send(update.to_frame(request.id.clone()));
            if terminal {
                return;
            }
        }
        loop {
            match receiver.recv().await {
                Ok(update) => {
                    let terminal = matches!(update, ActiveRequestUpdate::Terminal(_));
                    if frames.send(update.to_frame(request.id.clone())).is_err() || terminal {
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    send_result(
                        &frames,
                        request.id,
                        Ok(json!({
                            "subscribed": false,
                            "request_id": params.request_id,
                            "reason": "请求已结束",
                        })),
                    );
                    return;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    send_result(
                        &frames,
                        request.id,
                        Err((
                            INTERNAL_ERROR,
                            format!("订阅落后 {skipped} 个事件，请重新连接恢复"),
                        )),
                    );
                    return;
                }
            }
        }
    }

    async fn publish_update(
        &self,
        frames: &mpsc::UnboundedSender<ServerFrame>,
        request_id: &RequestId,
        update: ActiveRequestUpdate,
    ) {
        if let Some(active) = self.active.lock().await.get_mut(request_id) {
            active.publish(update.clone());
        }
        let _ = frames.send(update.to_frame(request_id.clone()));
    }

    async fn cancel(&self, request_id: &RequestId) -> Result<Value, (i64, String)> {
        let token = self
            .active
            .lock()
            .await
            .get(request_id)
            .map(|active| active.cancellation.clone());
        let Some(token) = token else {
            return Ok(json!({"cancelled": false, "reason": "请求未在执行"}));
        };
        token.cancel();
        self.approvals.cancel_request(request_id).await;
        Ok(json!({"cancelled": true}))
    }
}

fn parse_cron_add(args: &[String]) -> anyhow::Result<(String, ScheduleSpec, String, u32, u64)> {
    let name = args[1].clone();
    let schedule = if let Some(seconds) = args[2].strip_prefix("interval=") {
        ScheduleSpec::Interval {
            seconds: seconds.parse().context("interval 必须是正整数秒")?,
        }
    } else if let Some(expression) = args[2].strip_prefix("cron=") {
        ScheduleSpec::Cron {
            expression: expression.replace(',', " "),
        }
    } else {
        anyhow::bail!("schedule 必须是 interval=<秒> 或 cron=<分,时,日,月,周>");
    };
    let mut retries = 0_u32;
    let mut backoff = 5_u64;
    let mut prompt = Vec::new();
    for argument in &args[3..] {
        if let Some(value) = argument.strip_prefix("--retries=") {
            retries = value.parse().context("--retries 必须是非负整数")?;
        } else if let Some(value) = argument.strip_prefix("--backoff=") {
            backoff = value.parse().context("--backoff 必须是非负整数秒")?;
        } else {
            prompt.push(argument.as_str());
        }
    }
    if prompt.is_empty() {
        anyhow::bail!("cron prompt 不能为空");
    }
    Ok((name, schedule, prompt.join(" "), retries, backoff))
}

fn format_mcp_status(status: &crate::mcp::McpStatus, include_errors: bool) -> String {
    let mut lines = Vec::new();
    if let Some(error) = &status.file_error {
        lines.push(format!("配置错误：{error}"));
    }
    for server in &status.servers {
        let state = if server.connected {
            "已连接"
        } else {
            "不可用"
        };
        let tools = if server.tools.is_empty() {
            "无工具".to_owned()
        } else {
            server.tools.join("、")
        };
        let mut line = format!("{}：{} · {}", server.name, state, tools);
        if include_errors && let Some(error) = &server.error {
            line.push_str(&format!(" · {error}"));
        }
        lines.push(line);
    }
    if lines.is_empty() {
        "未配置 MCP server（期望 .my-agent/mcp.json）。".to_owned()
    } else {
        lines.join("\n")
    }
}

fn parse_params<T: for<'de> Deserialize<'de>>(params: &Value) -> Result<T, String> {
    serde_json::from_value(params.clone()).map_err(|error| format!("参数无效: {error}"))
}

fn send_result(
    frames: &mpsc::UnboundedSender<ServerFrame>,
    id: RequestId,
    result: Result<Value, (i64, String)>,
) {
    let response = match result {
        Ok(value) => JsonRpcResponse::success(id, value),
        Err((code, message)) => JsonRpcResponse::failure(id, code, message),
    };
    let _ = frames.send(ServerFrame::Response(response));
}

fn agent_event_update(event: AgentEvent) -> ActiveRequestUpdate {
    let (kind, data) = match event {
        AgentEvent::TurnStarted => (EventKind::TurnStarted, json!({})),
        AgentEvent::TextDelta(delta) => (EventKind::TextDelta, json!({"delta": delta})),
        AgentEvent::ToolStarted { call_id, name } => (
            EventKind::ToolStarted,
            json!({"tool_call_id": call_id, "name": name}),
        ),
        AgentEvent::ToolFinished {
            call_id,
            name,
            output,
        } => (
            EventKind::ToolFinished,
            json!({"tool_call_id": call_id, "name": name, "output": output}),
        ),
        AgentEvent::TurnCompleted { content } => {
            (EventKind::TurnCompleted, json!({"content": content}))
        }
    };
    ActiveRequestUpdate::Event { kind, data }
}

fn is_resolved_approval(update: &ActiveRequestUpdate, pending_ids: &HashSet<String>) -> bool {
    let ActiveRequestUpdate::Event {
        kind: EventKind::ApprovalRequired,
        data,
    } = update
    else {
        return false;
    };
    data["approval"]["id"]
        .as_str()
        .is_some_and(|id| !pending_ids.contains(id))
}
