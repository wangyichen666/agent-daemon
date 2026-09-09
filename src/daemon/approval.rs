use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{Mutex, mpsc, oneshot};

use super::protocol::{EventFrame, EventKind, RequestId, ServerFrame};
use crate::safety::Approval;

tokio::task_local! {
    static ACTIVE_APPROVAL_CONTEXT: ApprovalContext;
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PendingApprovalInfo {
    pub id: String,
    pub request_id: RequestId,
    pub prompt: String,
}

#[derive(Clone, Default)]
pub struct ApprovalBroker {
    inner: Arc<ApprovalBrokerInner>,
}

#[derive(Default)]
struct ApprovalBrokerInner {
    next_id: AtomicU64,
    pending: Mutex<HashMap<String, PendingApproval>>,
}

#[derive(Clone)]
struct ApprovalContext {
    request_id: RequestId,
    session_id: Option<String>,
    events: mpsc::UnboundedSender<ServerFrame>,
}

struct PendingApproval {
    info: PendingApprovalInfo,
    session_id: Option<String>,
    response: oneshot::Sender<bool>,
}

impl ApprovalBroker {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub async fn with_context<F>(
        &self,
        request_id: RequestId,
        events: mpsc::UnboundedSender<ServerFrame>,
        future: F,
    ) -> F::Output
    where
        F: Future,
    {
        ACTIVE_APPROVAL_CONTEXT
            .scope(
                ApprovalContext {
                    request_id,
                    session_id: None,
                    events,
                },
                future,
            )
            .await
    }

    pub async fn with_session_context<F>(
        &self,
        session_id: impl Into<String>,
        request_id: RequestId,
        events: mpsc::UnboundedSender<ServerFrame>,
        future: F,
    ) -> F::Output
    where
        F: Future,
    {
        ACTIVE_APPROVAL_CONTEXT
            .scope(
                ApprovalContext {
                    request_id,
                    session_id: Some(session_id.into()),
                    events,
                },
                future,
            )
            .await
    }

    pub async fn respond(&self, approval_id: &str, approved: bool) -> Result<()> {
        let Some(pending) = self.inner.pending.lock().await.remove(approval_id) else {
            bail!("找不到待审批项: {approval_id}");
        };
        pending
            .response
            .send(approved)
            .map_err(|_| anyhow::anyhow!("审批请求已结束: {approval_id}"))
    }

    pub async fn cancel_request(&self, request_id: &RequestId) {
        self.cancel_matching(None, request_id).await;
    }

    pub async fn cancel_request_in_session(&self, session_id: &str, request_id: &RequestId) {
        self.cancel_matching(Some(session_id), request_id).await;
    }

    async fn cancel_matching(&self, session_id: Option<&str>, request_id: &RequestId) {
        let mut pending = self.inner.pending.lock().await;
        let matching = pending
            .iter()
            .filter(|(_, value)| {
                &value.info.request_id == request_id
                    && session_id.is_none_or(|session| value.session_id.as_deref() == Some(session))
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<String>>();
        for id in matching {
            if let Some(approval) = pending.remove(&id) {
                let _ = approval.response.send(false);
            }
        }
    }

    pub async fn pending(&self) -> Vec<PendingApprovalInfo> {
        self.pending_for_session(None).await
    }

    pub async fn pending_for_session(&self, session_id: Option<&str>) -> Vec<PendingApprovalInfo> {
        let mut pending = self
            .inner
            .pending
            .lock()
            .await
            .values()
            .filter(|value| {
                session_id.is_none_or(|session| value.session_id.as_deref() == Some(session))
            })
            .map(|value| value.info.clone())
            .collect::<Vec<PendingApprovalInfo>>();
        pending.sort_by(|left, right| left.id.cmp(&right.id));
        pending
    }
}

#[async_trait]
impl Approval for ApprovalBroker {
    async fn request(&self, prompt: &str) -> Result<bool> {
        let context = ACTIVE_APPROVAL_CONTEXT
            .try_with(Clone::clone)
            .map_err(|_| anyhow::anyhow!("当前没有可接收审批事件的 daemon 请求"))?;
        let sequence = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let approval_id = format!("approval-{sequence}");
        let (response, receiver) = oneshot::channel();
        let info = PendingApprovalInfo {
            id: approval_id.clone(),
            request_id: context.request_id.clone(),
            prompt: prompt.to_owned(),
        };
        self.inner.pending.lock().await.insert(
            approval_id.clone(),
            PendingApproval {
                info: info.clone(),
                session_id: context.session_id,
                response,
            },
        );
        let frame = ServerFrame::Event(EventFrame::new(
            context.request_id,
            EventKind::ApprovalRequired,
            json!({"approval": info}),
        ));
        if context.events.send(frame).is_err() {
            self.inner.pending.lock().await.remove(&approval_id);
            bail!("审批事件接收端已断开");
        }

        match receiver.await {
            Ok(approved) => Ok(approved),
            Err(_) => {
                self.inner.pending.lock().await.remove(&approval_id);
                bail!("审批请求被取消: {approval_id}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn publishes_and_resolves_approval() {
        let broker = ApprovalBroker::new();
        let (events, mut receiver) = mpsc::unbounded_channel();
        let requesting = broker.clone();
        let task = tokio::spawn(async move {
            requesting
                .with_context(RequestId::Number(9), events, async {
                    requesting.request("执行危险操作").await
                })
                .await
        });

        let frame = receiver.recv().await.unwrap();
        let ServerFrame::Event(event) = frame else {
            panic!("预期审批事件");
        };
        let approval_id = event.data["approval"]["id"].as_str().unwrap();
        broker.respond(approval_id, true).await.unwrap();

        assert!(task.await.unwrap().unwrap());
        assert!(broker.pending().await.is_empty());
    }

    #[tokio::test]
    async fn concurrent_requests_keep_their_own_event_context() {
        let broker = ApprovalBroker::new();
        let (first_events, mut first_receiver) = mpsc::unbounded_channel();
        let (second_events, mut second_receiver) = mpsc::unbounded_channel();
        let first_broker = broker.clone();
        let first = tokio::spawn(async move {
            first_broker
                .with_context(RequestId::String("first".to_owned()), first_events, async {
                    first_broker.request("第一个审批").await
                })
                .await
        });
        let second_broker = broker.clone();
        let second = tokio::spawn(async move {
            second_broker
                .with_context(
                    RequestId::String("second".to_owned()),
                    second_events,
                    async { second_broker.request("第二个审批").await },
                )
                .await
        });

        let ServerFrame::Event(first_event) = first_receiver.recv().await.unwrap() else {
            panic!("预期第一个审批事件");
        };
        let ServerFrame::Event(second_event) = second_receiver.recv().await.unwrap() else {
            panic!("预期第二个审批事件");
        };
        assert_eq!(
            first_event.request_id,
            RequestId::String("first".to_owned())
        );
        assert_eq!(
            second_event.request_id,
            RequestId::String("second".to_owned())
        );
        let first_id = first_event.data["approval"]["id"].as_str().unwrap();
        let second_id = second_event.data["approval"]["id"].as_str().unwrap();
        broker.respond(first_id, true).await.unwrap();
        broker.respond(second_id, false).await.unwrap();
        assert!(first.await.unwrap().unwrap());
        assert!(!second.await.unwrap().unwrap());
    }

    #[tokio::test]
    async fn cancellation_is_scoped_to_session_even_when_request_ids_repeat() {
        let broker = ApprovalBroker::new();
        let (first_events, mut first_receiver) = mpsc::unbounded_channel();
        let (second_events, mut second_receiver) = mpsc::unbounded_channel();
        let first_broker = broker.clone();
        let first = tokio::spawn(async move {
            first_broker
                .with_session_context("session-a", RequestId::Number(1), first_events, async {
                    first_broker.request("第一个审批").await
                })
                .await
        });
        let second_broker = broker.clone();
        let second = tokio::spawn(async move {
            second_broker
                .with_session_context("session-b", RequestId::Number(1), second_events, async {
                    second_broker.request("第二个审批").await
                })
                .await
        });
        let first_id = match first_receiver.recv().await.unwrap() {
            ServerFrame::Event(event) => event.data["approval"]["id"].as_str().unwrap().to_owned(),
            _ => panic!("预期第一个审批事件"),
        };
        let second_id = match second_receiver.recv().await.unwrap() {
            ServerFrame::Event(event) => event.data["approval"]["id"].as_str().unwrap().to_owned(),
            _ => panic!("预期第二个审批事件"),
        };
        broker
            .cancel_request_in_session("session-a", &RequestId::Number(1))
            .await;
        assert!(!first.await.unwrap().unwrap());
        assert_eq!(broker.pending_for_session(Some("session-a")).await.len(), 0);
        broker.respond(&second_id, true).await.unwrap();
        assert!(second.await.unwrap().unwrap());
        let _ = first_id;
    }
}
