use std::collections::BTreeMap;
use std::env;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::debug;

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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderEvent {
    TextDelta(String),
}

#[async_trait]
pub trait Provider: Send + Sync {
    async fn chat(&self, messages: &[Message], tools: &[ToolSpec]) -> Result<Response>;

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        events: Option<mpsc::UnboundedSender<ProviderEvent>>,
    ) -> Result<Response> {
        let response = self.chat(messages, tools).await?;
        if let (Response::Text(text), Some(events)) = (&response, events) {
            let _ = events.send(ProviderEvent::TextDelta(text.clone()));
        }
        Ok(response)
    }
}

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
        let endpoint = if base_url
            .trim_end_matches('/')
            .ends_with("/chat/completions")
        {
            base_url.trim_end_matches('/').to_owned()
        } else {
            format!("{}/chat/completions", base_url.trim_end_matches('/'))
        };

        Ok(Self {
            client: reqwest::Client::new(),
            api_key,
            endpoint,
            model,
        })
    }

    fn request_messages(messages: &[Message]) -> Vec<Value> {
        messages
            .iter()
            .map(|message: &Message| {
                let mut value = json!({ "role": role_name(&message.role) });
                if !message.image_urls.is_empty() {
                    let mut parts = Vec::with_capacity(message.image_urls.len() + 1);
                    if let Some(content) = &message.content {
                        parts.push(json!({"type": "text", "text": content}));
                    }
                    parts.extend(message.image_urls.iter().map(
                        |url: &String| json!({"type": "image_url", "image_url": {"url": url}}),
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
                            .map(|call: &ToolCall| {
                                json!({
                                    "id": call.id,
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
                    value["tool_call_id"] = Value::String(tool_call_id.clone());
                }
                if let Some(name) = &message.name {
                    value["name"] = Value::String(name.clone());
                }
                value
            })
            .collect()
    }

    fn request_tools(tools: &[ToolSpec]) -> Vec<Value> {
        let mut ordered = tools.iter().collect::<Vec<&ToolSpec>>();
        ordered.sort_by(|left: &&ToolSpec, right: &&ToolSpec| left.name.cmp(&right.name));
        ordered
            .iter()
            .map(|tool: &&ToolSpec| {
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
}

#[async_trait]
impl Provider for OpenAiProvider {
    async fn chat(&self, messages: &[Message], tools: &[ToolSpec]) -> Result<Response> {
        self.chat_request(messages, tools, None).await
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        events: Option<mpsc::UnboundedSender<ProviderEvent>>,
    ) -> Result<Response> {
        self.chat_request(messages, tools, events).await
    }
}

impl OpenAiProvider {
    async fn chat_request(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        events: Option<mpsc::UnboundedSender<ProviderEvent>>,
    ) -> Result<Response> {
        let mut payload = json!({
            "model": self.model,
            "messages": Self::request_messages(messages),
            "stream": true,
        });
        if !tools.is_empty() {
            payload["tools"] = Value::Array(Self::request_tools(tools));
            payload["tool_choice"] = Value::String("auto".to_owned());
        }

        let response = self
            .client
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .json(&payload)
            .send()
            .await
            .context("调用 LLM 服务失败")?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.context("读取 LLM 错误响应失败")?;
            bail!("LLM 服务返回 {status}: {body}");
        }

        parse_sse(response, events).await
    }
}

#[derive(Default)]
struct PendingToolCall {
    id: String,
    name: String,
    arguments: String,
}

async fn parse_sse(
    response: reqwest::Response,
    events: Option<mpsc::UnboundedSender<ProviderEvent>>,
) -> Result<Response> {
    let mut stream = response.bytes_stream();
    let mut pending_bytes = Vec::new();
    let mut text = String::new();
    let mut calls: BTreeMap<usize, PendingToolCall> = BTreeMap::new();

    while let Some(chunk) = stream.next().await {
        pending_bytes.extend_from_slice(&chunk.context("读取 LLM 流式响应失败")?);
        while let Some(newline) = pending_bytes.iter().position(|byte: &u8| *byte == b'\n') {
            let mut line = pending_bytes.drain(..=newline).collect::<Vec<u8>>();
            while matches!(line.last(), Some(b'\n' | b'\r')) {
                line.pop();
            }
            consume_sse_line(&line, &mut text, &mut calls, events.as_ref())?;
        }
    }

    if !pending_bytes.is_empty() {
        consume_sse_line(&pending_bytes, &mut text, &mut calls, events.as_ref())?;
    }

    if calls.is_empty() {
        Ok(Response::Text(text))
    } else {
        let parsed = calls
            .into_values()
            .map(|call: PendingToolCall| {
                let arguments = if call.arguments.trim().is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(&call.arguments)
                        .with_context(|| format!("工具 {} 的 arguments 不是合法 JSON", call.name))?
                };
                Ok(ToolCall {
                    id: call.id,
                    name: call.name,
                    arguments,
                })
            })
            .collect::<Result<Vec<ToolCall>>>()?;
        Ok(Response::ToolCalls(parsed))
    }
}

fn consume_sse_line(
    line: &[u8],
    text: &mut String,
    calls: &mut BTreeMap<usize, PendingToolCall>,
    events: Option<&mpsc::UnboundedSender<ProviderEvent>>,
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
            prompt_tokens = usage.prompt_tokens,
            completion_tokens = usage.completion_tokens,
            prompt_cache_hit_tokens = usage.cache_hit_tokens(),
            prompt_cache_miss_tokens = usage.prompt_cache_miss_tokens.unwrap_or(0),
            "LLM token 用量"
        );
    }
    for choice in chunk.choices {
        if let Some(content) = choice.delta.content {
            if let Some(events) = events {
                let _ = events.send(ProviderEvent::TextDelta(content.clone()));
            }
            text.push_str(&content);
        }
        for delta_call in choice.delta.tool_calls {
            let call = calls.entry(delta_call.index).or_default();
            if let Some(id) = delta_call.id {
                call.id.push_str(&id);
            }
            if let Some(function) = delta_call.function {
                if let Some(name) = function.name {
                    call.name.push_str(&name);
                }
                if let Some(arguments) = function.arguments {
                    call.arguments.push_str(&arguments);
                }
            }
        }
    }
    Ok(())
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
                    .and_then(|details: &PromptTokenDetails| details.cached_tokens)
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

#[derive(Deserialize)]
struct StreamFunction {
    name: Option<String>,
    arguments: Option<String>,
}

fn required_env(name: &str) -> Result<String> {
    let value = env::var(name).with_context(|| format!("缺少环境变量 {name}"))?;
    if value.trim().is_empty() {
        bail!("环境变量 {name} 不能为空");
    }
    Ok(value)
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
    fn converts_messages_to_openai_shape() {
        let call = ToolCall {
            id: "call-1".to_owned(),
            name: "read_file".to_owned(),
            arguments: json!({"path": "README.md"}),
        };
        let messages = vec![
            Message::assistant_tool_calls(vec![call.clone()]),
            Message::tool_result(&call, "hello"),
        ];
        let values = OpenAiProvider::request_messages(&messages);

        assert_eq!(values[0]["tool_calls"][0]["id"], "call-1");
        assert_eq!(values[1]["tool_call_id"], "call-1");
        assert_eq!(values[1]["content"], "hello");
    }

    #[test]
    fn converts_user_images_to_openai_content_parts() {
        let messages = [Message::user_with_images(
            "描述图片",
            vec!["data:image/png;base64,AAAA".to_owned()],
        )];

        let values = OpenAiProvider::request_messages(&messages);

        assert_eq!(values[0]["role"], "user");
        assert_eq!(values[0]["content"][0]["type"], "text");
        assert_eq!(values[0]["content"][0]["text"], "描述图片");
        assert_eq!(values[0]["content"][1]["type"], "image_url");
        assert_eq!(
            values[0]["content"][1]["image_url"]["url"],
            "data:image/png;base64,AAAA"
        );
    }

    #[test]
    fn assembles_streamed_tool_call_fragments() {
        let mut text = String::new();
        let mut calls = BTreeMap::new();
        consume_sse_line(
            br#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"read_file","arguments":"{\"path\":"}}]}}]}"#,
            &mut text,
            &mut calls,
            None,
        )
        .unwrap();
        consume_sse_line(
            br#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"README.md\"}"}}]}}]}"#,
            &mut text,
            &mut calls,
            None,
        )
        .unwrap();

        let call = calls.get(&0).unwrap();
        assert_eq!(call.id, "call-1");
        assert_eq!(call.name, "read_file");
        assert_eq!(call.arguments, r#"{"path":"README.md"}"#);
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
        assert_eq!(deepseek.prompt_cache_miss_tokens, Some(20));
    }

    #[test]
    fn serializes_tool_specs_in_stable_name_order() {
        let specs = vec![
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
        ];

        let values = OpenAiProvider::request_tools(&specs);

        assert_eq!(values[0]["function"]["name"], "a_tool");
        assert_eq!(values[1]["function"]["name"], "z_tool");
    }
}
