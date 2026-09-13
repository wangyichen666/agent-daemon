use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Instant;
use tokio::sync::{Mutex, mpsc};
use tracing::{Instrument, info, info_span};

use super::protocol::{EventKind, JsonRpcRequest, JsonRpcResponse, RequestId, ServerFrame};
use super::{ActiveKey, ActiveRequest, ActiveRequestUpdate, DaemonState, SessionRuntime};
use crate::cron::ScheduleSpec;
use crate::loop_engine::{AgentEvent, CancellationToken};
use crate::session::SessionStatus;
use crate::slash::{SlashAction, SlashParse, SlashRegistry, SlashResponse};

const INVALID_PARAMS: i64 = -32602;
const METHOD_NOT_FOUND: i64 = -32601;
const INTERNAL_ERROR: i64 = -32603;
const REQUEST_CANCELLED: i64 = -32800;
const REQUEST_CONFLICT: i64 = -32001;

#[derive(Deserialize)]
struct ChatSendParams {
    message: String,
    #[serde(default)]
    session_id: Option<String>,
}

#[derive(Deserialize)]
struct ApprovalRespondParams {
    approval_id: String,
    approved: bool,
}

#[derive(Deserialize)]
struct CancelParams {
    request_id: RequestId,
    #[serde(default)]
    session_id: Option<String>,
}

#[derive(Deserialize)]
struct SessionResumeParams {
    session_id: String,
}

#[derive(Deserialize, Default)]
struct SessionSelectorParams {
    #[serde(default)]
    session_id: Option<String>,
}

