use std::collections::HashSet;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::protocol::{EventKind, JsonRpcRequest, JsonRpcResponse, RequestId, ServerFrame};
use super::{ActiveRequest, ActiveRequestUpdate, DaemonState};
use crate::loop_engine::{AgentEvent, CancellationToken};

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
