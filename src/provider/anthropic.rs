use std::collections::BTreeMap;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::debug;

use super::{
    ApiType, ExecutionIdentity, Message, Provider, ProviderCapabilities, ProviderEvent, Role,
    ToolArgumentsFragment, ToolCallStreamError, ToolSpec, outbound_wire_id, required_env,
    send_event,
};

pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: String,
    endpoint: String,
    model: String,
}

impl AnthropicProvider {
    pub fn from_env() -> Result<Self> {
        let api_key = required_env("OPENAI_API_KEY")?;
        let base_url = required_env("OPENAI_BASE_URL")?;
        let model = required_env("MODEL_NAME")?;
        Ok(Self::new(api_key, base_url, model))
    }

    pub(crate) fn new(api_key: String, base_url: String, model: String) -> Self {
        let endpoint = if base_url.trim_end_matches('/').ends_with("/v1/messages") {
            base_url.trim_end_matches('/').to_owned()
        } else {
            format!("{}/v1/messages", base_url.trim_end_matches('/'))
        };
        Self {
            client: reqwest::Client::new(),
            api_key,
            endpoint,
            model,
        }
    }

    fn request_payload(&self, messages: &[Message], tools: &[ToolSpec]) -> Value {
        let (system, messages) = request_messages(messages);
        let mut payload = json!({
            "model": self.model,
            "max_tokens": 8192,
            "messages": messages,
            "stream": true,
        });
        if !system.is_empty() {
            payload["system"] = Value::String(system);
        }
        if !tools.is_empty() {
            payload["tools"] = Value::Array(
                tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "name": tool.name,
                            "description": tool.description,
                            "input_schema": tool.parameters,
                        })
                    })
                    .collect(),
            );
        }
        payload
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn api_type(&self) -> ApiType {
        ApiType::AnthropicMessages
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::new(true, true)
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
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&self.request_payload(messages, tools))
            .send()
            .await
            .context("调用 Anthropic Messages 服务失败")?;
        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .context("读取 Anthropic 错误响应失败")?;
            anyhow::bail!("Anthropic 服务返回 {status}: {body}");
        }
        parse_sse(response, &events).await
    }
}

fn request_messages(messages: &[Message]) -> (String, Vec<Value>) {
    let mut system = Vec::new();
    let mut output: Vec<Value> = Vec::new();
    for message in messages {
        if message.role == Role::System {
            if let Some(content) = message.content.as_deref() {
                system.push(content.to_owned());
            }
            continue;
        }
        let (role, blocks) = match message.role {
            Role::User => ("user", user_blocks(message)),
            Role::Assistant => ("assistant", assistant_blocks(message)),
            Role::Tool => ("user", tool_result_blocks(message)),
            Role::System => continue,
        };
        if let Some(last) = output.last_mut()
            && last.get("role").and_then(Value::as_str) == Some(role)
        {
            if let Some(existing) = last.get_mut("content").and_then(Value::as_array_mut) {
                existing.extend(blocks);
            }
        } else {
            output.push(json!({"role": role, "content": blocks}));
        }
    }
    (system.join("\n\n"), output)
}

fn user_blocks(message: &Message) -> Vec<Value> {
    let mut blocks = Vec::new();
    if let Some(content) = message.content.as_deref() {
        blocks.push(json!({"type": "text", "text": content}));
    }
    for image in &message.image_urls {
        if let Some((media_type, data)) = parse_data_url(image) {
            blocks.push(json!({
                "type": "image",
                "source": {"type": "base64", "media_type": media_type, "data": data}
            }));
        } else {
            blocks.push(json!({
                "type": "image",
                "source": {"type": "url", "url": image}
            }));
        }
    }
    blocks
}

fn assistant_blocks(message: &Message) -> Vec<Value> {
    let mut blocks = Vec::new();
    if let Some(content) = message.content.as_deref() {
        blocks.push(json!({"type": "text", "text": content}));
    }
    blocks.extend(message.tool_calls.iter().map(|call| {
        json!({
            "type": "tool_use",
            "id": outbound_wire_id(&call.id).unwrap_or_else(|| position_wire_id(&call.id)),
            "name": call.name,
            "input": call.arguments,
        })
    }));
    blocks
}

