use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

use super::DaemonState;
use super::lifecycle::RuntimePaths;
#[cfg(test)]
use super::protocol::JsonRpcRequest;
use super::protocol::{
    JsonRpcResponse, MAX_FRAME_BYTES, RequestId, ServerFrame, decode_request, encode_frame,
    server_frame_request_id,
};
#[cfg(test)]
use crate::client::DaemonClient;

#[cfg(test)]
pub(crate) struct InMemoryEnvelope {
    pub request: JsonRpcRequest,
    pub frames: mpsc::UnboundedSender<ServerFrame>,
}

#[cfg(test)]
pub struct InMemoryServer;

#[cfg(test)]
impl InMemoryServer {
    pub fn start(state: Arc<DaemonState>) -> DaemonClient {
        let (requests, mut receiver) = mpsc::channel::<InMemoryEnvelope>(64);
        tokio::spawn(async move {
            while let Some(envelope) = receiver.recv().await {
                let state = state.clone();
                tokio::spawn(async move {
                    state
                        .handle_request(envelope.request, envelope.frames)
                        .await;
                });
            }
        });
        DaemonClient::in_memory(requests)
    }
}

pub async fn run_unix_server(
    state: Arc<DaemonState>,
    paths: &RuntimePaths,
    workspace: &std::path::Path,
) -> Result<()> {
    paths.prepare().await?;
    if paths.socket.exists() {
        tokio::fs::remove_file(&paths.socket)
            .await
            .with_context(|| format!("清理旧 socket 失败: {}", paths.socket.display()))?;
    }
    let listener = UnixListener::bind(&paths.socket)
        .with_context(|| format!("监听 Unix socket 失败: {}", paths.socket.display()))?;
    paths.mark_ready(workspace).await?;
    let (connection_done, mut done_receiver) = mpsc::unbounded_channel::<()>();
    let mut clients = 0usize;
    let mut accepted_any = false;
    let mut idle_since = None;
    let mut lifecycle_tick = tokio::time::interval(Duration::from_millis(250));
    let interrupt = tokio::signal::ctrl_c();
    tokio::pin!(interrupt);

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.context("接受 daemon 客户端连接失败")?;
                clients = clients.saturating_add(1);
                accepted_any = true;
                idle_since = None;
                let state = state.clone();
                let connection_done = connection_done.clone();
                tokio::spawn(async move {
                    if let Err(error) = serve_unix_connection(stream, state).await {
                        tracing::warn!(%error, "daemon 客户端连接异常结束");
                    }
                    let _ = connection_done.send(());
                });
            }
            Some(()) = done_receiver.recv() => {
                clients = clients.saturating_sub(1);
                if clients == 0 {
                    idle_since = Some(tokio::time::Instant::now());
                }
            }
            _ = lifecycle_tick.tick() => {
                if accepted_any
                    && clients == 0
                    && !state.has_active_turns().await
                    && idle_since.is_some_and(|since| since.elapsed() >= Duration::from_secs(2))
                {
                    break;
                }
            }
            _ = state.shutdown.cancelled() => break,
            result = &mut interrupt => {
                result.context("监听 daemon Ctrl-C 失败")?;
                break;
            }
        }
    }

    while state.has_active_turns().await {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    paths.cleanup().await;
    Ok(())
}

