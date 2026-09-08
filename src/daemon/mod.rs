pub mod approval;
pub mod handlers;
pub mod lifecycle;
pub mod protocol;
pub mod runtime;
pub mod server;

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::{Mutex, broadcast};

use self::approval::ApprovalBroker;
use self::protocol::{EventFrame, EventKind, JsonRpcResponse, RequestId, ServerFrame};
use crate::loop_engine::{CancellationToken, LoopEngine};
use crate::provider::Message;
use crate::session::SessionStore;

pub struct DaemonState {
    pub(crate) engine: Arc<LoopEngine>,
    pub(crate) history: Mutex<Vec<Message>>,
    pub(crate) session: Arc<SessionStore>,
    pub(crate) approvals: ApprovalBroker,
    pub(crate) active: Mutex<HashMap<RequestId, ActiveRequest>>,
    pub(crate) shutdown: CancellationToken,
}

const ACTIVE_REPLAY_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug)]
pub(crate) enum ActiveRequestUpdate {
    Event { kind: EventKind, data: Value },
    Terminal(Result<Value, (i64, String)>),
}

impl ActiveRequestUpdate {
    pub(crate) fn to_frame(&self, request_id: RequestId) -> ServerFrame {
        match self {
            Self::Event { kind, data } => {
                ServerFrame::Event(EventFrame::new(request_id, kind.clone(), data.clone()))
            }
            Self::Terminal(Ok(result)) => {
                ServerFrame::Response(JsonRpcResponse::success(request_id, result.clone()))
            }
            Self::Terminal(Err((code, message))) => {
                ServerFrame::Response(JsonRpcResponse::failure(request_id, *code, message.clone()))
            }
        }
    }

    fn replay_bytes(&self) -> usize {
        match self {
            Self::Event { data, .. } => data.to_string().len().saturating_add(64),
            Self::Terminal(Ok(result)) => result.to_string().len().saturating_add(64),
            Self::Terminal(Err((_, message))) => message.len().saturating_add(64),
        }
    }
}

pub(crate) struct ActiveRequest {
    pub(crate) cancellation: CancellationToken,
    updates: broadcast::Sender<ActiveRequestUpdate>,
    replay: VecDeque<ActiveRequestUpdate>,
    replay_bytes: usize,
}

impl ActiveRequest {
    pub(crate) fn new(cancellation: CancellationToken) -> Self {
        let (updates, _) = broadcast::channel(1024);
        Self {
            cancellation,
            updates,
            replay: VecDeque::new(),
            replay_bytes: 0,
        }
    }

    pub(crate) fn publish(&mut self, update: ActiveRequestUpdate) {
        let bytes = update.replay_bytes();
        self.replay.push_back(update.clone());
        self.replay_bytes = self.replay_bytes.saturating_add(bytes);
        while self.replay_bytes > ACTIVE_REPLAY_BYTES {
            let Some(removed) = self.replay.pop_front() else {
                break;
            };
            self.replay_bytes = self.replay_bytes.saturating_sub(removed.replay_bytes());
        }
        let _ = self.updates.send(update);
    }

    pub(crate) fn subscribe(
        &self,
    ) -> (
        Vec<ActiveRequestUpdate>,
        broadcast::Receiver<ActiveRequestUpdate>,
    ) {
        (
            self.replay.iter().cloned().collect(),
            self.updates.subscribe(),
        )
    }
}

impl DaemonState {
    pub fn new(
        engine: Arc<LoopEngine>,
        history: Vec<Message>,
        session: Arc<SessionStore>,
        approvals: ApprovalBroker,
    ) -> Self {
        Self {
            engine,
            history: Mutex::new(history),
            session,
            approvals,
            active: Mutex::new(HashMap::new()),
            shutdown: CancellationToken::new(),
        }
    }

    pub async fn has_active_turns(&self) -> bool {
        !self.active.lock().await.is_empty()
    }
}