#[derive(Deserialize)]
struct SlashExecuteParams {
    line: String,
    #[serde(default)]
    session_id: Option<String>,
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
                let result = match parse_params::<SessionSelectorParams>(&request.params) {
                    Ok(params) => self.session_load(params.session_id.as_deref()).await,
                    Err(error) => Err((INVALID_PARAMS, error)),
                };
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
            "session.trace" => {
                let result = match parse_params::<SessionResumeParams>(&request.params) {
                    Ok(params) => self.session_trace(&params.session_id).await,
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
                    Ok(params) => {
                        self.cancel(&params.request_id, params.session_id.as_deref())
                            .await
                    }
                    Err(error) => Err((INVALID_PARAMS, error)),
                };
                send_result(&frames, request.id, result);
            }
            "agent.subscribe" => self.handle_subscribe(request, frames).await,
            "slash.execute" => {
                let result = match parse_params::<SlashExecuteParams>(&request.params) {
                    Ok(params) => {
                        self.execute_slash(&params.line, params.session_id.as_deref())
                            .await
                    }
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

        let session = match self.session_runtime(params.session_id.as_deref()).await {
            Ok(session) => session,
            Err(error) => {
                send_result(&frames, request.id, Err(error));
                return;
            }
        };
        let session_id = session.id.clone();
        let started_at = Instant::now();
        info!(
            session_id = %session_id,
            request_id = ?request.id,
            "chat 请求开始"
        );
        let active_key = ActiveKey {
            session_id: session_id.clone(),
            request_id: request.id.clone(),
        };
        let cancellation = CancellationToken::new();
        let mut active = self.active.lock().await;
        if active.contains_key(&active_key) {
            drop(active);
            send_result(
                &frames,
                request.id,
                Err((REQUEST_CONFLICT, "请求 id 正在执行".to_owned())),
            );
            return;
        }
        active.insert(active_key.clone(), ActiveRequest::new(cancellation.clone()));
        drop(active);

        let (agent_events, mut event_receiver) = mpsc::unbounded_channel();
        let (approval_events, mut approval_receiver) = mpsc::unbounded_channel();
        let trace_request_id = request_id_label(&request.id);
        let turn_span = info_span!(
            "agent_turn",
            session_id = %session_id,
            request_id = ?request.id,
        );
        let run = self
            .approvals
            .with_session_context(
                session_id.clone(),
                request.id.clone(),
                approval_events,
                async {
                    let mut history = session.history.lock().await;
                    session
                        .engine
                        .run_turn_with_events_for_request(
                            &mut history,
                            params.message,
                            Some(agent_events),
                            cancellation,
                            Some(trace_request_id),
                        )
                        .await
                },
            )
            .instrument(turn_span);
        tokio::pin!(run);

        let result = loop {
            tokio::select! {
                result = &mut run => break result,
                event = event_receiver.recv() => {
                    if let Some(event) = event {
                        self.publish_update(
                            &frames,
                            &active_key,
                            agent_event_update(event),
                        ).await;
                    }
                }
                approval = approval_receiver.recv() => {
                    if let Some(ServerFrame::Event(event)) = approval {
                        self.publish_update(
                            &frames,
                            &active_key,
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
            self.publish_update(&frames, &active_key, agent_event_update(event))
                .await;
        }
        while let Ok(ServerFrame::Event(event)) = approval_receiver.try_recv() {
            self.publish_update(
                &frames,
                &active_key,
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
        match &response {
            Ok(_) => info!(
                session_id = %session_id,
                request_id = ?active_key.request_id,
                elapsed_ms = started_at.elapsed().as_millis() as u64,
                outcome = "completed",
                "chat 请求结束"
            ),
            Err((code, error)) => info!(
                session_id = %session_id,
                request_id = ?active_key.request_id,
                elapsed_ms = started_at.elapsed().as_millis() as u64,
                outcome = "failed",
                error_code = *code,
                error,
                "chat 请求结束"
            ),
        }
        self.publish_update(
            &frames,
            &active_key,
            ActiveRequestUpdate::Terminal(response),
        )
        .await;
        self.active.lock().await.remove(&active_key);
    }

    async fn session_runtime(
        &self,
        requested_id: Option<&str>,
    ) -> Result<Arc<SessionRuntime>, (i64, String)> {
        let session_id = match requested_id {
            Some(session_id) => session_id.to_owned(),
            None => self.legacy_session_id.lock().await.clone(),
        };
        if session_id == self.default_session.id {
            return Ok(self.default_session.clone());
        }
        if let Some(runtime) = self.sessions.lock().await.get(&session_id).cloned() {
            return Ok(runtime);
        }
        let store = self
            .session
            .open_session(&session_id)
            .map_err(|error| (INVALID_PARAMS, format!("{error:#}")))?;
        let store = Arc::new(store);
        let history = store
            .load()
            .await
            .map_err(|error| (INTERNAL_ERROR, format!("{error:#}")))?;
        let runtime = Arc::new(SessionRuntime {
            id: session_id.clone(),
            engine: Arc::new(self.default_session.engine.for_session(store.clone())),
            history: Mutex::new(history),
            store,
        });
        let mut sessions = self.sessions.lock().await;
        Ok(sessions
            .entry(session_id)
            .or_insert_with(|| runtime.clone())
            .clone())
    }

    async fn session_new(&self) -> Result<Value, (i64, String)> {
        let (session_id, store) = self
            .session
            .create_isolated_session()
            .map_err(|error| (INTERNAL_ERROR, format!("{error:#}")))?;
        let store = Arc::new(store);
        let runtime = Arc::new(SessionRuntime {
            id: session_id.clone(),
            engine: Arc::new(self.default_session.engine.for_session(store.clone())),
            history: Mutex::new(Vec::new()),
            store,
        });
        self.sessions
            .lock()
            .await
            .insert(session_id.clone(), runtime);
        *self.legacy_session_id.lock().await = session_id.clone();
        Ok(json!({
            "created": true,
            "session_id": session_id,
            "messages": [],
            "pending_approvals": [],
            "active_requests": [],
        }))
    }

    async fn session_load(&self, session_id: Option<&str>) -> Result<Value, (i64, String)> {
        let runtime = self.session_runtime(session_id).await?;
        self.session_snapshot(&runtime.id).await
    }

    async fn session_snapshot(&self, session_id: &str) -> Result<Value, (i64, String)> {
        // 活动 turn（尤其是等待人工审批时）会长期持有内存历史锁。恢复端必须仍能
        // 立即读取快照，因此以每条消息均已 flush 的 append-only 会话文件为来源。
        let runtime = self.session_runtime(Some(session_id)).await?;
        let history = runtime
            .store
            .load()
            .await
            .map_err(|error| (INTERNAL_ERROR, format!("{error:#}")))?;
        let active = self.active.lock().await;
        let active_requests = active
            .keys()
            .filter(|key| key.session_id == session_id)
            .map(|key| key.request_id.clone())
            .collect::<Vec<RequestId>>();
        let request_ids = active
            .keys()
            .filter(|key| key.session_id == session_id)
            .map(|key| key.request_id.clone())
            .collect::<HashSet<RequestId>>();
        drop(active);
        let approvals = self
            .approvals
            .pending_for_session(Some(session_id))
            .await
            .into_iter()
            .filter(|approval| request_ids.contains(&approval.request_id))
            .collect::<Vec<_>>();
        let status = if !approvals.is_empty() {
            "waiting"
        } else if !active_requests.is_empty() {
            "running"
        } else {
            "idle"
        };
        Ok(json!({
            "session_id": session_id,
            "messages": history,
            "pending_approvals": approvals,
            "active_requests": active_requests,
            "status": status,
        }))
    }

    async fn session_list(&self) -> Result<Value, (i64, String)> {
        Ok(json!({"sessions": self.session_infos().await?}))
    }

    async fn session_trace(&self, session_id: &str) -> Result<Value, (i64, String)> {
        let runtime = self.session_runtime(Some(session_id)).await?;
        let records = runtime
            .store
            .load_trace()
            .await
            .map_err(|error| (INTERNAL_ERROR, format!("{error:#}")))?;
        Ok(json!({
            "session_id": session_id,
            "records": records,
        }))
    }

    async fn session_infos(&self) -> Result<Vec<crate::session::SessionInfo>, (i64, String)> {
        let mut sessions = self
            .session
            .list_sessions()
            .await
            .map_err(|error| (INTERNAL_ERROR, format!("{error:#}")))?;
        let current_id = self.legacy_session_id.lock().await.clone();
        for session in &mut sessions {
            session.active = session.id == current_id;
        }
        let active_counts = self.active.lock().await.keys().fold(
            HashMap::<String, usize>::new(),
            |mut counts, key| {
                *counts.entry(key.session_id.clone()).or_default() += 1;
                counts
            },
        );
        let pending_sessions = self.approvals.pending_sessions().await;
        for session in &mut sessions {
            let active_requests = active_counts.get(&session.id).copied().unwrap_or(0);
            session.active_requests = active_requests;
            session.status = if pending_sessions.contains(&session.id) {
                SessionStatus::Waiting
            } else if active_requests > 0 {
                SessionStatus::Running
            } else {
                SessionStatus::Idle
            };
            session.updated_at = session.modified_at;
        }
        let runtimes = self
            .sessions
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for runtime in runtimes {
            if sessions.iter().any(|session| session.id == runtime.id) {
                continue;
            }
            sessions.push(crate::session::SessionInfo {
                id: runtime.id.clone(),
                path: runtime.store.path_for_session(&runtime.id),
                active: runtime.id == current_id,
                message_count: 0,
                modified_at: None,
                preview: None,
                status: if pending_sessions.contains(&runtime.id) {
                    SessionStatus::Waiting
                } else if active_counts.get(&runtime.id).copied().unwrap_or(0) > 0 {
                    SessionStatus::Running
                } else {
                    SessionStatus::Idle
                },
                active_requests: active_counts.get(&runtime.id).copied().unwrap_or(0),
                updated_at: None,
            });
        }
        if !sessions.iter().any(|session| session.id == current_id) {
            let active_requests = active_counts.get(&current_id).copied().unwrap_or(0);
            sessions.push(crate::session::SessionInfo {
                id: current_id.clone(),
                path: self.session.path_for_session(&current_id),
                active: true,
                message_count: 0,
                modified_at: None,
                preview: None,
                status: if pending_sessions.contains(&current_id) {
                    SessionStatus::Waiting
                } else if active_requests > 0 {
                    SessionStatus::Running
                } else {
                    SessionStatus::Idle
                },
                active_requests,
                updated_at: None,
            });
        }
        sessions.sort_by(|left, right| {
            right
                .active
                .cmp(&left.active)
                .then_with(|| right.modified_at.cmp(&left.modified_at))
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(sessions)
    }

    async fn session_resume(&self, session_id: &str) -> Result<Value, (i64, String)> {
        self.session_runtime(Some(session_id)).await?;
        let mut snapshot = self.session_snapshot(session_id).await?;
        snapshot["resumed"] = json!(true);
        Ok(snapshot)
    }

    async fn execute_slash(
        &self,
        line: &str,
        session_id: Option<&str>,
    ) -> Result<Value, (i64, String)> {
        let registry = SlashRegistry::builtin();
        let response = match registry.parse(line) {
            SlashParse::NotCommand => SlashResponse::Text {
                content: "输入不是 slash 命令".to_owned(),
            },
            SlashParse::Error(content) => SlashResponse::Text { content },
            SlashParse::Command(invocation) => match invocation.action {
                SlashAction::Help => SlashResponse::Text {
                    content: registry.help(),
                },
                SlashAction::Status => {
                    let current_session = self.session_runtime(session_id).await?;
                    let snapshot = self.session_snapshot(&current_session.id).await?;
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
                    sessions: self.session_infos().await?,
                    select: false,
                },
                SlashAction::Resume if invocation.args.is_empty() => SlashResponse::Sessions {
                    sessions: self
                        .session_infos()
                        .await?
                        .into_iter()
                        .filter(|session| session.message_count > 0)
                        .collect(),
                    select: true,
                },
                SlashAction::Resume => {
                    let sessions = self
                        .session_infos()
                        .await?
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
                SlashAction::Dogfood => self.execute_dogfood(session_id).await?,
                SlashAction::Web => SlashResponse::Text {
                    content: "请在 TUI 中使用 /web，或运行 `my-agent serve`。".to_owned(),
                },
                SlashAction::Exit => SlashResponse::Exit,
            },
        };
        serde_json::to_value(response)
            .map_err(|error| (INTERNAL_ERROR, format!("序列化 slash 响应失败：{error}")))
    }

    async fn execute_dogfood(
        &self,
        session_id: Option<&str>,
    ) -> Result<SlashResponse, (i64, String)> {
        let runtime = self.session_runtime(session_id).await?;
        let session_path = runtime.store.path_for_session(&runtime.id);
        let dogfood_path = export_dogfood_file(&runtime.id, &session_path, &self.daemon_log_path)
            .await
            .map_err(|error| (INTERNAL_ERROR, format!("生成 dogfood 日志失败：{error:#}")))?;
        info!(
            session_id = %runtime.id,
            dogfood_path = %dogfood_path.display(),
            "已导出 dogfood 日志"
        );
        Ok(SlashResponse::Text {
            content: format!("dogfood 已生成：{}", dogfood_path.display()),
        })
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
            .iter()
            .find(|(key, _)| {
                key.request_id == params.request_id
                    && params
                        .session_id
                        .as_deref()
                        .is_none_or(|session_id| key.session_id == session_id)
            })
            .map(|(_, active)| active.subscribe())
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
        active_key: &ActiveKey,
        update: ActiveRequestUpdate,
    ) {
        if let Some(active) = self.active.lock().await.get_mut(active_key) {
            active.publish(update.clone());
        }
        let _ = frames.send(update.to_frame(active_key.request_id.clone()));
    }

    async fn cancel(
        &self,
        request_id: &RequestId,
        session_id: Option<&str>,
    ) -> Result<Value, (i64, String)> {
        let matching_key = self
            .active
            .lock()
            .await
            .keys()
            .find(|key| {
                &key.request_id == request_id
                    && session_id.is_none_or(|session_id| key.session_id == session_id)
            })
            .cloned();
        let Some(matching_key) = matching_key else {
            return Ok(json!({"cancelled": false, "reason": "请求未在执行"}));
        };
        let token = self
            .active
            .lock()
            .await
            .get(&matching_key)
            .map(|active| active.cancellation.clone());
        let Some(token) = token else {
            return Ok(json!({"cancelled": false, "reason": "请求未在执行"}));
        };
        token.cancel();
        if let Some(session_id) = session_id {
            self.approvals
                .cancel_request_in_session(session_id, request_id)
                .await;
        } else {
            self.approvals.cancel_request(request_id).await;
        }
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

async fn export_dogfood_file(
    session_id: &str,
    session_path: &Path,
    daemon_log_path: &Path,
) -> anyhow::Result<PathBuf> {
    let session_bytes = match tokio::fs::read(session_path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("读取 session 文件失败: {}", session_path.display()));
        }
    };
    let daemon_bytes = match tokio::fs::read(daemon_log_path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("读取 daemon 日志失败: {}", daemon_log_path.display()));
        }
    };
    let session_content = String::from_utf8_lossy(&session_bytes);
    let daemon_content = String::from_utf8_lossy(&daemon_bytes);
    let session_marker = format!("session_id={session_id}");
    let quoted_session_marker = format!("session_id=\"{session_id}\"");
    let chain_logs = daemon_content
        .lines()
        // tracing-subscriber may decorate field names and `=` separately, for example
        // `\x1b[3msession_id\x1b[0m\x1b[2m=\x1b[0msession-...`. Match against a
        // plain-text copy so dogfood exports work whether ANSI colors are enabled or not.
        .map(strip_ansi_sequences)
        .filter(|line| line.contains(&session_marker) || line.contains(&quoted_session_marker))
        .collect::<Vec<_>>()
        .join("\n");
    let message_count = session_content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    let chain_log_count = chain_logs.lines().count();
    let generated_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("系统时间早于 UNIX_EPOCH")?
        .as_millis();
    let session_directory = session_path.parent().unwrap_or_else(|| Path::new("."));
    tokio::fs::create_dir_all(session_directory)
        .await
        .with_context(|| format!("创建 session 目录失败: {}", session_directory.display()))?;
    let session_stem = session_path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("session");
    let dogfood_path = session_directory.join(format!("dogfood-{session_stem}.log"));
    let mut output = format!(
        "# my-agent dogfood export\n\
schema_version=1\n\
generated_at_unix_ms={generated_at_ms}\n\
session_id={session_id}\n\
session_file={}\n\
daemon_log_file={}\n\
conversation_messages={message_count}\n\
chain_log_lines={chain_log_count}\n\n\
===== LLM / REACT CONVERSATION (RAW SESSION JSONL) =====\n",
        session_path.display(),
        daemon_log_path.display(),
    );
    if session_content.is_empty() {
        output.push_str("(当前 session 尚无持久化消息)\n");
    } else {
        output.push_str(&session_content);
        if !session_content.ends_with('\n') {
            output.push('\n');
        }
    }
    output.push_str("\n===== DAEMON CHAIN LOG (CURRENT SESSION ONLY) =====\n");
    if chain_logs.is_empty() {
        output.push_str("(未找到带当前 session_id 的 daemon 链路日志)\n");
    } else {
        output.push_str(&chain_logs);
        output.push('\n');
    }
    tokio::fs::write(&dogfood_path, output)
        .await
        .with_context(|| format!("写入 dogfood 文件失败: {}", dogfood_path.display()))?;
    tokio::fs::canonicalize(&dogfood_path)
        .await
        .with_context(|| format!("解析 dogfood 文件路径失败: {}", dogfood_path.display()))
}

fn strip_ansi_sequences(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = String::with_capacity(input.len());
    let mut cursor = 0;
    let mut plain_start = 0;

    while cursor < bytes.len() {
        if bytes[cursor] == 0x1b && bytes.get(cursor + 1) == Some(&b'[') {
            output.push_str(&input[plain_start..cursor]);
            cursor += 2;
            while cursor < bytes.len() {
                let byte = bytes[cursor];
                cursor += 1;
                if (0x40..=0x7e).contains(&byte) {
                    break;
                }
            }
            plain_start = cursor;
        } else {
            cursor += 1;
        }
    }
    output.push_str(&input[plain_start..]);
    output
}

fn parse_params<T: for<'de> Deserialize<'de>>(params: &Value) -> Result<T, String> {
    serde_json::from_value(params.clone()).map_err(|error| format!("参数无效: {error}"))
}

fn request_id_label(request_id: &RequestId) -> String {
    match request_id {
        RequestId::Number(value) => value.to_string(),
        RequestId::String(value) => value.clone(),
    }
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
        AgentEvent::ToolStarted {
            call_id,
            name,
            round,
        } => (
            EventKind::ToolStarted,
            json!({"tool_call_id": call_id, "name": name, "round": round}),
        ),
        AgentEvent::ToolFinished {
            call_id,
            name,
            output,
            round,
            duration_ms,
            success,
            error,
        } => (
            EventKind::ToolFinished,
            json!({
                "tool_call_id": call_id,
                "name": name,
                "output": output,
                "round": round,
                "duration_ms": duration_ms,
                "success": success,
                "error": error,
            }),
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

#[cfg(test)]
mod dogfood_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::export_dogfood_file;

    static NEXT_TEST: AtomicUsize = AtomicUsize::new(0);

    #[tokio::test]
    async fn exports_full_conversation_and_only_current_session_chain_logs() {
        let id = NEXT_TEST.fetch_add(1, Ordering::SeqCst);
        let directory =
            std::env::temp_dir().join(format!("my-agent-dogfood-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let session_id = "session-42.jsonl";
        let session_path = directory.join(session_id);
        let daemon_log_path = directory.join("daemon.log");
        let conversation = concat!(
            "{\"role\":\"user\",\"content\":\"检查项目\"}\n",
            "{\"role\":\"assistant\",\"tool_calls\":[{\"name\":\"exec\",\"arguments\":{\"command\":\"pwd\"}}]}\n",
            "{\"role\":\"tool\",\"content\":\"/workspace\",\"name\":\"exec\"}\n",
            "{\"role\":\"assistant\",\"content\":\"完成\"}\n",
        );
        std::fs::write(&session_path, conversation).unwrap();
        std::fs::write(
            &daemon_log_path,
            concat!(
                "\x1b[32m INFO\x1b[0m agent_turn{\x1b[3msession_id\x1b[0m\x1b[2m=\x1b[0msession-42.jsonl request_id=Number(7)}: 开始 ReAct 轮次\n",
                "INFO agent_turn{session_id=other.jsonl request_id=Number(8)}: 其他会话\n",
                "INFO session_id=session-42.jsonl request_id=Number(7): chat 请求结束\n",
            ),
        )
        .unwrap();

        let exported = export_dogfood_file(session_id, &session_path, &daemon_log_path)
            .await
            .unwrap();
        let content = std::fs::read_to_string(&exported).unwrap();

        assert_eq!(
            exported.file_name().and_then(|value| value.to_str()),
            Some("dogfood-session-42.log")
        );
        assert!(content.contains("conversation_messages=4"));
        assert!(content.contains("chain_log_lines=2"));
        assert!(content.contains(conversation));
        assert!(content.contains("request_id=Number(7)"));
        assert!(!content.contains("其他会话"));
        assert!(!content.contains('\x1b'));

        std::fs::remove_dir_all(directory).unwrap();
    }
}
