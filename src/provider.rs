mod anthropic;
mod ollama;
mod openai;
#[cfg(test)]
mod wire_tests;

use std::env;
use std::fmt;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

pub use anthropic::AnthropicProvider;
pub use ollama::OllamaProvider;
pub use openai::OpenAiProvider;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub role: Role,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_urls: Vec<String>,
}

impl Message {
    pub fn text(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
            image_urls: Vec::new(),
        }
    }

    pub fn assistant_tool_calls(calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: None,
            tool_calls: calls,
            tool_call_id: None,
            name: None,
            image_urls: Vec::new(),
        }
    }

    pub fn tool_result(call: &ToolCall, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: Some(call.id.clone()),
            name: Some(call.name.clone()),
            image_urls: Vec::new(),
        }
    }

    pub fn user_with_images(content: impl Into<String>, image_urls: Vec<String>) -> Self {
        Self {
            role: Role::User,
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
            image_urls,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    /// Agent 内部 execution identity。provider 出站前必须还原 wire id，禁止原样发送。
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Response {
    Text(String),
    ToolCalls(Vec<ToolCall>),
    ToolAssemblyFailed(ToolCallStreamError),
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub enum ApiType {
    OpenaiChat,
    AnthropicMessages,
    Ollama,
}

impl ApiType {
    pub fn from_env() -> Result<Self> {
        match env::var("API_TYPE") {
            Ok(value) => value.parse(),
            Err(env::VarError::NotPresent) => Ok(Self::OpenaiChat),
            Err(error) => Err(error).context("读取环境变量 API_TYPE 失败"),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenaiChat => "openai-chat",
            Self::AnthropicMessages => "anthropic-messages",
            Self::Ollama => "ollama",
        }
    }
}

impl fmt::Display for ApiType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ApiType {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "openai-chat" => Ok(Self::OpenaiChat),
            "anthropic-messages" => Ok(Self::AnthropicMessages),
            "ollama" => Ok(Self::Ollama),
            _ => bail!("API_TYPE 必须是 openai-chat、anthropic-messages 或 ollama"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderCapabilities {
    pub images: bool,
    pub tools: bool,
}

impl ProviderCapabilities {
    pub const fn new(images: bool, tools: bool) -> Self {
        Self { images, tools }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum IdentitySource {
    WireId(String),
    Position(u64),
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExecutionIdentity {
    pub domain: ApiType,
    pub source: IdentitySource,
}

impl ExecutionIdentity {
    pub fn wire(domain: ApiType, id: impl Into<String>) -> Self {
        Self {
            domain,
            source: IdentitySource::WireId(id.into()),
        }
    }

    pub const fn position(domain: ApiType, position: u64) -> Self {
        Self {
            domain,
            source: IdentitySource::Position(position),
        }
    }

    pub fn wire_id(&self) -> Option<&str> {
        match &self.source {
            IdentitySource::WireId(id) => Some(id),
            IdentitySource::Position(_) => None,
        }
    }

    pub fn encode(&self) -> String {
        match &self.source {
            IdentitySource::WireId(id) => format!(
                "tc1:{}:id:{}",
                self.domain,
                URL_SAFE_NO_PAD.encode(id.as_bytes())
            ),
            IdentitySource::Position(position) => {
                format!("tc1:{}:pos:{position}", self.domain)
            }
        }
    }

    pub fn decode(value: &str) -> Option<Self> {
        let mut parts = value.splitn(4, ':');
        if parts.next()? != "tc1" {
            return None;
        }
        let domain = parts.next()?.parse().ok()?;
        let source_kind = parts.next()?;
        let source = parts.next()?;
        match source_kind {
            "id" => {
                let bytes = URL_SAFE_NO_PAD.decode(source).ok()?;
                let id = String::from_utf8(bytes).ok()?;
                Some(Self::wire(domain, id))
            }
            "pos" => source
                .parse::<u64>()
                .ok()
                .map(|position| Self::position(domain, position)),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolArgumentsFragment {
    Append(String),
    AuthoritativeSnapshot(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCallStreamError {
    pub code: String,
    pub message: String,
}

impl ToolCallStreamError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderEvent {
    TextDelta(String),
    ToolCallStarted {
        exec_id: ExecutionIdentity,
        name: String,
    },
    ToolCallDelta {
        exec_id: ExecutionIdentity,
        fragment: ToolArgumentsFragment,
    },
    ToolCallCompleted {
        exec_id: ExecutionIdentity,
    },
    ToolCallFailed(ToolCallStreamError),
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn api_type(&self) -> ApiType {
        ApiType::OpenaiChat
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::new(true, true)
    }

    async fn chat(&self, _messages: &[Message], _tools: &[ToolSpec]) -> Result<Response> {
        bail!("该 provider 仅实现流式事件接口")
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        events: mpsc::UnboundedSender<ProviderEvent>,
    ) -> Result<()> {
        let response = self.chat(messages, tools).await?;
        emit_legacy_response(response, self.api_type(), &events)
    }
}

fn emit_legacy_response(
    response: Response,
    domain: ApiType,
    events: &mpsc::UnboundedSender<ProviderEvent>,
) -> Result<()> {
    match response {
        Response::Text(text) => send_event(events, ProviderEvent::TextDelta(text)),
        Response::ToolCalls(calls) => {
            if calls.is_empty() {
                return send_event(
                    events,
                    ProviderEvent::ToolCallFailed(ToolCallStreamError::new(
                        "empty_tool_calls",
                        "provider 返回了空工具调用批次",
                    )),
                );
            }
            for (position, call) in calls.into_iter().enumerate() {
                let identity = ExecutionIdentity::decode(&call.id).unwrap_or_else(|| {
                    if call.id.is_empty() {
                        ExecutionIdentity::position(domain, position as u64)
                    } else {
                        ExecutionIdentity::wire(domain, call.id)
                    }
                });
                send_event(
                    events,
                    ProviderEvent::ToolCallStarted {
                        exec_id: identity.clone(),
                        name: call.name,
                    },
                )?;
                send_event(
                    events,
                    ProviderEvent::ToolCallDelta {
                        exec_id: identity.clone(),
                        fragment: ToolArgumentsFragment::AuthoritativeSnapshot(
                            call.arguments.to_string(),
                        ),
                    },
                )?;
                send_event(
                    events,
                    ProviderEvent::ToolCallCompleted { exec_id: identity },
                )?;
            }
            Ok(())
        }
        Response::ToolAssemblyFailed(error) => {
            send_event(events, ProviderEvent::ToolCallFailed(error))
        }
    }
}

pub fn build_provider_from_env() -> Result<Box<dyn Provider>> {
    match ApiType::from_env()? {
        ApiType::OpenaiChat => Ok(Box::new(OpenAiProvider::from_env()?)),
        ApiType::AnthropicMessages => Ok(Box::new(AnthropicProvider::from_env()?)),
        ApiType::Ollama => Ok(Box::new(OllamaProvider::from_env()?)),
    }
}

pub(crate) fn required_env(name: &str) -> Result<String> {
    let value = env::var(name).with_context(|| format!("缺少环境变量 {name}"))?;
    if value.trim().is_empty() {
        bail!("环境变量 {name} 不能为空");
    }
    Ok(value)
}

pub(crate) fn optional_bool_env(name: &str, default: bool) -> Result<bool> {
    match env::var(name) {
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            _ => bail!("环境变量 {name} 必须是 true/false、1/0、yes/no 或 on/off"),
        },
        Err(env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error).with_context(|| format!("读取环境变量 {name} 失败")),
    }
}

pub(crate) fn send_event(
    events: &mpsc::UnboundedSender<ProviderEvent>,
    event: ProviderEvent,
) -> Result<()> {
    events
        .send(event)
        .map_err(|_| anyhow::anyhow!("provider 事件接收端已关闭"))
}

pub(crate) fn outbound_wire_id(internal_id: &str) -> Option<String> {
    ExecutionIdentity::decode(internal_id)
        .and_then(|identity| identity.wire_id().map(str::to_owned))
        .or_else(|| (!internal_id.is_empty()).then(|| internal_id.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_identity_is_reversible_and_domain_scoped() {
        let openai = ExecutionIdentity::wire(ApiType::OpenaiChat, "same-id");
        let anthropic = ExecutionIdentity::wire(ApiType::AnthropicMessages, "same-id");

        assert_ne!(openai.encode(), anthropic.encode());
        assert_eq!(ExecutionIdentity::decode(&openai.encode()), Some(openai));
        assert_eq!(
            ExecutionIdentity::decode(&anthropic.encode()),
            Some(anthropic)
        );
    }

    #[test]
    fn positional_identity_has_no_wire_id() {
        let identity = ExecutionIdentity::position(ApiType::Ollama, 2);
        assert_eq!(identity.wire_id(), None);
        assert_eq!(
            ExecutionIdentity::decode(&identity.encode()),
            Some(identity)
        );
    }
}
