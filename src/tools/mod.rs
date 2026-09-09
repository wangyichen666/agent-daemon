mod edit;
mod exec;
mod read;
mod write;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde_json::Value;
use thiserror::Error;

use crate::provider::{Message, ToolSpec};

pub use edit::EditFileTool;
pub use exec::ExecTool;
pub use read::ReadFileTool;
pub use write::WriteFileTool;

#[async_trait]
pub trait ToolCancellation: Send + Sync {
    fn is_cancelled(&self) -> bool;
    async fn cancelled(&self);
}

#[derive(Debug, Error)]
pub enum ToolAdmissionError {
    #[error("未知工具: {0}")]
    UnknownTool(String),
    #[error("工具 {tool} 参数校验失败: {message}")]
    InvalidArguments { tool: String, message: String },
}

impl ToolAdmissionError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::UnknownTool(_) => "unknown_tool",
            Self::InvalidArguments { .. } => "invalid_arguments",
        }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    fn is_read_only(&self) -> bool {
        false
    }
    async fn execute(&self, args: Value) -> Result<String>;

    async fn execute_rich(&self, args: Value) -> Result<ToolOutput> {
        self.execute(args).await.map(ToolOutput::text)
    }

    async fn execute_rich_with_cancellation(
        &self,
        args: Value,
        _cancellation: &dyn ToolCancellation,
    ) -> Result<ToolOutput> {
        self.execute_rich(args).await
    }
}

pub trait DynamicToolSource: Send + Sync {
    fn specs(&self) -> Vec<ToolSpec>;
    fn get(&self, name: &str) -> Option<Arc<dyn Tool>>;
}

pub struct ToolOutput {
    pub content: String,
    pub transient_messages: Vec<Message>,
}

impl ToolOutput {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            transient_messages: Vec::new(),
        }
    }
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    dynamic_sources: Vec<Arc<dyn DynamicToolSource>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<T>(&mut self, tool: T)
    where
        T: Tool + 'static,
    {
        self.tools.insert(tool.name().to_owned(), Arc::new(tool));
    }

    pub fn register_dynamic_source<T>(&mut self, source: Arc<T>)
    where
        T: DynamicToolSource + 'static,
    {
        self.dynamic_sources.push(source);
    }

    pub fn subset<'a>(&self, names: impl IntoIterator<Item = &'a str>) -> Result<Self> {
        let mut subset = Self::new();
        for name in names {
            let Some(tool) = self.tools.get(name) else {
                bail!("不可用的子 Agent 工具: {name}");
            };
            subset.tools.insert(name.to_owned(), tool.clone());
        }
        Ok(subset)
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self
            .tools
            .values()
            .map(|tool| ToolSpec {
                name: tool.name().to_owned(),
                description: tool.description().to_owned(),
                parameters: tool.parameters(),
            })
            .collect::<Vec<ToolSpec>>();
        specs.extend(
            self.dynamic_sources
                .iter()
                .flat_map(|source| source.specs()),
        );
        specs.sort_by(|left: &ToolSpec, right: &ToolSpec| left.name.cmp(&right.name));
        specs
    }

    #[allow(dead_code)]
    pub async fn execute(&self, name: &str, args: Value) -> Result<ToolOutput> {
        self.admit(name, &args)?;
        let tool = self
            .resolve(name)
            .ok_or_else(|| ToolAdmissionError::UnknownTool(name.to_owned()))?;
        tool.execute_rich(args).await
    }

    pub async fn execute_with_cancellation(
        &self,
        name: &str,
        args: Value,
        cancellation: &dyn ToolCancellation,
    ) -> Result<ToolOutput> {
        self.admit(name, &args)?;
        let tool = self
            .resolve(name)
            .ok_or_else(|| ToolAdmissionError::UnknownTool(name.to_owned()))?;
        tool.execute_rich_with_cancellation(args, cancellation)
            .await
    }

    pub fn admit_all(&self, calls: &[crate::provider::ToolCall]) -> Result<(), ToolAdmissionError> {
        for call in calls {
            self.admit(&call.name, &call.arguments)?;
        }
        Ok(())
    }

    fn admit(&self, name: &str, args: &Value) -> Result<(), ToolAdmissionError> {
        let Some(tool) = self.resolve(name) else {
            return Err(ToolAdmissionError::UnknownTool(name.to_owned()));
        };
        validate_value(&tool.parameters(), args, "$args").map_err(|message| {
            ToolAdmissionError::InvalidArguments {
                tool: name.to_owned(),
                message,
            }
        })
    }

    pub fn is_read_only(&self, name: &str) -> bool {
        self.resolve(name).is_some_and(|tool| tool.is_read_only())
    }

    fn resolve(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned().or_else(|| {
            self.dynamic_sources
                .iter()
                .find_map(|source| source.get(name))
        })
    }
}

fn validate_value(schema: &Value, value: &Value, path: &str) -> Result<(), String> {
    if let Some(expected) = schema.get("type").and_then(Value::as_str) {
        let valid = match expected {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "number" => value.is_number(),
            "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
            "boolean" => value.is_boolean(),
            "null" => value.is_null(),
            _ => true,
        };
        if !valid {
            return Err(format!("{path} 应为 {expected}"));
        }
    }

    let Some(object) = value.as_object() else {
        return Ok(());
    };
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for name in required.iter().filter_map(Value::as_str) {
            if !object.contains_key(name) {
                return Err(format!("{path} 缺少必填字段 {name}"));
            }
        }
    }

    let properties = schema.get("properties").and_then(Value::as_object);
    if let Some(properties) = properties {
        for (name, child) in object {
            if let Some(child_schema) = properties.get(name) {
                validate_value(child_schema, child, &format!("{path}.{name}"))?;
            } else if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
                return Err(format!("{path} 不允许额外字段 {name}"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validator_is_permissive_unless_extra_fields_are_explicitly_forbidden() {
        let permissive = json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        });
        assert!(validate_value(&permissive, &json!({"path": "a", "extra": 1}), "$args").is_ok());

        let strict = json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
            "additionalProperties": false
        });
        assert!(validate_value(&strict, &json!({"path": "a", "extra": 1}), "$args").is_err());
        assert!(validate_value(&strict, &json!({}), "$args").is_err());
        assert!(validate_value(&strict, &json!({"path": 42}), "$args").is_err());
    }
}
