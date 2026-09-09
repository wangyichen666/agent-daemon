use std::collections::BTreeMap;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::debug;

use super::{
    ApiType, ExecutionIdentity, Message, Provider, ProviderCapabilities, ProviderEvent, Role,
    ToolArgumentsFragment, ToolCallStreamError, ToolSpec, outbound_wire_id, required_env,
    send_event,
};

pub struct OpenAiProvider {
    client: reqwest::Client,
    api_key: String,
    endpoint: String,
    model: String,
}

impl OpenAiProvider {
    pub fn from_env() -> Result<Self> {
        let api_key = required_env("OPENAI_API_KEY")?;
        let base_url = required_env("OPENAI_BASE_URL")?;
        let model = required_env("MODEL_NAME")?;
        Ok(Self::new(api_key, base_url, model))
    }

    pub(crate) fn new(api_key: String, base_url: String, model: String) -> Self {
        let endpoint = if base_url
            .trim_end_matches('/')
            .ends_with("/chat/completions")
        {
            base_url.trim_end_matches('/').to_owned()
        } else {
            format!("{}/chat/completions", base_url.trim_end_matches('/'))
        };
        Self {
            client: reqwest::Client::new(),
            api_key,
            endpoint,
            model,
        }
    }

    pub(crate) fn request_messages(messages: &[Message]) -> Vec<Value> {
        messages
            .iter()
            .map(|message| {
                let mut value = json!({ "role": role_name(&message.role) });
                if !message.image_urls.is_empty() {
                    let mut parts = Vec::with_capacity(message.image_urls.len() + 1);
                    if let Some(content) = &message.content {
                        parts.push(json!({"type": "text", "text": content}));
                    }
                    parts.extend(message.image_urls.iter().map(
                        |url| json!({"type": "image_url", "image_url": {"url": url}}),
                    ));
                    value["content"] = Value::Array(parts);
                } else if let Some(content) = &message.content {
                    value["content"] = Value::String(content.clone());
                }
                if !message.tool_calls.is_empty() {
                    value["tool_calls"] = Value::Array(
                        message
                            .tool_calls
                            .iter()
                            .map(|call| {
                                json!({
                                    "id": outbound_wire_id(&call.id).unwrap_or_else(|| call.id.clone()),
                                    "type": "function",
                                    "function": {
                                        "name": call.name,
                                        "arguments": call.arguments.to_string(),
                                    }
                                })
                            })
                            .collect(),
                    );
                }
                if let Some(tool_call_id) = &message.tool_call_id {
                    value["tool_call_id"] = Value::String(
                        outbound_wire_id(tool_call_id).unwrap_or_else(|| tool_call_id.clone()),
                    );
                }
                if let Some(name) = &message.name {
                    value["name"] = Value::String(name.clone());
                }
                value
            })
            .collect()
    }

    pub(crate) fn request_tools(tools: &[ToolSpec]) -> Vec<Value> {
        let mut ordered = tools.iter().collect::<Vec<_>>();
        ordered.sort_by(|left, right| left.name.cmp(&right.name));
        ordered
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
            .collect()
    }

    fn request_payload(&self, messages: &[Message], tools: &[ToolSpec]) -> Value {
        // 保持既有 OpenAI 路径的字段、插入顺序与工具排序不变，以保留前缀缓存命中。
        let mut payload = json!({
            "model": self.model,
            "messages": Self::request_messages(messages),
            "stream": true,
        });
        if !tools.is_empty() {
            payload["tools"] = Value::Array(Self::request_tools(tools));
            payload["tool_choice"] = Value::String("auto".to_owned());
        }
        payload
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn api_type(&self) -> ApiType {
        ApiType::OpenaiChat
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
            .bearer_auth(&self.api_key)
            .json(&self.request_payload(messages, tools))
            .send()
            .await
            .context("调用 LLM 服务失败")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.context("读取 LLM 错误响应失败")?;
            anyhow::bail!("LLM 服务返回 {status}: {body}");
        }
        parse_sse(response, &events).await
    }
}

#[derive(Default)]
struct WireCall {
    id: Option<String>,
    name: Option<String>,
    identity: Option<ExecutionIdentity>,
    saw_arguments: bool,
}

async fn parse_sse(
    response: reqwest::Response,
    events: &mpsc::UnboundedSender<ProviderEvent>,
) -> Result<()> {
    let mut stream = response.bytes_stream();
    let mut pending_bytes = Vec::new();
    let mut calls = BTreeMap::new();
    while let Some(chunk) = stream.next().await {
        pending_bytes.extend_from_slice(&chunk.context("读取 LLM 流式响应失败")?);
        while let Some(newline) = pending_bytes.iter().position(|byte| *byte == b'\n') {
            let mut line = pending_bytes.drain(..=newline).collect::<Vec<u8>>();
            while matches!(line.last(), Some(b'\n' | b'\r')) {
                line.pop();
            }
            consume_sse_line(&line, &mut calls, events)?;
        }
    }
    if !pending_bytes.is_empty() {
        consume_sse_line(&pending_bytes, &mut calls, events)?;
    }
    complete_calls(calls, events)
}

