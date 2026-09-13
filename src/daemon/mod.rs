pub mod approval;
pub mod handlers;
pub mod lifecycle;
pub mod protocol;
pub mod runtime;
pub mod server;

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::{Mutex, broadcast};

use self::approval::ApprovalBroker;
use self::protocol::{EventFrame, EventKind, JsonRpcResponse, RequestId, ServerFrame};
use crate::cron::CronManager;
use crate::loop_engine::{CancellationToken, LoopEngine};
use crate::mcp::McpManager;
use crate::provider::Message;
use crate::safety::SafetyPolicy;
use crate::session::SessionStore;
use crate::skills::SkillLibrary;

pub struct DaemonState {
    pub(crate) session: Arc<SessionStore>,
    pub(crate) legacy_session_id: Mutex<String>,
    pub(crate) default_session: Arc<SessionRuntime>,
    pub(crate) sessions: Mutex<HashMap<String, Arc<SessionRuntime>>>,
    pub(crate) approvals: ApprovalBroker,
    pub(crate) safety: Option<Arc<SafetyPolicy>>,
    pub(crate) active: Mutex<HashMap<ActiveKey, ActiveRequest>>,
    pub(crate) shutdown: CancellationToken,
    pub(crate) skills: Option<SkillLibrary>,
    pub(crate) cron: Option<Arc<CronManager>>,
    pub(crate) mcp: Option<Arc<McpManager>>,
    pub(crate) daemon_log_path: PathBuf,
}

pub(crate) struct SessionRuntime {
    pub(crate) id: String,
    pub(crate) engine: Arc<LoopEngine>,
    pub(crate) history: Mutex<Vec<Message>>,
    pub(crate) store: Arc<SessionStore>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ActiveKey {
    pub(crate) session_id: String,
    pub(crate) request_id: RequestId,
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
    #[cfg(test)]
    pub fn new(
        engine: Arc<LoopEngine>,
        history: Vec<Message>,
        session: Arc<SessionStore>,
        approvals: ApprovalBroker,
    ) -> Self {
        Self::new_with_skills(engine, history, session, approvals, None)
    }

    #[cfg(test)]
    pub fn new_with_skills(
        engine: Arc<LoopEngine>,
        history: Vec<Message>,
        session: Arc<SessionStore>,
        approvals: ApprovalBroker,
        skills: Option<SkillLibrary>,
    ) -> Self {
        Self::new_with_services(engine, history, session, approvals, skills, None, None)
    }

    #[cfg(test)]
    pub fn new_with_services(
        engine: Arc<LoopEngine>,
        history: Vec<Message>,
        session: Arc<SessionStore>,
        approvals: ApprovalBroker,
        skills: Option<SkillLibrary>,
        cron: Option<Arc<CronManager>>,
        mcp: Option<Arc<McpManager>>,
    ) -> Self {
        let daemon_log_path = session
            .path_for_session(&session.current_id_sync())
            .with_file_name("daemon.log");
        Self::new_with_services_and_log_path(
            engine,
            history,
            session,
            approvals,
            skills,
            cron,
            mcp,
            daemon_log_path,
        )
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_services_and_log_path(
        engine: Arc<LoopEngine>,
        history: Vec<Message>,
        session: Arc<SessionStore>,
        approvals: ApprovalBroker,
        skills: Option<SkillLibrary>,
        cron: Option<Arc<CronManager>>,
        mcp: Option<Arc<McpManager>>,
        daemon_log_path: PathBuf,
    ) -> Self {
        Self::new_with_services_and_log_path_and_safety(
            engine,
            history,
            session,
            approvals,
            skills,
            cron,
            mcp,
            daemon_log_path,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_services_and_log_path_and_safety(
        engine: Arc<LoopEngine>,
        history: Vec<Message>,
        session: Arc<SessionStore>,
        approvals: ApprovalBroker,
        skills: Option<SkillLibrary>,
        cron: Option<Arc<CronManager>>,
        mcp: Option<Arc<McpManager>>,
        daemon_log_path: PathBuf,
        safety: Option<Arc<SafetyPolicy>>,
    ) -> Self {
        let default_session_id = session.current_id_sync();
        let default_session = Arc::new(SessionRuntime {
            id: default_session_id.clone(),
            engine,
            history: Mutex::new(history),
            store: session.clone(),
        });
        Self {
            session,
            legacy_session_id: Mutex::new(default_session_id),
            default_session,
            sessions: Mutex::new(HashMap::new()),
            approvals,
            safety,
            active: Mutex::new(HashMap::new()),
            shutdown: CancellationToken::new(),
            skills,
            cron,
            mcp,
            daemon_log_path,
        }
    }

    pub async fn has_active_turns(&self) -> bool {
        !self.active.lock().await.is_empty()
    }

    pub async fn has_persistent_background_work(&self) -> bool {
        match &self.cron {
            Some(cron) => cron.keeps_daemon_alive().await,
            None => false,
        }
    }

    pub async fn join_background(&self) {
        if let Some(cron) = &self.cron {
            cron.join().await;
        }
        if let Some(mcp) = &self.mcp {
            mcp.shutdown().await;
        }
    }
}