fn tool_result_blocks(message: &Message) -> Vec<Value> {
    vec![json!({
        "type": "tool_result",
        "tool_use_id": message.tool_call_id.as_deref()
            .and_then(outbound_wire_id)
            .unwrap_or_else(|| position_wire_id(message.tool_call_id.as_deref().unwrap_or("tool"))),
        "content": message.content.as_deref().unwrap_or_default(),
    })]
}

fn position_wire_id(value: &str) -> String {
    ExecutionIdentity::decode(value)
        .map(|identity| match identity.source {
            super::IdentitySource::Position(position) => format!("toolu_pos_{position}"),
            super::IdentitySource::WireId(id) => id,
        })
        .unwrap_or_else(|| value.to_owned())
}

fn parse_data_url(value: &str) -> Option<(&str, &str)> {
    let rest = value.strip_prefix("data:")?;
    let (media_type, data) = rest.split_once(";base64,")?;
    Some((media_type, data))
}

#[derive(Default)]
struct StreamState {
    tools: BTreeMap<u64, ExecutionIdentity>,
}

async fn parse_sse(
    response: reqwest::Response,
    events: &mpsc::UnboundedSender<ProviderEvent>,
) -> Result<()> {
    let mut stream = response.bytes_stream();
    let mut pending = Vec::new();
    let mut state = StreamState::default();
    while let Some(chunk) = stream.next().await {
        pending.extend_from_slice(&chunk.context("读取 Anthropic SSE 失败")?);
        while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
            let mut line = pending.drain(..=newline).collect::<Vec<_>>();
            while matches!(line.last(), Some(b'\n' | b'\r')) {
                line.pop();
            }
            consume_sse_line(&line, &mut state, events)?;
        }
    }
    if !pending.is_empty() {
        consume_sse_line(&pending, &mut state, events)?;
    }
    if !state.tools.is_empty() {
        send_event(
            events,
            ProviderEvent::ToolCallFailed(ToolCallStreamError::new(
                "incomplete_call",
                "Anthropic 流结束时仍有未完成的 tool_use content block",
            )),
        )?;
    }
    Ok(())
}

fn consume_sse_line(
    line: &[u8],
    state: &mut StreamState,
    events: &mpsc::UnboundedSender<ProviderEvent>,
) -> Result<()> {
    let line = String::from_utf8_lossy(line);
    let Some(data) = line.strip_prefix("data:") else {
        return Ok(());
    };
    let data = data.trim_start();
    if data.is_empty() || data == "[DONE]" {
        return Ok(());
    }
    let value: Value = serde_json::from_str(data).context("解析 Anthropic SSE 数据失败")?;
    match value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "message_start" => log_usage("message_start", value.pointer("/message/usage")),
        "content_block_start" => start_content_block(&value, state, events)?,
        "content_block_delta" => consume_content_delta(&value, state, events)?,
        "content_block_stop" => stop_content_block(&value, state, events)?,
        "message_delta" => log_usage("message_delta", value.get("usage")),
        "error" => {
            send_event(
                events,
                ProviderEvent::ToolCallFailed(ToolCallStreamError::new(
                    "provider_error",
                    value.to_string(),
                )),
            )?;
        }
        "message_stop" | "ping" => {}
        _ => {}
    }
    Ok(())
}

fn start_content_block(
    value: &Value,
    state: &mut StreamState,
    events: &mpsc::UnboundedSender<ProviderEvent>,
) -> Result<()> {
    let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
    let block = &value["content_block"];
    match block.get("type").and_then(Value::as_str) {
        Some("text") => {
            if let Some(text) = block.get("text").and_then(Value::as_str)
                && !text.is_empty()
            {
                send_event(events, ProviderEvent::TextDelta(text.to_owned()))?;
            }
        }
        Some("tool_use") => {
            let Some(id) = block.get("id").and_then(Value::as_str) else {
                return emit_failure(events, "missing_id", "Anthropic tool_use 缺少 id");
            };
            let Some(name) = block.get("name").and_then(Value::as_str) else {
                return emit_failure(events, "missing_name", "Anthropic tool_use 缺少 name");
            };
            let identity = ExecutionIdentity::wire(ApiType::AnthropicMessages, id);
            if state.tools.insert(index, identity.clone()).is_some() {
                return emit_failure(
                    events,
                    "identity_conflict",
                    format!("Anthropic content block {index} 被重复开始"),
                );
            }
            send_event(
                events,
                ProviderEvent::ToolCallStarted {
                    exec_id: identity,
                    name: name.to_owned(),
                },
            )?;
        }
        _ => {}
    }
    Ok(())
}

