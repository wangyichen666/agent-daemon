pub mod approval;
pub mod handlers;
pub mod lifecycle;
pub mod protocol;
pub mod runtime;
pub mod server;

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use self::approval::ApprovalBroker;
use self::protocol::RequestId;
use crate::loop_engine::{CancellationToken, LoopEngine};
use crate::provider::Message;
use crate::session::SessionStore;

pub struct DaemonState {
    pub(crate) engine: Arc<LoopEngine>,
    pub(crate) history: Mutex<Vec<Message>>,
    pub(crate) session: Arc<SessionStore>,
    pub(crate) approvals: ApprovalBroker,
    pub(crate) active: Mutex<HashMap<RequestId, CancellationToken>>,
    pub(crate) shutdown: CancellationToken,
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
