use std::collections::BTreeMap;

use crate::provider::{
    ExecutionIdentity, Message, Provider, ProviderEvent, Response, ToolArgumentsFragment, ToolCall,
    ToolCallStreamError, ToolSpec,
};

pub async fn collect_provider_response(
    provider: &dyn Provider,
    messages: &[Message],
    tools: &[ToolSpec],
) -> anyhow::Result<Response> {
    let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let request = provider.chat_stream(messages, tools, events);
    tokio::pin!(request);
    let mut assembler = ToolCallAssembler::default();
    loop {
        tokio::select! {
            result = &mut request => {
                result?;
                break;
            }
            event = receiver.recv() => {
                if let Some(event) = event {
                    assembler.accept(event);
                }
            }
        }
    }
    while let Ok(event) = receiver.try_recv() {
        assembler.accept(event);
    }
    Ok(assembler.finish())
}

#[derive(Default)]
pub struct ToolCallAssembler {
    text: String,
    calls: BTreeMap<ExecutionIdentity, PendingCall>,
    order: Vec<ExecutionIdentity>,
    failure: Option<ToolCallStreamError>,
}

struct PendingCall {
    name: String,
    arguments: String,
    snapshot_seen: bool,
    completed: bool,
}

impl ToolCallAssembler {
    pub fn accept(&mut self, event: ProviderEvent) -> Option<String> {
        if self.failure.is_some() {
            return match event {
                ProviderEvent::TextDelta(delta) => Some(delta),
                _ => None,
            };
        }
        match event {
            ProviderEvent::TextDelta(delta) => {
                self.text.push_str(&delta);
                Some(delta)
            }
            ProviderEvent::ToolCallStarted { exec_id, name } => {
                if name.trim().is_empty() {
                    self.fail("missing_name", "工具调用名称为空");
                } else if self.calls.contains_key(&exec_id) {
                    self.fail(
                        "identity_conflict",
                        format!("execution identity 重复开始：{}", exec_id.encode()),
                    );
                } else {
                    self.order.push(exec_id.clone());
                    self.calls.insert(
                        exec_id,
                        PendingCall {
                            name,
                            arguments: String::new(),
                            snapshot_seen: false,
                            completed: false,
                        },
                    );
                }
                None
            }
            ProviderEvent::ToolCallDelta { exec_id, fragment } => {
                let Some(call) = self.calls.get_mut(&exec_id) else {
                    self.fail(
                        "anonymous_delta",
                        format!("arguments 分片没有已开始的调用：{}", exec_id.encode()),
                    );
                    return None;
                };
                if call.completed {
                    self.fail(
                        "delta_after_complete",
                        format!("已完成调用收到迟到分片：{}", exec_id.encode()),
                    );
                    return None;
                }
                match fragment {
                    ToolArgumentsFragment::Append(fragment) => {
                        if call.snapshot_seen {
                            self.fail(
                                "delta_after_snapshot",
                                format!("权威快照后收到增量分片：{}", exec_id.encode()),
                            );
                        } else {
                            // 纯有序逐字节追加。绝不嗅探 JSON、比较长度或替换累计前缀。
                            call.arguments.push_str(&fragment);
                        }
                    }
                    ToolArgumentsFragment::AuthoritativeSnapshot(snapshot) => {
                        if call.snapshot_seen {
                            self.fail(
                                "duplicate_snapshot",
                                format!("调用收到重复权威快照：{}", exec_id.encode()),
                            );
                        } else {
                            call.arguments = snapshot;
                            call.snapshot_seen = true;
                        }
                    }
                }
                None
            }
            ProviderEvent::ToolCallCompleted { exec_id } => {
                let Some(call) = self.calls.get_mut(&exec_id) else {
                    self.fail(
                        "complete_without_start",
                        format!("工具完成事件没有对应开始：{}", exec_id.encode()),
                    );
                    return None;
                };
                if call.completed {
                    self.fail(
                        "duplicate_complete",
                        format!("工具调用重复完成：{}", exec_id.encode()),
                    );
                } else {
                    call.completed = true;
                }
                None
            }
            ProviderEvent::ToolCallFailed(error) => {
                self.failure = Some(error);
                None
            }
        }
    }

