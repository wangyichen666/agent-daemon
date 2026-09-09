use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::debug;

use super::{
    ApiType, ExecutionIdentity, Message, Provider, ProviderCapabilities, ProviderEvent, Role,
    ToolArgumentsFragment, ToolSpec, optional_bool_env, required_env, send_event,
};

pub struct OllamaProvider {
    client: reqwest::Client,
    endpoint: String,
    model: String,
    tools_enabled: bool,
}

impl OllamaProvider {
    pub fn from_env() -> Result<Self> {
        let base_url = std::env::var("OPENAI_BASE_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "http://localhost:11434".to_owned());
        let model = required_env("MODEL_NAME")?;
        let tools_enabled = optional_bool_env("OLLAMA_TOOLS_ENABLED", true)?;
        Ok(Self::new(base_url, model, tools_enabled))
    }

    pub(crate) fn new(base_url: String, model: String, tools_enabled: bool) -> Self {
        let endpoint = if base_url.trim_end_matches('/').ends_with("/api/chat") {
            base_url.trim_end_matches('/').to_owned()
        } else {
            format!("{}/api/chat", base_url.trim_end_matches('/'))
        };
        Self {
            client: reqwest::Client::new(),
            endpoint,
            model,
            tools_enabled,
        }
    }

    fn request_payload(&self, messages: &[Message], tools: &[ToolSpec]) -> Value {
        let mut payload = json!({
            "model": self.model,
            "messages": request_messages(messages),
            "stream": true,
        });
        if self.tools_enabled && !tools.is_empty() {
            payload["tools"] = Value::Array(
                tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "type": "function",
                            "function": {
                                "name": tool.name,
                                "description": tool.description,
                                "parameters": tool.parameters,
                            }
                        })
                    })
                    .collect(),
            );
        }
        payload
    }
}

#[async_trait]
impl Provider for OllamaProvider {
    fn api_type(&self) -> ApiType {
        ApiType::Ollama
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::new(false, self.tools_enabled)
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        events: mpsc::UnboundedSender<ProviderEvent>,
    ) -> Result<()> {
        let response = self
            .client
            .post(&self.endpoint)
            .json(&self.request_payload(messages, tools))
            .send()
            .await
            .context("调用 Ollama 服务失败")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.context("读取 Ollama 错误响应失败")?;
            anyhow::bail!("Ollama 服务返回 {status}: {body}");
        }
        parse_ndjson(response, &events).await
    }
}

fn request_messages(messages: &[Message]) -> Vec<Value> {
    messages
        .iter()
        .map(|message| {
            let mut value = json!({
                "role": role_name(&message.role),
                "content": message.content.as_deref().unwrap_or_default(),
            });
            if !message.tool_calls.is_empty() {
                value["tool_calls"] = Value::Array(
                    message
                        .tool_calls
                        .iter()
                        .map(|call| {
                            json!({
                                "function": {
                                    "name": call.name,
                                    "arguments": call.arguments,
                                }
                            })
                        })
                        .collect(),
                );
            }
            value
        })
        .collect()
}

async fn parse_ndjson(
    response: reqwest::Response,
    events: &mpsc::UnboundedSender<ProviderEvent>,
) -> Result<()> {
    let mut stream = response.bytes_stream();
    let mut pending = Vec::new();
    let mut position = 0_u64;
    while let Some(chunk) = stream.next().await {
        pending.extend_from_slice(&chunk.context("读取 Ollama NDJSON 失败")?);
        while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
            let mut line = pending.drain(..=newline).collect::<Vec<_>>();
            while matches!(line.last(), Some(b'\n' | b'\r')) {
                line.pop();
            }
            consume_ndjson_line(&line, &mut position, events)?;
        }
    }
    if !pending.is_empty() {
        consume_ndjson_line(&pending, &mut position, events)?;
    }
    Ok(())
}

fn consume_ndjson_line(
    line: &[u8],
    position: &mut u64,
    events: &mpsc::UnboundedSender<ProviderEvent>,
) -> Result<()> {
    if line.iter().all(u8::is_ascii_whitespace) {
        return Ok(());
    }
    let chunk: OllamaChunk = serde_json::from_slice(line).context("解析 Ollama NDJSON 失败")?;
    if let Some(error) = chunk.error {
        anyhow::bail!("Ollama 流返回错误: {error}");
    }
    if let Some(message) = chunk.message {
        if !message.content.is_empty() {
            send_event(events, ProviderEvent::TextDelta(message.content))?;
        }
        for call in message.tool_calls {
            let identity = ExecutionIdentity::position(ApiType::Ollama, *position);
            *position = position.saturating_add(1);
            send_event(
                events,
                ProviderEvent::ToolCallStarted {
                    exec_id: identity.clone(),
                    name: call.function.name,
                },
            )?;
            send_event(
                events,
                ProviderEvent::ToolCallDelta {
                    exec_id: identity.clone(),
                    fragment: ToolArgumentsFragment::AuthoritativeSnapshot(
                        call.function.arguments.to_string(),
                    ),
                },
            )?;
            send_event(
                events,
                ProviderEvent::ToolCallCompleted { exec_id: identity },
            )?;
        }
    }
    if chunk.done {
        debug!(
            provider = "ollama",
            input_tokens = chunk.prompt_eval_count.unwrap_or(0),
            output_tokens = chunk.eval_count.unwrap_or(0),
            cache_creation_input_tokens = 0,
            cache_read_input_tokens = 0,
            "LLM token 用量"
        );
    }
    Ok(())
}

#[derive(Deserialize)]
struct OllamaChunk {
    message: Option<OllamaMessage>,
    #[serde(default)]
    done: bool,
    prompt_eval_count: Option<u64>,
    eval_count: Option<u64>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct OllamaMessage {
    #[serde(default)]
    content: String,
    #[serde(default)]
    tool_calls: Vec<OllamaToolCall>,
}

#[derive(Deserialize)]
struct OllamaToolCall {
    function: OllamaFunction,
}

#[derive(Deserialize)]
struct OllamaFunction {
    name: String,
    #[serde(default)]
    arguments: Value,
}

fn role_name(role: &Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assigns_position_only_to_complete_idless_calls() {
        let (events, mut receiver) = mpsc::unbounded_channel();
        let mut position = 0;
        consume_ndjson_line(
            br#"{"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"one","arguments":{"a":1}}},{"function":{"name":"two","arguments":{"b":2}}}]},"done":false}"#,
            &mut position,
            &events,
        )
        .unwrap();
        let captured = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(position, 2);
        assert_eq!(captured.len(), 6);
        let ProviderEvent::ToolCallStarted { exec_id, .. } = &captured[0] else {
            panic!("缺少第一个开始事件");
        };
        assert_eq!(*exec_id, ExecutionIdentity::position(ApiType::Ollama, 0));
        let ProviderEvent::ToolCallStarted { exec_id, .. } = &captured[3] else {
            panic!("缺少第二个开始事件");
        };
        assert_eq!(*exec_id, ExecutionIdentity::position(ApiType::Ollama, 1));
    }

    #[test]
    fn omits_tools_when_capability_is_disabled() {
        let provider = OllamaProvider::new(
            "http://localhost:11434".to_owned(),
            "qwen".to_owned(),
            false,
        );
        let payload = provider.request_payload(
            &[],
            &[ToolSpec {
                name: "read_file".to_owned(),
                description: "read".to_owned(),
                parameters: json!({"type": "object"}),
            }],
        );
        assert!(payload.get("tools").is_none());
    }
}
