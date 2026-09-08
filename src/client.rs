use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{Mutex, mpsc};

use crate::daemon::protocol::{
    JsonRpcRequest, JsonRpcResponse, RequestId, ServerFrame, decode_request, decode_server_frame,
    encode_frame, server_frame_request_id,
};
#[cfg(test)]
use crate::daemon::server::InMemoryEnvelope;

#[derive(Clone)]
pub struct DaemonClient {
    inner: Arc<DaemonClientInner>,
}

struct DaemonClientInner {
    transport: ClientTransport,
    next_id: AtomicU64,
}

enum ClientTransport {
    #[cfg(test)]
    InMemory(mpsc::Sender<InMemoryEnvelope>),
    Unix {
        requests: mpsc::Sender<JsonRpcRequest>,
        pending: Arc<Mutex<HashMap<RequestId, mpsc::UnboundedSender<ServerFrame>>>>,
    },
}

impl DaemonClient {
    #[cfg(test)]
    pub(crate) fn in_memory(requests: mpsc::Sender<InMemoryEnvelope>) -> Self {
        Self {
            inner: Arc::new(DaemonClientInner {
                transport: ClientTransport::InMemory(requests),
                next_id: AtomicU64::new(1),
            }),
        }
    }

    pub async fn connect_unix(socket: &Path) -> Result<Self> {
        let stream = UnixStream::connect(socket)
            .await
            .with_context(|| format!("连接 daemon 失败: {}", socket.display()))?;
        let (reader, mut writer) = stream.into_split();
        let (requests, mut request_receiver) = mpsc::channel::<JsonRpcRequest>(64);
        let pending = Arc::new(Mutex::new(HashMap::<
            RequestId,
            mpsc::UnboundedSender<ServerFrame>,
        >::new()));
        let reader_pending = pending.clone();
        let writer_pending = pending.clone();

        tokio::spawn(async move {
            while let Some(request) = request_receiver.recv().await {
                let encoded = match encode_frame(&request) {
                    Ok(encoded) => encoded,
                    Err(error) => {
                        fail_pending(&writer_pending, &request.id, -32002, error.to_string()).await;
                        continue;
                    }
                };
                if let Err(error) = writer.write_all(&encoded).await {
                    fail_all_pending(&writer_pending, format!("写入 daemon 请求失败: {error}"))
                        .await;
                    return;
                }
                if let Err(error) = writer.flush().await {
                    fail_all_pending(&writer_pending, format!("刷新 daemon 请求失败: {error}"))
                        .await;
                    return;
                }
            }
        });

        tokio::spawn(async move {
            let mut reader = BufReader::new(reader);
            loop {
                let mut line = Vec::new();
                match reader.read_until(b'\n', &mut line).await {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(error) => {
                        fail_all_pending(&reader_pending, format!("读取 daemon 响应失败: {error}"))
                            .await;
                        return;
                    }
                }
                let frame = match decode_server_frame(&line) {
                    Ok(frame) => frame,
                    Err(error) => {
                        fail_all_pending(&reader_pending, error.to_string()).await;
                        return;
                    }
                };
                let id = server_frame_request_id(&frame).clone();
                let is_terminal = matches!(frame, ServerFrame::Response(_));
                let destination = reader_pending.lock().await.get(&id).cloned();
                if let Some(destination) = destination {
                    let _ = destination.send(frame);
                }
                if is_terminal {
                    reader_pending.lock().await.remove(&id);
                }
            }
            fail_all_pending(&reader_pending, "daemon 连接已关闭".to_owned()).await;
        });

        Ok(Self {
            inner: Arc::new(DaemonClientInner {
                transport: ClientTransport::Unix { requests, pending },
                next_id: AtomicU64::new(1),
            }),
        })
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<RpcStream> {
        let id = RequestId::Number(self.inner.next_id.fetch_add(1, Ordering::Relaxed));
        self.request_with_id(id, method, params).await
    }

    pub async fn request_with_id(
        &self,
        id: RequestId,
        method: &str,
        params: Value,
    ) -> Result<RpcStream> {
        let (frames, receiver) = mpsc::unbounded_channel();
        let request = JsonRpcRequest::new(id.clone(), method, params);
        let encoded = encode_frame(&request).context("编码 daemon 请求失败")?;
        let request = decode_request(&encoded).context("校验 daemon 请求失败")?;
        match &self.inner.transport {
            #[cfg(test)]
            ClientTransport::InMemory(requests) => {
                requests
                    .send(InMemoryEnvelope { request, frames })
                    .await
                    .context("daemon 内存传输已关闭")?;
            }
            ClientTransport::Unix { requests, pending } => {
                let mut destinations = pending.lock().await;
                if destinations.contains_key(&id) {
                    bail!("请求 id 已在等待响应");
                }
                destinations.insert(id.clone(), frames);
                drop(destinations);
                if requests.send(request).await.is_err() {
                    pending.lock().await.remove(&id);
                    bail!("daemon Unix 传输已关闭");
                }
            }
        }
        Ok(RpcStream { id, receiver })
    }
}

async fn fail_pending(
    pending: &Mutex<HashMap<RequestId, mpsc::UnboundedSender<ServerFrame>>>,
    id: &RequestId,
    code: i64,
    message: String,
) {
    if let Some(destination) = pending.lock().await.remove(id) {
        let _ = destination.send(ServerFrame::Response(JsonRpcResponse::failure(
            id.clone(),
            code,
            message,
        )));
    }
}

async fn fail_all_pending(
    pending: &Mutex<HashMap<RequestId, mpsc::UnboundedSender<ServerFrame>>>,
    message: String,
) {
    let destinations = std::mem::take(&mut *pending.lock().await);
    for (id, destination) in destinations {
        let _ = destination.send(ServerFrame::Response(JsonRpcResponse::failure(
            id,
            -32000,
            message.clone(),
        )));
    }
}

pub struct RpcStream {
    id: RequestId,
    receiver: mpsc::UnboundedReceiver<ServerFrame>,
}

impl RpcStream {
    pub fn request_id(&self) -> &RequestId {
        &self.id
    }

    pub async fn next(&mut self) -> Option<ServerFrame> {
        self.receiver.recv().await
    }
}