fn consume_sse_line(
    line: &[u8],
    calls: &mut BTreeMap<usize, WireCall>,
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
    let chunk: StreamChunk = serde_json::from_str(data).context("解析 LLM SSE 数据失败")?;
    if let Some(usage) = chunk.usage {
        debug!(
            provider = "openai-chat",
            input_tokens = usage.prompt_tokens,
            output_tokens = usage.completion_tokens,
            cache_read_input_tokens = usage.cache_hit_tokens(),
            cache_creation_input_tokens = usage.prompt_cache_miss_tokens.unwrap_or(0),
            "LLM token 用量"
        );
    }
    for choice in chunk.choices {
        if let Some(content) = choice.delta.content {
            send_event(events, ProviderEvent::TextDelta(content))?;
        }
        for delta in choice.delta.tool_calls {
            consume_tool_delta(delta, calls, events)?;
        }
    }
    Ok(())
}

fn consume_tool_delta(
    delta: StreamToolCall,
    calls: &mut BTreeMap<usize, WireCall>,
    events: &mpsc::UnboundedSender<ProviderEvent>,
) -> Result<()> {
    let call = calls.entry(delta.index).or_default();
    if let Some(id) = delta.id.filter(|id| !id.is_empty()) {
        match &call.id {
            Some(existing) if existing != &id => {
                return stream_failure(
                    events,
                    "identity_conflict",
                    format!(
                        "OpenAI 工具位置 {} 出现冲突 id：{existing} / {id}",
                        delta.index
                    ),
                );
            }
            None => call.id = Some(id),
            _ => {}
        }
    }
    let function = delta.function.unwrap_or_default();
    if let Some(name) = function.name.filter(|name| !name.is_empty()) {
        match &call.name {
            Some(existing) if existing != &name => {
                return stream_failure(
                    events,
                    "name_conflict",
                    format!(
                        "OpenAI 工具位置 {} 出现冲突名称：{existing} / {name}",
                        delta.index
                    ),
                );
            }
            None => call.name = Some(name),
            _ => {}
        }
    }
    if function.arguments.is_some() && call.identity.is_none() {
        start_call(delta.index, call, events)?;
    }
    if let Some(arguments) = function.arguments {
        call.saw_arguments = true;
        let Some(identity) = call.identity.clone() else {
            return stream_failure(
                events,
                "anonymous_delta",
                format!("OpenAI 工具位置 {} 的 arguments 无法归属", delta.index),
            );
        };
        send_event(
            events,
            ProviderEvent::ToolCallDelta {
                exec_id: identity,
                fragment: ToolArgumentsFragment::Append(arguments),
            },
        )?;
    }
    Ok(())
}

fn start_call(
    index: usize,
    call: &mut WireCall,
    events: &mpsc::UnboundedSender<ProviderEvent>,
) -> Result<()> {
    let (Some(id), Some(name)) = (call.id.as_deref(), call.name.as_deref()) else {
        return stream_failure(
            events,
            "anonymous_delta",
            format!("OpenAI 工具位置 {index} 在 id/name 完整前出现 arguments"),
        );
    };
    let identity = ExecutionIdentity::wire(ApiType::OpenaiChat, id);
    send_event(
        events,
        ProviderEvent::ToolCallStarted {
            exec_id: identity.clone(),
            name: name.to_owned(),
        },
    )?;
    call.identity = Some(identity);
    Ok(())
}

fn complete_calls(
    calls: BTreeMap<usize, WireCall>,
    events: &mpsc::UnboundedSender<ProviderEvent>,
) -> Result<()> {
    for (index, mut call) in calls {
        if call.identity.is_none() {
            start_call(index, &mut call, events)?;
        }
        let Some(identity) = call.identity else {
            return stream_failure(events, "incomplete_call", "OpenAI 工具调用缺少 identity");
        };
        send_event(
            events,
            ProviderEvent::ToolCallCompleted { exec_id: identity },
        )?;
    }
    Ok(())
}

fn stream_failure(
    events: &mpsc::UnboundedSender<ProviderEvent>,
    code: &str,
    message: impl Into<String>,
) -> Result<()> {
    send_event(
        events,
        ProviderEvent::ToolCallFailed(ToolCallStreamError::new(code, message)),
    )
}