async fn serve_unix_connection(stream: UnixStream, state: Arc<DaemonState>) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let (frames, mut frame_receiver) = mpsc::unbounded_channel::<ServerFrame>();
    let writer_task = tokio::spawn(async move {
        while let Some(frame) = frame_receiver.recv().await {
            let encoded = match encode_frame(&frame) {
                Ok(encoded) => encoded,
                Err(error) => {
                    let fallback = ServerFrame::Response(JsonRpcResponse::failure(
                        server_frame_request_id(&frame).clone(),
                        -32002,
                        error.to_string(),
                    ));
                    encode_frame(&fallback).context("编码超限错误响应失败")?
                }
            };
            writer
                .write_all(&encoded)
                .await
                .context("写入 daemon 响应失败")?;
            writer.flush().await.context("刷新 daemon 响应失败")?;
        }
        Ok::<(), anyhow::Error>(())
    });

    let mut reader = BufReader::new(reader);
    loop {
        let mut line = Vec::new();
        let read = reader
            .read_until(b'\n', &mut line)
            .await
            .context("读取 daemon 请求失败")?;
        if read == 0 {
            break;
        }
        if line.len() > MAX_FRAME_BYTES + 1 {
            let _ = frames.send(ServerFrame::Response(JsonRpcResponse::failure(
                RequestId::String("protocol".to_owned()),
                -32002,
                format!("协议帧超过 {MAX_FRAME_BYTES} 字节限制"),
            )));
            continue;
        }
        let request = match decode_request(&line) {
            Ok(request) => request,
            Err(error) => {
                let _ = frames.send(ServerFrame::Response(JsonRpcResponse::failure(
                    RequestId::String("protocol".to_owned()),
                    -32700,
                    error.to_string(),
                )));
                continue;
            }
        };
        let request_frames = frames.clone();
        let state = state.clone();
        tokio::spawn(async move {
            state.handle_request(request, request_frames).await;
        });
    }
    drop(frames);
    writer_task.await.context("daemon 响应写入任务异常终止")??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use anyhow::Result;
    use async_trait::async_trait;
    use serde_json::json;

    use super::*;
    use crate::context::{ContextConfig, ContextManager};
    use crate::daemon::approval::ApprovalBroker;
    use crate::daemon::protocol::{EventKind, JsonRpcResponse, RequestId};
    use crate::loop_engine::LoopEngine;
    use crate::plan::PlanStore;
    use crate::provider::{Message, Provider, Response, ToolSpec};
    use crate::session::SessionStore;
    use crate::tools::ToolRegistry;

    static NEXT_TEST: AtomicUsize = AtomicUsize::new(0);

    struct MockProvider {
        responses: Mutex<VecDeque<Response>>,
    }

    struct PendingProvider;

    #[async_trait]
    impl Provider for MockProvider {
        async fn chat(&self, _messages: &[Message], _tools: &[ToolSpec]) -> Result<Response> {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("mock 响应不足"))
        }
    }

    #[async_trait]
    impl Provider for PendingProvider {
        async fn chat(&self, _messages: &[Message], _tools: &[ToolSpec]) -> Result<Response> {
            std::future::pending().await
        }
    }

    async fn state_with_provider(
        provider: Arc<dyn Provider>,
    ) -> (Arc<DaemonState>, std::path::PathBuf) {
        let id = NEXT_TEST.fetch_add(1, Ordering::SeqCst);
        let session_path =
            std::env::temp_dir().join(format!("my-agent-daemon-{}-{id}.jsonl", std::process::id()));
        let session = Arc::new(SessionStore::new(&session_path));
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
        (
            Arc::new(DaemonState::new(
                engine,
                Vec::new(),
                session,
                ApprovalBroker::new(),
            )),
            session_path,
        )
    }

    async fn test_state() -> (Arc<DaemonState>, std::path::PathBuf) {
        state_with_provider(Arc::new(MockProvider {
            responses: Mutex::new(VecDeque::from([Response::Text("回环回答".to_owned())])),
        }))
        .await
    }

    #[tokio::test]
    async fn streams_chat_and_exposes_same_session_through_rpc() {
        let (state, session_path) = test_state().await;
        let client = InMemoryServer::start(state);
        let mut chat = client
            .request("chat.send", json!({"message": "你好"}))
            .await
            .unwrap();
        let mut events = Vec::new();
        let response = loop {
            match chat.next().await.unwrap() {
                ServerFrame::Event(event) => events.push(event.event),
                ServerFrame::Response(response) => break response,
            }
        };

        assert_eq!(response.result.unwrap()["content"], "回环回答");
        assert_eq!(
            events,
            [
                EventKind::TurnStarted,
                EventKind::TextDelta,
                EventKind::TurnCompleted,
            ]
        );

        let mut load = client.request("session.load", json!({})).await.unwrap();
        let ServerFrame::Response(JsonRpcResponse { result, .. }) = load.next().await.unwrap()
        else {
            panic!("预期 session.load 响应");
        };
        assert_eq!(result.unwrap()["messages"].as_array().unwrap().len(), 2);

        let mut list = client.request("session.list", json!({})).await.unwrap();
        let ServerFrame::Response(JsonRpcResponse { result, .. }) = list.next().await.unwrap()
        else {
            panic!("预期 session.list 响应");
        };
        let sessions = result.unwrap();
        assert_eq!(sessions["sessions"][0]["active"], true);
        assert_eq!(sessions["sessions"][0]["message_count"], 2);
        let _ = std::fs::remove_file(session_path);
    }

    #[tokio::test]
    async fn cancels_an_active_turn_by_request_id() {
        let (state, session_path) = state_with_provider(Arc::new(PendingProvider)).await;
        let client = InMemoryServer::start(state);
        let turn_id = RequestId::String("slow-turn".to_owned());
        let mut chat = client
            .request_with_id(turn_id.clone(), "chat.send", json!({"message": "等待"}))
            .await
            .unwrap();
        let started = tokio::time::timeout(std::time::Duration::from_secs(1), chat.next())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            started,
            ServerFrame::Event(event) if event.event == EventKind::TurnStarted
        ));

        let mut cancel = client
            .request("agent.cancel", json!({"request_id": turn_id}))
            .await
            .unwrap();
        let ServerFrame::Response(cancelled) = cancel.next().await.unwrap() else {
            panic!("预期取消响应");
        };
        assert_eq!(cancelled.result.unwrap()["cancelled"], true);

        let terminal = tokio::time::timeout(std::time::Duration::from_secs(1), chat.next())
            .await
            .unwrap()
            .unwrap();
        let ServerFrame::Response(response) = terminal else {
            panic!("预期取消终态响应");
        };
        assert_eq!(response.error.unwrap().code, -32800);
        let _ = std::fs::remove_file(session_path);
    }

    #[tokio::test]
    async fn unix_socket_uses_the_same_handlers_and_stops_cleanly() {
        let (state, session_path) = test_state().await;
        let runtime_directory = std::env::temp_dir().join(format!(
            "my-agent-unix-{}-{}",
            std::process::id(),
            NEXT_TEST.fetch_add(1, Ordering::SeqCst)
        ));
        let paths = RuntimePaths::for_test(runtime_directory.clone());
        let server_paths = paths.clone();
        let workspace = std::env::current_dir().unwrap();
        let server =
            tokio::spawn(async move { run_unix_server(state, &server_paths, &workspace).await });
        for _ in 0..100 {
            if paths.socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let client = DaemonClient::connect_unix(&paths.socket).await.unwrap();
        let mut chat = client
            .request("chat.send", json!({"message": "你好"}))
            .await
            .unwrap();
        loop {
            if matches!(chat.next().await.unwrap(), ServerFrame::Response(_)) {
                break;
            }
        }
        let mut stop = client.request("daemon.stop", json!({})).await.unwrap();
        assert!(matches!(
            stop.next().await.unwrap(),
            ServerFrame::Response(JsonRpcResponse { error: None, .. })
        ));
        drop(client);
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        let _ = std::fs::remove_file(session_path);
        let _ = std::fs::remove_dir_all(runtime_directory);
    }
}