fn consume_content_delta(
    value: &Value,
    state: &StreamState,
    events: &mpsc::UnboundedSender<ProviderEvent>,
) -> Result<()> {
    let delta = &value["delta"];
    match delta.get("type").and_then(Value::as_str) {
        Some("text_delta") => {
            if let Some(text) = delta.get("text").and_then(Value::as_str) {
                send_event(events, ProviderEvent::TextDelta(text.to_owned()))?;
            }
        }
        Some("input_json_delta") => {
            let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
            let Some(identity) = state.tools.get(&index) else {
                return emit_failure(
                    events,
                    "anonymous_delta",
                    format!("Anthropic content block {index} 的 JSON 分片没有 tool_use"),
                );
            };
            let fragment = delta
                .get("partial_json")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            send_event(
                events,
                ProviderEvent::ToolCallDelta {
                    exec_id: identity.clone(),
                    fragment: ToolArgumentsFragment::Append(fragment),
                },
            )?;
        }
        _ => {}
    }
    Ok(())
}

fn stop_content_block(
    value: &Value,
    state: &mut StreamState,
    events: &mpsc::UnboundedSender<ProviderEvent>,
) -> Result<()> {
    let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
    if let Some(identity) = state.tools.remove(&index) {
        send_event(
            events,
            ProviderEvent::ToolCallCompleted { exec_id: identity },
        )?;
    }
    Ok(())
}

fn emit_failure(
    events: &mpsc::UnboundedSender<ProviderEvent>,
    code: &str,
    message: impl Into<String>,
) -> Result<()> {
    send_event(
        events,
        ProviderEvent::ToolCallFailed(ToolCallStreamError::new(code, message)),
    )
}

fn log_usage(event: &str, usage: Option<&Value>) {
    let Some(usage) = usage else {
        return;
    };
    debug!(
        provider = "anthropic-messages",
        event,
        input_tokens = usage["input_tokens"].as_u64().unwrap_or(0),
        output_tokens = usage["output_tokens"].as_u64().unwrap_or(0),
        cache_creation_input_tokens = usage["cache_creation_input_tokens"].as_u64().unwrap_or(0),
        cache_read_input_tokens = usage["cache_read_input_tokens"].as_u64().unwrap_or(0),
        "LLM token 用量"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_system_tool_use_and_tool_result() {
        let identity = ExecutionIdentity::wire(ApiType::AnthropicMessages, "toolu_1");
        let call = super::super::ToolCall {
            id: identity.encode(),
            name: "read_file".to_owned(),
            arguments: json!({"path": "README.md"}),
        };
        let (system, messages) = request_messages(&[
            Message::text(Role::System, "system prompt"),
            Message::assistant_tool_calls(vec![call.clone()]),
            Message::tool_result(&call, "hello"),
        ]);
        assert_eq!(system, "system prompt");
        assert_eq!(messages[0]["content"][0]["id"], "toolu_1");
        assert_eq!(messages[1]["content"][0]["tool_use_id"], "toolu_1");
    }

    #[test]
    fn decodes_input_json_delta_lifecycle() {
        let (events, mut receiver) = mpsc::unbounded_channel();
        let mut state = StreamState::default();
        for line in [
            br#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"read_file","input":{}}}"#.as_slice(),
            br#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#.as_slice(),
            br#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"README.md\"}"}}"#.as_slice(),
            br#"data: {"type":"content_block_stop","index":1}"#.as_slice(),
        ] {
            consume_sse_line(line, &mut state, &events).unwrap();
        }
        let captured = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(captured.len(), 4);
        assert!(matches!(captured[0], ProviderEvent::ToolCallStarted { .. }));
        assert!(matches!(
            captured[3],
            ProviderEvent::ToolCallCompleted { .. }
        ));
    }
}
