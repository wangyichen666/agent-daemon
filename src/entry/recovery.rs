use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::client::{DaemonClient, RpcStream};
use crate::daemon::approval::PendingApprovalInfo;
use crate::daemon::protocol::RequestId;
use crate::provider::Message;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RecoverySnapshot {
    pub session_id: String,
    pub messages: Vec<Message>,
    pub pending_approvals: Vec<PendingApprovalInfo>,
    pub active_requests: Vec<RequestId>,
}

pub async fn load_snapshot(client: &DaemonClient) -> Result<RecoverySnapshot> {
    let value = crate::entry::cli::request_result(client, "session.load", json!({})).await?;
    serde_json::from_value(value).context("daemon session.load 恢复快照格式无效")
}

pub async fn respond_to_approval(
    client: &DaemonClient,
    approval_id: &str,
    approved: bool,
) -> Result<()> {
    crate::entry::cli::request_result(
        client,
        "approval.respond",
        json!({"approval_id": approval_id, "approved": approved}),
    )
    .await
    .map(|_| ())
}

pub async fn subscribe(client: &DaemonClient, request_id: &RequestId) -> Result<RpcStream> {
    client
        .request("agent.subscribe", json!({"request_id": request_id}))
        .await
}