#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    usage: Option<StreamUsage>,
}

#[derive(Deserialize)]
struct StreamUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    prompt_tokens_details: Option<PromptTokenDetails>,
    prompt_cache_hit_tokens: Option<u64>,
    prompt_cache_miss_tokens: Option<u64>,
}

impl StreamUsage {
    fn cache_hit_tokens(&self) -> u64 {
        self.prompt_cache_hit_tokens
            .or_else(|| {
                self.prompt_tokens_details
                    .as_ref()
                    .and_then(|details| details.cached_tokens)
            })
            .unwrap_or(0)
    }
}

#[derive(Deserialize)]
struct PromptTokenDetails {
    cached_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
}

#[derive(Default, Deserialize)]
struct StreamDelta {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<StreamToolCall>,
}

#[derive(Deserialize)]
struct StreamToolCall {
    index: usize,
    id: Option<String>,
    function: Option<StreamFunction>,
}

#[derive(Default, Deserialize)]
struct StreamFunction {
    name: Option<String>,
    arguments: Option<String>,
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
    use crate::provider::{IdentitySource, ToolCall};

    #[test]
    fn request_shape_and_wire_ids_remain_compatible() {
        let identity = ExecutionIdentity::wire(ApiType::OpenaiChat, "call-1");
        let call = ToolCall {
            id: identity.encode(),
            name: "read_file".to_owned(),
            arguments: json!({"path": "README.md"}),
        };
        let values = OpenAiProvider::request_messages(&[
            Message::assistant_tool_calls(vec![call.clone()]),
            Message::tool_result(&call, "hello"),
        ]);
        assert_eq!(values[0]["tool_calls"][0]["id"], "call-1");
        assert_eq!(values[1]["tool_call_id"], "call-1");
        assert_eq!(values[1]["content"], "hello");
    }

    #[test]
    fn converts_user_images_to_openai_content_parts() {
        let values = OpenAiProvider::request_messages(&[Message::user_with_images(
            "描述图片",
            vec!["data:image/png;base64,AAAA".to_owned()],
        )]);
        assert_eq!(values[0]["content"][0]["type"], "text");
        assert_eq!(values[0]["content"][1]["type"], "image_url");
    }

    #[test]
    fn emits_pure_ordered_argument_fragments() {
        let (events, mut receiver) = mpsc::unbounded_channel();
        let mut calls = BTreeMap::new();
        consume_sse_line(
            br#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"read_file","arguments":"{\"path\":"}}]}}]}"#,
            &mut calls,
            &events,
        )
        .unwrap();
        consume_sse_line(
            br#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"README.md\"}"}}]}}]}"#,
            &mut calls,
            &events,
        )
        .unwrap();
        complete_calls(calls, &events).unwrap();
        let captured = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
        assert!(matches!(
            &captured[1],
            ProviderEvent::ToolCallDelta { fragment: ToolArgumentsFragment::Append(value), .. }
                if value == r#"{"path":"#
        ));
        assert!(matches!(
            &captured[2],
            ProviderEvent::ToolCallDelta { fragment: ToolArgumentsFragment::Append(value), .. }
                if value == r#""README.md"}"#
        ));
        let ProviderEvent::ToolCallStarted { exec_id, .. } = &captured[0] else {
            panic!("缺少工具开始事件");
        };
        assert_eq!(exec_id.source, IdentitySource::WireId("call-1".to_owned()));
    }

    #[test]
    fn accepts_openai_and_deepseek_cache_usage_shapes() {
        let openai: StreamUsage = serde_json::from_value(json!({
            "prompt_tokens": 100,
            "completion_tokens": 5,
            "prompt_tokens_details": {"cached_tokens": 64}
        }))
        .unwrap();
        let deepseek: StreamUsage = serde_json::from_value(json!({
            "prompt_tokens": 100,
            "completion_tokens": 5,
            "prompt_cache_hit_tokens": 80,
            "prompt_cache_miss_tokens": 20
        }))
        .unwrap();
        assert_eq!(openai.cache_hit_tokens(), 64);
        assert_eq!(deepseek.cache_hit_tokens(), 80);
    }

    #[test]
    fn serializes_tool_specs_in_stable_name_order() {
        let values = OpenAiProvider::request_tools(&[
            ToolSpec {
                name: "z_tool".to_owned(),
                description: "z".to_owned(),
                parameters: json!({"type": "object"}),
            },
            ToolSpec {
                name: "a_tool".to_owned(),
                description: "a".to_owned(),
                parameters: json!({"type": "object"}),
            },
        ]);
        assert_eq!(values[0]["function"]["name"], "a_tool");
        assert_eq!(values[1]["function"]["name"], "z_tool");
    }
}