    pub fn finish(self) -> Response {
        if let Some(error) = self.failure {
            return Response::ToolAssemblyFailed(error);
        }
        for identity in &self.order {
            if self.calls.get(identity).is_some_and(|call| !call.completed) {
                return Response::ToolAssemblyFailed(ToolCallStreamError::new(
                    "incomplete_call",
                    format!("流结束时工具调用未完成：{}", identity.encode()),
                ));
            }
        }
        if self.calls.is_empty() {
            return Response::Text(self.text);
        }
        let mut output = Vec::with_capacity(self.order.len());
        for identity in self.order {
            let Some(call) = self.calls.get(&identity) else {
                return Response::ToolAssemblyFailed(ToolCallStreamError::new(
                    "identity_conflict",
                    "工具调用顺序索引与状态表不一致",
                ));
            };
            let arguments = if call.arguments.trim().is_empty() {
                serde_json::json!({})
            } else {
                match serde_json::from_str(&call.arguments) {
                    Ok(arguments) => arguments,
                    Err(error) => {
                        return Response::ToolAssemblyFailed(ToolCallStreamError::new(
                            "invalid_json",
                            format!("工具 {} 的 arguments 不是合法 JSON：{error}", call.name),
                        ));
                    }
                }
            };
            output.push(ToolCall {
                id: identity.encode(),
                name: call.name.clone(),
                arguments,
            });
        }
        Response::ToolCalls(output)
    }

    fn fail(&mut self, code: &str, message: impl Into<String>) {
        if self.failure.is_none() {
            self.failure = Some(ToolCallStreamError::new(code, message));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ApiType, ProviderEvent, ToolArgumentsFragment};
    use serde_json::json;

    fn identity(id: &str) -> ExecutionIdentity {
        ExecutionIdentity::wire(ApiType::OpenaiChat, id)
    }

    #[test]
    fn appends_even_when_one_fragment_is_valid_json() {
        let id = identity("trap");
        let mut assembler = ToolCallAssembler::default();
        assembler.accept(ProviderEvent::ToolCallStarted {
            exec_id: id.clone(),
            name: "demo".to_owned(),
        });
        assembler.accept(ProviderEvent::ToolCallDelta {
            exec_id: id.clone(),
            fragment: ToolArgumentsFragment::Append(r#"{"a":1}"#.to_owned()),
        });
        assembler.accept(ProviderEvent::ToolCallDelta {
            exec_id: id.clone(),
            fragment: ToolArgumentsFragment::Append(r#"{"a":2}"#.to_owned()),
        });
        assembler.accept(ProviderEvent::ToolCallCompleted { exec_id: id });

        let Response::ToolAssemblyFailed(error) = assembler.finish() else {
            panic!("合法 JSON 分片不得被提拔为完整快照");
        };
        assert_eq!(error.code, "invalid_json");
    }

    #[test]
    fn authoritative_snapshot_replaces_even_a_longer_prefix() {
        let id = identity("snapshot");
        let mut assembler = ToolCallAssembler::default();
        assembler.accept(ProviderEvent::ToolCallStarted {
            exec_id: id.clone(),
            name: "demo".to_owned(),
        });
        assembler.accept(ProviderEvent::ToolCallDelta {
            exec_id: id.clone(),
            fragment: ToolArgumentsFragment::Append(r#"{"long":"prefix"}"#.to_owned()),
        });
        assembler.accept(ProviderEvent::ToolCallDelta {
            exec_id: id.clone(),
            fragment: ToolArgumentsFragment::AuthoritativeSnapshot("{}".to_owned()),
        });
        assembler.accept(ProviderEvent::ToolCallCompleted { exec_id: id });
        let Response::ToolCalls(calls) = assembler.finish() else {
            panic!("权威快照应完成装配");
        };
        assert_eq!(calls[0].arguments, json!({}));
    }

    #[test]
    fn rejects_duplicate_completion_and_incomplete_eof() {
        let id = identity("duplicate");
        let mut duplicate = ToolCallAssembler::default();
        duplicate.accept(ProviderEvent::ToolCallStarted {
            exec_id: id.clone(),
            name: "demo".to_owned(),
        });
        duplicate.accept(ProviderEvent::ToolCallCompleted {
            exec_id: id.clone(),
        });
        duplicate.accept(ProviderEvent::ToolCallCompleted { exec_id: id });
        assert!(matches!(
            duplicate.finish(),
            Response::ToolAssemblyFailed(ToolCallStreamError { code, .. }) if code == "duplicate_complete"
        ));

        let mut incomplete = ToolCallAssembler::default();
        incomplete.accept(ProviderEvent::ToolCallStarted {
            exec_id: identity("incomplete"),
            name: "demo".to_owned(),
        });
        assert!(matches!(
            incomplete.finish(),
            Response::ToolAssemblyFailed(ToolCallStreamError { code, .. }) if code == "incomplete_call"
        ));
    }

    #[test]
    fn provider_boundary_contains_no_tool_specific_or_second_assembler_logic() {
        for source in [
            include_str!("provider/openai.rs"),
            include_str!("provider/anthropic.rs"),
            include_str!("provider/ollama.rs"),
        ] {
            for forbidden in [
                "write_file",
                "edit_file",
                "arguments.push_str",
                "arguments.len()",
                "arguments.replace",
            ] {
                assert!(
                    !source.contains(forbidden),
                    "provider 层出现禁止边界逻辑：{forbidden}"
                );
            }
        }
    }
}
